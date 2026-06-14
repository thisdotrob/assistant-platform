//! Projection repair: rebuild the central scheduled-work index from the
//! per-session `messages_in` source of truth.
//!
//! The session message metadata is authoritative; the central projection is a
//! derived, repairable index. If a central write was lost (crash between session
//! write and projection upsert) the two can disagree — a cancelled schedule that
//! still looks active centrally, or a phantom "fired" occurrence that would
//! suppress the next claim. Repair reads every scheduled message from a session's
//! inbound DB and makes the central projection agree:
//!
//! - the item row is re-projected to match the source's status, due time,
//!   recurrence, and revision;
//! - the source's `occurrence_seq` (the highest fired sequence) is honored: a
//!   marker keeps the central sequence counter aligned, and any occurrence the
//!   source does not consider fired is reset to pending so it can fire — without
//!   clearing a live lease, so in-flight runs are never disrupted.
//!
//! Reading the session DB is why this crate depends on claw-session; the central
//! writes reuse the [`crate::projection`] upserts.

use rusqlite::Connection;

use claw_session::{open_inbound, SessionError, SessionLayout};

use crate::model::{Occurrence, ScheduledMessageMeta};
use crate::projection::{upsert_item, ProjectionError};

#[derive(Debug)]
pub enum RepairError {
    Session(SessionError),
    Projection(ProjectionError),
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairError::Session(e) => write!(f, "repair session error: {e}"),
            RepairError::Projection(e) => write!(f, "repair projection error: {e}"),
        }
    }
}

impl std::error::Error for RepairError {}

impl From<SessionError> for RepairError {
    fn from(value: SessionError) -> Self {
        RepairError::Session(value)
    }
}

impl From<ProjectionError> for RepairError {
    fn from(value: ProjectionError) -> Self {
        RepairError::Projection(value)
    }
}

impl From<rusqlite::Error> for RepairError {
    fn from(value: rusqlite::Error) -> Self {
        RepairError::Projection(ProjectionError::Sqlite(value))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Scheduled items re-projected from session messages.
    pub items_reprojected: usize,
    /// Phantom-fired occurrences reset to pending (source did not consider them
    /// fired).
    pub occurrences_unfired: usize,
}

/// Reconcile the central projection for one session against its inbound message
/// store. Every message whose metadata parses as a [`ScheduledMessageMeta`] is
/// treated as a scheduled item; non-scheduling messages are skipped.
pub fn repair_session_projection(
    central: &Connection,
    layout: &SessionLayout,
    session_id: &str,
) -> Result<RepairReport, RepairError> {
    let session = open_inbound(layout)?;
    let metadatas: Vec<String> = {
        let mut stmt =
            session.prepare("SELECT metadata FROM messages_in WHERE metadata IS NOT NULL ORDER BY seq")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get::<_, String>(0)?);
        }
        out
    };

    let mut report = RepairReport::default();
    for metadata in metadatas {
        // Only scheduling messages parse into the full metadata shape; ordinary
        // messages carry unrelated metadata and are left untouched.
        let Ok(meta) = ScheduledMessageMeta::from_metadata_json(&metadata) else {
            continue;
        };
        report.occurrences_unfired += reconcile_item(central, &meta, session_id)?;
        report.items_reprojected += 1;
    }
    Ok(report)
}

/// Reconcile a single item's central rows against its authoritative metadata.
/// Returns the number of phantom-fired occurrences reset to pending.
pub fn reconcile_item(
    central: &Connection,
    meta: &ScheduledMessageMeta,
    session_id: &str,
) -> Result<usize, ProjectionError> {
    upsert_item(central, meta, Some(session_id))?;

    let item_id = meta.scheduled_item_id.as_str();
    let fired_through = meta.occurrence_seq as i64;

    // Keep the central sequence counter aligned with the source's highest fired
    // occurrence, even if the historical occurrence rows were lost. The
    // idempotency key is derived purely from (item, sequence); the historical
    // scheduled time is unknown, so it stays NULL — sweeps only consult it for
    // the *current* due slot, which never equals a past one.
    if fired_through > 0 {
        let key = Occurrence::new(item_id, meta.occurrence_seq, 0).idempotency_key;
        central.execute(
            "INSERT INTO scheduled_occurrences
                 (scheduled_item_id, sequence, idempotency_key, status)
             VALUES (?1, ?2, ?3, 'fired')
             ON CONFLICT(scheduled_item_id, sequence) DO UPDATE SET status = 'fired'",
            rusqlite::params![item_id, meta.occurrence_seq, key],
        )?;
    }

    // Any occurrence the source has not fired must not be 'fired' centrally, or
    // the next claim would be wrongly suppressed. Lease columns are preserved so
    // a genuinely in-flight run is not disturbed.
    let unfired = central.execute(
        "UPDATE scheduled_occurrences
             SET status = 'pending', fired_at = NULL
         WHERE scheduled_item_id = ?1 AND sequence > ?2 AND status = 'fired'",
        rusqlite::params![item_id, fired_through],
    )?;
    Ok(unfired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::claim_due;
    use crate::model::{ContextPolicy, LifecycleTransition, Recurrence, ScheduleIntent, ScheduleStatus};
    use crate::projection::{item_status, list_items, upsert_occurrence, OccurrenceStatus};
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};
    use claw_session::{enqueue_inbound, init_session, InboundMessage};
    use tempfile::TempDir;

    fn central() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut set = baseline_migrations(order);
        for m in crate::projection::migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn session(root: &TempDir, agent: &str, sess: &str) -> SessionLayout {
        let layout = SessionLayout::derive(root.path(), agent, sess).unwrap();
        init_session(&layout).unwrap();
        layout
    }

    fn meta(agent: i64, summary: &str, process_after: i64, recurring: bool) -> ScheduledMessageMeta {
        let recurrence = recurring.then_some(Recurrence::Every { seconds: 60 });
        ScheduledMessageMeta::create(
            agent,
            ScheduleIntent { created_by: "U1".into(), summary: summary.into(), created_at: 1 },
            process_after,
            recurrence,
            ContextPolicy::CurrentMemory,
        )
        .unwrap()
    }

    fn enqueue_meta(layout: &SessionLayout, m: &ScheduledMessageMeta) {
        enqueue_inbound(
            layout,
            &InboundMessage {
                sender: "host".into(),
                content: m.intent.summary.clone(),
                metadata: Some(m.to_metadata_json().unwrap()),
            },
        )
        .unwrap();
    }

    #[test]
    fn repair_rebuilds_missing_items_and_skips_non_scheduling_messages() {
        let root = TempDir::new().unwrap();
        let layout = session(&root, "ag1", "sess-1");
        let m = meta(1, "stretch", 2_000, true);
        enqueue_meta(&layout, &m);
        // A plain message with unrelated metadata must be ignored by repair.
        enqueue_inbound(
            &layout,
            &InboundMessage {
                sender: "user".into(),
                content: "hi".into(),
                metadata: Some("{\"foo\":1}".into()),
            },
        )
        .unwrap();
        // And one with no metadata at all.
        enqueue_inbound(
            &layout,
            &InboundMessage { sender: "user".into(), content: "yo".into(), metadata: None },
        )
        .unwrap();

        let central = central();
        assert!(list_items(&central, 1, None).unwrap().is_empty());

        let report = repair_session_projection(&central, &layout, "sess-1").unwrap();
        assert_eq!(report.items_reprojected, 1);

        let items = list_items(&central, 1, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, m.scheduled_item_id);
        assert_eq!(items[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(items[0].process_after, Some(2_000));
        assert_eq!(items[0].recurrence, Some(Recurrence::Every { seconds: 60 }));
    }

    #[test]
    fn repair_reconciles_a_stale_status() {
        let root = TempDir::new().unwrap();
        let layout = session(&root, "ag1", "sess-1");
        let central = central();

        // Central wrongly shows the item active; the source says it was cancelled.
        let mut active = meta(1, "x", 1_000, false);
        upsert_item(&central, &active, Some("sess-1")).unwrap();
        assert_eq!(item_status(&central, &active.scheduled_item_id).unwrap(), Some(ScheduleStatus::Active));

        active.transition(LifecycleTransition::Cancel).unwrap();
        enqueue_meta(&layout, &active);

        repair_session_projection(&central, &layout, "sess-1").unwrap();
        assert_eq!(
            item_status(&central, &active.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Cancelled)
        );
        // A cancelled item is never claimed.
        assert!(claim_due(&central, 9_999, "host", 30).unwrap().is_empty());
    }

    #[test]
    fn repair_unfires_a_phantom_occurrence_so_it_can_run() {
        let root = TempDir::new().unwrap();
        let layout = session(&root, "ag1", "sess-1");
        let central = central();

        let m = meta(1, "tick", 1_000, true);
        enqueue_meta(&layout, &m);
        upsert_item(&central, &m, Some("sess-1")).unwrap();
        // Central wrongly records occurrence 1 as fired; the source's
        // occurrence_seq is still 0 (nothing has fired).
        let occ = Occurrence::new(&m.scheduled_item_id, 1, 1_000);
        upsert_occurrence(&central, &occ, OccurrenceStatus::Fired, Some(1_000)).unwrap();

        let report = repair_session_projection(&central, &layout, "sess-1").unwrap();
        assert_eq!(report.occurrences_unfired, 1);

        // After repair the due occurrence can be claimed and runs once.
        let leases = claim_due(&central, 1_000, "host", 30).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].occurrence.sequence, 1);
    }

    #[test]
    fn repair_keeps_sequence_counter_aligned_after_a_wipe() {
        let root = TempDir::new().unwrap();
        let layout = session(&root, "ag1", "sess-1");
        let central = central();

        // Source: a recurring item that has already fired three times and
        // advanced its due time to slot 4. Central was wiped (no rows).
        let mut m = meta(1, "tick", 1_000, true);
        for _ in 0..3 {
            let occ = m.pending_occurrence().unwrap();
            m.record_fired(&occ);
        }
        assert_eq!(m.occurrence_seq, 3);
        assert_eq!(m.process_after, 1_180);
        enqueue_meta(&layout, &m);

        repair_session_projection(&central, &layout, "sess-1").unwrap();

        // The next claim must allocate sequence 4 (not restart at 1) at the
        // advanced due time, matching the source's accounting.
        let lease = claim_due(&central, 1_180, "host", 30).unwrap().pop().unwrap();
        assert_eq!(lease.occurrence.sequence, 4);
        assert_eq!(lease.occurrence.scheduled_for, 1_180);
    }
}
