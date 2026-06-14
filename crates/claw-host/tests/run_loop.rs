//! Offline coverage of the host run-loop: a full terminal turn round-trips over
//! real session DBs using `FakeRuntime` and an in-process fake shim that mirrors
//! a real container. No Docker, no Claude, no network — so this stays green in
//! the sandbox.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use claw_host::{sweep_once, Host, HostConfig, HostError, SweepReport};
use claw_runtime_docker::{
    FakeRuntime, ImageRef, LifecyclePolicy, OneCliReadiness, RunnerAuthMode,
};
use claw_session::{
    init_session, session_exists, verify_sequence_parity, LocalControl, SessionLayout,
};

fn ready() -> OneCliReadiness {
    OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: true,
    }
}

fn test_config(roots: Vec<PathBuf>, mode: RunnerAuthMode, onecli: OneCliReadiness) -> HostConfig {
    let mut config = HostConfig::new(ImageRef::new("claw-agent-base", "0.1.0"), roots, mode, onecli)
        // Stand in for an installation identifier so container names are
        // `{agent}-{session}`, matching production (the CA dir is unused here).
        .with_onecli_agent("testns".to_string(), PathBuf::new());
    // Tight cadence so the offline loop completes in milliseconds. The turn
    // backstop is kept well above the session DB `busy_timeout` (5s) so a
    // contended SQLite op under heavy parallel test load can't be mistaken for
    // a stalled turn.
    config.poll_interval = Duration::from_millis(5);
    config.turn_timeout = Duration::from_secs(30);
    config
}

/// A background thread standing in for the container: it lays a heartbeat, reads
/// inbound read-only, and emits one odd-seq echo per new inbound message —
/// exactly what a real runner does over the mounted session DBs.
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
                    // Mark handled only once the echo is actually committed: a
                    // transient SQLite lock blip over the bind mount must be
                    // retried on the next tick, not silently dropped. (The real
                    // shim must observe the same exactly-once-on-success rule.)
                    if fake.emit("text", &format!("echo: {content}")).is_ok() {
                        handled.insert(seq);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

/// Regression: a *partial* session — `inbound.db` present (so `session_exists`
/// is true and `init_session` is skipped) but the inbox/outbox bind-mount dirs
/// removed (a manual reset, a file-sharing hiccup) — must still spawn.
/// `ensure_spawned` re-asserts the managed dirs, so the missing bind sources are
/// rebuilt rather than failing `docker run` with "bind source path does not
/// exist".
#[test]
fn partial_session_recreates_missing_bind_dirs_before_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "partial1").unwrap();

    init_session(&layout).unwrap();

    // Leave the DBs, drop the bind-mount dirs: `session_exists` (keyed on
    // inbound.db) stays true, so `init_session` won't re-run on the next turn.
    std::fs::remove_dir_all(layout.inbox_dir()).unwrap();
    std::fs::remove_dir_all(layout.outbox_dir()).unwrap();
    assert!(
        session_exists(&layout),
        "inbound.db should still mark the session as existing"
    );
    assert!(!layout.inbox_dir().exists());
    assert!(!layout.outbox_dir().exists());

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let delivered = host.process_turn("hello", &mut out).unwrap();

    assert_eq!(delivered, 1);
    assert_eq!(String::from_utf8(out).unwrap(), "echo: hello\n");
    // The fix: the spawn path re-created the missing bind sources.
    assert!(
        layout.inbox_dir().exists(),
        "inbox dir should be recreated before spawn"
    );
    assert!(
        layout.outbox_dir().exists(),
        "outbox dir should be recreated before spawn"
    );

    host.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn full_turn_round_trips_over_fake_runtime_and_shim() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sess1").unwrap();

    // Initialize + migrate both session DBs before either side runs.
    init_session(&layout).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let delivered = host.process_turn("hello", &mut out).unwrap();

    assert_eq!(delivered, 1);
    assert_eq!(String::from_utf8(out).unwrap(), "echo: hello\n");
    assert!(host.container_id().is_some(), "a container should have been spawned");
    // The container name is `{agent}-{session}`, not a shared `claw-` prefix, so
    // distinct installations never collide on a common session id.
    assert_eq!(host.runtime().spawned[0].name, "testns-sess1");
    // Host-even / container-odd parity holds across both DBs.
    verify_sequence_parity(&layout).unwrap();

    host.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn catalog_memory_is_injected_into_inbound_metadata() {
    use claw_db::{apply, baseline_migrations, baseline_owner_modules, open_central, MigrationSet};
    use claw_memory::{
        upsert_entry, Confidence, MemoryFrontMatter, Retention, ReusePolicy, Scope, SourceType,
    };

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("main.db");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "mem1").unwrap();
    init_session(&layout).unwrap();

    // Migrate the central DB to the baseline (claw-memory v1 included) plus the
    // memory catalog v2, then seed one always-relevant, reusable entry. Drop the
    // connection before the turn runs so the host's own read is uncontended.
    {
        let order: Vec<String> = baseline_owner_modules().into_iter().map(str::to_string).collect();
        let mut conn = open_central(&central).unwrap();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        let mut catalog = MigrationSet::new(vec![claw_memory::MODULE_ID.to_string()]);
        for migration in claw_memory::migrations() {
            catalog.add(migration);
        }
        apply(&mut conn, &catalog).unwrap();

        let front = MemoryFrontMatter {
            memory_id: "mem-allchats".to_string(),
            owner_agent_group_id: "host".to_string(),
            scope: Scope::AllChats,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: None,
            captured_at: Some("2026-06-01T10:00:00Z".to_string()),
            confidence: Confidence::High,
            // SameScope + AllChats stays eligible under a default context, so this
            // proves the scope-matching path injects (not just BroaderOk).
            reuse_policy: ReusePolicy::SameScope,
            retention: Retention::Normal,
        };
        upsert_entry(&conn, 1, "notes/mem-allchats.md", &front).unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let groups = tmp.path().join("groups");
    let config = test_config(vec![sessions], RunnerAuthMode::Stub, ready())
        .with_memory(central, 1, 5, groups, "host".to_string());
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let delivered = host.process_turn("hello", &mut out).unwrap();

    host.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();

    // The content-only shim never sees metadata, so the reply is the plain echo.
    assert_eq!(delivered, 1);
    assert_eq!(String::from_utf8(out).unwrap(), "echo: hello\n");

    // The host injected the rendered catalog block into the inbound metadata
    // (seq 0, the first host-even message). The shim consuming it is the
    // live-only tail; here we assert the host wired it through.
    let conn = rusqlite::Connection::open(layout.inbound_db_path()).unwrap();
    let metadata: Option<String> = conn
        .query_row("SELECT metadata FROM messages_in WHERE seq = 0", [], |row| row.get(0))
        .unwrap();
    let metadata = metadata.expect("memory block should be injected into inbound metadata");
    assert!(metadata.contains("<retrieved_memories>"), "got {metadata:?}");
    assert!(metadata.contains("mem-allchats"), "got {metadata:?}");
}

#[test]
fn scheduler_sweep_fires_due_item_once_and_expires_sticky() {
    use claw_db::{apply, baseline_migrations, baseline_owner_modules, open_central, MigrationSet};
    use claw_router::open_sticky;
    use claw_scheduler::{upsert_item, ContextPolicy, ScheduleIntent, ScheduledMessageMeta};

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let central = tmp.path().join("main.db");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sched1").unwrap();
    init_session(&layout).unwrap();

    let now: i64 = 1_000;

    // Migrate the baseline, then layer the router (sticky-engagement) and scheduler
    // (items/occurrences) v2 tables exactly as `setup_steps::domain_migrations`
    // does. Seed one one-shot item due at `now` bound to the live session, and one
    // already-expired sticky window the sweep should clear.
    {
        let order: Vec<String> = baseline_owner_modules().into_iter().map(str::to_string).collect();
        let mut conn = open_central(&central).unwrap();
        apply(&mut conn, &baseline_migrations(order)).unwrap();

        let mut domain = MigrationSet::new(vec![
            claw_router::MODULE_ID.to_string(),
            claw_scheduler::MODULE_ID.to_string(),
        ]);
        for migration in claw_router::migrations().into_iter().chain(claw_scheduler::migrations()) {
            domain.add(migration);
        }
        apply(&mut conn, &domain).unwrap();

        let meta = ScheduledMessageMeta::create(
            1,
            ScheduleIntent { created_by: "U1".into(), summary: "daily ping".into(), created_at: 1 },
            now,
            None,
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        upsert_item(&conn, &meta, Some("sched1")).unwrap();

        open_sticky(&conn, 1, "chan:C1", Some("root-1"), None, Some(now - 1)).unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let config = test_config(vec![sessions.clone()], RunnerAuthMode::Stub, ready());
    let conn = open_central(&central).unwrap();
    let factory = FakeRuntime::new;

    let report = sweep_once(&conn, &sessions, "orchestrator", 1, "host-test", 30, &config, &factory, now)
        .unwrap();

    assert_eq!(report.expired_sticky, 1, "the stale sticky window should be swept");
    assert_eq!(report.fired, 1, "the due item should fire exactly one occurrence");

    // The scheduled turn drove the item's summary into the session; the fake shim
    // echoed it back at the first odd seq.
    let session_db = rusqlite::Connection::open(layout.outbound_db_path()).unwrap();
    let reply: String = session_db
        .query_row("SELECT content FROM messages_out WHERE seq = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reply, "echo: daily ping");

    // Exactly-once: a later sweep finds nothing due (the occurrence is fired) and
    // no sticky left to expire.
    let again = sweep_once(&conn, &sessions, "orchestrator", 1, "host-test", 30, &config, &factory, now + 100)
        .unwrap();
    assert_eq!(again, SweepReport { expired_sticky: 0, fired: 0 });

    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn two_turns_reuse_one_container() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sess2").unwrap();

    init_session(&layout).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out1: Vec<u8> = Vec::new();
    host.process_turn("first", &mut out1).unwrap();
    let id1 = host.container_id().cloned();

    let mut out2: Vec<u8> = Vec::new();
    host.process_turn("second", &mut out2).unwrap();
    let id2 = host.container_id().cloned();

    assert_eq!(String::from_utf8(out1).unwrap(), "echo: first\n");
    assert_eq!(String::from_utf8(out2).unwrap(), "echo: second\n");
    // The persistent per-session container is reused, not respawned.
    assert_eq!(id1, id2);

    host.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_restarted_host_resumes_its_delivered_watermark_and_does_not_re_deliver() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "resume1").unwrap();

    init_session(&layout).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    // First process: deliver one reply, which marks outbound seq 1 delivered.
    let config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    let mut first = Host::new(layout.clone(), FakeRuntime::new(), config.clone());
    let mut out1: Vec<u8> = Vec::new();
    first.process_turn("hello", &mut out1).unwrap();
    assert_eq!(String::from_utf8(out1).unwrap(), "echo: hello\n");
    first.shutdown().unwrap();
    drop(first);

    // A fresh Host on the SAME session stands in for a daemon restart: its
    // in-memory watermark starts at 0, but it must resume from the persisted
    // `delivered` marker (seq 1) instead of re-reading the prior reply. Without
    // the fix it would return the stale "echo: hello" before the new turn ran.
    let mut restarted = Host::new(layout.clone(), FakeRuntime::new(), config);
    let mut out2: Vec<u8> = Vec::new();
    let delivered = restarted.process_turn("again", &mut out2).unwrap();

    let rendered = String::from_utf8(out2).unwrap();
    assert_eq!(delivered, 1, "only the new turn's reply, not the stale one");
    assert_eq!(rendered, "echo: again\n");
    assert!(!rendered.contains("echo: hello"), "must not re-deliver the prior reply");
    verify_sequence_parity(&layout).unwrap();

    restarted.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn dead_container_is_reaped_and_host_recovers_on_next_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sess5").unwrap();

    init_session(&layout).unwrap();

    let mut config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    // Tight death thresholds so the first turn's never-refreshed placeholder
    // heartbeat goes stale in milliseconds. The shim heartbeats far faster than
    // `stale_after` once running, so the recovery turn stays live under jitter.
    config.policy = LifecyclePolicy::new(Duration::from_millis(30), Duration::from_millis(60));
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    // Turn 1: no shim is running, so nothing ever refreshes the heartbeat. The
    // container is spawned, then declared dead when the heartbeat goes stale.
    let mut out1: Vec<u8> = Vec::new();
    let err = host.process_turn("first", &mut out1).unwrap_err();
    assert!(matches!(err, HostError::ContainerDied { .. }), "got {err}");
    // The dead container was reaped and the handle cleared, leaving the host
    // ready to respawn rather than stuck polling the corpse.
    assert!(host.container_id().is_none(), "handle must be cleared after death");
    assert_eq!(host.runtime().spawned.len(), 1, "exactly one container spawned so far");
    assert_eq!(host.runtime().stopped.len(), 1, "the dead container was reaped");

    // Turn 2: a live shim now backs the session. The next turn must spawn a
    // fresh container (not reuse the reaped one) and deliver a reply, including
    // the message orphaned by the dead turn.
    let stop = Arc::new(AtomicBool::new(false));
    let shim = spawn_fake_shim(layout.clone(), stop.clone());

    let mut out2: Vec<u8> = Vec::new();
    let delivered = host.process_turn("second", &mut out2).unwrap();

    assert!(delivered >= 1);
    let rendered = String::from_utf8(out2).unwrap();
    // The inbound enqueued by the dead turn is picked up by the fresh container.
    assert!(rendered.contains("echo: first"), "orphaned message not recovered: {rendered:?}");
    assert_eq!(host.runtime().spawned.len(), 2, "a new container was spawned after death");
    verify_sequence_parity(&layout).unwrap();

    host.shutdown().unwrap();
    stop.store(true, Ordering::Relaxed);
    shim.join().unwrap();
}

#[test]
fn a_stale_pre_existing_heartbeat_is_refreshed_so_a_fresh_spawn_is_not_reaped() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "resume2").unwrap();

    init_session(&layout).unwrap();

    // Stand in for a session left behind by a previous daemon run that was down
    // longer than `stale_after`: a heartbeat file whose mtime is well past it.
    let hb = std::fs::File::create(layout.heartbeat_path()).unwrap();
    hb.set_modified(std::time::SystemTime::now() - Duration::from_secs(30)).unwrap();
    drop(hb);

    let mut config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    // `stale_after` (10s) comfortably exceeds the turn backstop (150ms): if the
    // spawn refreshes the heartbeat to "now", it cannot go stale within the turn,
    // so a turn with no shim ends in Timeout. Without the refresh the 30s-old
    // mtime is read on the first poll and the fresh container is reaped
    // immediately as ContainerDied — the regression this guards.
    config.policy = LifecyclePolicy::new(Duration::from_secs(5), Duration::from_secs(10));
    config.turn_timeout = Duration::from_millis(150);
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let err = host.process_turn("hello", &mut out).unwrap_err();
    assert!(matches!(err, HostError::Timeout { .. }), "expected Timeout, got {err}");
    assert!(host.container_id().is_some(), "container kept (booting), not reaped");
}

#[test]
fn claude_spawn_is_refused_when_onecli_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sess3").unwrap();

    init_session(&layout).unwrap();

    let not_ready = OneCliReadiness {
        proxy_configured: false,
        anthropic_secret_present: false,
        placeholder_injection_ok: false,
    };
    let config = test_config(vec![sessions], RunnerAuthMode::ClaudeOAuth, not_ready);
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let err = host.process_turn("hi", &mut out).unwrap_err();
    assert!(matches!(err, HostError::Spawn(_)), "got {err}");
    assert!(host.container_id().is_none(), "no container should be spawned");
}

#[test]
fn output_is_refused_when_shim_protocol_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "sess4").unwrap();

    init_session(&layout).unwrap();

    let mut config = test_config(vec![sessions], RunnerAuthMode::Stub, ready());
    // The shim advertises a version the run's declared protocol is not in.
    config.shim_supported = vec!["9.9.9".to_string()];
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let err = host.process_turn("hi", &mut out).unwrap_err();
    assert!(matches!(err, HostError::Protocol(_)), "got {err}");
    assert!(host.container_id().is_none());
}
