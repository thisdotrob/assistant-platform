//! Serve live turns driven by Slack Socket Mode inbound.
//!
//! This is the inbound counterpart to the terminal [`crate::run`] loop: instead
//! of stdin lines, turns are driven by Slack events arriving over Socket Mode.
//! Each Slack channel maps to its own per-channel session and container (created
//! lazily on first inbound), so two channels never share a container; the bot's
//! reply is posted back over the same [`SlackChannel`].
//!
//! The engine ([`serve_slack`]) is transport-injectable — it takes a
//! [`SocketOpener`] — so the full inbound→turn→deliver path is covered offline
//! with a scripted opener and a fake Web API, no websocket and no network. The
//! live websocket transport (`TungsteniteOpener`) and the one-call
//! [`serve_slack_live`] entry are compiled only under the non-default
//! `socket-mode` feature, mirroring how the real Docker runtime is gated.

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assistant_channel_slack::{
    run_listener, SlackApi, SlackChannel, SlackIdentity, SocketOpener,
};
use assistant_db::open_central;
use assistant_memory::{SourceChannel, SourceRef};
use assistant_permissions::{evaluate_sender, UnknownPolicy};
use assistant_router::{
    engagement_key, evaluate_engagement, expire_sticky, has_active_sticky, open_sticky, record_drop,
    ChannelAdapter, DeliveryTarget, DropReason, EngagementContext, EngagementDecision,
    EngagementMode, IgnoredMessagePolicy, MessageState, OutboundContent, RoutingEvent,
    SenderScope,
};
use assistant_runtime_docker::ContainerRuntime;
use assistant_scheduler::{
    advance_recurrence, cancel_item, claim_due, complete_item, complete_occurrence, latest_meta,
    list_items, pause_item, repair_session_projection, resume_item, upsert_item, write_meta,
    ContextPolicy, EpochSecs, LifecycleTransition, Occurrence, ProjectedItem, ProjectionError,
    Recurrence, ScheduleIntent, ScheduleStatus, ScheduledMessageMeta, Weekday,
};
use assistant_agent_protocol::CalendarRecurrence;
use assistant_session::{session_exists, InboundMessage, OutboundMessage, SessionLayout};
use rusqlite::Connection;

use crate::error::HostError;
use crate::run::{Host, HostConfig};
use crate::HOST_AGENT_GROUP;
use assistant_specialist_spec::SpecialistSpec;

/// How long a sticky-engagement window stays open after an engaging message, so
/// follow-ups in the same conversation keep the agent engaged without a fresh
/// mention. Each engaging message slides the window forward.
const STICKY_TTL_SECS: i64 = 3600;

/// How many recent inbound dedupe keys to remember. Bounds the dedup set on a
/// long-lived daemon; comfortably covers Slack's redelivery window across every
/// active channel.
const SEEN_CAPACITY: usize = 1024;

/// Bounded record of recently-handled inbound dedupe keys.
///
/// Slack Socket Mode is at-least-once: the same event can be redelivered — most
/// often around the periodic connection refresh, or whenever an ACK races a
/// connection teardown — and without this guard each redelivery would drive a
/// fresh turn and post a duplicate reply. Keys are remembered in arrival order
/// and evicted oldest-first past `capacity`, so the set stays bounded. In-memory
/// only: a process restart starts empty, so a redelivery that spans a restart
/// can still double-fire (rare, and bounded by Slack's short redelivery window).
struct RecentlySeen {
    order: VecDeque<String>,
    keys: HashSet<String>,
    capacity: usize,
}

impl RecentlySeen {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            keys: HashSet::new(),
            capacity,
        }
    }

    /// Record `key`. Returns `true` when it was newly inserted, `false` when it
    /// was already present (a redelivery to drop).
    fn insert(&mut self, key: &str) -> bool {
        if self.keys.contains(key) {
            return false;
        }
        if self.order.len() >= self.capacity
            && let Some(old) = self.order.pop_front()
        {
            self.keys.remove(&old);
        }
        self.order.push_back(key.to_string());
        self.keys.insert(key.to_string());
        true
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Drives the scheduler from the serve loop's idle windows. When set, due
/// scheduled items fire on the same thread that reads inbound frames, reusing
/// each channel's existing [`Host`] (so a fired turn never collides with the
/// per-channel container or races its session DBs).
pub struct SchedulerTickConfig {
    /// Lease owner recorded when this daemon claims a due occurrence. Stable per
    /// installation so a restarted daemon can reclaim its own stale leases.
    pub owner: String,
    /// How long a claimed occurrence is leased before another claimer may take
    /// it. Must exceed the worst-case turn duration so a slow turn's lease does
    /// not expire mid-run.
    pub lease_ttl_secs: i64,
    /// Minimum wall-clock between scheduler sweeps. Throttles the (sub-second)
    /// idle-read cadence down to a sane DB-poll interval.
    pub tick_interval: Duration,
}

/// Inputs for a Slack serve session.
pub struct SlackServeOptions {
    /// The instance's sessions directory; every channel's session DBs live here.
    pub sessions_dir: PathBuf,
    /// The session group every Slack channel shares (the per-channel session id
    /// is the Slack channel id).
    pub group: String,
    /// Base per-session host config, cloned for each channel's [`Host`].
    pub config: HostConfig,
    /// This installation's central DB (permissions, sticky-engagement, the
    /// dropped-message audit). Opened once for the serve session and read on
    /// every inbound event to gate it.
    pub central_db_path: PathBuf,
    /// How a message engages the agent. Defaults to [`EngagementMode::MentionSticky`]
    /// on the live path: a mention/DM engages, and follow-ups stay engaged while
    /// a sticky window is open.
    pub engagement: EngagementMode,
    /// What to do with a sender the instance does not know. Deny-by-default
    /// ([`UnknownPolicy::Strict`]) on the live path.
    pub policy: UnknownPolicy,
    /// When set, fire due scheduled items from the serve loop's idle windows.
    /// `None` leaves the daemon purely inbound-reactive (the offline tests and
    /// any non-scheduling caller).
    pub scheduler: Option<SchedulerTickConfig>,
    /// The registered specialist agents, resolved by `route_name`: a scheduled
    /// `run_as` item runs as the named agent, and an agent-to-agent `send_message`
    /// is routed to the named agent. Empty for the offline tests and any caller
    /// that registers no specialists.
    pub specialists: Vec<SpecialistSpec>,
}

/// The `send_message` destination name that resolves to the Slack-wired
/// orchestrator. A non-Slack-wired agent that sends here is telling the
/// orchestrator what happened; the orchestrator then runs a turn and decides
/// whether to post anything to Slack and, if so, which channel. It is the only
/// destination a non-Slack-wired agent may use to reach the human.
pub(crate) const ORCHESTRATOR_DESTINATION: &str = "orchestrator";

/// The session an agent's report to the orchestrator is handled in, keyed by the
/// reporting agent's session so each agent gets a stable orchestrator
/// conversation thread. It runs the orchestrator (not a specialist), so the
/// orchestrator can reason over the report and post to a channel of its choosing.
fn orchestrator_relay_session(conversation_root: &str) -> String {
    format!("a2a-orchestrator-{conversation_root}")
}

/// The stable conversation root of a session id: strip any leading
/// `a2a-<agent>-` relay prefixes so a multi-hop A2A exchange reuses one session
/// per `(target, root)` pair instead of nesting the whole chain into an
/// ever-growing id (e.g. `a2a-orchestrator-a2a-se-a2a-orchestrator-standing`).
/// `agents` is the set of routable agent names (specialists + the orchestrator)
/// so a route name containing a `-` is stripped as a whole, not split.
fn conversation_root<'a>(session_id: &'a str, agents: &[String]) -> &'a str {
    let mut root = session_id;
    while let Some(rest) = root.strip_prefix("a2a-") {
        match agents
            .iter()
            .filter_map(|a| rest.strip_prefix(a.as_str()))
            .find_map(|r| r.strip_prefix('-'))
        {
            Some(inner) => root = inner,
            None => break,
        }
    }
    root
}

/// A finished background `run_as` scheduled turn, reported from a worker thread
/// to the serve loop's drain. A `run_as` turn (an agent, not the orchestrator,
/// firing a scheduled item) runs in the background because real work — e.g. an SE
/// agent implementing an issue — takes far longer than the serve loop can block
/// on. The drain processes the turn's side-effect replies and finalizes the
/// occurrence. All fields owned (`Send`) so it crosses the worker→serve channel.
struct CompletedRunAs {
    /// The scheduled item that fired (carries its session, `run_as` route, and
    /// recurrence for finalization).
    item: ProjectedItem,
    /// The occurrence to finalize once the turn's effects are applied.
    occurrence: Occurrence,
    /// The turn's replies (side-effect rows to apply), or a failure reason.
    replies: Result<Vec<OutboundMessage>, String>,
}

/// Serve Slack turns until `stop` returns true.
///
/// Authenticates `channel` (its resolved bot identity drives self-author
/// filtering), then drives the Socket Mode `opener`: each routable, non-self
/// message is dispatched to a per-channel [`Host`] (spawned lazily via
/// `runtime_factory`), and every reply is posted back over `channel`, threaded
/// under the triggering message.
///
/// Returns `Err` only on an unrecoverable listener fault (e.g. a rejected app
/// token); a per-turn failure (session derive, turn, or delivery) is logged and
/// the loop continues, so one bad message never tears down the session.
pub fn serve_slack<A, R, F>(
    opener: &mut dyn SocketOpener,
    channel: &mut SlackChannel<A>,
    opts: SlackServeOptions,
    runtime_factory: F,
    stop: &dyn Fn() -> bool,
) -> Result<(), HostError>
where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    channel
        .start()
        .map_err(|e| HostError::Channel(e.to_string()))?;
    let identity = SlackIdentity {
        bot_user_id: channel.bot_user_id().unwrap_or_default().to_string(),
        self_bot_id: channel.self_bot_id().map(str::to_string),
    };

    // Open the central DB once for the whole session: every inbound event is
    // gated against it (sender permissions + engagement/sticky), so failing to
    // open it is fatal — serving ungated would bypass the deny-by-default gate.
    let conn = open_central(&opts.central_db_path).map_err(|e| HostError::Db(e.to_string()))?;

    // Before serving, rebuild the central scheduled-work projection from each
    // session's per-session source of truth, so a schedule survives a central
    // loss or a crash-torn write. Idempotent: with both already in sync it is a
    // no-op. Only meaningful when the scheduler sweeps (which also guarantees the
    // projection tables exist), so it is gated on a configured scheduler.
    if opts.scheduler.is_some() {
        repair_existing_sessions(&conn, &opts);
    }

    // A shared reborrow for the sink: `start` is done, and `deliver` only needs
    // `&self`, so the listener can hold the channel immutably while it runs.
    let channel_ref: &SlackChannel<A> = channel;
    // `RefCell` because both the inbound `sink` and the scheduler `tick` need to
    // get-or-create per-channel hosts, and `run_listener` holds both closures at
    // once. The loop is single-threaded and never calls them re-entrantly, so the
    // borrows never overlap at runtime.
    let hosts: RefCell<HashMap<String, Host<R>>> = RefCell::new(HashMap::new());
    // Drop Slack's at-least-once redeliveries before they drive a second turn.
    // Only the inbound `sink` touches this, so a plain `&mut` capture suffices.
    let mut seen = RecentlySeen::new(SEEN_CAPACITY);

    // Worker handles for background turns, so shutdown can join them.
    // Empty/unused when nothing runs in the background.
    let inflight: RefCell<Vec<JoinHandle<()>>> = RefCell::new(Vec::new());

    // Background `run_as` scheduled turns: a long-running agent turn (an agent, not
    // the orchestrator, firing a scheduled item) runs on a worker thread and
    // reports over `runas_tx`; the serve loop drains `runas_rx` to apply its
    // effects and finalize the occurrence. `run_as_inflight` holds the occurrence
    // keys currently running so a lease that expires mid-turn (real work outlasts
    // the lease TTL) is not re-fired. Worker handles reuse `inflight` for the
    // shutdown join.
    let (runas_tx, runas_rx) = std::sync::mpsc::channel::<CompletedRunAs>();
    let run_as_inflight: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    let result = {
        let mut sink = |event: RoutingEvent| {
            handle_event(
                event,
                &conn,
                &hosts,
                channel_ref,
                &opts,
                &runtime_factory,
                &mut seen,
            );
        };
        // Throttle the sub-second idle cadence down to the configured sweep
        // interval. With no scheduler configured the scheduler sweep is a no-op.
        let tick_interval = opts.scheduler.as_ref().map(|s| s.tick_interval);
        let mut last_tick: Option<Instant> = None;
        let mut tick = || {
            // Drain finished background `run_as` turns every idle window
            // (unthrottled), so a long agent turn's effects apply and its
            // occurrence finalizes as soon as it completes, independent of the
            // sweep cadence.
            drain_completed_runas(
                &runas_rx,
                &run_as_inflight,
                &conn,
                &hosts,
                channel_ref,
                &opts,
                &runtime_factory,
            );
            let Some(interval) = tick_interval else {
                return;
            };
            if last_tick.is_some_and(|t| t.elapsed() < interval) {
                return;
            }
            last_tick = Some(Instant::now());
            scheduler_tick(
                &conn,
                &hosts,
                channel_ref,
                &opts,
                &runtime_factory,
                &inflight,
                &runas_tx,
                &run_as_inflight,
            );
        };
        run_listener(opener, &identity, stop, &mut sink, &mut tick)
            .map_err(|e| HostError::Channel(e.to_string()))
    };

    // Shutdown drain: join every in-flight background worker (each reaps its own
    // container, bounded by the turn timeout), then run one final drain so a turn
    // that finished after the last idle tick still applies its effects before we
    // tear the per-channel hosts down.
    for handle in inflight.into_inner() {
        if let Err(err) = handle.join() {
            eprintln!("slack: a background worker panicked: {err:?}");
        }
    }
    // Final `run_as` drain: apply the effects of any background turn that finished
    // after the last idle tick (or during the shutdown join) before tearing down.
    drain_completed_runas(
        &runas_rx,
        &run_as_inflight,
        &conn,
        &hosts,
        channel_ref,
        &opts,
        &runtime_factory,
    );

    // Best-effort: stop every container this session spawned before returning.
    for (_chat, mut host) in hosts.into_inner() {
        let _ = host.shutdown();
    }
    result
}

/// Dispatch one routed Slack event: ensure a [`Host`] for its channel, run the
/// turn, and deliver each reply back into the same channel/thread. Failures are
/// logged and swallowed so the serve loop survives a single bad turn.
fn handle_event<A, R, F>(
    event: RoutingEvent,
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    seen: &mut RecentlySeen,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    // Drop the bot's own posts (the echo-loop guard) and anything that is not a
    // new message: edits, deletes, and reactions do not drive a turn in this
    // first cut. An empty body (e.g. a bare mention) has nothing to process.
    if event.is_self_author || event.state != MessageState::New || event.text.trim().is_empty() {
        return;
    }

    // Drop a redelivery of an event we've already handled. Marking it seen here —
    // before the gate and the turn — keeps a redelivery from re-running the gate
    // (double-auditing a drop, sliding a sticky window) or, if the first turn
    // failed, enqueuing a second inbound row that the container would answer
    // twice. The loop is single-threaded, so deliveries never overlap: a
    // redelivery is always read after the original's turn has fully returned.
    if !seen.insert(&event.dedupe_key) {
        return;
    }

    if !passes_gate(conn, &event, opts) {
        return;
    }

    // Compute the thread root before building the inbound message: it's needed
    // both as the `thread_id` stored on the inbound row (so the container can
    // filter history to this thread) and as the delivery `thread_root_id`.
    let thread_root = event
        .thread_root_id
        .clone()
        .unwrap_or_else(|| event.platform_message_id.clone());
    let inbound = InboundMessage {
        sender: event.sender_id.clone(),
        content: event.text.clone(),
        metadata: None,
        thread_id: Some(thread_root.clone()),
    };
    // Hold the hosts borrow only for the turn; the owned replies outlive it, so
    // delivery (which never touches `hosts`) runs after the borrow is released.
    let replies = {
        let mut map = hosts.borrow_mut();
        let host = match host_for(&mut map, &event.chat_id, opts, runtime_factory) {
            Some(host) => host,
            None => return,
        };
        match host.run_turn(&inbound) {
            Ok(replies) => replies,
            Err(err) => {
                eprintln!("slack: turn failed for channel {}: {err}", event.chat_id);
                return;
            }
        }
    };
    let target = DeliveryTarget {
        chat_id: event.chat_id.clone(),
        thread_root_id: Some(thread_root),
    };
    for reply in &replies {
        deliver_reply(
            conn,
            channel,
            &target,
            &event.chat_id,
            opts,
            reply,
            Some(&event.sender_id),
            None,
        );
    }
}

/// Get the per-channel [`Host`], creating (and lazily attaching) it on first use.
/// Shared by the inbound path and the scheduler tick so a channel always drives
/// one container under one session — a scheduled turn reuses the same `Host` as
/// inbound traffic rather than racing a second one. `None` on a layout error
/// (already logged); the caller skips that turn.
fn host_for<'m, R, F>(
    hosts: &'m mut HashMap<String, Host<R>>,
    session_id: &str,
    opts: &SlackServeOptions,
    runtime_factory: &F,
) -> Option<&'m mut Host<R>>
where
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
{
    match hosts.entry(session_id.to_string()) {
        Entry::Occupied(existing) => Some(existing.into_mut()),
        Entry::Vacant(slot) => {
            let layout = match SessionLayout::derive(&opts.sessions_dir, &opts.group, session_id) {
                Ok(layout) => layout,
                Err(err) => {
                    eprintln!("slack: cannot derive a session for channel {session_id}: {err}");
                    return None;
                }
            };
            Some(slot.insert(Host::new(layout, runtime_factory(), opts.config.clone())))
        }
    }
}

/// Rebuild the central projection for every existing session under this group
/// from its per-session source of truth. Run once at serve startup before the
/// listener: a schedule whose central row was lost (crash between the session
/// write and the projection upsert) is reconstructed, and a phantom-fired
/// occurrence is reset so it can fire. Each session is independent — one bad
/// session is logged and skipped, never aborting the rest. Sessions with no
/// scheduling rows reproject nothing.
fn repair_existing_sessions(conn: &Connection, opts: &SlackServeOptions) {
    let group_dir = opts.sessions_dir.join(&opts.group);
    let entries = match std::fs::read_dir(&group_dir) {
        Ok(entries) => entries,
        // No sessions yet (first run) is the common case, not an error.
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let session_id = entry.file_name().to_string_lossy().into_owned();
        let layout = match SessionLayout::derive(&opts.sessions_dir, &opts.group, &session_id) {
            Ok(layout) => layout,
            Err(err) => {
                eprintln!("slack: skipping repair for {session_id}: {err}");
                continue;
            }
        };
        if !session_exists(&layout) {
            continue;
        }
        match repair_session_projection(conn, &layout, &session_id) {
            Ok(report) if report.items_reprojected > 0 || report.occurrences_unfired > 0 => {
                eprintln!(
                    "slack: repaired schedules for {session_id}: {} item(s) reprojected, {} occurrence(s) unfired",
                    report.items_reprojected, report.occurrences_unfired
                );
            }
            Ok(_) => {}
            Err(err) => eprintln!("slack: repairing schedules for {session_id} failed: {err}"),
        }
    }
}

/// Fire any due scheduled items, exactly once each. Runs on the serve thread
/// between inbound frames (see [`SchedulerTickConfig`]). Sticky-engagement
/// windows are expired on the same cadence (the daemon has no other driver for
/// that maintenance). A one-off is prevented from re-firing by the scheduler's
/// own already-fired guard; a recurring item advances its projected
/// `process_after` to the next scheduled time after firing, so the next sweep
/// claims the following occurrence (drift-free — the advance is anchored to the
/// occurrence's scheduled time, not when it actually ran).
#[allow(clippy::too_many_arguments)]
fn scheduler_tick<A, R, F>(
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    inflight: &RefCell<Vec<JoinHandle<()>>>,
    runas_tx: &Sender<CompletedRunAs>,
    run_as_inflight: &RefCell<HashSet<String>>,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    let Some(cfg) = opts.scheduler.as_ref() else {
        return;
    };
    let now = now_epoch();

    // Keep engagement windows fresh on the same cadence; a failure only costs
    // stickiness hygiene, not scheduling.
    if let Err(err) = expire_sticky(conn, now) {
        eprintln!("slack: expiring sticky windows failed: {err}");
    }

    let leases = match claim_due(conn, now, &cfg.owner, cfg.lease_ttl_secs) {
        Ok(leases) => leases,
        Err(err) => {
            eprintln!("slack: scheduler claim failed: {err}");
            return;
        }
    };
    if leases.is_empty() {
        return;
    }

    // Resolve each claimed occurrence back to its item to recover the target
    // session and intent text. Only active items are claimable.
    let items = match list_items(conn, HOST_AGENT_GROUP, Some(ScheduleStatus::Active)) {
        Ok(items) => items,
        Err(err) => {
            eprintln!("slack: scheduler list_items failed: {err}");
            return;
        }
    };

    for lease in leases {
        let Some(item) = items
            .iter()
            .find(|it| it.id == lease.occurrence.scheduled_item_id)
        else {
            // The item is no longer active/visible; let the lease expire.
            continue;
        };
        let Some(session_id) = item.session_id.clone() else {
            eprintln!("slack: scheduled item {} has no session; skipping", item.id);
            continue;
        };

        // Pre-task gate: run the command before deciding to fire. Empty stdout
        // or non-zero exit → skip (advance schedule without inference). Non-empty
        // stdout → fire, injecting that stdout as the turn's metadata context.
        // A command that fails to spawn is non-fatal but leaves the lease to expire.
        let gate_metadata: Option<String>;
        if let Some(gate_cmd) = &item.gate_command {
            let outcome = match &item.gate_onecli_agent {
                Some(agent) => {
                    let default_image = opts.config.image.reference();
                    let image = item.gate_image.as_deref().unwrap_or(&default_image);
                    crate::scheduler::run_gate_in_container(
                        gate_cmd,
                        agent,
                        image,
                        &opts.config.onecli_ca_dir,
                        &item.gate_volumes,
                    )
                }
                None => crate::scheduler::run_gate(gate_cmd),
            };
            match outcome {
                Ok(crate::scheduler::GateOutcome::Skip) => {
                    if let Err(err) = finalize_firing(conn, &lease.occurrence, item, now) {
                        eprintln!("slack: finalizing gated-skip for channel {session_id}: {err}");
                    }
                    continue;
                }
                Ok(crate::scheduler::GateOutcome::Fire(metadata)) => {
                    gate_metadata = metadata;
                }
                Err(e) => {
                    eprintln!(
                        "slack: gate command error for item {}, leaving for retry: {e}",
                        lease.occurrence.scheduled_item_id
                    );
                    continue;
                }
            }
        } else {
            gate_metadata = None;
        }

        // A `run_as` item fires as a named agent (its own container/config) rather
        // than the orchestrator. Resolve the spec up front; a `run_as` that names
        // no registered agent is an error we skip (never a silent orchestrator
        // fallback), leaving the lease to expire so a fixed registration retries.
        let run_as = item.run_as.as_deref();
        let run_as_spec = run_as.and_then(|route| {
            opts.specialists.iter().find(|s| s.route_name == route)
        });
        if run_as.is_some() && run_as_spec.is_none() {
            eprintln!(
                "slack: scheduled item {} run_as={run_as:?} names no registered agent; skipping",
                item.id
            );
            continue;
        }

        // A background `run_as` turn for this occurrence may still be running: real
        // work (implementing an issue) outlasts the lease TTL, so the same
        // occurrence gets re-claimed on lease expiry. Skip re-firing it — the
        // worker finalizes the occurrence when it completes.
        if run_as_spec.is_some()
            && run_as_inflight
                .borrow()
                .contains(&lease.occurrence.idempotency_key)
        {
            continue;
        }

        // The specialist shim reads only the inbound content as its goal (it does
        // not consume the metadata channel the orchestrator uses), so for a
        // `run_as` turn fold the gate's JSON into the content; the orchestrator
        // path keeps passing it as metadata.
        let (content, metadata) = match (run_as_spec, &gate_metadata) {
            (Some(_), Some(md)) => (
                format!("{}\n\n<gate_context>\n{md}\n</gate_context>", item.intent),
                None,
            ),
            (Some(_), None) => (item.intent.clone(), None),
            (None, _) => (item.intent.clone(), gate_metadata),
        };
        let inbound = InboundMessage {
            sender: "scheduler".to_string(),
            content,
            metadata,
            thread_id: None,
        };

        // Fire. A `run_as` turn runs in the BACKGROUND — real agent work (e.g.
        // implementing an issue) far outlasts what the single serve thread can
        // block on — and its occurrence is finalized from the drain once it
        // completes. The orchestrator path runs inline (turns are quick) and
        // finalizes immediately.
        if let Some(spec) = run_as_spec {
            run_as_inflight
                .borrow_mut()
                .insert(lease.occurrence.idempotency_key.clone());
            spawn_run_as_turn(
                &opts.config,
                spec,
                &opts.sessions_dir,
                inbound,
                item.clone(),
                lease.occurrence.clone(),
                runtime_factory,
                runas_tx,
                inflight,
            );
            continue;
        }

        // Orchestrator: a quick turn on the serve thread. Key the inbound enqueue
        // on the occurrence so a retry (lease expiry) reuses the one inbound row.
        let idem = Some(lease.occurrence.idempotency_key.as_str());
        let replies = {
            let mut map = hosts.borrow_mut();
            let host = match host_for(&mut map, &session_id, opts, runtime_factory) {
                Some(host) => host,
                None => continue,
            };
            match host.run_turn_keyed(&inbound, idem) {
                Ok(replies) => replies,
                Err(err) => {
                    // The turn did not run: leave the occurrence pending so a
                    // later tick retries once the lease releases by TTL.
                    eprintln!("slack: scheduled turn failed for channel {session_id}: {err}");
                    continue;
                }
            }
        };
        finalize_scheduled_turn(
            conn,
            hosts,
            channel,
            opts,
            runtime_factory,
            item,
            &lease.occurrence,
            &replies,
            now,
        );
    }
}

/// Apply a finished scheduled turn's side-effect replies and finalize its
/// occurrence. Shared by the inline orchestrator path and the background
/// `run_as` drain, so both process replies and advance the schedule identically.
///
/// Replies route by kind: `send_message` goes through [`route_send_message`]
/// (agent→orchestrator→channel); a `run_as` agent's plain text is internal (never
/// posted — only the orchestrator surfaces to a channel), while the orchestrator's
/// own text posts; side-effect kinds (schedule/cancel/…/save_memory) apply via
/// [`deliver_reply`]. Then the per-session source of truth records the fire and
/// the central projection is finalized (recurring advance / one-off complete).
#[allow(clippy::too_many_arguments)]
fn finalize_scheduled_turn<A, R, F>(
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    item: &ProjectedItem,
    occurrence: &Occurrence,
    replies: &[OutboundMessage],
    now: EpochSecs,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    let session_id = match &item.session_id {
        Some(s) => s.clone(),
        None => return,
    };
    let run_as = item.run_as.as_deref();
    let run_as_spec = run_as.and_then(|route| opts.specialists.iter().find(|s| s.route_name == route));
    let target = DeliveryTarget {
        chat_id: session_id.clone(),
        thread_root_id: None,
    };
    for reply in replies {
        if reply.kind == "send_message" {
            // Only the Slack-wired orchestrator (run_as None, or a slack_wired
            // spec) may reach a channel; a specialist can only send to the
            // orchestrator.
            let sender_slack_wired = run_as_spec.map(|s| s.slack_wired).unwrap_or(true);
            route_send_message(
                conn,
                channel,
                hosts,
                opts,
                runtime_factory,
                &session_id,
                &reply.content,
                sender_slack_wired,
                0,
            );
            continue;
        }
        // A `run_as` agent's plain text is internal — never posted to a channel.
        if run_as.is_some() && reply.kind == "text" {
            continue;
        }
        deliver_reply(conn, channel, &target, &session_id, opts, reply, None, run_as);
    }

    // Record the fire in the per-session source of truth, then finalize the
    // central projection (recurring advance / one-off complete).
    match session_layout(opts, &session_id) {
        Ok(layout) => match latest_meta(&layout, &item.id) {
            Ok(Some(mut meta)) => {
                meta.record_fired(occurrence);
                if let Err(err) = write_meta(&layout, &meta) {
                    eprintln!("slack: recording a fired schedule for channel {session_id} failed: {err}");
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("slack: reading schedule source for channel {session_id} failed: {err}")
            }
        },
        Err(err) => eprintln!("slack: {err}"),
    }
    if let Err(err) = finalize_firing(conn, occurrence, item, now) {
        eprintln!("slack: finalizing scheduled firing for channel {session_id} failed: {err}");
    }
}

/// Spawn a background worker for a `run_as` scheduled turn. The worker runs the
/// agent's turn in a fresh, job-keyed session under the spec's group (so
/// concurrent work turns never collide on one session's container), with the
/// spec's long turn timeout, then reports the replies over `runas_tx` for the
/// serve loop's drain to apply and finalize. Handle is recorded in `inflight` so
/// shutdown joins it. Nothing here touches the central DB — finalization happens
/// on the serve thread from the drain.
#[allow(clippy::too_many_arguments)]
fn spawn_run_as_turn<R, F>(
    base_config: &HostConfig,
    spec: &SpecialistSpec,
    sessions_dir: &Path,
    inbound: InboundMessage,
    item: ProjectedItem,
    occurrence: Occurrence,
    runtime_factory: &F,
    runas_tx: &Sender<CompletedRunAs>,
    inflight: &RefCell<Vec<JoinHandle<()>>>,
) where
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    let base_config = base_config.clone();
    let spec = spec.clone();
    let sessions_dir = sessions_dir.to_path_buf();
    let factory = runtime_factory.clone();
    let tx = runas_tx.clone();
    // A stable, id-safe job key for this occurrence's session (alnum/`-`/`_`).
    let job_id = format!("{}-{}", item.id, occurrence.sequence);

    let handle = std::thread::spawn(move || {
        let replies = (|| {
            let layout = SessionLayout::derive(&sessions_dir, &spec.group_slug, &job_id)
                .map_err(|e| format!("deriving run_as session: {e}"))?;
            let config = crate::delegation::agent_host_config(&base_config, &spec, &sessions_dir)?;
            let mut host = Host::new(layout, factory(), config);
            let outcome = host.run_turn(&inbound).map_err(|e| e.to_string());
            let _ = host.shutdown();
            outcome
        })();
        let _ = tx.send(CompletedRunAs {
            item,
            occurrence,
            replies,
        });
    });
    inflight.borrow_mut().push(handle);
}

/// Drain finished background `run_as` turns: clear the in-flight guard, apply the
/// turn's replies, and finalize the occurrence. A failed turn is logged and still
/// finalized (one-shot, no retry churn — the recurring gate re-evaluates the
/// board next cycle). Called from the idle tick and at shutdown.
#[allow(clippy::too_many_arguments)]
fn drain_completed_runas<A, R, F>(
    completed: &Receiver<CompletedRunAs>,
    run_as_inflight: &RefCell<HashSet<String>>,
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    while let Ok(done) = completed.try_recv() {
        run_as_inflight
            .borrow_mut()
            .remove(&done.occurrence.idempotency_key);
        let now = now_epoch();
        match done.replies {
            Ok(replies) => finalize_scheduled_turn(
                conn,
                hosts,
                channel,
                opts,
                runtime_factory,
                &done.item,
                &done.occurrence,
                &replies,
                now,
            ),
            Err(err) => {
                // Finalize anyway so the occurrence doesn't re-fire and churn.
                eprintln!(
                    "slack: run_as turn for item {} failed: {err}; finalizing (no retry)",
                    done.item.id
                );
                if let Err(e) = finalize_firing(conn, &done.occurrence, &done.item, now) {
                    eprintln!("slack: finalizing failed run_as for item {}: {e}", done.item.id);
                }
            }
        }
    }
}

/// Record a fired occurrence's lifecycle effects in one transaction: mark the
/// occurrence fired, then either advance a recurring item to its next scheduled
/// time or complete a one-off. Both writes commit together so a crash can never
/// leave a recurring item with its occurrence fired but its `process_after`
/// un-advanced — which would silently halt the recurrence (the fired time no
/// longer claims via the already-fired guard). The recurring advance is anchored
/// to the occurrence's *scheduled* time, so the cadence stays drift-free on a
/// late run; the one-off completion drops the item from `claim_due`'s active set
/// so the sweep stops re-examining it each tick.
fn finalize_firing(
    conn: &Connection,
    occurrence: &Occurrence,
    item: &ProjectedItem,
    now: EpochSecs,
) -> Result<(), ProjectionError> {
    let tx = conn.unchecked_transaction()?;
    complete_occurrence(&tx, occurrence, now)?;
    match &item.recurrence {
        Some(recurrence) => {
            advance_recurrence(&tx, &item.id, recurrence.next_after(occurrence.scheduled_for))?;
        }
        None => complete_item(&tx, &item.id)?,
    }
    tx.commit()?;
    Ok(())
}

/// The wire payload a `send_message` row carries: the destination name and the
/// message text (see the shim's `send_message` tool). `to` is either
/// [`ORCHESTRATOR_DESTINATION`] (surface to the human via the orchestrator) or a
/// registered specialist's `route_name` (agent-to-agent).
#[derive(serde::Deserialize)]
struct SendMessagePayload {
    to: String,
    text: String,
}

/// The wire payload an agent's `schedule_message` action carries in its outbound
/// `content` (the serde body of [`assistant_agent_protocol::OutboundAction::ScheduleMessage`]
/// without the action tag, which travels as the row `kind`). Timing is one of:
/// `after_seconds` alone (fire once), `after_seconds` + `every_seconds` (fixed
/// interval), or `calendar` (wall-clock recurrence — the host computes the first
/// fire, so `after_seconds` is ignored).
#[derive(serde::Deserialize)]
struct SchedulePayload {
    text: String,
    #[serde(default)]
    after_seconds: Option<i64>,
    #[serde(default)]
    every_seconds: Option<i64>,
    #[serde(default)]
    calendar: Option<CalendarRecurrence>,
}

/// Deliver one reply to Slack, or — when it is a side-effect action
/// (`schedule_message`, `cancel_schedule`, `pause_schedule`, `resume_schedule`,
/// `save_memory`) — perform that side effect instead of posting it. These actions
/// are never user-visible text, so the raw payload must not be delivered; the
/// run's own confirmation text (a separate reply) is what the user sees. Failures
/// are logged, not retried, matching the rest of the loop.
fn deliver_reply<A>(
    conn: &Connection,
    channel: &SlackChannel<A>,
    target: &DeliveryTarget,
    session_id: &str,
    opts: &SlackServeOptions,
    reply: &OutboundMessage,
    source_user_id: Option<&str>,
    run_as: Option<&str>,
) where
    A: SlackApi,
{
    if reply.kind == "schedule_message" {
        if let Err(err) = create_schedule(conn, opts, session_id, &reply.content, run_as) {
            eprintln!("slack: scheduling a message from a turn in channel {session_id} failed: {err}");
        }
        return;
    }
    if reply.kind == "cancel_schedule" {
        if let Err(err) = cancel_schedule(conn, opts, session_id, &reply.content) {
            eprintln!("slack: cancelling a schedule from a turn in channel {session_id} failed: {err}");
        }
        return;
    }
    if reply.kind == "pause_schedule" {
        if let Err(err) = pause_schedule(conn, opts, session_id, &reply.content) {
            eprintln!("slack: pausing a schedule from a turn in channel {session_id} failed: {err}");
        }
        return;
    }
    if reply.kind == "resume_schedule" {
        if let Err(err) = resume_schedule(conn, opts, session_id, &reply.content) {
            eprintln!("slack: resuming a schedule from a turn in channel {session_id} failed: {err}");
        }
        return;
    }
    if reply.kind == "save_memory" {
        match opts.config.memory.as_ref() {
            Some(mem) => {
                // Record where the turn ran (channel/thread/sender) for provenance
                // and citation. This is stamped, never filtered on — retrieval
                // stays unscoped (the instance is the isolation boundary).
                let source_ref = SourceRef {
                    channel: Some(SourceChannel::Slack),
                    chat_id: Some(target.chat_id.clone()),
                    thread_id: target.thread_root_id.clone(),
                    message_id: None,
                    permalink: None,
                };
                if let Err(err) = crate::memory::write_memory(
                    conn,
                    &mem.groups_dir,
                    &mem.owner,
                    mem.agent_group_id,
                    &reply.content,
                    Some(source_ref),
                    source_user_id.map(str::to_string),
                ) {
                    eprintln!("slack: saving a memory from a turn in channel {session_id} failed: {err}");
                }
            }
            None => eprintln!(
                "slack: a turn in channel {session_id} emitted save_memory but memory is not configured; dropping"
            ),
        }
        return;
    }
    if reply.kind == "send_message" {
        // Agent-to-agent / channel routing for `send_message` is wired in a later
        // phase (destination resolution + permission enforcement). Until then,
        // drop the row rather than posting its raw payload JSON to the channel.
        eprintln!(
            "slack: dropping a send_message row in channel {session_id} (routing not yet wired): {}",
            reply.content
        );
        return;
    }
    if let Err(err) = channel.deliver(target, &to_content(reply)) {
        eprintln!("slack: delivery failed for channel {session_id}: {err}");
    }
}

/// Route a `send_message` action emitted by an agent turn. Destinations:
///
/// - [`ORCHESTRATOR_DESTINATION`] — the reporting agent tells the orchestrator
///   what happened. The host runs an **orchestrator** turn (its own relay
///   session) over the message; the orchestrator then decides whether to post to
///   Slack and, if so, which channel — by emitting its own `send_message` to a
///   channel id, handled below. The host never decides the channel.
/// - a registered specialist's `route_name` — agent-to-agent: run that agent over
///   a dedicated a2a session, then route/apply its side-effect replies.
/// - anything else — treated as a **Slack channel id**, but only when the sender
///   is the Slack-wired orchestrator (`sender_is_slack_wired`). This is the single
///   channel-posting path; a non-Slack-wired agent hitting it is dropped, which is
///   how the "only the orchestrator posts to Slack" rule is enforced.
///
/// `depth` bounds hops so a report → relay → post chain can't loop forever.
#[allow(clippy::too_many_arguments)]
fn route_send_message<A, R, F>(
    conn: &Connection,
    channel: &SlackChannel<A>,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    source_session_id: &str,
    payload: &str,
    sender_is_slack_wired: bool,
    depth: u32,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    const MAX_DEPTH: u32 = 4;
    if depth > MAX_DEPTH {
        eprintln!("slack: send_message hop limit reached from {source_session_id}; dropping");
        return;
    }
    let msg: SendMessagePayload = match serde_json::from_str(payload) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("slack: bad send_message payload from {source_session_id}: {e}");
            return;
        }
    };

    // Normalise the sender's session to its conversation root, so a multi-hop
    // exchange reuses one stable session per (target, root) instead of nesting the
    // whole chain into an ever-growing session id. Routing is by the explicit
    // `to`, so the sender label / return path are unaffected.
    let agents: Vec<String> = opts
        .specialists
        .iter()
        .map(|s| s.route_name.clone())
        .chain(std::iter::once(ORCHESTRATOR_DESTINATION.to_string()))
        .collect();
    let root = conversation_root(source_session_id, &agents);

    // Report to the orchestrator: run an orchestrator turn over the report and let
    // IT decide whether/where to post. The relay session runs the orchestrator
    // (base config, via `host_for`), not a specialist; its own `send_message`
    // replies (to a channel id) are what actually post — handled by recursing with
    // `sender_is_slack_wired = true`.
    if msg.to == ORCHESTRATOR_DESTINATION {
        let relay = orchestrator_relay_session(root);
        let inbound = InboundMessage {
            sender: format!("agent:{source_session_id}"),
            content: msg.text,
            metadata: None,
            thread_id: None,
        };
        let replies = {
            let mut map = hosts.borrow_mut();
            let Some(host) = host_for(&mut map, &relay, opts, runtime_factory) else {
                return;
            };
            match host.run_turn(&inbound) {
                Ok(replies) => replies,
                Err(err) => {
                    eprintln!("slack: orchestrator relay turn failed for {relay}: {err}");
                    return;
                }
            }
        };
        // The orchestrator decides the outcome. Its `send_message` to a channel id
        // posts; its `schedule_message` queues follow-up; plain text in a relay
        // session has no channel and is intentionally not surfaced.
        for reply in &replies {
            match reply.kind.as_str() {
                "send_message" => route_send_message(
                    conn,
                    channel,
                    hosts,
                    opts,
                    runtime_factory,
                    &relay,
                    &reply.content,
                    true,
                    depth + 1,
                ),
                "schedule_message" => {
                    if let Err(err) = create_schedule(conn, opts, &relay, &reply.content, None) {
                        eprintln!("slack: orchestrator relay schedule failed: {err}");
                    }
                }
                _ => {}
            }
        }
        return;
    }

    // Agent-to-agent: run the named specialist over a dedicated a2a session.
    if let Some(spec) = opts.specialists.iter().find(|s| s.route_name == msg.to) {
        let a2a_session = format!("a2a-{}-{}", spec.route_name, root);
        let layout = match session_layout(opts, &a2a_session) {
            Ok(layout) => layout,
            Err(err) => {
                eprintln!("slack: a2a session layout for {a2a_session}: {err}");
                return;
            }
        };
        let config = match crate::delegation::agent_host_config(&opts.config, spec, &opts.sessions_dir)
        {
            Ok(config) => config,
            Err(err) => {
                eprintln!("slack: a2a config for {}: {err}", spec.route_name);
                return;
            }
        };
        let inbound = InboundMessage {
            sender: format!("agent:{source_session_id}"),
            content: msg.text,
            metadata: None,
            thread_id: None,
        };
        let mut host = Host::new(layout, runtime_factory(), config);
        let outcome = host.run_turn(&inbound);
        let _ = host.shutdown();
        let replies = match outcome {
            Ok(replies) => replies,
            Err(err) => {
                eprintln!("slack: a2a turn for {} failed: {err}", spec.route_name);
                return;
            }
        };
        for reply in &replies {
            match reply.kind.as_str() {
                "schedule_message" => {
                    if let Err(err) =
                        create_schedule(conn, opts, &a2a_session, &reply.content, Some(&spec.route_name))
                    {
                        eprintln!("slack: a2a schedule for {} failed: {err}", spec.route_name);
                    }
                }
                "send_message" => route_send_message(
                    conn,
                    channel,
                    hosts,
                    opts,
                    runtime_factory,
                    &a2a_session,
                    &reply.content,
                    // The target specialist is not Slack-wired; it too can only
                    // reach the orchestrator, never a channel.
                    false,
                    depth + 1,
                ),
                // A specialist's plain text is internal — it is not surfaced to a
                // channel (only the orchestrator relays to the human).
                _ => {}
            }
        }
        return;
    }

    // Otherwise `to` is a Slack channel id — but only the Slack-wired orchestrator
    // may post. This is the sole channel-posting path; dropping it for anyone else
    // enforces "only the orchestrator reaches Slack".
    if sender_is_slack_wired {
        let target = DeliveryTarget {
            chat_id: msg.to.clone(),
            thread_root_id: None,
        };
        let content = OutboundContent::Text { body: msg.text };
        if let Err(err) = channel.deliver(&target, &content) {
            eprintln!("slack: orchestrator post to channel {} failed: {err}", msg.to);
        }
        return;
    }

    eprintln!(
        "slack: send_message to forbidden destination {:?} from non-Slack-wired {source_session_id}; dropping",
        msg.to
    );
}

/// Derive the per-session layout the scheduling source of truth is written to.
fn session_layout(opts: &SlackServeOptions, session_id: &str) -> Result<SessionLayout, String> {
    SessionLayout::derive(&opts.sessions_dir, &opts.group, session_id)
        .map_err(|e| format!("deriving session {session_id}: {e}"))
}

/// Project a scheduled item from an agent's `schedule_message` action, due in the
/// session the turn ran in. The host owns the item's identity, agent group, and
/// creation time; the agent supplies only the intent text and timing. The item's
/// metadata is written to the session's `messages_in` as the source of truth
/// first, then projected into the central index — so a central write lost to a
/// crash is rebuilt from the session on the next serve start ([`repair_session_projection`]).
fn create_schedule(
    conn: &Connection,
    opts: &SlackServeOptions,
    session_id: &str,
    payload: &str,
    run_as: Option<&str>,
) -> Result<(), String> {
    let payload: SchedulePayload =
        serde_json::from_str(payload).map_err(|e| format!("bad schedule_message payload: {e}"))?;
    let now = now_epoch();
    let (process_after, recurrence) = resolve_timing(&payload, now)?;
    let intent = ScheduleIntent {
        created_by: "agent".to_string(),
        summary: payload.text,
        created_at: now,
    };
    let mut meta = ScheduledMessageMeta::create(
        HOST_AGENT_GROUP,
        intent,
        process_after,
        recurrence,
        ContextPolicy::default(),
    )
    .map_err(|e| e.to_string())?;
    // An item scheduled by an agent running under `run_as` inherits that agent, so
    // its one-off tasks fire as the same agent (a self-sustaining SE loop) rather
    // than falling back to the orchestrator.
    meta.run_as = run_as.map(str::to_string);
    let layout = session_layout(opts, session_id)?;
    write_meta(&layout, &meta).map_err(|e| e.to_string())?;
    upsert_item(conn, &meta, Some(session_id)).map_err(|e| e.to_string())?;
    Ok(())
}

/// Derive an item's first-fire time and recurrence from a `schedule_message`
/// payload. A `calendar` recurrence computes its own first fire from `now` (so
/// `after_seconds` is ignored); otherwise the first fire is `now + after_seconds`
/// and `every_seconds`, if present, sets a fixed interval.
fn resolve_timing(
    payload: &SchedulePayload,
    now: EpochSecs,
) -> Result<(EpochSecs, Option<Recurrence>), String> {
    if let Some(calendar) = &payload.calendar {
        let recurrence = calendar_to_recurrence(calendar)?;
        recurrence.validate().map_err(|e| e.to_string())?;
        let first = recurrence
            .first_on_or_after(now)
            .ok_or_else(|| "calendar recurrence produced no first occurrence".to_string())?;
        return Ok((first, Some(recurrence)));
    }
    let recurrence = payload
        .every_seconds
        .map(|seconds| Recurrence::Every { seconds });
    Ok((now + payload.after_seconds.unwrap_or(0), recurrence))
}

/// Translate the human-facing wire calendar spec (`HH:MM` local time, IANA tz,
/// weekday names) into the canonical scheduler recurrence.
fn calendar_to_recurrence(spec: &CalendarRecurrence) -> Result<Recurrence, String> {
    match spec {
        CalendarRecurrence::Daily { at, tz } => Ok(Recurrence::Daily {
            minute_of_day: parse_hhmm(at)?,
            tz: tz.clone(),
        }),
        CalendarRecurrence::Weekly { days, at, tz } => Ok(Recurrence::Weekly {
            weekdays: parse_weekdays(days)?,
            minute_of_day: parse_hhmm(at)?,
            tz: tz.clone(),
        }),
        CalendarRecurrence::Monthly { day, at, tz } => Ok(Recurrence::Monthly {
            day: *day,
            minute_of_day: parse_hhmm(at)?,
            tz: tz.clone(),
        }),
    }
}

/// Parse an `HH:MM` 24-hour time into minutes since local midnight.
fn parse_hhmm(at: &str) -> Result<u32, String> {
    let (h, m) = at
        .split_once(':')
        .ok_or_else(|| format!("time {at:?} must be HH:MM"))?;
    let h: u32 = h.parse().map_err(|_| format!("bad hour in {at:?}"))?;
    let m: u32 = m.parse().map_err(|_| format!("bad minute in {at:?}"))?;
    if h > 23 || m > 59 {
        return Err(format!("time {at:?} out of range"));
    }
    Ok(h * 60 + m)
}

fn parse_weekdays(days: &[String]) -> Result<Vec<Weekday>, String> {
    days.iter().map(|d| parse_weekday(d)).collect()
}

fn parse_weekday(day: &str) -> Result<Weekday, String> {
    match day.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => Err(format!("unknown weekday {other:?}")),
    }
}

/// Apply a lifecycle transition to the item's per-session source of truth before
/// the central projection moves, so a repair reconstructs the new status. When
/// the session has no source row for the item — created before this path, or via
/// the operator stopgap — the source write is skipped (the caller's central
/// transition still runs, matching the prior central-only behavior). An illegal
/// transition (e.g. resuming an active item) is a silent no-op here, mirroring
/// the central projection's own no-op on the same input.
fn transition_source(
    opts: &SlackServeOptions,
    session_id: &str,
    item_id: &str,
    transition: LifecycleTransition,
) -> Result<(), String> {
    let layout = session_layout(opts, session_id)?;
    let Some(mut meta) = latest_meta(&layout, item_id).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    if meta.transition(transition).is_err() {
        return Ok(());
    }
    write_meta(&layout, &meta).map_err(|e| e.to_string())
}

/// The wire payload a schedule-lifecycle action (`cancel_schedule`,
/// `pause_schedule`, `resume_schedule`) carries in its outbound `content` (the
/// serde body of the matching [`assistant_agent_protocol::OutboundAction`]). The
/// id is one the agent read from the host-injected `<schedules>` block.
#[derive(serde::Deserialize)]
struct ScheduleItemRef {
    scheduled_item_id: String,
}

impl ScheduleItemRef {
    fn parse(kind: &str, payload: &str) -> Result<Self, String> {
        serde_json::from_str(payload).map_err(|e| format!("bad {kind} payload: {e}"))
    }
}

/// Cancel a scheduled item an agent's `cancel_schedule` action named. A terminal
/// transition on the central index, matching [`create_schedule`]'s central-only
/// projection; an unknown or already-terminal id is a silent no-op (see
/// [`assistant_scheduler::cancel_item`]).
fn cancel_schedule(
    conn: &Connection,
    opts: &SlackServeOptions,
    session_id: &str,
    payload: &str,
) -> Result<(), String> {
    let payload = ScheduleItemRef::parse("cancel_schedule", payload)?;
    transition_source(opts, session_id, &payload.scheduled_item_id, LifecycleTransition::Cancel)?;
    cancel_item(conn, &payload.scheduled_item_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Pause a scheduled item an agent's `pause_schedule` action named, suspending it
/// from the swept set until resumed. Central-only like [`cancel_schedule`]; only
/// an `active` item pauses, so an unknown or non-active id is a silent no-op (see
/// [`assistant_scheduler::pause_item`]).
fn pause_schedule(
    conn: &Connection,
    opts: &SlackServeOptions,
    session_id: &str,
    payload: &str,
) -> Result<(), String> {
    let payload = ScheduleItemRef::parse("pause_schedule", payload)?;
    transition_source(opts, session_id, &payload.scheduled_item_id, LifecycleTransition::Pause)?;
    pause_item(conn, &payload.scheduled_item_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Resume a paused scheduled item an agent's `resume_schedule` action named,
/// returning it to the swept set. Central-only like [`cancel_schedule`]; only a
/// `paused` item resumes, so an unknown or non-paused id is a silent no-op (see
/// [`assistant_scheduler::resume_item`]).
fn resume_schedule(
    conn: &Connection,
    opts: &SlackServeOptions,
    session_id: &str,
    payload: &str,
) -> Result<(), String> {
    let payload = ScheduleItemRef::parse("resume_schedule", payload)?;
    transition_source(opts, session_id, &payload.scheduled_item_id, LifecycleTransition::Resume)?;
    resume_item(conn, &payload.scheduled_item_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Gate an inbound event before it drives a turn: deny-by-default sender
/// permissions, then engagement (mention/sticky). Returns whether the message
/// should drive a turn; a refusal records an audited drop (or, on a DB error,
/// logs and skips). On an engaging message in sticky mode the sticky window is
/// (re)opened so follow-ups in the same conversation stay engaged.
fn passes_gate(conn: &Connection, event: &RoutingEvent, opts: &SlackServeOptions) -> bool {
    // 1. Permissions: an unknown sender never drives a turn under Strict. The
    //    rejection is audited as a dropped message so it stays visible.
    match evaluate_sender(conn, &event.channel_kind, &event.sender_id, opts.policy) {
        Ok(decision) if decision.is_allow() => {}
        Ok(_) => {
            let _ = record_drop(
                conn,
                &event.channel_kind,
                Some(&event.sender_id),
                DropReason::UnknownSender,
                Some(&event.text),
            );
            return false;
        }
        Err(err) => {
            eprintln!("slack: permission check failed for channel {}: {err}", event.chat_id);
            return false;
        }
    }

    // 2. Engagement: by default a mention/DM engages, and follow-ups stay
    //    engaged while a sticky window is open for the conversation.
    let now = now_epoch();
    let has_sticky = match has_active_sticky(conn, HOST_AGENT_GROUP, &event.engagement_key, now) {
        Ok(active) => active,
        Err(err) => {
            eprintln!("slack: sticky lookup failed for channel {}: {err}", event.chat_id);
            return false;
        }
    };
    // The sender already passed the access gate above, so the agent accepts any
    // sender here (`SenderScope::All`); ignored messages are dropped rather than
    // accumulated in this first cut.
    let ctx = EngagementContext {
        sender_is_member: true,
        has_active_sticky: has_sticky,
    };
    match evaluate_engagement(
        event,
        &opts.engagement,
        SenderScope::All,
        IgnoredMessagePolicy::Drop,
        ctx,
    ) {
        EngagementDecision::Engage => {}
        EngagementDecision::Drop { reason } => {
            let _ = record_drop(
                conn,
                &event.channel_kind,
                Some(&event.sender_id),
                reason,
                Some(&event.text),
            );
            return false;
        }
        // Under `Drop` policy a non-engaging message is `Drop`, never
        // `Accumulate`; `Ignore` is only for self-authored events, already
        // filtered upstream. Both are no-ops here for safety.
        EngagementDecision::Accumulate | EngagementDecision::Ignore => return false,
    }

    // 3. Engaged: slide the sticky window forward so the conversation stays
    //    engaged for a while without another mention. Best-effort — a failure
    //    here only costs stickiness, not the turn.
    if opts.engagement == EngagementMode::MentionSticky {
        // Key the window to the thread the reply lands in, not the inbound
        // event's key: a top-level mention is keyed `slack:C`, but the reply is
        // delivered threaded under the trigger (see delivery above), so a
        // follow-up in that thread keys to `slack:C:<root>` and would otherwise
        // miss this window. Mirror the delivery thread root so follow-ups match.
        let root = event
            .thread_root_id
            .clone()
            .unwrap_or_else(|| event.platform_message_id.clone());
        let window_key = engagement_key(&event.channel_kind, &event.chat_id, Some(&root));
        if let Err(err) = open_sticky(
            conn,
            HOST_AGENT_GROUP,
            &window_key,
            Some(&root),
            None,
            Some(now + STICKY_TTL_SECS),
        ) {
            eprintln!("slack: opening sticky failed for channel {}: {err}", event.chat_id);
        }
    }

    true
}

/// Map a session-level [`OutboundMessage`] to channel [`OutboundContent`]. The
/// shim emits `text` rows for now; richer kinds are a later slice, so their
/// content is delivered as plain text rather than dropped.
fn to_content(message: &OutboundMessage) -> OutboundContent {
    OutboundContent::Text {
        body: message.content.clone(),
    }
}

#[cfg(feature = "socket-mode")]
pub use live::serve_slack_live;

/// The live wiring: build the real websocket opener and curl-backed Web API
/// channel, then drive [`serve_slack`] with the real Docker runtime. Compiled
/// only under `socket-mode` so the offline build stays websocket-free.
#[cfg(feature = "socket-mode")]
mod live {
    use std::path::PathBuf;

    use super::{serve_slack, SlackServeOptions};
    use crate::error::HostError;
    use assistant_channel_slack::{ProxyInjection, SlackChannel, TungsteniteOpener};
    use assistant_runtime_docker::DockerCliRuntime;

    /// Serve Slack Socket Mode turns until `stop` trips. The real Slack tokens
    /// live only in the OneCLI vault: both the inbound opener (`apps.connections.open`)
    /// and the outbound Web API client route through `proxy_url` (trusting the CA
    /// at `ca_cert`) carrying a placeholder Bearer that the proxy swaps for the
    /// real `xapp-`/`xoxb-` on the wire, by request path — so this process never
    /// holds a Slack token.
    pub fn serve_slack_live(
        proxy_url: String,
        ca_cert: PathBuf,
        opts: SlackServeOptions,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), HostError> {
        let injection = ProxyInjection { proxy_url, ca_cert };
        let mut channel = SlackChannel::via_proxy(injection.clone());
        let mut opener = TungsteniteOpener::via_proxy(injection);
        serve_slack(&mut opener, &mut channel, opts, DockerCliRuntime::new, stop)
    }
}

#[cfg(test)]
mod a2a_tests {
    use super::conversation_root;

    fn agents() -> Vec<String> {
        ["se", "ax", "orchestrator"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn conversation_root_collapses_nested_a2a_prefixes() {
        let a = agents();
        // A base (non-a2a) session is its own root.
        assert_eq!(conversation_root("standing", &a), "standing");
        assert_eq!(conversation_root("D0BMLE47SFP", &a), "D0BMLE47SFP");
        // One and many nested hops both collapse to the base root.
        assert_eq!(conversation_root("a2a-orchestrator-standing", &a), "standing");
        assert_eq!(
            conversation_root("a2a-orchestrator-a2a-se-a2a-orchestrator-standing", &a),
            "standing"
        );
        // The two directions of a conversation therefore reuse two STABLE sessions
        // (a2a-orchestrator-<root> and a2a-se-<root>), never growing.
        let se_to_orch = format!("a2a-orchestrator-{}", conversation_root("a2a-se-standing", &a));
        assert_eq!(se_to_orch, "a2a-orchestrator-standing");
    }

    #[test]
    fn conversation_root_leaves_unknown_prefixes_intact() {
        let a = agents();
        // `a2a-` followed by an unknown agent isn't stripped (nothing to collapse).
        assert_eq!(conversation_root("a2a-mystery-standing", &a), "a2a-mystery-standing");
    }
}
