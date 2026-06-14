//! PR 3 acceptance tests for the per-session DB protocol:
//! - host enqueues inbound and reads outbound from a fake writer;
//! - stale `processing_ack` cleanup works only under the stopped-session lock;
//! - read-write `outbound.db` open is rejected while a container is alive;
//! - old (v1) session fixtures migrate lazily on open;
//! - the runner compatibility check refuses unsupported schema versions.

use std::time::Duration;

use claw_session::{
    container_liveness, current_outbound_compat, enqueue_inbound, enqueue_inbound_keyed,
    lazy_migrate, migrations_v1_only, open_outbound_recovery, read_outbound, schema_version,
    verify_sequence_parity, DbKind, InboundMessage, Liveness, LocalControl, SchemaCompat,
    SessionError, SessionLayout, CURRENT_INBOUND_VERSION,
};

const TTL: Duration = Duration::from_secs(30);

fn layout(root: &std::path::Path) -> SessionLayout {
    SessionLayout::derive(root, "orchestrator", "sess-1").unwrap()
}

#[test]
fn host_enqueues_inbound_and_reads_outbound_from_fake_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();

    // Host enqueues an inbound message (even sequence).
    let in_seq = enqueue_inbound(
        control.layout(),
        &InboundMessage {
            sender: "local-cli".to_string(),
            content: "hello".to_string(),
            metadata: None,
        },
    )
    .unwrap();
    assert_eq!(in_seq % 2, 0);

    // Fake container reads inbound read-only, then emits an odd-seq reply.
    let container = control.fake_container();
    container.start("run-1").unwrap();
    let seen = container.read_inbound().unwrap();
    assert_eq!(seen, vec![(in_seq, "hello".to_string())]);
    let out_seq = container.emit("text", "hi back").unwrap();
    assert_eq!(out_seq % 2, 1);
    container.stop().unwrap();

    // Host reads outbound read-only.
    let outbound = read_outbound(control.layout(), current_outbound_compat()).unwrap();
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].seq, out_seq);
    assert_eq!(outbound[0].content, "hi back");

    // Parity holds across both DBs.
    verify_sequence_parity(control.layout()).unwrap();
}

#[test]
fn second_host_inbound_message_uses_next_even_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();
    let msg = |c: &str| InboundMessage {
        sender: "local-cli".to_string(),
        content: c.to_string(),
        metadata: None,
    };
    let a = enqueue_inbound(control.layout(), &msg("a")).unwrap();
    let b = enqueue_inbound(control.layout(), &msg("b")).unwrap();
    assert_eq!(a, 0);
    assert_eq!(b, 2);
}

#[test]
fn keyed_inbound_enqueue_is_idempotent_on_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();
    let msg = |c: &str| InboundMessage {
        sender: "scheduler".to_string(),
        content: c.to_string(),
        metadata: None,
    };

    // A scheduler firing keyed on the occurrence: the first attempt writes the
    // inbound row; a later retry (same key) reuses it instead of duplicating.
    let first = enqueue_inbound_keyed(control.layout(), &msg("ping"), Some("occ-1")).unwrap();
    let retry = enqueue_inbound_keyed(control.layout(), &msg("ping"), Some("occ-1")).unwrap();
    assert_eq!(first, retry);
    assert_eq!(first % 2, 0);

    // A different occurrence's key gets its own row.
    let other = enqueue_inbound_keyed(control.layout(), &msg("ping"), Some("occ-2")).unwrap();
    assert_ne!(other, first);

    // Keyless enqueues are never deduped (each is a distinct human message).
    let k1 = enqueue_inbound_keyed(control.layout(), &msg("hi"), None).unwrap();
    let k2 = enqueue_inbound_keyed(control.layout(), &msg("hi"), None).unwrap();
    assert_ne!(k1, k2);

    // Exactly three rows landed: occ-1 (once), occ-2, and two keyless = 4. The
    // keyed retry did not add a row, so the container sees one row per occurrence.
    let container = control.fake_container();
    container.start("run-1").unwrap();
    let seen = container.read_inbound().unwrap();
    assert_eq!(seen.len(), 4);
    let ping_count = seen.iter().filter(|(_, c)| c == "ping").count();
    assert_eq!(ping_count, 2, "occ-1 retry must not duplicate its inbound row");
}

#[test]
fn outbound_rw_rejected_while_container_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();

    let container = control.fake_container();
    container.start("run-1").unwrap();
    assert_eq!(container_liveness(control.layout(), TTL), Liveness::Alive);

    assert!(matches!(
        open_outbound_recovery(control.layout(), TTL),
        Err(SessionError::ContainerAlive { .. })
    ));
}

#[test]
fn stale_ack_cleanup_only_under_stopped_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();

    // Container starts, claims an inbound seq, then dies without releasing it.
    let in_seq = enqueue_inbound(
        control.layout(),
        &InboundMessage {
            sender: "local-cli".to_string(),
            content: "do work".to_string(),
            metadata: None,
        },
    )
    .unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();
    container.claim(in_seq, "run-1").unwrap();

    // While alive, recovery (and thus cleanup) is refused.
    assert!(matches!(
        open_outbound_recovery(control.layout(), TTL),
        Err(SessionError::ContainerAlive { .. })
    ));

    // Container dies (heartbeat removed, state stopped).
    container.stop().unwrap();
    assert_eq!(container_liveness(control.layout(), TTL), Liveness::Stopped);

    let guard = open_outbound_recovery(control.layout(), TTL).unwrap();
    let removed = guard.cleanup_stale_acks().unwrap();
    assert_eq!(removed, 1);
    guard
        .write_recovery_meta("last_recovery", "cleared stale ack")
        .unwrap();
}

#[test]
fn recovery_lock_is_exclusive() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();
    // No container ever ran -> stopped.
    let guard = open_outbound_recovery(control.layout(), TTL).unwrap();
    // A second recovery open must fail while the first guard holds the lock.
    assert!(matches!(
        open_outbound_recovery(control.layout(), TTL),
        Err(SessionError::SessionLocked { .. })
    ));
    drop(guard);
    // Once released, recovery can be taken again.
    let _again = open_outbound_recovery(control.layout(), TTL).unwrap();
}

#[test]
fn old_v1_fixture_migrates_lazily_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = layout(tmp.path());
    std::fs::create_dir_all(layout.dir()).unwrap();

    // Fabricate an old inbound.db pinned at schema v1.
    {
        let mut conn = claw_session::db::open_read_write(&layout.inbound_db_path()).unwrap();
        lazy_migrate(&mut conn, DbKind::Inbound, &migrations_v1_only(DbKind::Inbound)).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
    }

    // Opening through the normal host path migrates it forward lazily.
    let conn = claw_session::open_inbound(&layout).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), CURRENT_INBOUND_VERSION);
    // The v2 column now exists.
    let has_col: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages_in') WHERE name = 'edited_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_col, 1);
}

#[test]
fn old_v1_outbound_fixture_migrates_lazily_on_recovery() {
    // An archived session whose outbound.db is pinned at v1 migrates forward
    // when the host opens it (here via the recovery path, which is the only
    // host write path for outbound.db).
    let tmp = tempfile::tempdir().unwrap();
    let layout = layout(tmp.path());
    std::fs::create_dir_all(layout.dir()).unwrap();
    {
        let mut conn = claw_session::db::open_read_write(&layout.outbound_db_path()).unwrap();
        lazy_migrate(&mut conn, DbKind::Outbound, &migrations_v1_only(DbKind::Outbound)).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
    }
    let guard = open_outbound_recovery(&layout, TTL).unwrap();
    assert_eq!(schema_version(guard.connection()).unwrap(), 2);
}

#[test]
fn runner_refuses_unsupported_schema_version() {
    let tmp = tempfile::tempdir().unwrap();
    let control = LocalControl::new(layout(tmp.path()));
    control.init().unwrap();
    control.fake_container().emit("text", "ok").unwrap();

    // A runner that only supports v1 must refuse the v2 outbound DB.
    let err = read_outbound(control.layout(), SchemaCompat::exact(1)).unwrap_err();
    match err {
        SessionError::UnsupportedSchemaVersion {
            found,
            supported_max,
            ..
        } => {
            assert_eq!(found, 2);
            assert_eq!(supported_max, 1);
        }
        other => panic!("expected unsupported schema version, got {other:?}"),
    }
}
