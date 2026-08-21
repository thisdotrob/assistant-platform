//! Offline coverage of the Slack serve loop: a Socket Mode event drives a full
//! turn over real session DBs using `FakeRuntime`, an in-process fake shim, and
//! a fake Slack Web API — gated by deny-by-default sender permissions and
//! mention-sticky engagement read from a real central DB. No websocket, no
//! Docker, no network.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use assistant_channel_slack::{
    SlackApi, SlackApiError, SlackBotIdentity, SlackChannel, SocketConn, SocketError, SocketOpener,
};
use assistant_db::{apply, baseline_migrations, baseline_owner_modules, open_central};
use assistant_host::{
    serve_slack, EngagementMode, HostConfig, SchedulerTickConfig, SlackServeOptions, UnknownPolicy,
};
use assistant_permissions::{add_user_dm, create_user};
use assistant_router::{count_drops_by_reason, DropReason};
use assistant_scheduler::{
    item_status, list_items, next_claimable_occurrence, pause_item, upsert_item, ContextPolicy,
    Recurrence, ScheduleIntent, ScheduleStatus, ScheduledMessageMeta, Weekday,
};
use assistant_runtime_docker::{FakeRuntime, ImageRef, OneCliReadiness, RunnerAuthMode};
use assistant_session::{
    init_session, session_exists, verify_sequence_parity, LocalControl,
    SessionLayout,
};

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
                    if fake.emit_reply(seq,"text", &format!("echo: {content}")).is_ok() {
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
                    if fake.emit_reply(seq,"schedule_message", &payload).is_ok() {
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
                    if fake.emit_reply(seq,"cancel_schedule", &payload).is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// A fake shim that emits one outbound action of an arbitrary `kind` (carrying
/// `payload`) per new inbound. Used for the pause/resume interception tests,
/// which share the cancel shim's shape but a different action kind.
fn spawn_action_shim(
    layout: SessionLayout,
    stop: Arc<AtomicBool>,
    kind: &'static str,
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
                    if fake.emit_reply(seq,kind, &payload).is_ok() {
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
                        fake.emit_reply(seq,"text", &format!("echo: {content}"))
                    } else {
                        fake.emit_reply(seq,"save_memory", &payload)
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
fn a_calendar_schedule_message_records_a_recurrence_with_a_computed_first_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // A calendar recurrence carries a local wall-clock time, not after_seconds;
    // the host parses it into a canonical Weekly recurrence and computes the
    // first fire itself.
    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = r#"{"text":"weekday standup","calendar":{"kind":"weekly","days":["mon","tue","wed","thu","fri"],"at":"09:00","tz":"Europe/London"}}"#;
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
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"standup every weekday at 9"}"#,
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
        "a schedule_message action must not be posted to Slack: {:?}",
        posts.borrow()
    );

    let conn = open_central(&central).unwrap();
    let items = list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap();
    assert_eq!(items.len(), 1, "the turn created exactly one active scheduled item");
    assert_eq!(items[0].session_id.as_deref(), Some("C1"));
    assert_eq!(items[0].intent, "weekday standup");
    assert_eq!(
        items[0].recurrence,
        Some(Recurrence::Weekly {
            weekdays: vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ],
            minute_of_day: 9 * 60,
            tz: "Europe/London".into(),
        })
    );
    let due = items[0].process_after.expect("a calendar item has a computed first fire");
    assert!(
        due >= now_secs(),
        "the computed first fire is in the future: due={due} now={}",
        now_secs()
    );
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
fn a_turns_pause_schedule_action_marks_the_item_paused_and_is_not_posted() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // Seed an active recurring item the turn will pause by id.
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

    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = format!(r#"{{"scheduled_item_id":"{item_id}"}}"#);
    let shim = spawn_action_shim(layout.clone(), shim_stop.clone(), "pause_schedule", payload);

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"pause my standup"}"#,
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
        "a pause_schedule action must not be posted to Slack: {:?}",
        posts.borrow()
    );

    // The item is now paused — out of the active set but not terminal.
    let conn = open_central(&central).unwrap();
    assert_eq!(item_status(&conn, &item_id).unwrap(), Some(ScheduleStatus::Paused));
    assert!(
        list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap().is_empty(),
        "a paused item must not remain in the active set"
    );
    drop(conn);

    verify_sequence_parity(&layout).unwrap();

    shim_stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_turns_resume_schedule_action_marks_the_item_active_and_is_not_posted() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("central.db");
    migrate_central(&central);
    register_dm(&central, "rob", "U1");

    let layout = SessionLayout::derive(&sessions, "slack", "C1").unwrap();
    init_session(&layout).unwrap();

    // Seed an item, then pause it, so the turn can resume it by id.
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
        pause_item(&conn, &item_id).unwrap();
    }

    let shim_stop = Arc::new(AtomicBool::new(false));
    let payload = format!(r#"{{"scheduled_item_id":"{item_id}"}}"#);
    let shim = spawn_action_shim(layout.clone(), shim_stop.clone(), "resume_schedule", payload);

    let exhausted = Rc::new(Cell::new(false));
    let posts: Posts = Rc::new(RefCell::new(Vec::new()));
    let mut channel = SlackChannel::new(FakeApi {
        posts: posts.clone(),
        on_post: Rc::new(|| {}),
    });
    let mut opener = ScriptedOpener {
        frames: vec![events_api(
            "env-1",
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"resume my standup"}"#,
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
        "a resume_schedule action must not be posted to Slack: {:?}",
        posts.borrow()
    );

    // The item is active again — back in the swept set.
    let conn = open_central(&central).unwrap();
    assert_eq!(item_status(&conn, &item_id).unwrap(), Some(ScheduleStatus::Active));
    assert_eq!(
        list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap().len(),
        1,
        "a resumed item must rejoin the active set"
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
