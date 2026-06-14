//! Central SQLite connection setup.

use std::path::Path;

use rusqlite::Connection;

use crate::migrate::DbError;

/// Open (creating if absent) the file-backed central database with the standard
/// pragmas: WAL journaling for concurrent readers, foreign keys on, and a busy
/// timeout so brief lock contention waits instead of failing.
pub fn open_central(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;",
    )?;
    Ok(conn)
}

/// Open an in-memory central database (tests). WAL is meaningless for
/// `:memory:`, so only the non-journal pragmas are applied.
pub fn open_in_memory() -> Result<Connection, DbError> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}
