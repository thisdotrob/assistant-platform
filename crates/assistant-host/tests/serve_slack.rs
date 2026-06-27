//! Offline coverage of the Slack serve loop: a Socket Mode event drives a full
//! turn over real session DBs using `FakeRuntime`, an in-process fake shim, and
//! a fake Slack Web API — gated by deny-by-default sender permissions and
//! mention-sticky engagement read from a real central DB. No websocket, no
//! Docker, no network.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use assistant_channel_slack::{
    SlackApi, SlackApiError, SlackBotIdentity, SlackChannel, SocketConn, SocketError, SocketOpener,
};
use assistant_agent_graph::store;
use assistant_db::{apply, baseline_migrations, baseline_owner_modules, open_central};
use assistant_host::{
    serve_slack, EngagementMode, HostConfig, SchedulerTickConfig, SlackServeOptions, UnknownPolicy,
};
use assistant_specialist_spec::SpecialistSpec;
use assistant_permissions::{add_user_dm, create_user};
use assistant_router::{count_drops_by_reason, DropReason};
use assistant_scheduler::{
    item_status, list_items, next_claimable_occurrence, upsert_item, ContextPolicy, Recurrence,
    ScheduleIntent, ScheduleStatus, ScheduledMessageMeta,
};
use assistant_runtime_docker::{FakeRuntime, ImageRef, OneCliReadiness, RunnerAuthMode};
use assistant_session::{
    init_session, session_exists, verify_sequence_parity, FakeContainer, LocalControl,
    SessionLayout,
};

/// Profile id the delegation tests register and key their provenance assertions
/// on. Mirrors the real browser specialist's profile id ("browser-specialist");
/// the fake delegate payloads route on the spec's `route_name` ("browser").
const STUB_PROFILE_ID: &str = "browser-specialist";

/// A well-formed specialist spec for the offline delegation tests. The serve loop
/// runs on `FakeRuntime` and never launches a real specialist, so the tests build
/// a spec locally rather than depending on the out-of-tree browser specialist
/// crate; the structural fields (route/profile/group/limits) mirror that spec.
fn stub_specialist_spec() -> SpecialistSpec {
    SpecialistSpec {
        route_name: "browser".to_string(),
        description: "browses the web and reads live pages".to_string(),
        profile_id: STUB_PROFILE_ID.to_string(),
        profile_version: "0.1.0".to_string(),
        group_slug: "browser-1".to_string(),
        image_repository: "assistant-specialist-browser".to_string(),
        image_tag: "0.1.0".to_string(),
        image_digest: None,
        max_specialists: 1,
        max_concurrent_jobs: 8,
        max_artifact_bytes: 50 * 1024 * 1024,
        system_prompt: "You are a web browsing specialist.".to_string(),
        tools: vec!["Bash".to_string()],
        allowed_tools: vec!["Bash(agent-browser:*)".to_string()],
        max_turns: 40,
        extra_env: Vec::new(),
    }
}

/// Recorded `chat.postMessage` calls: `(channel, thread_ts, text)`.
type Posts = Rc<RefCell<Vec<(String, Option<String>, String)>>>;

fn ready() -> OneCliReadiness {
    OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: true,
    }
}

fn test_config(sessions: PathBuf) -> HostConfig {
    let mut config = HostConfig::new(
        ImageRef::new("assistant-base", "0.1.0"),
        vec![sessions],
        RunnerAuthMode::Stub,
        ready(),
    )
    .with_onecli_agent("testns".to_string(), PathBuf::new());
    config.poll_interval = Duration::from_millis(5);
    config.turn_timeout = Duration::from_secs(30);
    config
}

/// Build a serve-options for the test, gated with mention-sticky engagement and
/// the deny-by-default strict sender policy (the live defaults).
fn test_opts(sessions: PathBuf, central: PathBuf) -> SlackServeOptions {
    SlackServeOptions {
        config: test_config(sessions.clone()),
        sessions_dir: sessions,
        group: "slack".to_string(),
        central_db_path: central,
        engagement: EngagementMode::MentionSticky,
        policy: UnknownPolicy::Strict,
        // Inbound-only by default; the scheduler tests opt in explicitly.
        scheduler: None,
        // No delegation by default; the delegation tests opt in explicitly.
        specialists: Vec::new(),
    }
}

/// Create and migrate a central DB (baseline + assistant-router's sticky/drops) at
/// `path`, as setup would leave it. Each helper opens its own connection and
/// drops it, so nothing holds the DB while the serve loop writes to it.
fn migrate_central(path: &Path) {
    let order: Vec<String> = baseline_owner_modules()
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut set = baseline_migrations(order);
    for migration in assistant_router::migrations() {
        set.add(migration);
    }
    // The scheduler's projection/occurrence tables, as setup_steps applies them.
    for migration in assistant_scheduler::migrations() {
        set.add(migration);
    }
    // The agent-graph job tables (specialist_jobs), so a delegated turn can open,
    // run, and complete a specialist job, as setup_steps applies them.
    for migration in store::migrations() {
        set.add(migration);
    }
    // The memory catalog (v2), so a turn's save_memory action can project a row.
    for migration in assistant_memory::migrations() {
        set.add(migration);
    }
    let mut conn = open_central(path).unwrap();
    apply(&mut conn, &set).unwrap();
}

/// Register a user with a Slack DM route so its messages pass the sender gate.
fn register_dm(path: &Path, handle: &str, address: &str) {
    let conn = open_central(path).unwrap();
    let id = create_user(&conn, handle, None).unwrap();
    add_user_dm(&conn, id, "slack", address).unwrap();
}

fn drop_count(path: &Path, reason: DropReason) -> i64 {
    let conn = open_central(path).unwrap();
    count_drops_by_reason(&conn, reason).unwrap()
}

/// The same in-process container stand-in used by the terminal run-loop test:
/// lay a heartbeat, read inbound, and emit one odd-seq echo per new message.
fn spawn_fake_shim(layout: SessionLayout, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    if fake.emit("text", &format!("echo: {content}")).is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake shim that emits one `schedule_message` action (carrying `payload` as
/// its outbound content) per new inbound, instead of an echo. Lets a test cover
/// the host's interception of a scheduling action emitted mid-turn.
fn spawn_scheduling_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    payload: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, _content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    if fake.emit("schedule_message", &payload).is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake shim that emits one `cancel_schedule` action (carrying `payload` as
/// its outbound content) per new inbound, instead of an echo. Lets a test cover
/// the host's interception of a cancellation action emitted mid-turn.
fn spawn_cancel_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    payload: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, _content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    if fake.emit("cancel_schedule", &payload).is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake shim that emits one `save_memory` action (carrying `payload`) for the
/// first new inbound it sees, then echoes every later inbound. Lets a test drive
/// a turn that writes a memory, followed by a turn that should see that memory
/// hydrated into its injected `<retrieved_memories>` block.
fn spawn_memory_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    payload: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        let mut saved = false;
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    let emitted = if saved {
                        fake.emit("text", &format!("echo: {content}"))
                    } else {
                        fake.emit("save_memory", &payload)
                    };
                    if emitted.is_ok() {
                        handled.insert(seq);
                        saved = true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake orchestrator shim that emits one `delegate` action (carrying `payload`)
/// for the first new inbound, then echoes every later inbound. The first turn
/// drives a delegation; the host runs the specialist and re-injects its result
/// as a fresh follow-up turn, which this shim echoes — that echo is the only
/// user-facing post.
fn spawn_delegating_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    payload: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        let mut delegated = false;
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    let emitted = if delegated {
                        fake.emit("text", &format!("echo: {content}"))
                    } else {
                        fake.emit("delegate", &payload)
                    };
                    if emitted.is_ok() {
                        handled.insert(seq);
                        delegated = true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Serve the dynamic, job-keyed specialist containers. A delegated turn runs in
/// its own session under `{sessions}/{group}/{job_id}`, but the job id is minted
/// at runtime, so this watcher polls the specialist group directory for new
/// session dirs and serves each the way `spawn_fake_shim` serves the orchestrator
/// channel: lay a heartbeat, read inbound, and echo a specialist-shaped reply per
/// new message. One thread serves every job that appears.
fn spawn_specialist_watcher(
    sessions: PathBuf,
    group: String,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let group_dir = sessions.join(&group);
        let mut jobs: std::collections::HashMap<String, (FakeContainer, HashSet<i64>)> =
            std::collections::HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            // Discover any new job session dirs the host created since last sweep.
            if let Ok(entries) = std::fs::read_dir(&group_dir) {
                for entry in entries.flatten() {
                    let Some(job_id) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if jobs.contains_key(&job_id) {
                        continue;
                    }
                    let Ok(layout) = SessionLayout::derive(&sessions, &group, &job_id) else {
                        continue;
                    };
                    if !session_exists(&layout) {
                        continue;
                    }
                    let fake = LocalControl::new(layout).fake_container();
                    fake.start("run-spec").ok();
                    jobs.insert(job_id, (fake, HashSet::new()));
                }
            }
            // Serve every known job: keep its heartbeat fresh and answer inbound.
            for (fake, handled) in jobs.values_mut() {
                fake.heartbeat().ok();
                if let Ok(inbound) = fake.read_inbound() {
                    for (seq, content) in inbound {
                        if handled.contains(&seq) {
                            continue;
                        }
                        fake.claim(seq, "fake-specialist").ok();
                        if fake.emit("text", &format!("specialist did: {content}")).is_ok() {
                            handled.insert(seq);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Like [`spawn_specialist_watcher`], but withholds the specialist's reply until a
/// `release` file appears on disk. It keeps every discovered job container's
/// heartbeat fresh (so the gated job is not reaped as dead) but emits nothing
/// until released. This pins a specialist job "in flight" on its background worker
/// thread — the worker's turn blocks waiting for this reply — so a test can prove
/// the serve loop keeps handling other inbound while a specialist runs, then drop
/// the gate to let the job finish and deliver its follow-up.
fn spawn_gated_specialist_watcher(
    sessions: PathBuf,
    group: String,
    stop: Arc<AtomicBool>,
    release: PathBuf,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let group_dir = sessions.join(&group);
        let mut jobs: std::collections::HashMap<String, (FakeContainer, HashSet<i64>)> =
            std::collections::HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            if let Ok(entries) = std::fs::read_dir(&group_dir) {
                for entry in entries.flatten() {
                    let Some(job_id) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if jobs.contains_key(&job_id) {
                        continue;
                    }
                    let Ok(layout) = SessionLayout::derive(&sessions, &group, &job_id) else {
                        continue;
                    };
                    if !session_exists(&layout) {
                        continue;
                    }
                    let fake = LocalControl::new(layout).fake_container();
                    fake.start("run-spec").ok();
                    jobs.insert(job_id, (fake, HashSet::new()));
                }
            }
            let released = release.exists();
            for (fake, handled) in jobs.values_mut() {
                // Always heartbeat so a gated container is not reaped as dead while
                // its reply is withheld.
                fake.heartbeat().ok();
                if !released {
                    continue;
                }
                if let Ok(inbound) = fake.read_inbound() {
                    for (seq, content) in inbound {
                        if handled.contains(&seq) {
                            continue;
                        }
                        fake.claim(seq, "fake-specialist").ok();
                        if fake.emit("text", &format!("specialist did: {content}")).is_ok() {
                            handled.insert(seq);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Like [`spawn_delegating_shim`], but delegates on *every* fresh user mention
/// rather than only the first — so feeding N app_mentions fans out N specialist
/// jobs. A re-injected specialist follow-up (its content carries the
/// `A delegated sub-task finished` marker the host wraps results in) is echoed as
/// text, not re-delegated: re-delegating would emit a nested `delegate` the host
/// drops, leaving the follow-up with no post. This keeps the loop bounded (each
/// mention delegates once, each result is relayed once) while letting a test drive
/// many concurrent delegations from one orchestrator channel.
fn spawn_multi_delegating_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    payload: String,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let control = LocalControl::new(layout);
        let fake = control.fake_container();
        fake.start("run-1").ok();
        let mut handled: HashSet<i64> = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            fake.heartbeat().ok();
            if let Ok(inbound) = fake.read_inbound() {
                for (seq, content) in inbound {
                    if handled.contains(&seq) {
                        continue;
                    }
                    fake.claim(seq, "fake-shim").ok();
                    let emitted = if content.contains("A delegated sub-task finished") {
                        fake.emit("text", &format!("echo: {content}"))
                    } else {
                        fake.emit("delegate", &payload)
                    };
                    if emitted.is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Like [`spawn_gated_specialist_watcher`], but the gate is "I have seen
/// `expected` specialist jobs in flight at the same time" rather than a file. It
/// withholds *every* job's reply until it has concurrently discovered `expected`
/// live job sessions, recording the high-water mark of simultaneously-in-flight
/// jobs in `max_seen`. So no specialist can finish until `expected` of them are
/// running at once — a direct, deterministic proof that the serve loop fanned the
/// jobs out onto concurrent workers (under the old synchronous path only one job
/// could exist at a time and the gate would never open). A generous wall-clock
/// deadline releases the gate anyway so a regression fails the `max_seen`
/// assertion cleanly instead of hanging the test.
fn spawn_concurrency_watcher(
    sessions: PathBuf,
    group: String,
    stop: Arc<AtomicBool>,
    expected: usize,
    max_seen: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let group_dir = sessions.join(&group);
        let mut jobs: std::collections::HashMap<String, (FakeContainer, HashSet<i64>)> =
            std::collections::HashMap::new();
        let started = Instant::now();
        let mut released = false;
        while !stop.load(Ordering::Relaxed) {
            if let Ok(entries) = std::fs::read_dir(&group_dir) {
                for entry in entries.flatten() {
                    let Some(job_id) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if jobs.contains_key(&job_id) {
                        continue;
                    }
                    let Ok(layout) = SessionLayout::derive(&sessions, &group, &job_id) else {
                        continue;
                    };
                    if !session_exists(&layout) {
                        continue;
                    }
                    let fake = LocalControl::new(layout).fake_container();
                    fake.start("run-spec").ok();
                    jobs.insert(job_id, (fake, HashSet::new()));
                }
            }
            // High-water mark of jobs alive at the same instant: a job session dir
            // persists once created, and a job stays gated (no reply emitted) until
            // release, so a count of `expected` here means `expected` workers were
            // simultaneously mid-turn.
            max_seen.fetch_max(jobs.len(), Ordering::Relaxed);
            if !released
                && (jobs.len() >= expected || started.elapsed() > Duration::from_secs(20))
            {
                released = true;
            }
            for (fake, handled) in jobs.values_mut() {
                // Heartbeat always so a gated container is not reaped as dead.
                fake.heartbeat().ok();
                if !released {
                    continue;
                }
                if let Ok(inbound) = fake.read_inbound() {
                    for (seq, content) in inbound {
                        if handled.contains(&seq) {
                            continue;
                        }
                        fake.claim(seq, "fake-specialist").ok();
                        if fake.emit("text", &format!("specialist did: {content}")).is_ok() {
                            handled.insert(seq);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake Slack Web API: returns a canned bot identity and records every post.
/// `on_post` fires after each post is recorded so the test can stop the loop.
struct FakeApi {
    posts: Posts,
    on_post: Rc<dyn Fn()>,
}

impl SlackApi for FakeApi {
    fn auth_test(&self) -> Result<SlackBotIdentity, SlackApiError> {
        Ok(SlackBotIdentity {
            bot_user_id: "U_BOT".to_string(),
            team: "T1".to_string(),
            bot_id: Some("B_BOT".to_string()),
        })
    }

    fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String, SlackApiError> {
        self.posts.borrow_mut().push((
            channel.to_string(),
            thread_ts.map(str::to_string),
            text.to_string(),
        ));
        (self.on_post)();
        Ok("1700000000.000100".to_string())
    }
}

/// Hands out a single connection scripted with `frames`; the next open fails and
/// flips `exhausted`, which the test's `stop` reads to end the run cleanly.
struct ScriptedOpener {
    frames: Vec<String>,
    handed_out: bool,
    exhausted: Rc<Cell<bool>>,
}

impl SocketOpener for ScriptedOpener {
    fn open(&mut self) -> Result<Box<dyn SocketConn>, SocketError> {
        if self.handed_out {
            self.exhausted.set(true);
            return Err(SocketError::Connect("no more connections".to_string()));
        }
        self.handed_out = true;
        Ok(Box::new(ScriptedConn {
            frames: std::mem::take(&mut self.frames).into(),
        }))
    }
}

/// A scripted frame that the connection turns into a `SocketError::Idle` yield
/// (no frame arrived in the read window) rather than a delivered event. Lets a
/// test step the serve loop's scheduler tick without a real timed read.
const IDLE: &str = "__IDLE__";

fn idle() -> String {
    IDLE.to_string()
}

struct ScriptedConn {
    frames: VecDeque<String>,
}

impl SocketConn for ScriptedConn {
    fn read(&mut self) -> Result<String, SocketError> {
        match self.frames.pop_front() {
            Some(frame) if frame == IDLE => Err(SocketError::Idle),
            Some(frame) => Ok(frame),
            None => Err(SocketError::Closed),
        }
    }
    fn ack(&mut self, _frame: &str) -> Result<(), SocketError> {
        Ok(())
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Seed one already-due scheduled item (process_after in the past) under the
/// host agent group, targeting `session`. Returns `(item_id, process_after)` so
/// a test can probe the occurrence's claimability afterwards. `recurrence` lets
/// a test seed an out-of-scope recurring item.
fn seed_due_item(
    path: &Path,
    session: &str,
    text: &str,
    recurrence: Option<Recurrence>,
) -> (String, i64) {
    let conn = open_central(path).unwrap();
    let now = now_secs();
    let intent = ScheduleIntent {
        created_by: "admin".to_string(),
        summary: text.to_string(),
        created_at: now,
    };
    // Agent group 1 matches assistant-host's single-host HOST_AGENT_GROUP.
    let meta = ScheduledMessageMeta::create(
        1,
        intent,
        now - 10,
        recurrence,
        ContextPolicy::default(),
    )
    .unwrap();
    upsert_item(&conn, &meta, Some(session)).unwrap();
    (meta.scheduled_item_id, meta.process_after)
}

/// Wrap an inner Events API event in an `events_api` Socket Mode envelope.
fn events_api(envelope_id: &str, inner: &str) -> String {
    format!(
        r#"{{"envelope_id":"{envelope_id}","type":"events_api","payload":{{"type":"event_callback","event":{inner}}}}}"#
    )
}

#[test]
fn slack_mention_from_a_known_user_drives_a_turn_and_reply_is_threaded() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    // U1 is a registered DM, so it clears the deny-by-default sender gate.
    register_dm(&central, "rob", "U1");

    // serve_slack derives `{sessions}/slack/C1`; pre-init it and back it with a
    // shim so the lazily-created Host finds a live container to poll.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), shim_stop.clone());

    // Stop the listener once the first reply has been posted.
    let stop_flag = Rc::new(Cell::new(false));
    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));

    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: {
            let stop_flag = stop_flag.clone();
            Rc::new(move || stop_flag.set(true))
        },
    });
    // An app_mention engages under MentionSticky; the sender U1 is known.
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"hello"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let opts = test_opts(sessions, central);

    let stop = {
        let stop_flag = stop_flag.clone();
        let exhausted = exhausted.clone();
        move || stop_flag.get() || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    let recorded = posts.borrow();
    assert_eq!(recorded.len(), 1, "exactly one reply posted: {recorded:?}");
    assert_eq!(recorded[0].0, "C1", "posted to the originating channel");
    // A top-level trigger threads its reply under the triggering message's ts.
    assert_eq!(recorded[0].1.as_deref(), Some("100.1"));
    assert_eq!(recorded[0].2, "echo: hello");

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_redelivered_event_does_not_drive_a_second_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), shim_stop.clone());

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));

    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        // Do NOT stop on the first post: the run must read both frames so the
        // second (the redelivery) gets a chance to — and must not — reply.
        on_post: Rc::new(|| {}),
    });
    // The same user message (same `ts`) delivered twice under different envelope
    // ids — exactly how Slack redelivers an event it considers unacked.
    let mut opener = ScriptedOpener {
        frames: vec![
            events_api(
                "env-1",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"hello"}"#,
            ),
            events_api(
                "env-2",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"hello"}"#,
            ),
        ],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let opts = test_opts(sessions, central);

    // Both frames drain, then the opener flips `exhausted` to end the run.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    let recorded = posts.borrow();
    assert_eq!(
        recorded.len(),
        1,
        "a redelivery of the same event must not post a second reply: {recorded:?}"
    );
    assert_eq!(recorded[0].2, "echo: hello");

    // One inbound row, one reply — the redelivery never reached the session DB.
    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn an_unknown_sender_is_denied_and_audited_without_driving_a_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    // No user is registered, so U1 is unknown under the strict policy.

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));

    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"hello"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let opts = test_opts(sessions.clone(), central.clone());

    // No reply is posted (the sender is denied), so the run ends when the single
    // scripted connection drains and the opener flips `exhausted`.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    assert!(
        posts.borrow().is_empty(),
        "an unknown sender must not drive a turn: {:?}",
        posts.borrow()
    );
    // The rejection is audited as a dropped message...
    assert_eq!(drop_count(&central, DropReason::UnknownSender), 1);
    // ...and no session DB was ever created for the channel.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    assert!(!session_exists(&layout));
}

#[test]
fn mention_sticky_keeps_a_known_users_followups_engaged() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), shim_stop.clone());

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));

    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // First a top-level mention (engages + opens the sticky window keyed to the
    // thread its reply lands in), then a plain reply *in that thread* with no
    // mention — which would not engage on its own, but the active sticky window
    // for the thread keeps the conversation engaged. The follow-up carries
    // `thread_ts == 100.1` (the mention's ts), exactly as a human reply to the
    // bot's threaded answer arrives.
    let mut opener = ScriptedOpener {
        frames: vec![
            events_api(
                "env-1",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"hello"}"#,
            ),
            events_api(
                "env-2",
                r#"{"type":"message","channel":"C1","user":"U1","ts":"200.2","thread_ts":"100.1","text":"again"}"#,
            ),
        ],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let opts = test_opts(sessions, central);

    // Run until both scripted frames drain and the opener flips `exhausted`.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    let recorded = posts.borrow();
    assert_eq!(
        recorded.len(),
        2,
        "the mention and the sticky follow-up both drive a turn: {recorded:?}"
    );
    assert_eq!(recorded[0].2, "echo: hello");
    assert_eq!(recorded[0].1.as_deref(), Some("100.1"));
    assert_eq!(recorded[1].2, "echo: again");
    // The follow-up is a reply within the mention's thread, so its answer threads
    // under the same root rather than under its own ts.
    assert_eq!(recorded[1].1.as_deref(), Some("100.1"));

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn the_bots_own_echo_is_not_re_driven() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));

    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // A message authored by our own bot (bot_id == auth.test's bot_id) with no
    // `user`, exactly as the bot's own chat.postMessage echo arrives.
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-self",
            r#"{"type":"message","channel":"C1","bot_id":"B_BOT","ts":"9.0","text":"echo: hello"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let opts = test_opts(sessions.clone(), central);

    // No reply is posted (the event is self-authored), so the run ends when the
    // single scripted connection drains and the opener flips `exhausted`.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    assert!(
        posts.borrow().is_empty(),
        "the bot's own echo must not drive a turn: {:?}",
        posts.borrow()
    );
    // No session DB was ever created for the channel.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    assert!(!session_exists(&layout));
}

#[test]
fn a_due_one_off_item_fires_once_top_level_and_does_not_re_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);

    // The scheduler fires independent of the sender gate, but the fired turn
    // still needs a live container for its channel: pre-init the session and
    // back it with a shim, exactly as an inbound turn would.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();
    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), shim_stop.clone());

    // A due one-off item targeting channel C1.
    let (item_id, process_after) = seed_due_item(&central, "C1", "ping", None);

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // No inbound frames — only idle yields, so the loop ticks the scheduler. The
    // first tick fires the item; the rest are no-ops (the occurrence is already
    // fired). When the frames drain the opener flips `exhausted` and the loop ends.
    let mut opener = ScriptedOpener {
        frames: vec![idle(), idle(), idle()],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions, central.clone());
    opts.scheduler = Some(SchedulerTickConfig {
        owner: "test-daemon".to_string(),
        lease_ttl_secs: 300,
        // Tick on every idle yield (no throttling) so the test is deterministic.
        tick_interval: Duration::ZERO,
    });

    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    let recorded = posts.borrow();
    assert_eq!(recorded.len(), 1, "the due item fires exactly once: {recorded:?}");
    assert_eq!(recorded[0].0, "C1", "posted to the item's channel");
    // A scheduled message posts top-level, with no thread root.
    assert_eq!(recorded[0].1, None, "scheduled posts are top-level: {recorded:?}");
    assert_eq!(recorded[0].2, "echo: ping");

    let conn = open_central(&central).unwrap();
    // The occurrence is fired, so it is no longer claimable at the same time —
    // the already-fired guard makes the firing exactly-once.
    assert!(
        next_claimable_occurrence(&conn, &item_id, process_after)
            .unwrap()
            .is_none(),
        "a fired one-off occurrence must not be re-claimable"
    );
    // The one-off is also marked completed, so it drops out of the active set the
    // sweep walks rather than lingering active for a no-op claim every tick.
    assert_eq!(
        item_status(&conn, &item_id).unwrap(),
        Some(ScheduleStatus::Completed),
        "a fired one-off is marked completed"
    );
    assert!(
        list_items(&conn, 1, Some(ScheduleStatus::Active))
            .unwrap()
            .is_empty(),
        "a completed one-off is no longer in the active listing"
    );
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_recurring_item_fires_each_tick_and_advances_drift_free() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);

    // A recurring fired turn drives a real container just like a one-off, so the
    // channel's session must be pre-initialized and backed by a shim.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();
    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), shim_stop.clone());

    // A due recurring item: seeded 10s overdue with a 5s cadence, so each tick's
    // advanced `process_after` is still in the past and the next tick re-fires.
    const INTERVAL: i64 = 5;
    let (item_id, process_after) =
        seed_due_item(&central, "C1", "tick", Some(Recurrence::Every { seconds: INTERVAL }));

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // Exactly two idle yields → exactly two ticks. Each tick fires one occurrence
    // (the item stays due across both), so the frame count bounds the fire count.
    let mut opener = ScriptedOpener {
        frames: vec![idle(), idle()],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions, central.clone());
    opts.scheduler = Some(SchedulerTickConfig {
        owner: "test-daemon".to_string(),
        lease_ttl_secs: 300,
        tick_interval: Duration::ZERO,
    });

    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    let recorded = posts.borrow();
    // Re-fires: unlike a one-off (which fires once) or the old skip (which fired
    // never), a recurring item fires once per tick while it stays due.
    assert_eq!(recorded.len(), 2, "the recurring item fires on each tick: {recorded:?}");
    for post in recorded.iter() {
        assert_eq!(post.0, "C1", "posted to the item's channel");
        assert_eq!(post.1, None, "scheduled posts are top-level: {recorded:?}");
        assert_eq!(post.2, "echo: tick");
    }
    drop(recorded);

    let conn = open_central(&central).unwrap();
    // The advance is anchored to each occurrence's scheduled time, so after two
    // firings `process_after` is exactly orig + 2*interval regardless of when the
    // ticks actually ran — no wall-clock drift.
    let item = list_items(&conn, 1, Some(ScheduleStatus::Active))
        .unwrap()
        .into_iter()
        .find(|it| it.id == item_id)
        .expect("a recurring item stays active after firing");
    assert_eq!(
        item.process_after,
        Some(process_after + 2 * INTERVAL),
        "process_after advanced two intervals, drift-free"
    );
    // The cadence continues: the next (third) occurrence is claimable at the
    // advanced time.
    let next = next_claimable_occurrence(&conn, &item_id, process_after + 2 * INTERVAL)
        .unwrap()
        .expect("the recurring item keeps producing occurrences");
    assert_eq!(next.sequence, 3, "the next occurrence is the third in the series");
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_turns_schedule_message_action_is_recorded_centrally_and_not_posted() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // The turn emits a recurring schedule_message action; the host must record it
    // and suppress delivery (a scheduling action is not user-visible text).
    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = r#"{"text":"daily standup","after_seconds":120,"every_seconds":3600}"#;
    let shim = spawn_scheduling_shim(layout.clone(), shim_stop.clone(), payload.to_string());

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"remind me daily"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };
    let opts = test_opts(sessions, central.clone());

    // No post is made (the action is intercepted), so the run ends when the
    // single scripted connection drains and the opener flips `exhausted`.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    assert!(
        posts.borrow().is_empty(),
        "a schedule_message action must not be posted to Slack: {:?}",
        posts.borrow()
    );

    // The action was projected into the central index, due in this channel's
    // session, owned by the host (agent group 1).
    let conn = open_central(&central).unwrap();
    let items = list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap();
    assert_eq!(items.len(), 1, "the turn created exactly one active scheduled item");
    assert_eq!(items[0].session_id.as_deref(), Some("C1"));
    assert_eq!(items[0].intent, "daily standup");
    assert_eq!(items[0].recurrence, Some(Recurrence::Every { seconds: 3600 }));
    let due = items[0].process_after.expect("a recurring item has a due time");
    let expected = now_secs() + 120;
    assert!(
        (due - expected).abs() <= 5,
        "due roughly 120s out: due={due} expected≈{expected}"
    );
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_turns_cancel_schedule_action_marks_the_item_cancelled_and_is_not_posted() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // Seed an active item the turn will cancel by id. Owned by the host agent
    // group (1) and due in this channel's session, mirroring create_schedule.
    let meta = ScheduledMessageMeta::create(
        1,
        ScheduleIntent {
            created_by: "agent".to_string(),
            summary: "daily standup".to_string(),
            created_at: now_secs(),
        },
        now_secs() + 120,
        Some(Recurrence::Every { seconds: 3600 }),
        ContextPolicy::default(),
    )
    .unwrap();
    let item_id = meta.scheduled_item_id.clone();
    {
        let conn = open_central(&central).unwrap();
        upsert_item(&conn, &meta, Some("C1")).unwrap();
    }

    // The turn emits a cancel_schedule action naming that id; the host must mark
    // it cancelled and suppress delivery (a cancellation is not user-visible text).
    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = format!(r#"{{"scheduled_item_id":"{item_id}"}}"#);
    let shim = spawn_cancel_shim(layout.clone(), shim_stop.clone(), payload);

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"cancel my standup"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };
    let opts = test_opts(sessions, central.clone());

    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    assert!(
        posts.borrow().is_empty(),
        "a cancel_schedule action must not be posted to Slack: {:?}",
        posts.borrow()
    );

    // The item is now cancelled — gone from the active set and terminal.
    let conn = open_central(&central).unwrap();
    assert_eq!(item_status(&conn, &item_id).unwrap(), Some(ScheduleStatus::Cancelled));
    assert!(
        list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap().is_empty(),
        "a cancelled item must not remain in the active set"
    );
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_turns_save_memory_action_is_written_and_surfaced_in_a_later_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    let groups = tmp.path().join("groups");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // The first turn emits a save_memory action; the host must write the note +
    // catalog row and suppress delivery (a memory write is not user-visible text).
    // Every later turn echoes, so the second turn produces the only post.
    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = r#"{"content":"the user prefers terse replies","title":"reply style"}"#;
    let shim = spawn_memory_shim(layout.clone(), shim_stop.clone(), payload.to_string());

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // A top-level mention drives turn 1 (save_memory, no post) and opens the
    // sticky window; the in-thread follow-up drives turn 2 (echo), which the host
    // enriches with the just-written memory in its inbound metadata.
    let mut opener = ScriptedOpener {
        frames: vec![
            events_api(
                "env-1",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"remember this"}"#,
            ),
            events_api(
                "env-2",
                r#"{"type":"message","channel":"C1","user":"U1","ts":"200.2","thread_ts":"100.1","text":"recall"}"#,
            ),
        ],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    // Enable memory on the per-channel host's config so the turn's save_memory is
    // written and a later turn's retrieval is hydrated from disk.
    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.config = test_config(sessions.clone()).with_memory(
        central.clone(),
        1,
        5,
        groups.clone(),
        "ag_orchestrator".to_string(),
    );

    // Run until both scripted frames drain and the opener flips `exhausted`.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // Only the echo turn posts; the save_memory action is intercepted.
    let recorded = posts.borrow();
    assert_eq!(recorded.len(), 1, "only the echo turn posts: {recorded:?}");
    assert_eq!(recorded[0].2, "echo: recall");
    drop(recorded);

    // The action projected exactly one catalog row under the host agent group...
    let conn = open_central(&central).unwrap();
    let rows = assistant_memory::entries_for_agent(&conn, 1).unwrap();
    assert_eq!(rows.len(), 1, "the turn wrote exactly one memory");

    // Provenance is recorded from the turn that produced it: the Slack channel,
    // its thread root (the top-level mention's ts), and the sender — stamped for
    // citation, never filtered on (the entry stays unscoped/all_chats).
    assert_eq!(rows[0].source_user_id.as_deref(), Some("U1"));
    let source_ref = rows[0].source_ref.as_ref().expect("provenance recorded");
    assert_eq!(source_ref.channel, Some(assistant_memory::SourceChannel::Slack));
    assert_eq!(source_ref.chat_id.as_deref(), Some("C1"));
    assert_eq!(source_ref.thread_id.as_deref(), Some("100.1"));
    drop(conn);

    // ...and a markdown note exists on disk under the orchestrator memory root.
    let note = groups
        .join("orchestrator")
        .join("memory")
        .join(&rows[0].rel_path);
    assert!(note.exists(), "the memory note was written at {note:?}");

    // The later turn (inbound seq 2) carries the hydrated memory as injected
    // context — the catalog block plus the note's actual body text.
    let session_db = rusqlite::Connection::open(layout.inbound_db_path()).unwrap();
    let metadata: Option<String> = session_db
        .query_row("SELECT metadata FROM messages_in WHERE seq = 2", [], |row| row.get(0))
        .unwrap();
    let metadata = metadata.expect("the second turn's inbound carries injected memory");
    assert!(metadata.contains("<retrieved_memories>"), "got {metadata:?}");
    assert!(
        metadata.contains("the user prefers terse replies"),
        "the hydrated body text is injected: {metadata:?}"
    );
    drop(session_db);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_delegate_action_runs_a_specialist_and_re_injects_its_result() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    // The orchestrator channel container for C1.
    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    // Turn 1 emits a `delegate`; the re-injected specialist result is echoed by a
    // later turn — that echo is the only user-facing post.
    let payload = r#"{"specialist":"browser","goal":"find the price of widgets","facts":["https://example.com"],"constraints":["be concise"]}"#;
    let shim = spawn_delegating_shim(layout.clone(), shim_stop.clone(), payload.to_string());
    // The specialist runs in a job-keyed session under `browser-1`; serve it.
    let watcher =
        spawn_specialist_watcher(sessions.clone(), "browser-1".to_string(), shim_stop.clone());

    // Stop once the follow-up post lands.
    let stop_flag = Rc::new(Cell::new(false));
    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: {
            let stop_flag = stop_flag.clone();
            Rc::new(move || stop_flag.set(true))
        },
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"price please"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.specialists = vec![stub_specialist_spec()];

    let stop = {
        let stop_flag = stop_flag.clone();
        let exhausted = exhausted.clone();
        move || stop_flag.get() || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // Exactly one post: the orchestrator's follow-up turn echoing the specialist's
    // result (the delegate turn itself emitted no ack text). It threads under the
    // trigger and carries the specialist marker plus the goal that reached it.
    let recorded = posts.borrow();
    assert_eq!(recorded.len(), 1, "one follow-up post after delegation: {recorded:?}");
    assert_eq!(recorded[0].0, "C1");
    assert_eq!(
        recorded[0].1.as_deref(),
        Some("100.1"),
        "the follow-up threads under the original trigger"
    );
    assert!(
        recorded[0].2.contains("A delegated sub-task finished"),
        "the re-injected result carries the delegation-finished marker: {:?}",
        recorded[0].2
    );
    assert!(
        recorded[0].2.contains("find the price of widgets"),
        "the goal reached the specialist: {:?}",
        recorded[0].2
    );
    drop(recorded);

    // The browser specialist group was created on first delegation, and the one
    // job ran to terminal success (none left queued/running).
    let conn = open_central(&central).unwrap();
    assert_eq!(
        store::specialist_group_count(&conn, STUB_PROFILE_ID).unwrap(),
        1,
        "the browser specialist group was created once"
    );
    assert_eq!(
        store::running_or_queued_job_count(&conn, STUB_PROFILE_ID).unwrap(),
        0,
        "the delegated job is no longer in flight"
    );
    let succeeded: i64 = conn
        .query_row(
            "SELECT count(*) FROM specialist_jobs WHERE status = 'succeeded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(succeeded, 1, "exactly one delegated job succeeded");
    // Provenance: the container that ran the job is linked to it.
    let run_links: i64 = conn
        .query_row("SELECT count(*) FROM specialist_job_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(run_links, 1, "the job records exactly one run link");
    drop(conn);

    // The orchestrator session ran two turns (the mention + the re-injected
    // result), each with its reply — parity holds.
    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
    watcher.join().unwrap();
}

#[test]
fn a_delegate_to_an_unknown_specialist_is_reported_and_runs_no_job() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    // An unknown specialist: the host rejects it before opening any job, so no
    // specialist container is ever spawned (no watcher needed).
    let payload = r#"{"specialist":"database","goal":"run a query"}"#;
    let shim = spawn_delegating_shim(layout.clone(), shim_stop.clone(), payload.to_string());

    let stop_flag = Rc::new(Cell::new(false));
    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: {
            let stop_flag = stop_flag.clone();
            Rc::new(move || stop_flag.set(true))
        },
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"do a thing"}"#,
        )],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.specialists = vec![stub_specialist_spec()];

    let stop = {
        let stop_flag = stop_flag.clone();
        let exhausted = exhausted.clone();
        move || stop_flag.get() || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // The rejection is surfaced to the user in the same thread (the orchestrator
    // never got to acknowledge, so silence would leave them waiting).
    let recorded = posts.borrow();
    assert_eq!(recorded.len(), 1, "the failure is reported once: {recorded:?}");
    assert_eq!(recorded[0].1.as_deref(), Some("100.1"), "in the trigger's thread");
    assert!(
        recorded[0].2.contains("Sorry"),
        "an apology is posted: {:?}",
        recorded[0].2
    );
    drop(recorded);

    // No specialist group and no job were created — the payload was rejected
    // before either was opened.
    let conn = open_central(&central).unwrap();
    assert_eq!(
        store::specialist_group_count(&conn, STUB_PROFILE_ID).unwrap(),
        0,
        "no specialist group is created for an unknown specialist"
    );
    let jobs: i64 = conn
        .query_row("SELECT count(*) FROM specialist_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 0, "no job row is created for an unknown specialist");
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_second_inbound_is_handled_while_a_specialist_is_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    // The gate: the specialist withholds its reply until this file appears, so its
    // background worker stays mid-turn while the serve loop keeps serving inbound.
    let release = tmp.path().join("release");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    // Turn 1 delegates; turns 2+ echo. The delegated worker is pinned on the gate.
    let payload = r#"{"specialist":"browser","goal":"find the price of widgets","facts":["https://example.com"]}"#;
    let shim = spawn_delegating_shim(layout.clone(), shim_stop.clone(), payload.to_string());
    let watcher = spawn_gated_specialist_watcher(
        sessions.clone(),
        "browser-1".to_string(),
        shim_stop.clone(),
        release.clone(),
    );

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    // Drop the gate the instant the first post lands. That first post is the
    // SECOND inbound's echo — proof the serve loop handled it while the specialist
    // worker was still blocked (the gate cannot have been released any earlier, so
    // the job was necessarily in flight). Releasing then lets the job finish so its
    // follow-up is delivered at the shutdown drain.
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: {
            let release = release.clone();
            Rc::new(move || {
                let _ = std::fs::write(&release, b"go");
            })
        },
    });
    let mut opener = ScriptedOpener {
        frames: vec![
            events_api(
                "env-1",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"price please"}"#,
            ),
            events_api(
                "env-2",
                r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.2","text":"say hi"}"#,
            ),
        ],
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.specialists = vec![stub_specialist_spec()];

    // Stop on `exhausted` alone: the second `open()` flips it, ending the run after
    // both frames are consumed. (No `on_post` stop — on_post is the gate release.)
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // Two posts, in this order: the second inbound's echo FIRST (serviced while the
    // delegated job was still in flight on its worker thread), then the delegated
    // follow-up, delivered once the gate released and the shutdown drain joined the
    // worker. Under the old synchronous path the delegate turn would have blocked
    // the serve thread on the gate and this ordering would be impossible.
    let recorded = posts.borrow();
    assert_eq!(
        recorded.len(),
        2,
        "second-inbound echo + delegated follow-up: {recorded:?}"
    );
    assert_eq!(
        recorded[0].2, "echo: say hi",
        "the second inbound is serviced first, while the specialist is in flight: {:?}",
        recorded[0].2
    );
    assert!(
        recorded[1].2.contains("A delegated sub-task finished"),
        "the delegated result follows once the gate releases: {:?}",
        recorded[1].2
    );
    drop(recorded);

    // The delegated job still ran to terminal success with its provenance link.
    let conn = open_central(&central).unwrap();
    assert_eq!(
        store::running_or_queued_job_count(&conn, STUB_PROFILE_ID).unwrap(),
        0,
        "the delegated job is no longer in flight after the drain"
    );
    let succeeded: i64 = conn
        .query_row(
            "SELECT count(*) FROM specialist_jobs WHERE status = 'succeeded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(succeeded, 1, "the delegated job succeeded after release");
    let run_links: i64 = conn
        .query_row("SELECT count(*) FROM specialist_job_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(run_links, 1, "the job records exactly one run link");
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
    watcher.join().unwrap();
}

/// Fan out the full concurrency cap (`DEFAULT_MAX_CONCURRENT_JOBS` = 8) of
/// delegations from one channel at once and prove they run on concurrent workers:
/// the gated watcher refuses to answer any specialist until it has seen all 8 in
/// flight simultaneously, so all 8 completing at all is proof the serve loop never
/// serialized them. Exercises 8 concurrent specialist session DBs + the central
/// job/provenance writes under load — the over-provisioned cap's headroom in
/// practice.
#[test]
fn many_specialists_run_concurrently_up_to_the_cap() {
    const N: usize = 8; // == DEFAULT_MAX_CONCURRENT_JOBS

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = r#"{"specialist":"browser","goal":"find the price of widgets","facts":["https://example.com"]}"#;
    let shim = spawn_multi_delegating_shim(layout.clone(), shim_stop.clone(), payload.to_string());
    // The gate opens only once all N jobs are concurrently in flight.
    let max_seen = Arc::new(AtomicUsize::new(0));
    let watcher = spawn_concurrency_watcher(
        sessions.clone(),
        "browser-1".to_string(),
        shim_stop.clone(),
        N,
        max_seen.clone(),
    );

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // N distinct app_mentions (distinct ts so none is deduped), each delegating.
    let frames: Vec<String> = (1..=N)
        .map(|i| {
            events_api(
                &format!("env-{i}"),
                &format!(
                    r#"{{"type":"app_mention","channel":"C1","user":"U1","ts":"100.{i}","text":"lookup {i}"}}"#
                ),
            )
        })
        .collect();
    let mut opener = ScriptedOpener {
        frames,
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.specialists = vec![stub_specialist_spec()];

    // Stop once the frames drain and the second open() flips `exhausted`; the
    // shutdown drain joins all workers and delivers every follow-up.
    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // The gate proved all N were in flight at the same instant.
    assert_eq!(
        max_seen.load(Ordering::Relaxed),
        N,
        "all {N} specialist jobs were in flight concurrently"
    );

    // Every delegation produced a follow-up post (relaying its result), all
    // threaded back into the originating channel.
    let recorded = posts.borrow();
    assert_eq!(recorded.len(), N, "one follow-up per delegation: {recorded:?}");
    for post in recorded.iter() {
        assert_eq!(post.0, "C1");
        assert!(
            post.2.contains("A delegated sub-task finished"),
            "each post relays a specialist result: {:?}",
            post.2
        );
    }
    drop(recorded);

    // One specialist group, all N jobs succeeded with a provenance link, none left
    // in flight.
    let conn = open_central(&central).unwrap();
    assert_eq!(
        store::specialist_group_count(&conn, STUB_PROFILE_ID).unwrap(),
        1,
        "the single browser specialist group is reused across all jobs"
    );
    assert_eq!(
        store::running_or_queued_job_count(&conn, STUB_PROFILE_ID).unwrap(),
        0,
        "no job is left in flight after the drain"
    );
    let succeeded: i64 = conn
        .query_row(
            "SELECT count(*) FROM specialist_jobs WHERE status = 'succeeded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(succeeded as usize, N, "all {N} delegated jobs succeeded");
    let run_links: i64 = conn
        .query_row("SELECT count(*) FROM specialist_job_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(run_links as usize, N, "each job records exactly one run link");
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
    watcher.join().unwrap();
}

/// One delegation past the concurrency cap is rejected, not queued: with 8 jobs
/// already in flight a 9th `delegate` fails at `begin_specialist`
/// (`ConcurrencyLimitReached`) and is surfaced to the user as an apology, while
/// the 8 admitted jobs still run to success. No central drain runs between the
/// back-to-back frames, so the 8 are provably still in flight when the 9th is
/// refused — the cap is a hard ceiling, the over-provisioned value notwithstanding.
#[test]
fn a_delegation_past_the_concurrency_cap_is_rejected() {
    const ADMITTED: usize = 8; // == DEFAULT_MAX_CONCURRENT_JOBS
    const TOTAL: usize = ADMITTED + 1;

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = r#"{"specialist":"browser","goal":"find the price of widgets","facts":["https://example.com"]}"#;
    let shim = spawn_multi_delegating_shim(layout.clone(), shim_stop.clone(), payload.to_string());
    let max_seen = Arc::new(AtomicUsize::new(0));
    let watcher = spawn_concurrency_watcher(
        sessions.clone(),
        "browser-1".to_string(),
        shim_stop.clone(),
        ADMITTED,
        max_seen.clone(),
    );

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    // TOTAL back-to-back app_mentions, no idle frame between them: the serve loop
    // never reaches a drain tick mid-batch, so the first 8 jobs stay `running`
    // (gated) and the 9th `begin_specialist` sees the cap full.
    let frames: Vec<String> = (1..=TOTAL)
        .map(|i| {
            events_api(
                &format!("env-{i}"),
                &format!(
                    r#"{{"type":"app_mention","channel":"C1","user":"U1","ts":"100.{i}","text":"lookup {i}"}}"#
                ),
            )
        })
        .collect();
    let mut opener = ScriptedOpener {
        frames,
        handed_out: false,
        exhausted: exhausted.clone(),
    };

    let mut opts = test_opts(sessions.clone(), central.clone());
    opts.specialists = vec![stub_specialist_spec()];

    let stop = {
        let exhausted = exhausted.clone();
        move || exhausted.get()
    };
    serve_slack(&mut opener, &mut channel, opts, FakeRuntime::new, &stop).unwrap();

    // Exactly one apology (the rejected 9th) and one relayed result per admitted
    // job.
    let recorded = posts.borrow();
    let rejected: Vec<&String> = recorded
        .iter()
        .map(|p| &p.2)
        .filter(|t| t.contains("Sorry"))
        .collect();
    let relayed = recorded
        .iter()
        .filter(|p| p.2.contains("A delegated sub-task finished"))
        .count();
    assert_eq!(rejected.len(), 1, "exactly one delegation is rejected: {recorded:?}");
    assert!(
        rejected[0].contains("maximum 8 concurrent jobs"),
        "the rejection cites the concurrency cap: {:?}",
        rejected[0]
    );
    assert_eq!(relayed, ADMITTED, "every admitted job relays its result: {recorded:?}");
    drop(recorded);

    // The 9th never created a job row (rejected before `insert_job`); the 8
    // admitted all succeeded with a provenance link, none left in flight.
    let conn = open_central(&central).unwrap();
    let total_jobs: i64 = conn
        .query_row("SELECT count(*) FROM specialist_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total_jobs as usize, ADMITTED,
        "the over-cap delegation created no job row"
    );
    assert_eq!(
        store::running_or_queued_job_count(&conn, STUB_PROFILE_ID).unwrap(),
        0,
        "no job is left in flight after the drain"
    );
    let succeeded: i64 = conn
        .query_row(
            "SELECT count(*) FROM specialist_jobs WHERE status = 'succeeded'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(succeeded as usize, ADMITTED, "all admitted jobs succeeded");
    let run_links: i64 = conn
        .query_row("SELECT count(*) FROM specialist_job_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(run_links as usize, ADMITTED, "each admitted job records one run link");
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
    watcher.join().unwrap();
}
