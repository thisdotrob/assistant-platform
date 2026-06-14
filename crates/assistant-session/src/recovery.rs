//! Outbound DB recovery exception.
//!
//! The host normally never writes to `outbound.db`. The single exception is
//! recovery: when no container is alive, the host may take an exclusive session
//! lock and open `outbound.db` read-write to migrate it, clean up stale
//! `processing_ack` claims, or write recovery metadata. Liveness is judged from
//! the `.heartbeat` file freshness and the `container_state` row; the lock then
//! guarantees no second host path races in.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension};

use crate::db::{open_read_only, open_read_write};
use crate::error::SessionError;
use crate::layout::{DbKind, SessionLayout};
use crate::migrate::lazy_migrate;
use crate::schema::outbound_migrations;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Stopped,
}

/// Default heartbeat freshness window. A heartbeat newer than this means a
/// container is presumed alive.
pub const DEFAULT_HEARTBEAT_TTL: Duration = Duration::from_secs(30);

fn heartbeat_fresh(layout: &SessionLayout, ttl: Duration) -> bool {
    let path = layout.heartbeat_path();
    let Ok(meta) = fs::metadata(&path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age <= ttl,
        // Heartbeat timestamp is in the future — treat as fresh.
        Err(_) => true,
    }
}

fn container_state_alive(layout: &SessionLayout) -> bool {
    let path = layout.outbound_db_path();
    if !path.exists() {
        return false;
    }
    let Ok(conn) = open_read_only(&path) else {
        return false;
    };
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM container_state WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    matches!(status.as_deref(), Some("alive"))
}

/// Judge container liveness from heartbeat freshness or a live `container_state`
/// row. Either signal being current means the container is presumed alive.
pub fn container_liveness(layout: &SessionLayout, ttl: Duration) -> Liveness {
    if heartbeat_fresh(layout, ttl) || container_state_alive(layout) {
        Liveness::Alive
    } else {
        Liveness::Stopped
    }
}

/// An exclusive on-disk session lock. Created with `create_new` so a second
/// acquisition fails while the first guard is alive; removed on drop.
#[derive(Debug)]
pub struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    pub fn acquire(layout: &SessionLayout) -> Result<Self, SessionError> {
        let path = layout.lock_path();
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Self { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SessionError::SessionLocked { path })
            }
            Err(source) => Err(SessionError::Io { path, source }),
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A read-write handle to `outbound.db` proving the recovery preconditions held:
/// no container was alive and the exclusive lock is held for the guard's
/// lifetime. Dropping it releases the lock.
pub struct RecoveryGuard {
    conn: Connection,
    _lock: SessionLock,
}

impl RecoveryGuard {
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Remove all `processing_ack` claims. Safe only here, because the guard
    /// proves no container is alive to own a claim.
    pub fn cleanup_stale_acks(&self) -> Result<usize, SessionError> {
        let removed = self.conn.execute("DELETE FROM processing_ack", [])?;
        Ok(removed)
    }

    /// Record recovery metadata in `session_state`.
    pub fn write_recovery_meta(&self, key: &str, value: &str) -> Result<(), SessionError> {
        self.conn.execute(
            "INSERT INTO session_state (key, value, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }
}

/// Open `outbound.db` read-write under the recovery exception. Refuses while a
/// container is alive; otherwise takes the exclusive lock, opens read-write, and
/// lazily migrates (migration is a permitted recovery write).
pub fn open_outbound_recovery(
    layout: &SessionLayout,
    ttl: Duration,
) -> Result<RecoveryGuard, SessionError> {
    if container_liveness(layout, ttl) == Liveness::Alive {
        return Err(SessionError::ContainerAlive {
            detail: "heartbeat fresh or container_state=alive".to_string(),
        });
    }
    let lock = SessionLock::acquire(layout)?;
    // Re-check after locking: a container could have started between the first
    // check and acquiring the lock.
    if container_liveness(layout, ttl) == Liveness::Alive {
        return Err(SessionError::ContainerAlive {
            detail: "container became alive after lock".to_string(),
        });
    }
    let mut conn = open_read_write(&layout.outbound_db_path())?;
    lazy_migrate(&mut conn, DbKind::Outbound, &outbound_migrations())?;
    Ok(RecoveryGuard { conn, _lock: lock })
}
