//! Audit-recording over the central `dropped_messages` table.
//!
//! The router records every message it declines so operators can see what is
//! being turned away and why. This slice is audit-only: it inserts and reports
//! drops but does not route. The table's `created_at` is left to its DB default
//! so the audit timestamp is the central clock, not a caller-supplied value.

use rusqlite::Connection;

use crate::model::{DropReason, DroppedMessage, RouterError};

/// Record a dropped inbound message. Returns the new audit row id.
pub fn record_drop(
    conn: &Connection,
    channel: &str,
    sender: Option<&str>,
    reason: DropReason,
    payload: Option<&str>,
) -> Result<i64, RouterError> {
    conn.execute(
        "INSERT INTO dropped_messages (channel, sender, reason, payload) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![channel, sender, reason.as_str(), payload],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The most recent drops, newest first, capped at `limit`.
pub fn list_drops(conn: &Connection, limit: u32) -> Result<Vec<DroppedMessage>, RouterError> {
    let mut stmt = conn.prepare(
        "SELECT id, channel, sender, reason, payload, created_at \
         FROM dropped_messages ORDER BY id DESC LIMIT ?1",
    )?;
    // Read raw columns inside the closure; parse `reason` afterward so a bad
    // stored value surfaces as a RouterError rather than a rusqlite error.
    let rows = stmt.query_map([limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (id, channel, sender, reason_str, payload, created_at) = r?;
        out.push(DroppedMessage {
            id,
            channel,
            sender,
            reason: DropReason::parse(&reason_str)?,
            payload,
            created_at,
        });
    }
    Ok(out)
}

/// Total number of recorded drops.
pub fn count_drops(conn: &Connection) -> Result<i64, RouterError> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM dropped_messages", [], |r| r.get(0))?;
    Ok(n)
}

/// Number of recorded drops for a given reason.
pub fn count_drops_by_reason(conn: &Connection, reason: DropReason) -> Result<i64, RouterError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dropped_messages WHERE reason = ?1",
        [reason.as_str()],
        |r| r.get(0),
    )?;
    Ok(n)
}
