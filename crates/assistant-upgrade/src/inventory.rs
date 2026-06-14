//! Instance schema inventory.
//!
//! Reports the recorded schema state of a long-lived instance without mutating
//! it: the central DB's recorded product/platform identity and per-module
//! migration high-water marks, plus each per-session DB's schema version. Owns
//! no tables of its own — it reads assistant-db's meta tables through assistant-db's
//! public API and each session DB's `schema_meta` through assistant-session's.

use std::fs::{self, ReadDir};
use std::path::Path;

use assistant_config::InstanceLayout;
use assistant_db::VersionRecord;
use assistant_session::{db as session_db, schema_version, DbKind, SessionLayout};

use crate::error::UpgradeError;

/// The recorded schema state of one session's inbound/outbound DBs. A `None`
/// version means that DB file is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSchema {
    pub agent_group_id: String,
    pub session_id: String,
    pub inbound_version: Option<u32>,
    pub outbound_version: Option<u32>,
}

/// A read-only snapshot of an instance's recorded schema state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInventory {
    /// Whether the central DB file exists on disk.
    pub central_present: bool,
    /// The identity/versions last recorded in the central DB, if any.
    pub recorded: Option<VersionRecord>,
    /// Highest applied central migration version per module, sorted by module id.
    pub applied_central_versions: Vec<(String, u32)>,
    /// Per-session schema versions, sorted by (agent group id, session id).
    pub sessions: Vec<SessionSchema>,
}

/// A session directory found under the sessions root, with its validated layout.
pub(crate) struct DiscoveredSession {
    pub agent_group_id: String,
    pub session_id: String,
    pub layout: SessionLayout,
}

/// Take a read-only inventory of the instance at `layout`. Never creates the
/// central DB or session DBs: absent files are reported as absent rather than
/// materialized.
pub fn inventory(layout: &InstanceLayout) -> Result<InstanceInventory, UpgradeError> {
    let central_path = layout.central_db_path();
    let (central_present, recorded, applied_central_versions) = if central_path.exists() {
        let conn = assistant_db::open_central(&central_path)?;
        let recorded = assistant_db::read_version_record(&conn)?;
        let applied = assistant_db::applied_versions(&conn)?;
        (true, recorded, applied)
    } else {
        (false, None, Vec::new())
    };

    let mut sessions = Vec::new();
    for discovered in discover_sessions(&layout.sessions_dir())? {
        sessions.push(SessionSchema {
            inbound_version: read_session_version(&discovered.layout, DbKind::Inbound)?,
            outbound_version: read_session_version(&discovered.layout, DbKind::Outbound)?,
            agent_group_id: discovered.agent_group_id,
            session_id: discovered.session_id,
        });
    }

    Ok(InstanceInventory {
        central_present,
        recorded,
        applied_central_versions,
        sessions,
    })
}

/// Walk `<sessions_root>/<agent_group_id>/<session_id>` two levels deep and
/// return each session's validated layout, sorted by (group, session). Entries
/// that are not directories, are not valid UTF-8, or carry identifiers that
/// would fail the session id charset are skipped — discovery addresses the
/// sessions it can safely reach, it does not create or repair them.
pub(crate) fn discover_sessions(
    sessions_dir: &Path,
) -> Result<Vec<DiscoveredSession>, UpgradeError> {
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for group_entry in read_dir(sessions_dir)? {
        let group_entry = group_entry.map_err(|source| UpgradeError::Io {
            path: sessions_dir.to_path_buf(),
            source,
        })?;
        let group_path = group_entry.path();
        if !is_dir(&group_path)? {
            continue;
        }
        let Some(agent_group_id) = group_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };

        for session_entry in read_dir(&group_path)? {
            let session_entry = session_entry.map_err(|source| UpgradeError::Io {
                path: group_path.clone(),
                source,
            })?;
            if !is_dir(&session_entry.path())? {
                continue;
            }
            let Some(session_id) = session_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };

            // Re-deriving through SessionLayout both rebuilds the db paths and
            // rejects any identifier that could escape the sessions root.
            let Ok(layout) = SessionLayout::derive(sessions_dir, &agent_group_id, &session_id)
            else {
                continue;
            };

            out.push(DiscoveredSession {
                agent_group_id: agent_group_id.clone(),
                session_id,
                layout,
            });
        }
    }

    out.sort_by(|a, b| {
        a.agent_group_id
            .cmp(&b.agent_group_id)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(out)
}

/// Read one session DB's recorded schema version read-only, or `None` if the DB
/// file is absent.
fn read_session_version(
    layout: &SessionLayout,
    kind: DbKind,
) -> Result<Option<u32>, UpgradeError> {
    let db_path = layout.db_path(kind);
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = session_db::open_read_only(&db_path)?;
    Ok(Some(schema_version(&conn)?))
}

fn read_dir(path: &Path) -> Result<ReadDir, UpgradeError> {
    fs::read_dir(path).map_err(|source| UpgradeError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn is_dir(path: &Path) -> Result<bool, UpgradeError> {
    let meta = fs::metadata(path).map_err(|source| UpgradeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(meta.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, record_versions, Migration, MigrationSet, VersionRecord};
    use assistant_session::{lazy_migrate, migrations_for, migrations_v1_only};
    use tempfile::TempDir;

    fn instance_layout(tmp: &TempDir) -> InstanceLayout {
        InstanceLayout::derive(tmp.path(), "testns", None).unwrap()
    }

    #[test]
    fn empty_instance_reports_nothing() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);

        let inv = inventory(&layout).unwrap();
        assert!(!inv.central_present);
        assert!(inv.recorded.is_none());
        assert!(inv.applied_central_versions.is_empty());
        assert!(inv.sessions.is_empty());
    }

    #[test]
    fn central_versions_are_read_back() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);
        fs::create_dir_all(&layout.root).unwrap();

        let mut conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
        let mut set = MigrationSet::new(vec!["assistant-db".to_string()]);
        set.add(Migration::new(
            "assistant-db",
            1,
            "base",
            "CREATE TABLE t (id INTEGER);",
        ));
        apply(&mut conn, &set).unwrap();
        record_versions(
            &conn,
            &VersionRecord {
                product_id: "testprod".to_string(),
                product_version: "0.1.0".to_string(),
                platform_version: "0.1.0".to_string(),
                modules: vec![("assistant-db".to_string(), "0.1.0".to_string())],
            },
        )
        .unwrap();
        drop(conn);

        let inv = inventory(&layout).unwrap();
        assert!(inv.central_present);
        let recorded = inv.recorded.expect("recorded version present");
        assert_eq!(recorded.product_id, "testprod");
        assert_eq!(recorded.platform_version, "0.1.0");
        assert_eq!(
            inv.applied_central_versions,
            vec![("assistant-db".to_string(), 1)]
        );
    }

    #[test]
    fn per_session_versions_are_reported() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);
        let sessions_root = layout.sessions_dir();

        // Session A: both DBs migrated to the current schema (inbound v3, outbound v2).
        let a = SessionLayout::derive(&sessions_root, "groupone", "sessone").unwrap();
        fs::create_dir_all(a.dir()).unwrap();
        let mut a_in = session_db::open_read_write(&a.inbound_db_path()).unwrap();
        lazy_migrate(&mut a_in, DbKind::Inbound, &migrations_for(DbKind::Inbound)).unwrap();
        let mut a_out = session_db::open_read_write(&a.outbound_db_path()).unwrap();
        lazy_migrate(&mut a_out, DbKind::Outbound, &migrations_for(DbKind::Outbound)).unwrap();
        drop(a_in);
        drop(a_out);

        // Session B: inbound at the old v1 schema, outbound DB never created.
        let b = SessionLayout::derive(&sessions_root, "grouptwo", "sesstwo").unwrap();
        fs::create_dir_all(b.dir()).unwrap();
        let mut b_in = session_db::open_read_write(&b.inbound_db_path()).unwrap();
        lazy_migrate(&mut b_in, DbKind::Inbound, &migrations_v1_only(DbKind::Inbound)).unwrap();
        drop(b_in);

        let inv = inventory(&layout).unwrap();
        assert_eq!(inv.sessions.len(), 2);

        let sa = inv
            .sessions
            .iter()
            .find(|s| s.session_id == "sessone")
            .unwrap();
        assert_eq!(sa.inbound_version, Some(3));
        assert_eq!(sa.outbound_version, Some(2));

        let sb = inv
            .sessions
            .iter()
            .find(|s| s.session_id == "sesstwo")
            .unwrap();
        assert_eq!(sb.inbound_version, Some(1));
        assert_eq!(sb.outbound_version, None);
    }

    #[test]
    fn inventory_does_not_create_absent_dbs() {
        let tmp = TempDir::new().unwrap();
        let layout = instance_layout(&tmp);

        inventory(&layout).unwrap();

        // A read-only inventory must not materialize the central DB.
        assert!(!layout.central_db_path().exists());
    }
}
