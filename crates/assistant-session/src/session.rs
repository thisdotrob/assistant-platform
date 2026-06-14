//! Host-side session API: initialize a session folder, enqueue inbound
//! messages, project destinations/routing, and read outbound messages.
//!
//! Write discipline:
//! - the host writes only to `inbound.db`, using open-write-close;
//! - host (inbound) messages take even sequence numbers, container (outbound)
//!   messages take odd ones, so the two never collide and parity is verifiable;
//! - the host opens `outbound.db` read-only here; read-write access lives behind
//!   the recovery exception in [`crate::recovery`].

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{open_read_only, open_read_write};
use crate::error::SessionError;
use crate::layout::{DbKind, SessionLayout};
use crate::migrate::{check_runner_compatibility, lazy_migrate, schema_version, SchemaCompat};
use crate::schema::{
    inbound_migrations, outbound_migrations, CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION,
};

/// A message the host enqueues for the container to process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    pub sender: String,
    pub content: String,
    #[serde(default)]
    pub metadata: Option<String>,
}

/// A message the container emitted, read back by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub seq: i64,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub metadata: Option<String>,
    pub created_at: String,
}

/// A destination row projected into the inbound DB before container wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub destination_id: String,
    pub kind: String,
    pub display_name: Option<String>,
}

/// Create the session folder and migrate both DBs to the current shipped
/// schema. This is the host's pre-start coordination point: it migrates
/// `outbound.db` before any container is alive, which is the only non-recovery
/// path on which the host writes to `outbound.db`. Sets the container state to
/// `stopped`.
pub fn init_session(layout: &SessionLayout) -> Result<(), SessionError> {
    for dir in layout.managed_dirs() {
        fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            path: dir.clone(),
            source,
        })?;
    }

    let mut inbound = open_read_write(&layout.inbound_db_path())?;
    lazy_migrate(&mut inbound, DbKind::Inbound, &inbound_migrations())?;

    let mut outbound = open_read_write(&layout.outbound_db_path())?;
    lazy_migrate(&mut outbound, DbKind::Outbound, &outbound_migrations())?;
    outbound.execute(
        "INSERT INTO container_state (id, status, run_id, updated_at)
         VALUES (1, 'stopped', NULL, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET status = 'stopped', updated_at = datetime('now')",
        [],
    )?;

    Ok(())
}

/// Open `inbound.db` read-write and lazily migrate it. Host use only.
pub fn open_inbound(layout: &SessionLayout) -> Result<Connection, SessionError> {
    let mut conn = open_read_write(&layout.inbound_db_path())?;
    lazy_migrate(&mut conn, DbKind::Inbound, &inbound_migrations())?;
    Ok(conn)
}

/// Open `outbound.db` read-only, verifying the on-disk schema is within the
/// runner's supported range. Refuses unsupported versions.
pub fn open_outbound_read(
    layout: &SessionLayout,
    compat: SchemaCompat,
) -> Result<Connection, SessionError> {
    let conn = open_read_only(&layout.outbound_db_path())?;
    let found = schema_version(&conn)?;
    check_runner_compatibility(DbKind::Outbound, found, compat)?;
    Ok(conn)
}

fn next_seq(conn: &Connection, table: &str, odd: bool) -> Result<i64, SessionError> {
    let max: Option<i64> = conn
        .query_row(&format!("SELECT MAX(seq) FROM {table}"), [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(match max {
        None => {
            if odd {
                1
            } else {
                0
            }
        }
        Some(m) => m + 2,
    })
}

/// Enqueue an inbound message using open-write-close discipline. Returns the
/// assigned (even) sequence number.
pub fn enqueue_inbound(
    layout: &SessionLayout,
    message: &InboundMessage,
) -> Result<i64, SessionError> {
    enqueue_inbound_keyed(layout, message, None)
}

/// Enqueue an inbound message, optionally idempotent on `idempotency_key`. When
/// a key is given and a row already carries it, the existing row's sequence is
/// returned and nothing is written — so a scheduler that re-runs the same
/// occurrence's turn (after a failed attempt left the lease to expire) reuses
/// the one inbound row instead of accumulating duplicates a later container
/// would each reply to. The keyless path (`None`) always inserts.
pub fn enqueue_inbound_keyed(
    layout: &SessionLayout,
    message: &InboundMessage,
    idempotency_key: Option<&str>,
) -> Result<i64, SessionError> {
    let mut conn = open_inbound(layout)?;
    if let Some(key) = idempotency_key {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT seq FROM messages_in WHERE idempotency_key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            return Ok(seq);
        }
    }
    let seq = next_seq(&conn, "messages_in", false)?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO messages_in (seq, sender, content, metadata, idempotency_key, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        rusqlite::params![
            seq,
            message.sender,
            message.content,
            message.metadata,
            idempotency_key
        ],
    )?;
    tx.commit()?;
    Ok(seq)
}

/// Read all outbound (container-written, odd-seq) messages in order.
pub fn read_outbound(
    layout: &SessionLayout,
    compat: SchemaCompat,
) -> Result<Vec<OutboundMessage>, SessionError> {
    let conn = open_outbound_read(layout, compat)?;
    let mut stmt = conn.prepare(
        "SELECT seq, kind, content, metadata, created_at FROM messages_out ORDER BY seq",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(OutboundMessage {
            seq: row.get(0)?,
            kind: row.get(1)?,
            content: row.get(2)?,
            metadata: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Mark an outbound sequence as delivered to its channel (recorded in inbound).
pub fn mark_delivered(layout: &SessionLayout, seq: i64) -> Result<(), SessionError> {
    let conn = open_inbound(layout)?;
    conn.execute(
        "INSERT OR REPLACE INTO delivered (seq, delivered_at) VALUES (?1, datetime('now'))",
        rusqlite::params![seq],
    )?;
    Ok(())
}

/// The highest outbound sequence already marked delivered for this session, or 0
/// when none. A long-lived host keeps this watermark in memory, but it resets to
/// 0 on process restart; reading it back lets a restarted host resume from the
/// persisted marker instead of re-reading (and re-delivering) a reply the prior
/// process already handed off.
pub fn max_delivered_seq(layout: &SessionLayout) -> Result<i64, SessionError> {
    let conn = open_inbound(layout)?;
    let max =
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM delivered", [], |row| row.get(0))?;
    Ok(max)
}

/// Refresh the destination projection in the inbound DB.
pub fn set_destinations(
    layout: &SessionLayout,
    destinations: &[Destination],
) -> Result<(), SessionError> {
    let mut conn = open_inbound(layout)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM destinations", [])?;
    for d in destinations {
        tx.execute(
            "INSERT INTO destinations (destination_id, kind, display_name, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![d.destination_id, d.kind, d.display_name],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Set a routing projection key in the inbound DB.
pub fn set_routing(layout: &SessionLayout, key: &str, value: &str) -> Result<(), SessionError> {
    let conn = open_inbound(layout)?;
    conn.execute(
        "INSERT INTO session_routing (key, value, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// Verify host-even / container-odd sequence parity holds across both DBs.
pub fn verify_sequence_parity(layout: &SessionLayout) -> Result<(), SessionError> {
    let inbound = open_read_only(&layout.inbound_db_path())?;
    check_parity(&inbound, "messages_in", DbKind::Inbound, false)?;

    if layout.outbound_db_path().exists() {
        let outbound = open_read_only(&layout.outbound_db_path())?;
        check_parity(&outbound, "messages_out", DbKind::Outbound, true)?;
    }
    Ok(())
}

fn check_parity(
    conn: &Connection,
    table: &str,
    kind: DbKind,
    expect_odd: bool,
) -> Result<(), SessionError> {
    let want_remainder: i64 = if expect_odd { 1 } else { 0 };
    let offending: Option<i64> = conn
        .query_row(
            &format!("SELECT seq FROM {table} WHERE (seq % 2) != ?1 LIMIT 1"),
            rusqlite::params![want_remainder],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(seq) = offending {
        return Err(SessionError::SequenceParity { db_kind: kind, seq });
    }
    Ok(())
}

/// Runner-side compatibility defaults for the current build.
pub fn current_inbound_compat() -> SchemaCompat {
    SchemaCompat::exact(CURRENT_INBOUND_VERSION)
}

pub fn current_outbound_compat() -> SchemaCompat {
    SchemaCompat::exact(CURRENT_OUTBOUND_VERSION)
}

/// True when a session folder has been initialized (inbound DB present).
pub fn session_exists(layout: &SessionLayout) -> bool {
    layout.inbound_db_path().exists()
}

/// Convenience for callers that hold a path rather than a layout.
pub fn db_file_exists(path: &Path) -> bool {
    path.exists()
}
