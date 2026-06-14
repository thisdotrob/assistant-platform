//! End-to-end proof of the product upgrade entry point against a real instance
//! on disk. A "prior baseline" instance is fabricated under a temp HOME — a
//! central DB that has never been migrated, plus per-session DBs left at the
//! old v1 schema — and then driven through the public `assistant_cli::upgrade`. The
//! run must apply the full central baseline, sweep every session forward to the
//! current schema, and then be a no-op on rerun. A dry run must report the same
//! work while writing nothing, and an uninitialized instance must be refused.

use std::path::Path;

use assistant_cli::{upgrade, BootstrapRequest};
use assistant_config::InstanceLayout;
use assistant_session::{
    db as session_db, lazy_migrate, migrations_v1_only, schema_version, DbKind, SessionLayout,
    CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION,
};

const NAMESPACE: &str = "testns";

fn request(home: &Path, dry_run: bool) -> BootstrapRequest {
    BootstrapRequest {
        namespace: NAMESPACE.to_string(),
        product_id: "testprod".to_string(),
        product_version: "0.1.0".to_string(),
        instance: None,
        enabled_modules: Vec::new(),
        home: Some(home.to_path_buf()),
        protected_roots: Vec::new(),
        dry_run,
    }
}

fn layout(home: &Path) -> InstanceLayout {
    InstanceLayout::derive(home, NAMESPACE, None).unwrap()
}

/// Create the instance root with a central DB that exists but has never been
/// migrated — the state a fresh or interrupted bootstrap leaves behind.
fn fabricate_empty_central(layout: &InstanceLayout) {
    std::fs::create_dir_all(&layout.root).unwrap();
    let conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
    drop(conn);
}

/// Create one session whose inbound + outbound DBs sit at the old v1 schema.
fn fabricate_v1_session(layout: &InstanceLayout, group: &str, session: &str) -> SessionLayout {
    let sl = SessionLayout::derive(&layout.sessions_dir(), group, session).unwrap();
    std::fs::create_dir_all(sl.dir()).unwrap();
    let mut inbound = session_db::open_read_write(&sl.inbound_db_path()).unwrap();
    lazy_migrate(&mut inbound, DbKind::Inbound, &migrations_v1_only(DbKind::Inbound)).unwrap();
    drop(inbound);
    let mut outbound = session_db::open_read_write(&sl.outbound_db_path()).unwrap();
    lazy_migrate(
        &mut outbound,
        DbKind::Outbound,
        &migrations_v1_only(DbKind::Outbound),
    )
    .unwrap();
    drop(outbound);
    sl
}

fn session_versions(sl: &SessionLayout) -> (u32, u32) {
    let inbound = session_db::open_read_only(&sl.inbound_db_path()).unwrap();
    let outbound = session_db::open_read_only(&sl.outbound_db_path()).unwrap();
    (
        schema_version(&inbound).unwrap(),
        schema_version(&outbound).unwrap(),
    )
}

#[test]
fn upgrade_applies_central_baseline_and_advances_sessions_then_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let layout = layout(home.path());
    fabricate_empty_central(&layout);
    let sl = fabricate_v1_session(&layout, "groupone", "sessone");

    // Sanity: the session really is behind before we upgrade.
    assert_eq!(session_versions(&sl), (1, 1));

    let code = upgrade(request(home.path(), false));
    assert_eq!(code, 0, "upgrade should succeed");

    // Central: the full baseline was applied (assistant-session's owner migration
    // among them).
    let conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
    let applied = assistant_db::applied_versions(&conn).unwrap();
    drop(conn);
    assert!(!applied.is_empty(), "central baseline applied");
    assert!(
        applied.iter().any(|(m, v)| m == "assistant-session" && *v == 1),
        "assistant-session baseline recorded: {applied:?}"
    );

    // Per-session: both DBs are now at the current schema.
    assert_eq!(
        session_versions(&sl),
        (CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION)
    );

    // Rerun is a no-op and still succeeds.
    let rerun = upgrade(request(home.path(), false));
    assert_eq!(rerun, 0, "idempotent rerun should succeed");
    assert_eq!(
        session_versions(&sl),
        (CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION)
    );
}

#[test]
fn dry_run_reports_work_but_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let layout = layout(home.path());
    fabricate_empty_central(&layout);
    let sl = fabricate_v1_session(&layout, "groupone", "sessone");

    let code = upgrade(request(home.path(), true));
    assert_eq!(code, 0, "dry run should succeed");

    // Central: nothing was recorded.
    let conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
    assert!(
        assistant_db::applied_versions(&conn).unwrap().is_empty(),
        "dry run must not record central migrations"
    );
    drop(conn);

    // Per-session: the DBs are untouched, still at v1.
    assert_eq!(session_versions(&sl), (1, 1));
}

#[test]
fn upgrade_on_an_uninitialized_instance_is_refused() {
    let home = tempfile::tempdir().unwrap();
    // No instance fabricated: the central DB does not exist.
    let code = upgrade(request(home.path(), false));
    assert_eq!(code, 1, "upgrading a non-bootstrapped instance must fail");
}
