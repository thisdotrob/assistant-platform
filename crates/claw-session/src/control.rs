//! Local control test harness.
//!
//! [`FakeContainer`] stands in for the real runner: it owns `outbound.db`,
//! writes odd-sequence messages, maintains the heartbeat and `container_state`,
//! and records `processing_ack` claims. The host side drives `claw-session`'s
//! public API against it, exercising the session DB loop without Docker or
//! Claude.

use std::fs;

use rusqlite::OptionalExtension;

use crate::db::open_read_write;
use crate::error::SessionError;
use crate::layout::{DbKind, SessionLayout};
use crate::migrate::{check_runner_compatibility, schema_version, SchemaCompat};
use crate::session::{self, current_outbound_compat};

/// A host-side handle that initializes a session and hands out a fake container.
pub struct LocalControl {
    layout: SessionLayout,
}

impl LocalControl {
    pub fn new(layout: SessionLayout) -> Self {
        Self { layout }
    }

    pub fn init(&self) -> Result<(), SessionError> {
        session::init_session(&self.layout)
    }

    pub fn layout(&self) -> &SessionLayout {
        &self.layout
    }

    pub fn fake_container(&self) -> FakeContainer {
        FakeContainer {
            layout: self.layout.clone(),
            compat: current_outbound_compat(),
        }
    }
}

/// Simulated container process that owns `outbound.db`.
pub struct FakeContainer {
    layout: SessionLayout,
    compat: SchemaCompat,
}

impl FakeContainer {
    /// Mark the container alive and lay down a fresh heartbeat.
    pub fn start(&self, run_id: &str) -> Result<(), SessionError> {
        self.set_status("alive", Some(run_id))?;
        self.heartbeat()
    }

    /// Touch the heartbeat file to the current time.
    pub fn heartbeat(&self) -> Result<(), SessionError> {
        let path = self.layout.heartbeat_path();
        fs::write(&path, b"alive").map_err(|source| SessionError::Io { path, source })
    }

    /// Emit an outbound message with the next odd sequence number. The runner
    /// refuses unsupported schema versions before writing.
    pub fn emit(&self, kind: &str, content: &str) -> Result<i64, SessionError> {
        let mut conn = open_read_write(&self.layout.outbound_db_path())?;
        let found = schema_version(&conn)?;
        check_runner_compatibility(DbKind::Outbound, found, self.compat)?;

        let max: Option<i64> = conn
            .query_row("SELECT MAX(seq) FROM messages_out", [], |r| r.get(0))
            .optional()?
            .flatten();
        let seq = match max {
            None => 1,
            Some(m) => m + 2,
        };
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO messages_out (seq, kind, content, metadata, created_at)
             VALUES (?1, ?2, ?3, NULL, datetime('now'))",
            rusqlite::params![seq, kind, content],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// Record a `processing_ack` claim for an inbound sequence.
    pub fn claim(&self, in_seq: i64, claimed_by: &str) -> Result<(), SessionError> {
        let conn = open_read_write(&self.layout.outbound_db_path())?;
        conn.execute(
            "INSERT OR REPLACE INTO processing_ack (in_seq, claimed_by, claimed_at)
             VALUES (?1, ?2, datetime('now'))",
            rusqlite::params![in_seq, claimed_by],
        )?;
        Ok(())
    }

    /// Read inbound messages the way the real container would: read-only.
    pub fn read_inbound(&self) -> Result<Vec<(i64, String)>, SessionError> {
        let conn = crate::db::open_read_only(&self.layout.inbound_db_path())?;
        let mut stmt = conn.prepare("SELECT seq, content FROM messages_in ORDER BY seq")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark the container stopped and remove its heartbeat.
    pub fn stop(&self) -> Result<(), SessionError> {
        self.set_status("stopped", None)?;
        let path = self.layout.heartbeat_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SessionError::Io { path, source }),
        }
    }

    fn set_status(&self, status: &str, run_id: Option<&str>) -> Result<(), SessionError> {
        let conn = open_read_write(&self.layout.outbound_db_path())?;
        conn.execute(
            "INSERT INTO container_state (id, status, run_id, updated_at)
             VALUES (1, ?1, ?2, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status,
                 run_id = excluded.run_id,
                 updated_at = excluded.updated_at",
            rusqlite::params![status, run_id],
        )?;
        Ok(())
    }
}
