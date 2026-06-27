//! The central scheduled-work projection: a lightweight, derived index of the
//! scheduling state that lives in session message metadata.
//!
//! The per-session `messages_in.metadata` JSON ([`ScheduledMessageMeta`]) is the
//! source of truth; this projection exists so the host can list, filter, and run
//! a due-work sweep across all agents without opening every session DB. Every row
//! here is rebuildable from the owning session message, so the projection is
//! repairable (M7.4) and all writes are idempotent upserts.
//!
//! assistant-scheduler already owns the baseline `scheduled_items` and
//! `scheduled_occurrences` tables at version 1, so this crate's own migration
//! starts at version 2. v2 adds execution-lease columns to the occurrence table
//! (so a due sweep can claim an occurrence exactly once) plus the listing/sweep
//! indexes. Times are epoch seconds; `process_after` is stored in the baseline's
//! TEXT column and compared with `CAST(... AS INTEGER)`.

use assistant_db::Migration;
use rusqlite::{Connection, OptionalExtension};

use crate::model::{EpochSecs, Occurrence, Recurrence, ScheduleStatus, ScheduledMessageMeta};

const SCHEDULER_PROJECTION_V2: &str = "
ALTER TABLE scheduled_occurrences ADD COLUMN scheduled_for INTEGER;
ALTER TABLE scheduled_occurrences ADD COLUMN lease_owner TEXT;
ALTER TABLE scheduled_occurrences ADD COLUMN lease_expires_at INTEGER;
ALTER TABLE scheduled_occurrences ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_scheduled_items_agent_status ON scheduled_items (agent_group_id, status);
CREATE INDEX idx_scheduled_items_due ON scheduled_items (status, process_after);
";

/// assistant-scheduler's central-DB migrations beyond the baseline `scheduled_items` /
/// `scheduled_occurrences` (v1). The lease columns and listing indexes are v2.
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        crate::MODULE_ID,
        2,
        "scheduled_projection_leases",
        SCHEDULER_PROJECTION_V2,
    )]
}

#[derive(Debug)]
pub enum ProjectionError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    /// A stored TEXT value did not parse back into its enum — a corrupt row that
    /// must not be coerced into a silent default.
    UnknownEnum { column: &'static str, value: String },
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionError::Sqlite(e) => write!(f, "scheduler projection sqlite error: {e}"),
            ProjectionError::Json(e) => write!(f, "scheduler projection json error: {e}"),
            ProjectionError::UnknownEnum { column, value } => {
                write!(f, "scheduler projection column {column} has unparseable value {value:?}")
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

impl From<rusqlite::Error> for ProjectionError {
    fn from(value: rusqlite::Error) -> Self {
        ProjectionError::Sqlite(value)
    }
}

impl From<serde_json::Error> for ProjectionError {
    fn from(value: serde_json::Error) -> Self {
        ProjectionError::Json(value)
    }
}

/// An occurrence's lifecycle in the projection. The lease (owner/expiry/attempt)
/// is tracked separately in dedicated columns; this status only distinguishes a
/// not-yet-run occurrence from a fired one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccurrenceStatus {
    Pending,
    Fired,
}

impl OccurrenceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OccurrenceStatus::Pending => "pending",
            OccurrenceStatus::Fired => "fired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(OccurrenceStatus::Pending),
            "fired" => Some(OccurrenceStatus::Fired),
            _ => None,
        }
    }
}

/// One projected scheduled item, mirroring the `scheduled_items` row. This is a
/// derived view; the authoritative state is the session message metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedItem {
    pub id: String,
    pub agent_group_id: i64,
    pub session_id: Option<String>,
    pub intent: String,
    pub process_after: Option<EpochSecs>,
    pub recurrence: Option<Recurrence>,
    pub status: ScheduleStatus,
    pub revision: u32,
}

/// Project a scheduled message's metadata into the central `scheduled_items`
/// index. Idempotent on the stable `scheduled_item_id`, so re-projecting the
/// same (or a repaired) message overwrites in place rather than duplicating.
pub fn upsert_item(
    conn: &Connection,
    meta: &ScheduledMessageMeta,
    session_id: Option<&str>,
) -> Result<(), ProjectionError> {
    let recurrence_json = match &meta.recurrence {
        Some(rec) => Some(serde_json::to_string(rec)?),
        None => None,
    };
    conn.execute(
        "INSERT INTO scheduled_items
             (id, agent_group_id, session_id, intent, process_after, recurrence, status, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             agent_group_id = excluded.agent_group_id,
             session_id     = excluded.session_id,
             intent         = excluded.intent,
             process_after  = excluded.process_after,
             recurrence     = excluded.recurrence,
             status         = excluded.status,
             revision       = excluded.revision",
        rusqlite::params![
            meta.scheduled_item_id,
            meta.agent_group_id,
            session_id,
            meta.intent.summary,
            meta.process_after.to_string(),
            recurrence_json,
            meta.status.as_str(),
            meta.revision,
        ],
    )?;
    Ok(())
}

/// Record an occurrence in the projection at the given status. Idempotent on the
/// occurrence identity `(scheduled_item_id, sequence)`; a duplicate sweep that
/// recomputes the same occurrence overwrites the same row rather than inserting a
/// second. The idempotency key and scheduled time are preserved.
pub fn upsert_occurrence(
    conn: &Connection,
    occurrence: &Occurrence,
    status: OccurrenceStatus,
    fired_at: Option<EpochSecs>,
) -> Result<(), ProjectionError> {
    // `fired_at` is the baseline TEXT column; store epoch seconds as a decimal
    // string, matching how `process_after` is stored.
    let fired_at_text = fired_at.map(|t| t.to_string());
    conn.execute(
        "INSERT INTO scheduled_occurrences
             (scheduled_item_id, sequence, idempotency_key, scheduled_for, fired_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(scheduled_item_id, sequence) DO UPDATE SET
             idempotency_key = excluded.idempotency_key,
             scheduled_for   = excluded.scheduled_for,
             fired_at        = excluded.fired_at,
             status          = excluded.status",
        rusqlite::params![
            occurrence.scheduled_item_id,
            occurrence.sequence,
            occurrence.idempotency_key,
            occurrence.scheduled_for,
            fired_at_text,
            status.as_str(),
        ],
    )?;
    Ok(())
}

/// Advance a recurring item's next due time after an occurrence fires. The new
/// `process_after` is the recurrence's next scheduled time, which the caller
/// computes from the fired occurrence's *scheduled* time (not wall-clock
/// completion), so the cadence never drifts on a late run. Only this column
/// changes — the item stays active and keeps its recurrence — so the next sweep
/// claims the following occurrence at the advanced time. A one-off never calls
/// this (it completes instead). A no-op if the item is absent. `process_after`
/// is stored as a decimal epoch string, matching [`upsert_item`].
pub fn advance_recurrence(
    conn: &Connection,
    scheduled_item_id: &str,
    next_process_after: EpochSecs,
) -> Result<(), ProjectionError> {
    conn.execute(
        "UPDATE scheduled_items SET process_after = ?1 WHERE id = ?2",
        rusqlite::params![next_process_after.to_string(), scheduled_item_id],
    )?;
    Ok(())
}

/// Mark a one-off item completed once its single occurrence has fired. Unlike a
/// recurring item (which stays active and advances), a fired one-off has no
/// further work, so moving it out of `active` drops it from [`claim_due`]'s
/// swept set; without this it would linger active and every sweep would
/// re-examine it (a cheap no-op via the already-fired guard). Scoped to an
/// `active` row so a paused/cancelled item is never force-completed, matching
/// the `Active -> Completed` lifecycle rule. A no-op if the item is absent or
/// not active.
pub fn complete_item(conn: &Connection, scheduled_item_id: &str) -> Result<(), ProjectionError> {
    conn.execute(
        "UPDATE scheduled_items SET status = 'completed' WHERE id = ?1 AND status = 'active'",
        rusqlite::params![scheduled_item_id],
    )?;
    Ok(())
}

/// Cancel a schedule by id — a terminal transition that drops it from
/// [`claim_due`]'s swept set so it never fires again. Scoped to an `active` or
/// `paused` row so a `completed` or already-`cancelled` item is a no-op (cancel
/// is terminal and idempotent), matching the `{Active,Paused} -> Cancelled`
/// lifecycle rule. A no-op if the item is absent.
pub fn cancel_item(conn: &Connection, scheduled_item_id: &str) -> Result<(), ProjectionError> {
    conn.execute(
        "UPDATE scheduled_items SET status = 'cancelled' \
         WHERE id = ?1 AND status IN ('active', 'paused')",
        rusqlite::params![scheduled_item_id],
    )?;
    Ok(())
}

const ITEM_COLUMNS: &str =
    "id, agent_group_id, session_id, intent, process_after, recurrence, status, revision";

/// List an agent's projected scheduled items, optionally narrowed to one status,
/// ordered by due time then id. Scoped to a single `agent_group_id`, never
/// crossing agents.
pub fn list_items(
    conn: &Connection,
    agent_group_id: i64,
    status: Option<ScheduleStatus>,
) -> Result<Vec<ProjectedItem>, ProjectionError> {
    let mut out = Vec::new();
    match status {
        Some(status) => {
            let sql = format!(
                "SELECT {ITEM_COLUMNS} FROM scheduled_items \
                 WHERE agent_group_id = ?1 AND status = ?2 \
                 ORDER BY CAST(process_after AS INTEGER) ASC, id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![agent_group_id, status.as_str()])?;
            while let Some(row) = rows.next()? {
                out.push(row_to_item(row)?);
            }
        }
        None => {
            let sql = format!(
                "SELECT {ITEM_COLUMNS} FROM scheduled_items \
                 WHERE agent_group_id = ?1 \
                 ORDER BY CAST(process_after AS INTEGER) ASC, id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![agent_group_id])?;
            while let Some(row) = rows.next()? {
                out.push(row_to_item(row)?);
            }
        }
    }
    Ok(out)
}

/// Read a single item's status by id, or `None` if it is not in the projection.
pub fn item_status(
    conn: &Connection,
    scheduled_item_id: &str,
) -> Result<Option<ScheduleStatus>, ProjectionError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT status FROM scheduled_items WHERE id = ?1",
            rusqlite::params![scheduled_item_id],
            |row| row.get(0),
        )
        .optional()?;
    match raw {
        None => Ok(None),
        Some(s) => ScheduleStatus::parse(&s)
            .map(Some)
            .ok_or(ProjectionError::UnknownEnum { column: "status", value: s }),
    }
}

fn row_to_item(row: &rusqlite::Row) -> Result<ProjectedItem, ProjectionError> {
    let process_after_raw: Option<String> = row.get(4)?;
    let recurrence_raw: Option<String> = row.get(5)?;
    let status_raw: String = row.get(6)?;

    let process_after = match process_after_raw {
        Some(s) => Some(
            s.parse::<EpochSecs>()
                .map_err(|_| ProjectionError::UnknownEnum { column: "process_after", value: s })?,
        ),
        None => None,
    };
    let recurrence = match recurrence_raw {
        Some(s) => Some(serde_json::from_str::<Recurrence>(&s)?),
        None => None,
    };
    let status = ScheduleStatus::parse(&status_raw)
        .ok_or(ProjectionError::UnknownEnum { column: "status", value: status_raw })?;

    Ok(ProjectedItem {
        id: row.get(0)?,
        agent_group_id: row.get(1)?,
        session_id: row.get(2)?,
        intent: row.get(3)?,
        process_after,
        recurrence,
        status,
        revision: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextPolicy, ScheduleIntent};
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
        // The projection layers v2 on top of the baseline, so the baseline that
        // creates scheduled_items / scheduled_occurrences must run first.
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut set = baseline_migrations(order);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn intent(summary: &str) -> ScheduleIntent {
        ScheduleIntent {
            created_by: "U1".into(),
            summary: summary.into(),
            created_at: 1_000,
        }
    }

    fn meta(agent: i64, summary: &str, process_after: EpochSecs, recurring: bool) -> ScheduledMessageMeta {
        let recurrence = recurring.then_some(Recurrence::Every { seconds: 3_600 });
        ScheduledMessageMeta::create(
            agent,
            intent(summary),
            process_after,
            recurrence,
            ContextPolicy::CurrentMemory,
        )
        .unwrap()
    }

    #[test]
    fn v2_migration_starts_at_version_two() {
        let only = migrations();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].module_id, crate::MODULE_ID);
        assert_eq!(only[0].version, 2);
    }

    #[test]
    fn v2_adds_lease_columns_and_indexes() {
        let conn = db();
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('scheduled_occurrences')")
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        for expected in ["scheduled_for", "lease_owner", "lease_expires_at", "attempt"] {
            assert!(cols.contains(&expected.to_string()), "missing column {expected}");
        }
        for idx in ["idx_scheduled_items_agent_status", "idx_scheduled_items_due"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    rusqlite::params![idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing index {idx}");
        }
    }

    #[test]
    fn upsert_item_round_trips_and_is_idempotent() {
        let conn = db();
        let m = meta(7, "stretch", 2_000, true);
        upsert_item(&conn, &m, Some("sess-1")).unwrap();

        let rows = list_items(&conn, 7, None).unwrap();
        assert_eq!(rows.len(), 1);
        let got = &rows[0];
        assert_eq!(got.id, m.scheduled_item_id);
        assert_eq!(got.agent_group_id, 7);
        assert_eq!(got.session_id.as_deref(), Some("sess-1"));
        assert_eq!(got.intent, "stretch");
        assert_eq!(got.process_after, Some(2_000));
        assert_eq!(got.recurrence, Some(Recurrence::Every { seconds: 3_600 }));
        assert_eq!(got.status, ScheduleStatus::Active);
        assert_eq!(got.revision, 1);

        // Re-projecting the same item with a changed status/revision updates in
        // place rather than inserting a duplicate.
        let mut m2 = m.clone();
        m2.transition(crate::model::LifecycleTransition::Pause).unwrap();
        upsert_item(&conn, &m2, Some("sess-1")).unwrap();
        let rows = list_items(&conn, 7, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ScheduleStatus::Paused);
    }

    #[test]
    fn list_filters_by_status_and_is_agent_scoped() {
        let conn = db();
        upsert_item(&conn, &meta(1, "a", 3_000, false), None).unwrap();
        let mut paused = meta(1, "b", 1_000, false);
        paused.transition(crate::model::LifecycleTransition::Pause).unwrap();
        upsert_item(&conn, &paused, None).unwrap();
        // A different agent's item must never appear in agent 1's listing.
        upsert_item(&conn, &meta(2, "other", 500, false), None).unwrap();

        let active = list_items(&conn, 1, Some(ScheduleStatus::Active)).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].intent, "a");

        let all = list_items(&conn, 1, None).unwrap();
        // Ordered by due time ascending: paused "b" (1000) before active "a" (3000).
        assert_eq!(all.iter().map(|i| i.intent.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);

        let agent2 = list_items(&conn, 2, None).unwrap();
        assert_eq!(agent2.len(), 1);
        assert_eq!(agent2[0].intent, "other");
    }

    #[test]
    fn item_status_reads_or_none() {
        let conn = db();
        let m = meta(3, "x", 1_000, false);
        upsert_item(&conn, &m, None).unwrap();
        assert_eq!(
            item_status(&conn, &m.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Active)
        );
        assert_eq!(item_status(&conn, "sched_missing").unwrap(), None);
    }

    #[test]
    fn advance_recurrence_moves_only_process_after() {
        let conn = db();
        let m = meta(4, "tick", 1_000, true);
        upsert_item(&conn, &m, Some("sess-r")).unwrap();

        advance_recurrence(&conn, &m.scheduled_item_id, 1_060).unwrap();

        let rows = list_items(&conn, 4, None).unwrap();
        assert_eq!(rows.len(), 1);
        let got = &rows[0];
        // The due time advanced; everything else is untouched.
        assert_eq!(got.process_after, Some(1_060));
        assert_eq!(got.session_id.as_deref(), Some("sess-r"));
        assert_eq!(got.intent, "tick");
        assert_eq!(got.recurrence, Some(Recurrence::Every { seconds: 3_600 }));
        assert_eq!(got.status, ScheduleStatus::Active);
        assert_eq!(got.revision, 1);

        // Advancing an unknown item touches nothing.
        advance_recurrence(&conn, "sched_missing", 9_999).unwrap();
        assert_eq!(list_items(&conn, 4, None).unwrap().len(), 1);
    }

    #[test]
    fn complete_item_marks_an_active_one_off_completed_only() {
        let conn = db();
        let m = meta(8, "once", 1_000, false);
        upsert_item(&conn, &m, Some("sess-1")).unwrap();

        complete_item(&conn, &m.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &m.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Completed)
        );
        // A completed item drops out of the active listing the sweep walks.
        assert!(list_items(&conn, 8, Some(ScheduleStatus::Active))
            .unwrap()
            .is_empty());

        // Idempotent on a non-active row: completing again changes nothing.
        complete_item(&conn, &m.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &m.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Completed)
        );

        // The active-only guard protects a paused item from being force-completed.
        let mut paused = meta(8, "held", 2_000, false);
        paused
            .transition(crate::model::LifecycleTransition::Pause)
            .unwrap();
        upsert_item(&conn, &paused, None).unwrap();
        complete_item(&conn, &paused.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &paused.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Paused)
        );

        // An unknown id is a no-op.
        complete_item(&conn, "sched_missing").unwrap();
        assert_eq!(item_status(&conn, "sched_missing").unwrap(), None);
    }

    #[test]
    fn cancel_item_marks_active_or_paused_cancelled_only() {
        let conn = db();

        // An active item cancels and drops out of the swept active listing.
        let active = meta(9, "active", 1_000, true);
        upsert_item(&conn, &active, Some("sess-1")).unwrap();
        cancel_item(&conn, &active.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &active.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Cancelled)
        );
        assert!(list_items(&conn, 9, Some(ScheduleStatus::Active))
            .unwrap()
            .is_empty());

        // A paused item is cancellable too (the terminal verb wins).
        let mut paused = meta(9, "held", 2_000, false);
        paused
            .transition(crate::model::LifecycleTransition::Pause)
            .unwrap();
        upsert_item(&conn, &paused, None).unwrap();
        cancel_item(&conn, &paused.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &paused.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Cancelled)
        );

        // Cancel is idempotent on an already-cancelled row.
        cancel_item(&conn, &active.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &active.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Cancelled)
        );

        // A completed item is never resurrected into cancelled.
        let done = meta(9, "done", 3_000, false);
        upsert_item(&conn, &done, None).unwrap();
        complete_item(&conn, &done.scheduled_item_id).unwrap();
        cancel_item(&conn, &done.scheduled_item_id).unwrap();
        assert_eq!(
            item_status(&conn, &done.scheduled_item_id).unwrap(),
            Some(ScheduleStatus::Completed)
        );

        // An unknown id is a no-op.
        cancel_item(&conn, "sched_missing").unwrap();
        assert_eq!(item_status(&conn, "sched_missing").unwrap(), None);
    }

    #[test]
    fn occurrence_upsert_is_idempotent_on_identity() {
        let conn = db();
        let m = meta(1, "x", 1_000, true);
        upsert_item(&conn, &m, None).unwrap();
        let occ = m.pending_occurrence().unwrap();

        upsert_occurrence(&conn, &occ, OccurrenceStatus::Pending, None).unwrap();
        // A duplicate sweep recomputes the same (item, sequence) and overwrites.
        upsert_occurrence(&conn, &occ, OccurrenceStatus::Fired, Some(1_005)).unwrap();

        let (count, status, fired_at, key): (i64, String, Option<String>, String) = conn
            .query_row(
                "SELECT count(*), max(status), max(fired_at), max(idempotency_key)
                 FROM scheduled_occurrences
                 WHERE scheduled_item_id = ?1 AND sequence = ?2",
                rusqlite::params![occ.scheduled_item_id, occ.sequence],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "duplicate sweep must not double-insert");
        assert_eq!(status, "fired");
        assert_eq!(fired_at.as_deref(), Some("1005"));
        assert_eq!(key, occ.idempotency_key);
    }
}
