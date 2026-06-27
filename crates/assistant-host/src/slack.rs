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
    advance_recurrence, cancel_item, claim_due, complete_item, complete_occurrence, list_items,
    upsert_item, ContextPolicy, EpochSecs, Occurrence, ProjectedItem, ProjectionError, Recurrence,
    ScheduleIntent, ScheduleStatus, ScheduledMessageMeta,
};
use assistant_session::{InboundMessage, OutboundMessage, SessionLayout};
use rusqlite::Connection;

use crate::delegation::SpecialistTicket;
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
    /// The specialists the orchestrator may delegate to: a `delegate` row from a
    /// turn is routed to a matching spec by `route_name`, run as a specialist job,
    /// and its result is re-injected as a fresh follow-up turn. Empty drops any
    /// `delegate` row (the offline tests that don't exercise delegation, and any
    /// non-delegating caller).
    pub specialists: Vec<SpecialistSpec>,
}

/// A finished background specialist job, reported from a worker thread back to
/// the serve loop's drain. Carries everything the serve thread needs to
/// terminalize the job ([`crate::delegation::finish_specialist`]) and route the
/// follow-up turn back to the originating channel/thread. All fields are owned,
/// so it is `Send` and crosses the worker→serve channel.
struct CompletedDelegation {
    /// The orchestrator channel/session the delegation originated in.
    session_id: String,
    /// Where the follow-up reply is delivered (same channel + thread as the ack).
    target: DeliveryTarget,
    /// The human who triggered the delegation, for save-memory provenance on the
    /// follow-up turn. `None` for a non-human trigger.
    source_user_id: Option<String>,
    /// The agent-graph job id this result terminalizes.
    job_id: String,
    /// The original delegated goal, woven into the re-injection text.
    goal: String,
    /// The resolved specialist's artifact-size ceiling, enforced when the job is
    /// terminalized ([`crate::delegation::finish_specialist`]).
    max_artifact_bytes: u64,
    /// The worker's result: the specialist's joined text and the container that
    /// ran it (run-link provenance), or a human-readable failure reason.
    outcome: Result<(String, Option<String>), String>,
}

/// Serve Slack turns until `stop` returns true.
///
/// Authenticates `channel` (its resolved bot identity drives self-author
/// filtering), then drives the Socket Mode `opener`: each routable, non-self
/// message is dispatched to a per-channel [`Host`] (spawned lazily via
/// `runtime_factory`), and every reply is posted back over `channel`, threaded
/// under the triggering message.
///
/// A `delegate` action runs on a background worker thread (so a slow browsing
/// specialist never blocks inbound handling); its result is re-injected as a
/// follow-up turn from the serve loop's idle drain. Because the follow-up runs
/// later, on the serve thread, it is ordered after any inbound handled in the
/// meantime — a specialist answer can arrive after a reply to a newer message.
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

    // Background specialist delegation: a `delegate` action is started on the
    // serve thread, then run on a worker thread that reports the finished job over
    // `completion_tx`. The serve loop drains `completion_rx` (idle tick + at
    // shutdown) to run the follow-up turn. `inflight` holds the worker handles so
    // shutdown can join them. Empty/unused when no caller delegates.
    let (completion_tx, completion_rx) = std::sync::mpsc::channel::<CompletedDelegation>();
    let inflight: RefCell<Vec<JoinHandle<()>>> = RefCell::new(Vec::new());

    let result = {
        let mut sink = |event: RoutingEvent| {
            handle_event(
                event,
                &conn,
                &hosts,
                channel_ref,
                &opts,
                &runtime_factory,
                &completion_tx,
                &inflight,
                &mut seen,
            );
        };
        // Throttle the sub-second idle cadence down to the configured sweep
        // interval. With no scheduler configured the scheduler sweep is a no-op.
        let tick_interval = opts.scheduler.as_ref().map(|s| s.tick_interval);
        let mut last_tick: Option<Instant> = None;
        let mut tick = || {
            // Drain finished specialist jobs every idle window (unthrottled) so a
            // background worker's result follows up promptly, independent of the
            // scheduler sweep cadence.
            drain_completed_delegations(
                &completion_rx,
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
            scheduler_tick(&conn, &hosts, channel_ref, &opts, &runtime_factory);
        };
        run_listener(opener, &identity, stop, &mut sink, &mut tick)
            .map_err(|e| HostError::Channel(e.to_string()))
    };

    // Shutdown drain: join every in-flight specialist worker (each reaps its own
    // job container inside `run_specialist_turn`, bounded by the turn timeout),
    // then run one final drain so a job that finished after the last idle tick
    // still delivers its follow-up before we tear the per-channel hosts down.
    for handle in inflight.into_inner() {
        if let Err(err) = handle.join() {
            eprintln!("slack: a specialist worker panicked: {err:?}");
        }
    }
    drain_completed_delegations(&completion_rx, &conn, &hosts, channel_ref, &opts, &runtime_factory);

    // Best-effort: stop every container this session spawned before returning.
    for (_chat, mut host) in hosts.into_inner() {
        let _ = host.shutdown();
    }
    result
}

/// Dispatch one routed Slack event: ensure a [`Host`] for its channel, run the
/// turn, and deliver each reply back into the same channel/thread. Failures are
/// logged and swallowed so the serve loop survives a single bad turn.
#[allow(clippy::too_many_arguments)]
fn handle_event<A, R, F>(
    event: RoutingEvent,
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    completion_tx: &Sender<CompletedDelegation>,
    inflight: &RefCell<Vec<JoinHandle<()>>>,
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

    let inbound = InboundMessage {
        sender: event.sender_id.clone(),
        content: event.text.clone(),
        metadata: None,
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

    // Thread the reply under the triggering message's root — its own ts when the
    // trigger was top-level — so a channel stays uncluttered and a threaded ask
    // gets a threaded answer.
    let thread_root = event
        .thread_root_id
        .clone()
        .unwrap_or_else(|| event.platform_message_id.clone());
    let target = DeliveryTarget {
        chat_id: event.chat_id.clone(),
        thread_root_id: Some(thread_root),
    };
    // Two passes so the orchestrator's acknowledgment posts before any (blocking)
    // specialist run. A `delegate` turn emits its `delegate` row alongside an
    // "on it" text row; delivering the non-delegate rows first lets the user see
    // that ack immediately, then the specialist runs and its result follows up.
    for reply in &replies {
        if reply.kind == "delegate" {
            continue;
        }
        deliver_reply(conn, channel, &target, &event.chat_id, opts, reply, Some(&event.sender_id));
    }
    for reply in &replies {
        if reply.kind != "delegate" {
            continue;
        }
        process_delegation(
            conn,
            channel,
            opts,
            runtime_factory,
            completion_tx,
            inflight,
            &target,
            &event.chat_id,
            &reply.content,
            Some(&event.sender_id),
        );
    }
}

/// Start a `delegate` action emitted by an orchestrator turn and hand it to a
/// background worker, so a slow browsing specialist never blocks inbound frame
/// handling. Phase 1 ([`crate::delegation::begin_specialist`]) opens and starts
/// the job on the serve thread under the concurrency cap; phase 2 runs on the
/// worker ([`dispatch_specialist`]); phase 3 ([`finish_one_delegation`], from the
/// drain) re-injects the result as a fresh follow-up turn. A start-time rejection
/// (bad payload, unknown specialist, cap reached) never spawned a worker, so it
/// is surfaced to the user in the same thread — the orchestrator already
/// acknowledged, so silence would leave them waiting.
#[allow(clippy::too_many_arguments)]
fn process_delegation<A, R, F>(
    conn: &Connection,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
    completion_tx: &Sender<CompletedDelegation>,
    inflight: &RefCell<Vec<JoinHandle<()>>>,
    target: &DeliveryTarget,
    session_id: &str,
    payload: &str,
    source_user_id: Option<&str>,
) where
    A: SlackApi,
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    if opts.specialists.is_empty() {
        eprintln!(
            "slack: a turn in channel {session_id} emitted delegate but specialist delegation is not configured; dropping"
        );
        return;
    }

    let ticket = match crate::delegation::begin_specialist(conn, &opts.specialists, payload) {
        Ok(ticket) => ticket,
        Err(reason) => {
            let body = format!("Sorry — I couldn't complete that request: {reason}");
            if let Err(err) = channel.deliver(target, &OutboundContent::Text { body }) {
                eprintln!(
                    "slack: delivering a delegation failure to channel {session_id} failed: {err}"
                );
            }
            return;
        }
    };

    dispatch_specialist(
        ticket,
        &opts.config,
        &opts.sessions_dir,
        runtime_factory,
        completion_tx,
        inflight,
        session_id,
        target,
        source_user_id,
    );
}

/// Spawn a background worker that runs a started specialist job's container turn
/// off the serve thread, reporting the finished job over `completion_tx`. Every
/// value the worker touches is an owned clone, so the spawned closure borrows
/// nothing from the serve loop's stack (`Send + 'static`). The join handle is
/// recorded in `inflight` so shutdown can join the worker (each reaps its own
/// container in [`crate::delegation::run_specialist_turn`]).
#[allow(clippy::too_many_arguments)]
fn dispatch_specialist<R, F>(
    ticket: SpecialistTicket,
    base_config: &HostConfig,
    sessions_dir: &Path,
    runtime_factory: &F,
    completion_tx: &Sender<CompletedDelegation>,
    inflight: &RefCell<Vec<JoinHandle<()>>>,
    session_id: &str,
    target: &DeliveryTarget,
    source_user_id: Option<&str>,
) where
    R: ContainerRuntime + 'static,
    R::Error: std::fmt::Display,
    F: Fn() -> R + Send + Clone + 'static,
{
    let SpecialistTicket {
        job_id,
        packet,
        spec,
    } = ticket;
    let goal = packet.goal.clone();
    let max_artifact_bytes = spec.max_artifact_bytes;
    let base_config = base_config.clone();
    let sessions_dir = sessions_dir.to_path_buf();
    let factory = runtime_factory.clone();
    let tx = completion_tx.clone();
    let session_id = session_id.to_string();
    let target = target.clone();
    let source_user_id = source_user_id.map(str::to_string);

    let handle = std::thread::spawn(move || {
        let outcome = crate::delegation::run_specialist_turn(
            &base_config,
            &sessions_dir,
            &spec,
            &factory,
            &job_id,
            &packet,
        );
        // The serve loop owns the central DB; the worker only reports the result.
        // A send error means the serve loop is already gone (shutting down).
        let _ = tx.send(CompletedDelegation {
            session_id,
            target,
            source_user_id,
            job_id,
            goal,
            max_artifact_bytes,
            outcome,
        });
    });
    inflight.borrow_mut().push(handle);
}

/// Drain every background specialist job that has reported back, running each
/// one's follow-up turn on the serve thread. Called from the idle tick
/// (unthrottled) and once more at shutdown after the workers are joined. No-op
/// when nothing has finished.
fn drain_completed_delegations<A, R, F>(
    completed: &Receiver<CompletedDelegation>,
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
) where
    A: SlackApi,
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
{
    while let Ok(done) = completed.try_recv() {
        finish_one_delegation(done, conn, hosts, channel, opts, runtime_factory);
    }
}

/// Phase 3 for one finished delegation, on the serve thread: terminalize the job
/// ([`crate::delegation::finish_specialist`]), then re-inject the specialist's
/// result as a fresh follow-up turn through the same per-channel [`Host`] (so it
/// reuses the channel container, never a second one) and deliver its replies. A
/// hard specialist failure is surfaced to the user in the same thread so the
/// acknowledged ask is not left hanging. A nested `delegate` from the follow-up is
/// dropped: a specialist result must not spawn another specialist run in the same
/// chain (bounds re-delegation to one level). Side-effect rows still take effect.
fn finish_one_delegation<A, R, F>(
    done: CompletedDelegation,
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
) where
    A: SlackApi,
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
{
    let CompletedDelegation {
        session_id,
        target,
        source_user_id,
        job_id,
        goal,
        max_artifact_bytes,
        outcome,
    } = done;

    let reinjected =
        match crate::delegation::finish_specialist(conn, &job_id, &goal, max_artifact_bytes, outcome)
        {
            Ok(text) => text,
            Err(reason) => {
                let body = format!("Sorry — I couldn't complete that request: {reason}");
                if let Err(err) = channel.deliver(&target, &OutboundContent::Text { body }) {
                    eprintln!(
                        "slack: delivering a delegation failure to channel {session_id} failed: {err}"
                    );
                }
                return;
            }
        };

    let inbound = InboundMessage {
        sender: "specialist".to_string(),
        content: reinjected,
        metadata: None,
    };
    let replies = {
        let mut map = hosts.borrow_mut();
        let host = match host_for(&mut map, &session_id, opts, runtime_factory) {
            Some(host) => host,
            None => return,
        };
        match host.run_turn(&inbound) {
            Ok(replies) => replies,
            Err(err) => {
                eprintln!(
                    "slack: follow-up turn after delegation failed for channel {session_id}: {err}"
                );
                return;
            }
        }
    };

    for reply in &replies {
        if reply.kind == "delegate" {
            eprintln!(
                "slack: dropping a nested delegate from a post-delegation follow-up in channel {session_id}"
            );
            continue;
        }
        deliver_reply(
            conn,
            channel,
            &target,
            &session_id,
            opts,
            reply,
            source_user_id.as_deref(),
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

/// Fire any due scheduled items, exactly once each. Runs on the serve thread
/// between inbound frames (see [`SchedulerTickConfig`]). Sticky-engagement
/// windows are expired on the same cadence (the daemon has no other driver for
/// that maintenance). A one-off is prevented from re-firing by the scheduler's
/// own already-fired guard; a recurring item advances its projected
/// `process_after` to the next scheduled time after firing, so the next sweep
/// claims the following occurrence (drift-free — the advance is anchored to the
/// occurrence's scheduled time, not when it actually ran).
fn scheduler_tick<A, R, F>(
    conn: &Connection,
    hosts: &RefCell<HashMap<String, Host<R>>>,
    channel: &SlackChannel<A>,
    opts: &SlackServeOptions,
    runtime_factory: &F,
) where
    A: SlackApi,
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
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

        let inbound = InboundMessage {
            sender: "scheduler".to_string(),
            content: item.intent.clone(),
            metadata: None,
        };
        let replies = {
            let mut map = hosts.borrow_mut();
            let host = match host_for(&mut map, &session_id, opts, runtime_factory) {
                Some(host) => host,
                None => continue,
            };
            // Key the inbound enqueue on the occurrence so a retry (after a
            // failed attempt left the lease to expire) reuses the one inbound
            // row instead of duplicating it.
            match host.run_turn_keyed(&inbound, Some(&lease.occurrence.idempotency_key)) {
                Ok(replies) => replies,
                Err(err) => {
                    // The turn did not run: leave the occurrence pending so a
                    // later tick retries once the lease releases by TTL.
                    eprintln!("slack: scheduled turn failed for channel {session_id}: {err}");
                    continue;
                }
            }
        };

        // A scheduled message posts top-level in its channel (no thread root).
        let target = DeliveryTarget {
            chat_id: session_id.clone(),
            thread_root_id: None,
        };
        // A scheduled turn has no human sender; provenance records the channel
        // only (no source user).
        for reply in &replies {
            deliver_reply(conn, channel, &target, &session_id, opts, reply, None);
        }

        // The turn ran, so finalize the firing — even if a delivery hiccuped,
        // re-running the turn would double-process. Delivery failures are logged,
        // not retried, in this first cut.
        if let Err(err) = finalize_firing(conn, &lease.occurrence, item, now) {
            eprintln!("slack: finalizing scheduled firing for channel {session_id} failed: {err}");
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

/// The wire payload an agent's `schedule_message` action carries in its outbound
/// `content` (the serde body of [`assistant_agent_protocol::OutboundAction::ScheduleMessage`]
/// without the action tag, which travels as the row `kind`). `every_seconds`
/// absent = fire once.
#[derive(serde::Deserialize)]
struct SchedulePayload {
    text: String,
    after_seconds: i64,
    #[serde(default)]
    every_seconds: Option<i64>,
}

/// Deliver one reply to Slack, or — when it is a side-effect action
/// (`schedule_message`, `cancel_schedule`, `save_memory`) — perform that side
/// effect instead of posting it. These actions are never user-visible text, so the raw payload
/// must not be delivered; the run's own confirmation text (a separate reply) is
/// what the user sees. Failures are logged, not retried, matching the rest of
/// the loop.
fn deliver_reply<A>(
    conn: &Connection,
    channel: &SlackChannel<A>,
    target: &DeliveryTarget,
    session_id: &str,
    opts: &SlackServeOptions,
    reply: &OutboundMessage,
    source_user_id: Option<&str>,
) where
    A: SlackApi,
{
    if reply.kind == "schedule_message" {
        if let Err(err) = create_schedule(conn, session_id, &reply.content) {
            eprintln!("slack: scheduling a message from a turn in channel {session_id} failed: {err}");
        }
        return;
    }
    if reply.kind == "cancel_schedule" {
        if let Err(err) = cancel_schedule(conn, &reply.content) {
            eprintln!("slack: cancelling a schedule from a turn in channel {session_id} failed: {err}");
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
    if reply.kind == "delegate" {
        // Delegation is driven from the inbound path (`handle_event`), which
        // intercepts `delegate` rows before delivery. Any reaching here came via
        // direct delivery (e.g. a scheduled turn), which has no delegation driver,
        // so drop it rather than posting raw payload JSON.
        eprintln!("slack: dropping a delegate row reached via direct delivery in channel {session_id}");
        return;
    }
    if let Err(err) = channel.deliver(target, &to_content(reply)) {
        eprintln!("slack: delivery failed for channel {session_id}: {err}");
    }
}

/// Project a scheduled item from an agent's `schedule_message` action into the
/// central index, due in the session the turn ran in. The host owns the item's
/// identity, agent group, and creation time; the agent supplies only the intent
/// text and timing. Central-only, matching the operator stopgap
/// ([`crate::admin::create_scheduled_message`]) — durably reconstructing items
/// from the per-session source of truth is a separate concern.
fn create_schedule(conn: &Connection, session_id: &str, payload: &str) -> Result<(), String> {
    let payload: SchedulePayload =
        serde_json::from_str(payload).map_err(|e| format!("bad schedule_message payload: {e}"))?;
    let now = now_epoch();
    let intent = ScheduleIntent {
        created_by: "agent".to_string(),
        summary: payload.text,
        created_at: now,
    };
    let recurrence = payload.every_seconds.map(|seconds| Recurrence::Every { seconds });
    let meta = ScheduledMessageMeta::create(
        HOST_AGENT_GROUP,
        intent,
        now + payload.after_seconds,
        recurrence,
        ContextPolicy::default(),
    )
    .map_err(|e| e.to_string())?;
    upsert_item(conn, &meta, Some(session_id)).map_err(|e| e.to_string())?;
    Ok(())
}

/// The wire payload an agent's `cancel_schedule` action carries in its outbound
/// `content` (the serde body of
/// [`assistant_agent_protocol::OutboundAction::CancelSchedule`]). The id is one
/// the agent read from the host-injected `<active_schedules>` block.
#[derive(serde::Deserialize)]
struct CancelSchedulePayload {
    scheduled_item_id: String,
}

/// Cancel a scheduled item an agent's `cancel_schedule` action named. A terminal
/// transition on the central index, matching [`create_schedule`]'s central-only
/// projection; an unknown or already-terminal id is a silent no-op (see
/// [`assistant_scheduler::cancel_item`]).
fn cancel_schedule(conn: &Connection, payload: &str) -> Result<(), String> {
    let payload: CancelSchedulePayload =
        serde_json::from_str(payload).map_err(|e| format!("bad cancel_schedule payload: {e}"))?;
    cancel_item(conn, &payload.scheduled_item_id).map_err(|e| e.to_string())?;
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
