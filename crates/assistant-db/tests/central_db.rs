//! Integration tests for the central DB: baseline schema application on a real
//! on-disk file, idempotent rerun, version recording, and checksum drift.

use assistant_db::schema::baseline_owner_modules;
use assistant_db::{
    apply, baseline_migrations, open_central, record_versions, DbError, Migration, MigrationSet,
    VersionRecord, BASELINE_TABLES,
};

fn module_order() -> Vec<String> {
    baseline_owner_modules()
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn baseline_creates_all_tables_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("main.db");
    let mut conn = open_central(&db_path).unwrap();

    let set = baseline_migrations(module_order());
    apply(&mut conn, &set).unwrap();

    for table in BASELINE_TABLES {
        assert!(table_exists(&conn, table), "missing table {table}");
    }
    // Meta tables exist too.
    assert!(table_exists(&conn, "schema_migrations"));
    assert!(table_exists(&conn, "module_versions"));
}

#[test]
fn rerun_open_and_migrate_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("main.db");

    {
        let mut conn = open_central(&db_path).unwrap();
        let first = apply(&mut conn, &baseline_migrations(module_order())).unwrap();
        assert_eq!(first.applied.len(), baseline_owner_modules().len());
    }
    {
        let mut conn = open_central(&db_path).unwrap();
        let second = apply(&mut conn, &baseline_migrations(module_order())).unwrap();
        assert!(second.applied.is_empty());
        assert_eq!(second.skipped.len(), baseline_owner_modules().len());
    }
}

#[test]
fn version_recording_persists() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("main.db");
    let mut conn = open_central(&db_path).unwrap();
    apply(&mut conn, &baseline_migrations(module_order())).unwrap();

    record_versions(
        &conn,
        &VersionRecord {
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            modules: vec![("assistant-core".to_string(), "0.1.0".to_string())],
        },
    )
    .unwrap();

    let recorded: String = conn
        .query_row(
            "SELECT platform_version FROM module_versions WHERE module_id = 'assistant-core'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, "0.1.0");
}

#[test]
fn checksum_drift_refuses_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("main.db");

    {
        let mut conn = open_central(&db_path).unwrap();
        let mut set = MigrationSet::new(vec!["assistant-core".to_string()]);
        set.add(Migration::new(
            "assistant-core",
            1,
            "thing",
            "CREATE TABLE thing (id INTEGER);",
        ));
        apply(&mut conn, &set).unwrap();
    }
    {
        let mut conn = open_central(&db_path).unwrap();
        let mut drifted = MigrationSet::new(vec!["assistant-core".to_string()]);
        drifted.add(Migration::new(
            "assistant-core",
            1,
            "thing",
            "CREATE TABLE thing (id INTEGER, extra TEXT);",
        ));
        assert!(matches!(
            apply(&mut conn, &drifted),
            Err(DbError::ChecksumMismatch { .. })
        ));
    }
}
