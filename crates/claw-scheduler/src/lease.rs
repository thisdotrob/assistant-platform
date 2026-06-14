//! Due-work sweep and execution leases.
//!
//! A host sweep periodically asks "what scheduled work is due now?" and claims
//! each due occurrence with a short-lived lease before running it. The lease is
//! what makes a due occurrence fire *exactly once* even when sweeps overlap or a
//! runner crashes:
//!
//! - occurrence identity is `(scheduled_item_id, sequence)`, a primary key, so a
//!   duplicate sweep recomputing the same occurrence cannot insert a second row;
//! - a claim only succeeds if the occurrence is still pending and either
//!   unleased or its lease has expired, so a held, live lease blocks every other
//!   claimant;
//! - an expired lease is reclaimable (stale-lease recovery), and each claim bumps
//!   the occurrence's attempt counter so retries are observable;
//! - a scheduled time that has already fired is never re-claimed, even if the
//!   item's `process_after` has not yet advanced in the projection.
//!
//! All times are epoch seconds; the caller supplies `now`, so sweeps are
//! deterministic and the crate never reads the wall clock.

use rusqlite::{Connection, OptionalExtension};

use crate::model::{EpochSecs, Occurrence};
use crate::projection::ProjectionError;

/// A claimed occurrence: the right to run it until `expires_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub occurrence: Occurrence,
    pub owner: String,
    pub expires_at: EpochSecs,
    /// The occurrence's attempt counter after this claim (1 on first claim, 2+
    /// after stale-lease recovery).
    pub attempt: i64,
}

/// Claim every due occurrence the caller can, returning one [`Lease`] per claimed
/// occurrence. An item is due when it is active and its `process_after` is at or
/// before `now`. Occurrences already held under a live lease, already fired for
/// the current scheduled time, or belonging to non-active items are skipped.
pub fn claim_due(
    conn: &Connection,
    now: EpochSecs,
    owner: &str,
    lease_ttl_secs: i64,
) -> Result<Vec<Lease>, ProjectionError> {
    let due: Vec<(String, EpochSecs)> = {
        let mut stmt = conn.prepare(
            "SELECT id, CAST(process_after AS INTEGER) FROM scheduled_items \
             WHERE status = 'active' AND process_after IS NOT NULL \
               AND CAST(process_after AS INTEGER) <= ?1 \
             ORDER BY CAST(process_after AS INTEGER) ASC, id ASC",
        )?;
        let mut rows = stmt.query(rusqlite::params![now])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, String>(0)?, row.get::<_, EpochSecs>(1)?));
        }
        out
    };

    let mut leases = Vec::new();
    for (item_id, process_after) in due {
        if let Some(occurrence) = next_claimable_occurrence(conn, &item_id, process_after)?
            && let Some(lease) = acquire_lease(conn, &occurrence, owner, now, lease_ttl_secs)?
        {
            leases.push(lease);
        }
    }
    Ok(leases)
}

/// The occurrence a due item should run next, or `None` if there is nothing to
/// claim. Reuses an existing pending occurrence (so a stale lease can be
/// recovered rather than duplicated); never re-runs a scheduled time that has
/// already fired; otherwise allocates the next sequence for the current
/// `process_after`.
pub fn next_claimable_occurrence(
    conn: &Connection,
    scheduled_item_id: &str,
    process_after: EpochSecs,
) -> Result<Option<Occurrence>, ProjectionError> {
    // A still-pending occurrence is the one to (re)claim.
    let pending: Option<(u64, Option<EpochSecs>)> = conn
        .query_row(
            "SELECT sequence, scheduled_for FROM scheduled_occurrences \
             WHERE scheduled_item_id = ?1 AND status = 'pending' \
             ORDER BY sequence DESC LIMIT 1",
            rusqlite::params![scheduled_item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((sequence, scheduled_for)) = pending {
        let scheduled_for = scheduled_for.unwrap_or(process_after);
        return Ok(Some(Occurrence::new(scheduled_item_id, sequence, scheduled_for)));
    }

    // The current scheduled time may already have fired (e.g. the projection's
    // process_after has not yet advanced after a recurring run). Do not re-run it.
    let already_fired: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM scheduled_occurrences \
             WHERE scheduled_item_id = ?1 AND status = 'fired' AND scheduled_for = ?2 LIMIT 1",
            rusqlite::params![scheduled_item_id, process_after],
            |row| row.get(0),
        )
        .optional()?;
    if already_fired.is_some() {
        return Ok(None);
    }

    let max_sequence: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM scheduled_occurrences WHERE scheduled_item_id = ?1",
        rusqlite::params![scheduled_item_id],
        |row| row.get(0),
    )?;
    let next_sequence = (max_sequence as u64) + 1;
    Ok(Some(Occurrence::new(scheduled_item_id, next_sequence, process_after)))
}

/// Try to acquire a lease on `occurrence` for `owner` until `now + lease_ttl`.
/// Returns the [`Lease`] on success, or `None` if the occurrence is already fired
/// or held under a live lease. Idempotent: the occurrence row is created if
/// absent, never duplicated.
pub fn acquire_lease(
    conn: &Connection,
    occurrence: &Occurrence,
    owner: &str,
    now: EpochSecs,
    lease_ttl_secs: i64,
) -> Result<Option<Lease>, ProjectionError> {
    // Ensure the occurrence row exists without disturbing an existing one.
    conn.execute(
        "INSERT INTO scheduled_occurrences
             (scheduled_item_id, sequence, idempotency_key, scheduled_for, status, attempt)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0)
         ON CONFLICT(scheduled_item_id, sequence) DO NOTHING",
        rusqlite::params![
            occurrence.scheduled_item_id,
            occurrence.sequence,
            occurrence.idempotency_key,
            occurrence.scheduled_for,
        ],
    )?;

    let expires_at = now + lease_ttl_secs;
    let claimed = conn.execute(
        "UPDATE scheduled_occurrences
             SET lease_owner = ?1, lease_expires_at = ?2, attempt = attempt + 1
         WHERE scheduled_item_id = ?3 AND sequence = ?4 AND status = 'pending'
           AND (lease_owner IS NULL OR lease_expires_at <= ?5)",
        rusqlite::params![owner, expires_at, occurrence.scheduled_item_id, occurrence.sequence, now],
    )?;
    if claimed == 0 {
        return Ok(None);
    }

    let attempt: i64 = conn.query_row(
        "SELECT attempt FROM scheduled_occurrences \
         WHERE scheduled_item_id = ?1 AND sequence = ?2",
        rusqlite::params![occurrence.scheduled_item_id, occurrence.sequence],
        |row| row.get(0),
    )?;
    Ok(Some(Lease {
        occurrence: occurrence.clone(),
        owner: owner.to_string(),
        expires_at,
        attempt,
    }))
}

/// Mark an occurrence fired and release its lease. After this, the occurrence is
/// never claimed again. `fired_at` is stored in the baseline TEXT column as a
/// decimal epoch string, matching `process_after`.
pub fn complete_occurrence(
    conn: &Connection,
    occurrence: &Occurrence,
    fired_at: EpochSecs,
) -> Result<(), ProjectionError> {
    conn.execute(
        "UPDATE scheduled_occurrences
             SET status = 'fired', fired_at = ?1, lease_owner = NULL, lease_expires_at = NULL
         WHERE scheduled_item_id = ?2 AND sequence = ?3",
        rusqlite::params![fired_at.to_string(), occurrence.scheduled_item_id, occurrence.sequence],
    )?;
    Ok(())
}

/// Link a container run to the occurrence that triggered it, recording the
/// occurrence's idempotency key in the run registry's `scheduled_message_id`
/// column — the exactly-once execution token that ties one run to one firing.
///
/// `container_runs` is owned by claw-runtime-docker; the scheduler only writes
/// the scheduler-owned linkage column on the shared central DB, so no dependency
/// on that crate is taken. Returns true if a matching run row existed.
pub fn link_run_to_occurrence(
    conn: &Connection,
    run_id: &str,
    occurrence: &Occurrence,
) -> Result<bool, ProjectionError> {
    let updated = conn.execute(
        "UPDATE container_runs SET scheduled_message_id = ?1 WHERE id = ?2",
        rusqlite::params![occurrence.idempotency_key, run_id],
    )?;
    Ok(updated > 0)
}

/// The run id that executed the occurrence with this idempotency key, if any.
/// Lets a sweep cross-check whether a firing already produced a run.
pub fn run_for_occurrence(
    conn: &Connection,
    idempotency_key: &str,
) -> Result<Option<String>, ProjectionError> {
    let run_id = conn
        .query_row(
            "SELECT id FROM container_runs WHERE scheduled_message_id = ?1 LIMIT 1",
            rusqlite::params![idempotency_key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextPolicy, Recurrence, ScheduleIntent, ScheduledMessageMeta};
    use crate::projection::upsert_item;
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
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

    fn project(conn: &Connection, agent: i64, summary: &str, process_after: EpochSecs, recurring: bool) -> ScheduledMessageMeta {
        let recurrence = recurring.then_some(Recurrence::Every { seconds: 60 });
        let meta = ScheduledMessageMeta::create(
            agent,
            ScheduleIntent { created_by: "U1".into(), summary: summary.into(), created_at: 1 },
            process_after,
            recurrence,
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        upsert_item(conn, &meta, Some("sess")).unwrap();
        meta
    }

    #[test]
    fn claims_due_active_item_once() {
        let conn = db();
        let meta = project(&conn, 1, "ping", 1_000, false);

        let leases = claim_due(&conn, 1_000, "host-a", 30).unwrap();
        assert_eq!(leases.len(), 1);
        let lease = &leases[0];
        assert_eq!(lease.occurrence.scheduled_item_id, meta.scheduled_item_id);
        assert_eq!(lease.occurrence.sequence, 1);
        assert_eq!(lease.occurrence.scheduled_for, 1_000);
        assert_eq!(lease.expires_at, 1_030);
        assert_eq!(lease.attempt, 1);
        // The derived idempotency key matches the model's derivation.
        assert_eq!(
            lease.occurrence.idempotency_key,
            Occurrence::new(&meta.scheduled_item_id, 1, 1_000).idempotency_key
        );
    }

    #[test]
    fn not_yet_due_items_are_not_claimed() {
        let conn = db();
        project(&conn, 1, "later", 5_000, false);
        let leases = claim_due(&conn, 4_999, "host-a", 30).unwrap();
        assert!(leases.is_empty());
    }

    #[test]
    fn live_lease_blocks_a_second_claim() {
        let conn = db();
        project(&conn, 1, "ping", 1_000, false);

        let first = claim_due(&conn, 1_000, "host-a", 30).unwrap();
        assert_eq!(first.len(), 1);
        // A second sweep before the lease expires gets nothing — no double-run.
        let second = claim_due(&conn, 1_005, "host-b", 30).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn expired_lease_is_recoverable_with_incremented_attempt() {
        let conn = db();
        let meta = project(&conn, 1, "ping", 1_000, false);

        let first = claim_due(&conn, 1_000, "host-a", 30).unwrap();
        assert_eq!(first[0].attempt, 1);
        assert_eq!(first[0].expires_at, 1_030);

        // After expiry a different host can recover the stale lease.
        let recovered = claim_due(&conn, 1_031, "host-b", 30).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].occurrence.sequence, 1, "same occurrence, not a new one");
        assert_eq!(recovered[0].owner, "host-b");
        assert_eq!(recovered[0].attempt, 2);
        assert_eq!(recovered[0].occurrence.scheduled_item_id, meta.scheduled_item_id);
    }

    #[test]
    fn completed_occurrence_is_never_reclaimed() {
        let conn = db();
        project(&conn, 1, "ping", 1_000, false);

        let lease = claim_due(&conn, 1_000, "host-a", 30).unwrap().pop().unwrap();
        complete_occurrence(&conn, &lease.occurrence, 1_002).unwrap();

        // Even though the item row is still active with process_after=1000, the
        // fired scheduled time must not produce another claim.
        let again = claim_due(&conn, 2_000, "host-a", 30).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn paused_item_is_not_claimed() {
        let conn = db();
        let mut meta = ScheduledMessageMeta::create(
            1,
            ScheduleIntent { created_by: "U1".into(), summary: "p".into(), created_at: 1 },
            1_000,
            None,
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        meta.transition(crate::model::LifecycleTransition::Pause).unwrap();
        upsert_item(&conn, &meta, None).unwrap();

        let leases = claim_due(&conn, 2_000, "host-a", 30).unwrap();
        assert!(leases.is_empty());
    }

    #[test]
    fn run_links_to_occurrence_by_idempotency_key() {
        let conn = db();
        project(&conn, 1, "ping", 1_000, false);
        let lease = claim_due(&conn, 1_000, "host-a", 30).unwrap().pop().unwrap();

        conn.execute(
            "INSERT INTO container_runs (id, session_id, agent_group_id, status) \
             VALUES ('run-1', 'sess', 1, 'created')",
            [],
        )
        .unwrap();

        assert!(link_run_to_occurrence(&conn, "run-1", &lease.occurrence).unwrap());
        assert_eq!(
            run_for_occurrence(&conn, &lease.occurrence.idempotency_key).unwrap(),
            Some("run-1".to_string())
        );
        // Linking a non-existent run reports no row touched.
        assert!(!link_run_to_occurrence(&conn, "missing", &lease.occurrence).unwrap());
        // An unrelated key has no run.
        assert_eq!(run_for_occurrence(&conn, "deadbeef").unwrap(), None);
    }

    #[test]
    fn recurring_item_claims_next_sequence_after_advance() {
        let conn = db();
        let mut meta = project(&conn, 1, "tick", 1_000, true);

        // First occurrence: claim, fire, complete.
        let lease1 = claim_due(&conn, 1_000, "host-a", 30).unwrap().pop().unwrap();
        assert_eq!(lease1.occurrence.sequence, 1);
        meta.record_fired(&lease1.occurrence);
        complete_occurrence(&conn, &lease1.occurrence, 1_001).unwrap();
        // Source of truth advanced process_after to 1060; reproject.
        assert_eq!(meta.process_after, 1_060);
        upsert_item(&conn, &meta, Some("sess")).unwrap();

        // Before the next scheduled time, nothing is due.
        assert!(claim_due(&conn, 1_059, "host-a", 30).unwrap().is_empty());

        // At/after the advanced time the next occurrence is claimed.
        let lease2 = claim_due(&conn, 1_060, "host-a", 30).unwrap().pop().unwrap();
        assert_eq!(lease2.occurrence.sequence, 2);
        assert_eq!(lease2.occurrence.scheduled_for, 1_060);
    }
}
