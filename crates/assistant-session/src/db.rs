//! Session SQLite connection setup.
//!
//! Session DBs use `journal_mode=DELETE` (not WAL) so the host and container see
//! a consistent file across the container mount boundary.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::SessionError;

fn apply_pragmas(conn: &Connection) -> Result<(), SessionError> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = DELETE;",
    )?;
    Ok(())
}

/// Open a session DB read-write, creating it if absent. Used by the host for
/// `inbound.db` and, only under the recovery exception, for `outbound.db`.
pub fn open_read_write(path: &Path) -> Result<Connection, SessionError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

/// Open a session DB read-only. Used for the container's `inbound.db` view and
/// the host's normal `outbound.db` view.
pub fn open_read_only(path: &Path) -> Result<Connection, SessionError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // A read-only connection cannot change journal mode, but foreign-key and
    // busy-timeout pragmas are still honoured.
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}
