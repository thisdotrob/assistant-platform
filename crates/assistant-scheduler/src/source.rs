//! The per-session source of truth for scheduled work.
//!
//! A schedule's authoritative state is its [`ScheduledMessageMeta`], stored in
//! the session's `inbound.db`. To keep that bounded — a recurring item can fire
//! forever — each item occupies exactly **one** row, keyed on the item id and
//! rewritten in place as the schedule changes (created, paused/resumed/cancelled,
//! fired). The central projection ([`crate::projection`]) is derived from these
//! rows and rebuilt from them by [`crate::repair`]; the runner shim skips them
//! by their reserved sender so they never drive a turn.

use rusqlite::OptionalExtension;

use assistant_session::{
    open_inbound, upsert_inbound_meta, InboundMessage, SessionError, SessionLayout,
};

use crate::model::{ScheduleError, ScheduledMessageMeta};

/// The reserved `messages_in.sender` of a scheduling source-of-truth row. The
/// runner shim recognizes this sender and skips the row (no turn, no reply).
/// Mirrored as a string literal in `shim/src/index.js`.
pub const SCHEDULE_META_SENDER: &str = "schedule-meta";

/// Retry a source op through transient SQLite lock/IO blips before surfacing.
///
/// These writes happen on the host's reply path while a live container's shim
/// polls the same session DB over a bind mount. `busy_timeout` covers
/// `SQLITE_BUSY`/`LOCKED` but not `SQLITE_IOERR_LOCK` (a failed POSIX advisory
/// lock), which can surface under heavy concurrent access. A bounded backoff
/// re-opens the connection each attempt; non-transient errors (e.g. a metadata
/// parse failure) fall through immediately. Mirrors `retry_transient` on the
/// host side (crates/assistant-host/src/run.rs).
fn with_retry<T>(mut op: impl FnMut() -> Result<T, SourceError>) -> Result<T, SourceError> {
    const RETRIES: u32 = 4;
    for attempt in 0..RETRIES {
        match op() {
            Err(SourceError::Session(SessionError::Sqlite(_))) => {
                std::thread::sleep(std::time::Duration::from_millis(10 * u64::from(attempt + 1)));
            }
            other => return other,
        }
    }
    op()
}

#[derive(Debug)]
pub enum SourceError {
    Session(SessionError),
    Schedule(ScheduleError),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Session(e) => write!(f, "schedule source session error: {e}"),
            SourceError::Schedule(e) => write!(f, "schedule source metadata error: {e}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<SessionError> for SourceError {
    fn from(value: SessionError) -> Self {
        SourceError::Session(value)
    }
}

impl From<ScheduleError> for SourceError {
    fn from(value: ScheduleError) -> Self {
        SourceError::Schedule(value)
    }
}

/// The stable idempotency key of an item's source-of-truth row, so every write
/// for the same item rewrites the one row rather than appending a new one.
pub fn meta_idempotency_key(scheduled_item_id: &str) -> String {
    format!("sched-meta:{scheduled_item_id}")
}

/// Write the item's current metadata as its per-session source of truth,
/// inserting the row on first write and rewriting it in place thereafter.
pub fn write_meta(layout: &SessionLayout, meta: &ScheduledMessageMeta) -> Result<(), SourceError> {
    let metadata = meta.to_metadata_json()?;
    let key = meta_idempotency_key(&meta.scheduled_item_id);
    with_retry(|| {
        let message = InboundMessage {
            sender: SCHEDULE_META_SENDER.to_string(),
            content: meta.intent.summary.clone(),
            metadata: Some(metadata.clone()),
            thread_id: None,
        };
        upsert_inbound_meta(layout, &key, &message)?;
        Ok(())
    })
}

/// Read an item's current source-of-truth metadata, or `None` when the session
/// has no row for it (e.g. an item created before this path existed).
pub fn latest_meta(
    layout: &SessionLayout,
    scheduled_item_id: &str,
) -> Result<Option<ScheduledMessageMeta>, SourceError> {
    let key = meta_idempotency_key(scheduled_item_id);
    let json: Option<String> = with_retry(|| {
        let conn = open_inbound(layout)?;
        conn.query_row(
            "SELECT metadata FROM messages_in WHERE idempotency_key = ?1",
            [&key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| SourceError::Session(SessionError::from(e)))
    })?;
    match json {
        Some(json) => Ok(Some(ScheduledMessageMeta::from_metadata_json(&json)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextPolicy, LifecycleTransition, Recurrence, ScheduleIntent, ScheduleStatus};
    use assistant_session::init_session;
    use tempfile::TempDir;

    fn session(root: &TempDir) -> SessionLayout {
        let layout = SessionLayout::derive(root.path(), "ag1", "sess-1").unwrap();
        init_session(&layout).unwrap();
        layout
    }

    fn meta(summary: &str, recurring: bool) -> ScheduledMessageMeta {
        let recurrence = recurring.then_some(Recurrence::Every { seconds: 60 });
        ScheduledMessageMeta::create(
            1,
            ScheduleIntent { created_by: "U1".into(), summary: summary.into(), created_at: 1 },
            1_000,
            recurrence,
            ContextPolicy::CurrentMemory,
        )
        .unwrap()
    }

    #[test]
    fn write_then_read_round_trips_the_meta() {
        let root = TempDir::new().unwrap();
        let layout = session(&root);
        let m = meta("stretch", true);

        assert!(latest_meta(&layout, &m.scheduled_item_id).unwrap().is_none());
        write_meta(&layout, &m).unwrap();
        let back = latest_meta(&layout, &m.scheduled_item_id).unwrap().unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn rewriting_an_item_keeps_one_row_with_the_latest_status() {
        let root = TempDir::new().unwrap();
        let layout = session(&root);
        let mut m = meta("stretch", true);
        write_meta(&layout, &m).unwrap();

        m.transition(LifecycleTransition::Pause).unwrap();
        write_meta(&layout, &m).unwrap();

        // The read reflects the latest write; the row was rewritten, not appended.
        let back = latest_meta(&layout, &m.scheduled_item_id).unwrap().unwrap();
        assert_eq!(back.status, ScheduleStatus::Paused);

        let conn = open_inbound(&layout).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn distinct_items_get_distinct_rows() {
        let root = TempDir::new().unwrap();
        let layout = session(&root);
        let a = meta("stretch", false);
        let b = meta("walk", false);
        assert_ne!(a.scheduled_item_id, b.scheduled_item_id);
        write_meta(&layout, &a).unwrap();
        write_meta(&layout, &b).unwrap();

        let conn = open_inbound(&layout).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }
}
