//! Central module migration registry.
//!
//! Migrations are namespaced by stable module ID and applied in
//! module-dependency order, then by version within each module. Every applied
//! migration's content checksum is recorded; a later run whose registered SQL
//! no longer matches the recorded checksum refuses to proceed.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// A single forward migration owned by one module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub module_id: String,
    pub version: u32,
    pub name: String,
    pub sql: String,
}

impl Migration {
    pub fn new(
        module_id: impl Into<String>,
        version: u32,
        name: impl Into<String>,
        sql: impl Into<String>,
    ) -> Self {
        Self {
            module_id: module_id.into(),
            version,
            name: name.into(),
            sql: sql.into(),
        }
    }

    /// Content checksum used to detect drift in already-applied migrations.
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

/// A registry of migrations plus the module ordering used to apply them.
#[derive(Debug, Clone, Default)]
pub struct MigrationSet {
    module_order: Vec<String>,
    migrations: Vec<Migration>,
}

impl MigrationSet {
    pub fn new(module_order: Vec<String>) -> Self {
        Self {
            module_order,
            migrations: Vec::new(),
        }
    }

    pub fn add(&mut self, migration: Migration) -> &mut Self {
        self.migrations.push(migration);
        self
    }

    pub fn module_order(&self) -> &[String] {
        &self.module_order
    }

    /// Validate and return migrations sorted by (module dependency order,
    /// version). Errors if any migration's module is not in the order, or if a
    /// `(module, version)` pair is duplicated.
    pub fn ordered(&self) -> Result<Vec<&Migration>, DbError> {
        let rank: BTreeMap<&str, usize> = self
            .module_order
            .iter()
            .enumerate()
            .map(|(i, m)| (m.as_str(), i))
            .collect();

        let mut seen: BTreeMap<(&str, u32), ()> = BTreeMap::new();
        for migration in &self.migrations {
            if !rank.contains_key(migration.module_id.as_str()) {
                return Err(DbError::UnknownMigrationModule {
                    module_id: migration.module_id.clone(),
                });
            }
            let key = (migration.module_id.as_str(), migration.version);
            if seen.insert(key, ()).is_some() {
                return Err(DbError::DuplicateMigration {
                    module_id: migration.module_id.clone(),
                    version: migration.version,
                });
            }
        }

        let mut ordered: Vec<&Migration> = self.migrations.iter().collect();
        ordered.sort_by_key(|m| (rank[m.module_id.as_str()], m.version));
        Ok(ordered)
    }
}

/// Product/platform identity and per-module versions recorded after migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRecord {
    pub product_id: String,
    pub product_version: String,
    pub platform_version: String,
    pub modules: Vec<(String, String)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub applied: Vec<(String, u32)>,
    pub skipped: Vec<(String, u32)>,
}

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    UnknownMigrationModule {
        module_id: String,
    },
    DuplicateMigration {
        module_id: String,
        version: u32,
    },
    ChecksumMismatch {
        module_id: String,
        version: u32,
        recorded: String,
        computed: String,
    },
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            DbError::UnknownMigrationModule { module_id } => write!(
                f,
                "migration references module {module_id:?} which is not in the module order"
            ),
            DbError::DuplicateMigration { module_id, version } => {
                write!(f, "duplicate migration {module_id}:{version}")
            }
            DbError::ChecksumMismatch {
                module_id,
                version,
                recorded,
                computed,
            } => write!(
                f,
                "checksum mismatch for {module_id}:{version}: recorded {recorded}, registered {computed}"
            ),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(value: rusqlite::Error) -> Self {
        DbError::Sqlite(value)
    }
}

/// Create the registry's own bookkeeping tables. Idempotent.
pub fn ensure_meta_tables(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             module_id  TEXT    NOT NULL,
             version    INTEGER NOT NULL,
             name       TEXT    NOT NULL,
             checksum   TEXT    NOT NULL,
             applied_at TEXT    NOT NULL,
             PRIMARY KEY (module_id, version)
         );
         CREATE TABLE IF NOT EXISTS module_versions (
             module_id        TEXT NOT NULL PRIMARY KEY,
             module_version   TEXT NOT NULL,
             product_id       TEXT NOT NULL,
             product_version  TEXT NOT NULL,
             platform_version TEXT NOT NULL,
             recorded_at      TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// Apply all pending migrations in dependency/version order. Already-applied
/// migrations are checksum-verified and skipped; a mismatch aborts the run.
pub fn apply(conn: &mut Connection, set: &MigrationSet) -> Result<MigrationReport, DbError> {
    ensure_meta_tables(conn)?;
    let ordered = set.ordered()?;

    let mut report = MigrationReport::default();
    for migration in ordered {
        let computed = migration.checksum();
        let recorded: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE module_id = ?1 AND version = ?2",
                rusqlite::params![migration.module_id, migration.version],
                |row| row.get(0),
            )
            .optional()?;

        match recorded {
            Some(recorded) if recorded != computed => {
                return Err(DbError::ChecksumMismatch {
                    module_id: migration.module_id.clone(),
                    version: migration.version,
                    recorded,
                    computed,
                });
            }
            Some(_) => {
                report
                    .skipped
                    .push((migration.module_id.clone(), migration.version));
            }
            None => {
                let tx = conn.transaction()?;
                tx.execute_batch(&migration.sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations (module_id, version, name, checksum, applied_at)
                     VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                    rusqlite::params![
                        migration.module_id,
                        migration.version,
                        migration.name,
                        computed
                    ],
                )?;
                tx.commit()?;
                report
                    .applied
                    .push((migration.module_id.clone(), migration.version));
            }
        }
    }

    Ok(report)
}

/// Read back the recorded product/platform identity and per-module versions, or
/// `None` if nothing has been recorded yet (a never-bootstrapped DB). Identity
/// is taken from the most recently recorded row.
pub fn read_version_record(conn: &Connection) -> Result<Option<VersionRecord>, DbError> {
    ensure_meta_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT module_id, module_version, product_id, product_version, platform_version
         FROM module_versions ORDER BY recorded_at DESC, module_id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let Some((_, _, product_id, product_version, platform_version)) = rows.first().cloned() else {
        return Ok(None);
    };
    let mut modules: Vec<(String, String)> = rows
        .into_iter()
        .map(|(module_id, module_version, _, _, _)| (module_id, module_version))
        .collect();
    modules.sort();
    Ok(Some(VersionRecord {
        product_id,
        product_version,
        platform_version,
        modules,
    }))
}

/// The highest applied migration version per module, sorted by module id. A
/// module with no applied migrations does not appear.
pub fn applied_versions(conn: &Connection) -> Result<Vec<(String, u32)>, DbError> {
    ensure_meta_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT module_id, MAX(version) FROM schema_migrations GROUP BY module_id ORDER BY module_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record product/platform identity and per-module versions. Upserts so reruns
/// keep the latest recorded versions.
pub fn record_versions(conn: &Connection, record: &VersionRecord) -> Result<(), DbError> {
    ensure_meta_tables(conn)?;
    for (module_id, module_version) in &record.modules {
        conn.execute(
            "INSERT INTO module_versions
                 (module_id, module_version, product_id, product_version, platform_version, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(module_id) DO UPDATE SET
                 module_version = excluded.module_version,
                 product_id = excluded.product_id,
                 product_version = excluded.product_version,
                 platform_version = excluded.platform_version,
                 recorded_at = excluded.recorded_at",
            rusqlite::params![
                module_id,
                module_version,
                record.product_id,
                record.product_version,
                record.platform_version
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    fn set_with(order: &[&str], migrations: Vec<Migration>) -> MigrationSet {
        let mut set = MigrationSet::new(order.iter().map(|s| s.to_string()).collect());
        for m in migrations {
            set.add(m);
        }
        set
    }

    #[test]
    fn applies_in_module_then_version_order() {
        let set = set_with(
            &["claw-a", "claw-b"],
            vec![
                Migration::new("claw-b", 1, "b1", "CREATE TABLE b (id INTEGER);"),
                Migration::new("claw-a", 2, "a2", "CREATE TABLE a2 (id INTEGER);"),
                Migration::new("claw-a", 1, "a1", "CREATE TABLE a1 (id INTEGER);"),
            ],
        );
        let ordered: Vec<_> = set
            .ordered()
            .unwrap()
            .iter()
            .map(|m| (m.module_id.clone(), m.version))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("claw-a".to_string(), 1),
                ("claw-a".to_string(), 2),
                ("claw-b".to_string(), 1),
            ]
        );
    }

    #[test]
    fn unknown_module_is_rejected() {
        let set = set_with(
            &["claw-a"],
            vec![Migration::new("claw-z", 1, "z", "SELECT 1;")],
        );
        assert!(matches!(
            set.ordered(),
            Err(DbError::UnknownMigrationModule { .. })
        ));
    }

    #[test]
    fn duplicate_version_is_rejected() {
        let set = set_with(
            &["claw-a"],
            vec![
                Migration::new("claw-a", 1, "x", "SELECT 1;"),
                Migration::new("claw-a", 1, "y", "SELECT 2;"),
            ],
        );
        assert!(matches!(
            set.ordered(),
            Err(DbError::DuplicateMigration { .. })
        ));
    }

    #[test]
    fn forward_migration_on_empty_db_creates_tables() {
        let mut conn = mem();
        let set = set_with(
            &["claw-a"],
            vec![Migration::new(
                "claw-a",
                1,
                "create_thing",
                "CREATE TABLE thing (id INTEGER PRIMARY KEY);",
            )],
        );
        let report = apply(&mut conn, &set).unwrap();
        assert_eq!(report.applied, vec![("claw-a".to_string(), 1)]);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='thing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn rerun_is_idempotent_and_skips_applied() {
        let mut conn = mem();
        let set = set_with(
            &["claw-a"],
            vec![Migration::new(
                "claw-a",
                1,
                "create_thing",
                "CREATE TABLE thing (id INTEGER PRIMARY KEY);",
            )],
        );
        apply(&mut conn, &set).unwrap();
        let second = apply(&mut conn, &set).unwrap();
        assert!(second.applied.is_empty());
        assert_eq!(second.skipped, vec![("claw-a".to_string(), 1)]);
    }

    #[test]
    fn checksum_mismatch_refuses() {
        let mut conn = mem();
        let original = set_with(
            &["claw-a"],
            vec![Migration::new("claw-a", 1, "v1", "CREATE TABLE thing (id INTEGER);")],
        );
        apply(&mut conn, &original).unwrap();

        // Same (module, version), different SQL => drift.
        let drifted = set_with(
            &["claw-a"],
            vec![Migration::new(
                "claw-a",
                1,
                "v1",
                "CREATE TABLE thing (id INTEGER, extra TEXT);",
            )],
        );
        assert!(matches!(
            apply(&mut conn, &drifted),
            Err(DbError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn records_versions() {
        let conn = mem();
        let record = VersionRecord {
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            modules: vec![
                ("claw-core".to_string(), "0.1.0".to_string()),
                ("claw-db".to_string(), "0.1.0".to_string()),
            ],
        };
        record_versions(&conn, &record).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM module_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        // Upsert keeps a single row per module.
        record_versions(&conn, &record).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM module_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn read_version_record_round_trips_or_is_none() {
        let conn = mem();
        assert!(read_version_record(&conn).unwrap().is_none());

        let record = VersionRecord {
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            modules: vec![
                ("claw-db".to_string(), "0.1.0".to_string()),
                ("claw-core".to_string(), "0.1.0".to_string()),
            ],
        };
        record_versions(&conn, &record).unwrap();

        let read = read_version_record(&conn).unwrap().unwrap();
        assert_eq!(read.product_id, "assistant");
        assert_eq!(read.platform_version, "0.1.0");
        // Modules come back sorted by id.
        assert_eq!(
            read.modules,
            vec![
                ("claw-core".to_string(), "0.1.0".to_string()),
                ("claw-db".to_string(), "0.1.0".to_string()),
            ]
        );
    }

    #[test]
    fn applied_versions_reports_max_per_module() {
        let mut conn = mem();
        let set = set_with(
            &["claw-a", "claw-b"],
            vec![
                Migration::new("claw-a", 1, "a1", "CREATE TABLE a1 (id INTEGER);"),
                Migration::new("claw-a", 2, "a2", "CREATE TABLE a2 (id INTEGER);"),
                Migration::new("claw-b", 1, "b1", "CREATE TABLE b1 (id INTEGER);"),
            ],
        );
        apply(&mut conn, &set).unwrap();
        assert_eq!(
            applied_versions(&conn).unwrap(),
            vec![("claw-a".to_string(), 2), ("claw-b".to_string(), 1)]
        );
    }
}
