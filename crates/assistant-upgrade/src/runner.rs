//! Idempotent upgrade runner.
//!
//! Applies any pending central migrations to an existing instance and then
//! eagerly migrates every per-session DB forward to the current schema. The run
//! is idempotent (a rerun applies nothing) and resumable (a run interrupted
//! after some sessions migrated picks up the rest on the next run, because each
//! migration is checksum-verified and skipped once recorded).
//!
//! All writes are confined to the instance root: the central DB path is a fixed
//! join under the root, and every session path is rebuilt through the validated
//! [`assistant_session::SessionLayout`], which forbids identifiers that could escape
//! the sessions root. A defensive [`guard_under_root`] check refuses any write
//! target that nonetheless resolves outside the root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path};

use assistant_config::InstanceLayout;
use assistant_db::{MigrationSet, VersionRecord};
use assistant_session::{db as session_db, lazy_migrate, migrations_for, schema_version, DbKind};

use crate::error::UpgradeError;
use crate::inventory::discover_sessions;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpgradeOptions {
    /// Report what would migrate without applying anything.
    pub dry_run: bool,
}

/// The migrations applied (or, under dry-run, that would be applied) to one
/// session's DBs. Sessions already at the current schema are omitted from the
/// report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpgrade {
    pub agent_group_id: String,
    pub session_id: String,
    pub inbound_applied: Vec<u32>,
    pub outbound_applied: Vec<u32>,
}

/// The outcome of an upgrade run: central migrations applied/skipped and the
/// sessions touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeReport {
    pub dry_run: bool,
    pub central_applied: Vec<(String, u32)>,
    pub central_skipped: Vec<(String, u32)>,
    pub sessions: Vec<SessionUpgrade>,
}

/// Apply pending central migrations and sweep every per-session DB forward.
///
/// `central_migrations` is the full current central migration set; already
/// applied migrations are checksum-verified and skipped. `version_record` is
/// the identity/versions re-recorded after a real (non-dry) run. Refuses to run
/// against an instance whose central DB does not yet exist.
pub fn upgrade_instance(
    layout: &InstanceLayout,
    central_migrations: &MigrationSet,
    version_record: &VersionRecord,
    options: UpgradeOptions,
) -> Result<UpgradeReport, UpgradeError> {
    let central_path = layout.central_db_path();
    if !central_path.exists() {
        return Err(UpgradeError::InstanceNotInitialized { path: central_path });
    }
    guard_under_root(&layout.root, &central_path)?;

    let (central_applied, central_skipped) = if options.dry_run {
        plan_central(&central_path, central_migrations)?
    } else {
        apply_central(&central_path, central_migrations, version_record)?
    };

    let sessions = sweep_sessions(layout, options.dry_run)?;

    Ok(UpgradeReport {
        dry_run: options.dry_run,
        central_applied,
        central_skipped,
        sessions,
    })
}

type MigrationList = Vec<(String, u32)>;

fn apply_central(
    central_path: &Path,
    set: &MigrationSet,
    record: &VersionRecord,
) -> Result<(MigrationList, MigrationList), UpgradeError> {
    let mut conn = assistant_db::open_central(central_path)?;
    let report = assistant_db::apply(&mut conn, set)?;
    assistant_db::record_versions(&conn, record)?;
    Ok((report.applied, report.skipped))
}

/// Determine which central migrations would apply without writing anything. A
/// migration is pending when its version exceeds the recorded high-water mark
/// for its module (platform migration sequences are contiguous per module).
fn plan_central(
    central_path: &Path,
    set: &MigrationSet,
) -> Result<(MigrationList, MigrationList), UpgradeError> {
    let conn = assistant_db::open_central(central_path)?;
    let recorded_max: BTreeMap<String, u32> =
        assistant_db::applied_versions(&conn)?.into_iter().collect();

    let mut would_apply = Vec::new();
    let mut would_skip = Vec::new();
    for migration in set.ordered()? {
        let max = recorded_max
            .get(&migration.module_id)
            .copied()
            .unwrap_or(0);
        let entry = (migration.module_id.clone(), migration.version);
        if migration.version > max {
            would_apply.push(entry);
        } else {
            would_skip.push(entry);
        }
    }
    Ok((would_apply, would_skip))
}

fn sweep_sessions(
    layout: &InstanceLayout,
    dry_run: bool,
) -> Result<Vec<SessionUpgrade>, UpgradeError> {
    let mut out = Vec::new();
    for discovered in discover_sessions(&layout.sessions_dir())? {
        let inbound_applied = sweep_db(&layout.root, &discovered.layout, DbKind::Inbound, dry_run)?;
        let outbound_applied =
            sweep_db(&layout.root, &discovered.layout, DbKind::Outbound, dry_run)?;
        if !inbound_applied.is_empty() || !outbound_applied.is_empty() {
            out.push(SessionUpgrade {
                agent_group_id: discovered.agent_group_id,
                session_id: discovered.session_id,
                inbound_applied,
                outbound_applied,
            });
        }
    }
    Ok(out)
}

/// Migrate one session DB forward, or report what would migrate under dry-run.
/// An absent DB file is left absent — the host creates session DBs lazily, so
/// upgrade migrates only what already exists.
fn sweep_db(
    root: &Path,
    session_layout: &assistant_session::SessionLayout,
    kind: DbKind,
    dry_run: bool,
) -> Result<Vec<u32>, UpgradeError> {
    let db_path = session_layout.db_path(kind);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let migrations = migrations_for(kind);

    if dry_run {
        let conn = session_db::open_read_only(&db_path)?;
        let current = schema_version(&conn)?;
        Ok(migrations
            .iter()
            .map(|m| m.version)
            .filter(|v| *v > current)
            .collect())
    } else {
        guard_under_root(root, &db_path)?;
        let mut conn = session_db::open_read_write(&db_path)?;
        Ok(lazy_migrate(&mut conn, kind, &migrations)?)
    }
}

/// Refuse any write target that contains a parent (`..`) component or whose
/// fully resolved path does not lie under the instance root. Canonicalizing both
/// sides resolves symlinks, so a symlinked directory planted inside the instance
/// tree cannot redirect a write outside the real root — a purely lexical
/// `starts_with` check would miss that. Every caller guards on the target's
/// existence first, so canonicalization always sees a real file.
fn guard_under_root(root: &Path, path: &Path) -> Result<(), UpgradeError> {
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(UpgradeError::WriteOutsideRoot {
            path: path.to_path_buf(),
        });
    }
    let canonical_root = fs::canonicalize(root).map_err(|source| UpgradeError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|source| UpgradeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if canonical_path.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(UpgradeError::WriteOutsideRoot {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_session::{lazy_migrate, migrations_v1_only, SessionLayout};
    use std::fs;
    use tempfile::TempDir;

    fn instance_layout(tmp: &TempDir) -> InstanceLayout {
        InstanceLayout::derive(tmp.path(), "testns", None).unwrap()
    }

    fn central_set(versions: &[u32]) -> MigrationSet {
        let mut set = MigrationSet::new(vec!["assistant-db".to_string()]);
        for &v in versions {
            let (name, sql): (&str, &str) = match v {
                1 => ("base", "CREATE TABLE t (id INTEGER);"),
                2 => ("add_extra", "ALTER TABLE t ADD COLUMN extra TEXT;"),
                _ => unreachable!("test only models versions 1 and 2"),
            };
            set.add(assistant_db::Migration::new("assistant-db", v, name, sql));
        }
        set
    }

    fn version_record() -> VersionRecord {
        VersionRecord {
            product_id: "testprod".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            modules: vec![("assistant-db".to_string(), "0.1.0".to_string())],
        }
    }

    /// Build an instance whose central DB is at assistant-db v1.
    fn build_v1_instance(layout: &InstanceLayout) {
        fs::create_dir_all(&layout.root).unwrap();
        let mut conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
        assistant_db::apply(&mut conn, &central_set(&[1])).unwrap();
        assistant_db::record_versions(&conn, &version_record()).unwrap();
    }

    /// Create a session whose inbound + outbound DBs are at the old v1 schema.
    fn add_v1_session(sessions_dir: &Path, group: &str, session: &str) {
        let sl = SessionLayout::derive(sessions_dir, group, session).unwrap();
        fs::create_dir_all(sl.dir()).unwrap();
        let mut inbound = session_db::open_read_write(&sl.inbound_db_path()).unwrap();
        lazy_migrate(&mut inbound, DbKind::Inbound, &migrations_v1_only(DbKind::Inbound)).unwrap();
        let mut outbound = session_db::open_read_write(&sl.outbound_db_path()).unwrap();
        lazy_migrate(
            &mut outbound,
            DbKind::Outbound,
            &migrations_v1_only(DbKind::Outbound),
        )
        .unwrap();
    }

    #[test]
    fn upgrade_from_baseline_then_idempotent_rerun() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);
        build_v1_instance(&layout);
        add_v1_session(&layout.sessions_dir(), "groupone", "sessone");

        let report = upgrade_instance(
            &layout,
            &central_set(&[1, 2]),
            &version_record(),
            UpgradeOptions::default(),
        )
        .unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.central_applied, vec![("assistant-db".to_string(), 2)]);
        assert_eq!(report.central_skipped, vec![("assistant-db".to_string(), 1)]);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].inbound_applied, vec![2, 3]);
        assert_eq!(report.sessions[0].outbound_applied, vec![2]);

        // Central DB is now at v2 on disk.
        let conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
        assert_eq!(
            assistant_db::applied_versions(&conn).unwrap(),
            vec![("assistant-db".to_string(), 2)]
        );
        drop(conn);

        // Rerun applies nothing.
        let rerun = upgrade_instance(
            &layout,
            &central_set(&[1, 2]),
            &version_record(),
            UpgradeOptions::default(),
        )
        .unwrap();
        assert!(rerun.central_applied.is_empty());
        assert_eq!(
            rerun.central_skipped,
            vec![("assistant-db".to_string(), 1), ("assistant-db".to_string(), 2)]
        );
        assert!(rerun.sessions.is_empty());
    }

    #[test]
    fn dry_run_reports_pending_but_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);
        build_v1_instance(&layout);
        add_v1_session(&layout.sessions_dir(), "groupone", "sessone");

        let report = upgrade_instance(
            &layout,
            &central_set(&[1, 2]),
            &version_record(),
            UpgradeOptions { dry_run: true },
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.central_applied, vec![("assistant-db".to_string(), 2)]);
        assert_eq!(report.central_skipped, vec![("assistant-db".to_string(), 1)]);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].inbound_applied, vec![2, 3]);
        assert_eq!(report.sessions[0].outbound_applied, vec![2]);

        // Nothing actually migrated: central still at v1, session still at v1.
        let conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
        assert_eq!(
            assistant_db::applied_versions(&conn).unwrap(),
            vec![("assistant-db".to_string(), 1)]
        );
        drop(conn);
        let sl = SessionLayout::derive(&layout.sessions_dir(), "groupone", "sessone").unwrap();
        let inbound = session_db::open_read_only(&sl.inbound_db_path()).unwrap();
        assert_eq!(schema_version(&inbound).unwrap(), 1);
    }

    #[test]
    fn missing_central_db_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);

        let err = upgrade_instance(
            &layout,
            &central_set(&[1]),
            &version_record(),
            UpgradeOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::InstanceNotInitialized { .. }));
    }

    #[test]
    fn guard_allows_a_real_path_inside_the_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("instance");
        fs::create_dir_all(&root).unwrap();
        let inside = root.join("main.db");
        fs::write(&inside, b"x").unwrap();

        assert!(guard_under_root(&root, &inside).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn guard_refuses_a_symlink_escaping_the_root() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("instance");
        fs::create_dir_all(&root).unwrap();
        // A real file outside the instance root...
        let outside = tmp.path().join("outside.db");
        fs::write(&outside, b"x").unwrap();
        // ...reachable through a symlink that lexically sits inside the root.
        let link = root.join("inbound.db");
        symlink(&outside, &link).unwrap();

        let err = guard_under_root(&root, &link).unwrap_err();
        assert!(matches!(err, UpgradeError::WriteOutsideRoot { .. }));
    }
}
