//! Scheduler readiness checks.
//!
//! Per the contract: the sweep loop is running, the central projection is
//! consistent with session message state, and no due work is stuck past its
//! lease expiry. The sweep-loop and projection-consistency checks depend on
//! host-side runtime state, so they take an injected probe (the host supplies
//! liveness / a consistency comparison); the stuck-work check is a pure query
//! against the central DB and runs anywhere.
//!
//! `CheckStatus` mirrors the readiness status shape used by other platform
//! crates. The crates duplicate the small enum rather than couple to each other,
//! honoring the module dependency boundary; the host unifies them when
//! aggregating overall readiness.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::model::EpochSecs;
use crate::projection::ProjectionError;

/// The outcome of one readiness check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    /// Not evaluated here (e.g. requires the running host); the caller must run
    /// it in the target environment.
    Skipped { detail: String },
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass)
    }

    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckStatus::Fail { .. })
    }
}

/// Check that the host's due-work sweep loop is alive, via an injected liveness
/// probe (e.g. last-tick-within-threshold).
pub fn sweep_loop_running(probe: impl FnOnce() -> bool) -> CheckStatus {
    if probe() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: "scheduler sweep loop is not running".to_string(),
        }
    }
}

/// Check that the central projection agrees with the session message source of
/// truth, via an injected comparison probe (the host reconciles per session; a
/// `true` result means no drift was found).
pub fn projections_consistent(probe: impl FnOnce() -> bool) -> CheckStatus {
    if probe() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: "central projection drifted from session message state; run repair".to_string(),
        }
    }
}

/// Check that no due occurrence is stuck past its lease expiry — a pending
/// occurrence whose lease expired at or before `now` should have been reclaimed
/// by the sweep. Any such rows indicate the sweep is not keeping up.
pub fn no_work_stuck_past_lease(
    conn: &Connection,
    now: EpochSecs,
) -> Result<CheckStatus, ProjectionError> {
    let stuck: i64 = conn.query_row(
        "SELECT COUNT(*) FROM scheduled_occurrences \
         WHERE status = 'pending' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?1",
        rusqlite::params![now],
        |row| row.get(0),
    )?;
    if stuck == 0 {
        Ok(CheckStatus::Pass)
    } else {
        Ok(CheckStatus::Fail {
            detail: format!("{stuck} due occurrence(s) stuck past lease expiry"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{acquire_lease, claim_due};
    use crate::model::{ContextPolicy, Occurrence, ScheduleIntent, ScheduledMessageMeta};
    use crate::projection::upsert_item;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};

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

    #[test]
    fn probe_checks_pass_and_fail() {
        assert!(sweep_loop_running(|| true).is_pass());
        assert!(sweep_loop_running(|| false).is_blocking_failure());
        assert!(projections_consistent(|| true).is_pass());
        assert!(projections_consistent(|| false).is_blocking_failure());
    }

    #[test]
    fn stuck_lease_check_passes_when_clear_and_fails_when_expired() {
        let conn = db();
        let meta = ScheduledMessageMeta::create(
            1,
            ScheduleIntent { created_by: "U1".into(), summary: "x".into(), created_at: 1 },
            1_000,
            None,
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        upsert_item(&conn, &meta, None).unwrap();

        // Claim creates a pending occurrence with a live lease until 1030.
        let leases = claim_due(&conn, 1_000, "host", 30).unwrap();
        assert_eq!(leases.len(), 1);

        // While the lease is live, nothing is stuck.
        assert!(no_work_stuck_past_lease(&conn, 1_010).unwrap().is_pass());
        // Past expiry with the occurrence still pending, it is flagged as stuck.
        assert!(no_work_stuck_past_lease(&conn, 1_031).unwrap().is_blocking_failure());
    }

    #[test]
    fn completed_occurrence_is_not_counted_as_stuck() {
        let conn = db();
        let meta = ScheduledMessageMeta::create(
            1,
            ScheduleIntent { created_by: "U1".into(), summary: "x".into(), created_at: 1 },
            1_000,
            None,
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        upsert_item(&conn, &meta, None).unwrap();
        let occ = Occurrence::new(&meta.scheduled_item_id, 1, 1_000);
        acquire_lease(&conn, &occ, "host", 1_000, 30).unwrap();
        crate::lease::complete_occurrence(&conn, &occ, 1_005).unwrap();

        // Even long past the (released) lease, a fired occurrence is not stuck.
        assert!(no_work_stuck_past_lease(&conn, 9_999).unwrap().is_pass());
    }

    #[test]
    fn check_status_round_trips_json() {
        let status = CheckStatus::Fail { detail: "x".into() };
        let json = serde_json::to_string(&status).unwrap();
        let back: CheckStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}
