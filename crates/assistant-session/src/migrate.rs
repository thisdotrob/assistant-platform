//! Lazy per-session migration runner.
//!
//! Each session DB carries its own `schema_meta` (current schema version +
//! protocol version) and `schema_migrations` (one row per applied migration
//! with a content checksum). Migrations are a linear, per-DB-kind sequence
//! applied lazily whenever a session is opened. Already-applied migrations are
//! checksum-verified; drift aborts the open.

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::SessionError;
use crate::layout::DbKind;

/// Protocol version the host and runner speak over the session DBs. Bumped only
/// when the message exchange contract itself changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// One forward migration for a single session DB kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigration {
    pub db_kind: DbKind,
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

impl SessionMigration {
    pub fn checksum(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.sql.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }
}

/// Inclusive range of session DB schema versions a runner can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaCompat {
    pub min: u32,
    pub max: u32,
}

impl SchemaCompat {
    pub fn exact(version: u32) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    pub fn supports(&self, version: u32) -> bool {
        self.min <= version && version <= self.max
    }
}

/// Create the bookkeeping tables. Idempotent; preserves container read-only
/// access because it only ever adds tables, never drops them.
pub fn ensure_meta(conn: &Connection, kind: DbKind) -> Result<(), SessionError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta (
             id               INTEGER PRIMARY KEY CHECK (id = 1),
             db_kind          TEXT    NOT NULL,
             schema_version   INTEGER NOT NULL,
             protocol_version INTEGER NOT NULL,
             updated_at       TEXT    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             name       TEXT    NOT NULL,
             checksum   TEXT    NOT NULL,
             applied_at TEXT    NOT NULL
         );",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_meta (id, db_kind, schema_version, protocol_version, updated_at)
         VALUES (1, ?1, 0, ?2, datetime('now'))",
        rusqlite::params![kind.as_str(), PROTOCOL_VERSION],
    )?;
    Ok(())
}

/// The current schema version recorded in `schema_meta` (0 before any migration).
pub fn schema_version(conn: &Connection) -> Result<u32, SessionError> {
    let version: Option<i64> = conn
        .query_row("SELECT schema_version FROM schema_meta WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(version.unwrap_or(0) as u32)
}

/// Refuse to operate against a schema version the runner cannot read.
pub fn check_runner_compatibility(
    kind: DbKind,
    found: u32,
    compat: SchemaCompat,
) -> Result<(), SessionError> {
    if compat.supports(found) {
        Ok(())
    } else {
        Err(SessionError::UnsupportedSchemaVersion {
            db_kind: kind,
            found,
            supported_min: compat.min,
            supported_max: compat.max,
        })
    }
}

/// Apply pending migrations in version order. Already-applied migrations are
/// checksum-verified and skipped; the recorded schema version is advanced to the
/// highest applied version. Returns the versions newly applied.
pub fn lazy_migrate(
    conn: &mut Connection,
    kind: DbKind,
    migrations: &[SessionMigration],
) -> Result<Vec<u32>, SessionError> {
    ensure_meta(conn, kind)?;

    // The declared sequence must be contiguous and strictly increasing from 1.
    let mut expected = 1u32;
    for m in migrations {
        if m.db_kind != kind || m.version != expected {
            return Err(SessionError::BadMigrationSequence {
                db_kind: kind,
                version: m.version,
            });
        }
        expected += 1;
    }

    let mut applied = Vec::new();
    for m in migrations {
        let computed = m.checksum();
        let recorded: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                rusqlite::params![m.version],
                |row| row.get(0),
            )
            .optional()?;

        match recorded {
            Some(recorded) if recorded != computed => {
                return Err(SessionError::ChecksumMismatch {
                    db_kind: kind,
                    version: m.version,
                    recorded,
                    computed,
                });
            }
            Some(_) => {}
            None => {
                let tx = conn.transaction()?;
                tx.execute_batch(m.sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, name, checksum, applied_at)
                     VALUES (?1, ?2, ?3, datetime('now'))",
                    rusqlite::params![m.version, m.name, computed],
                )?;
                tx.commit()?;
                applied.push(m.version);
            }
        }
    }

    let target = migrations.last().map(|m| m.version).unwrap_or(0);
    conn.execute(
        "UPDATE schema_meta SET schema_version = ?1, updated_at = datetime('now') WHERE id = 1",
        rusqlite::params![target],
    )?;

    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mig(kind: DbKind, version: u32, name: &'static str, sql: &'static str) -> SessionMigration {
        SessionMigration {
            db_kind: kind,
            version,
            name,
            sql,
        }
    }

    #[test]
    fn applies_then_skips_on_rerun() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migs = [mig(
            DbKind::Inbound,
            1,
            "base",
            "CREATE TABLE t (id INTEGER PRIMARY KEY);",
        )];
        let first = lazy_migrate(&mut conn, DbKind::Inbound, &migs).unwrap();
        assert_eq!(first, vec![1]);
        assert_eq!(schema_version(&conn).unwrap(), 1);
        let second = lazy_migrate(&mut conn, DbKind::Inbound, &migs).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn non_contiguous_sequence_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migs = [
            mig(DbKind::Inbound, 1, "a", "CREATE TABLE a (id INTEGER);"),
            mig(DbKind::Inbound, 3, "c", "CREATE TABLE c (id INTEGER);"),
        ];
        assert!(matches!(
            lazy_migrate(&mut conn, DbKind::Inbound, &migs),
            Err(SessionError::BadMigrationSequence { .. })
        ));
    }

    #[test]
    fn checksum_drift_aborts() {
        let mut conn = Connection::open_in_memory().unwrap();
        let v1 = [mig(DbKind::Inbound, 1, "base", "CREATE TABLE t (id INTEGER);")];
        lazy_migrate(&mut conn, DbKind::Inbound, &v1).unwrap();
        let drifted = [mig(
            DbKind::Inbound,
            1,
            "base",
            "CREATE TABLE t (id INTEGER, x TEXT);",
        )];
        assert!(matches!(
            lazy_migrate(&mut conn, DbKind::Inbound, &drifted),
            Err(SessionError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn compat_range_enforced() {
        let compat = SchemaCompat { min: 1, max: 2 };
        assert!(check_runner_compatibility(DbKind::Outbound, 2, compat).is_ok());
        assert!(matches!(
            check_runner_compatibility(DbKind::Outbound, 3, compat),
            Err(SessionError::UnsupportedSchemaVersion { .. })
        ));
    }
}
