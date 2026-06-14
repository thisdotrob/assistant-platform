//! Readiness checks for the agent graph.
//!
//! These are pure, host-drivable predicates the boot sequence can run to assert
//! the graph is in a sane state: that required specialist profiles are
//! registered, and that no delegation job is stranded in a non-terminal state
//! without a live container (an "orphan").

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::job::SpecialistJob;
use crate::registry::SpecialistRegistry;

/// Mirrors the host readiness vocabulary without taking a dependency on the
/// concrete readiness crate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    Skipped { detail: String },
}

/// Pass only when every required profile id is present in the registry.
pub fn profiles_resolve(registry: &SpecialistRegistry, required: &[&str]) -> CheckStatus {
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|id| !registry.is_registered(id))
        .collect();
    if missing.is_empty() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: format!("unregistered specialist profiles: {}", missing.join(", ")),
        }
    }
}

/// The job ids of any job still in a non-terminal state. A host that holds a
/// snapshot of jobs uses this to find orphans after a restart.
pub fn find_orphaned_jobs(jobs: &[SpecialistJob]) -> Vec<String> {
    jobs.iter()
        .filter(|j| !j.status.is_terminal())
        .map(|j| j.job_id.clone())
        .collect()
}

/// Pass only when the central DB holds no non-terminal specialist jobs.
pub fn no_orphaned_jobs(conn: &Connection) -> Result<CheckStatus, rusqlite::Error> {
    let stranded: i64 = conn.query_row(
        "SELECT count(*) FROM specialist_jobs WHERE status IN ('queued', 'running')",
        [],
        |r| r.get(0),
    )?;
    Ok(if stranded == 0 {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: format!("{stranded} specialist job(s) stranded in a non-terminal state"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobBudget, JobEvent, SpecialistJob};
    use crate::registry::{ProfileLimits, RegisteredProfile, SpecialistRegistry};

    fn registry() -> SpecialistRegistry {
        let mut r = SpecialistRegistry::new();
        r.register(RegisteredProfile::specialist(
            "browser-specialist",
            "0.1.0",
            ProfileLimits::new(1, 2),
        ));
        r
    }

    #[test]
    fn profiles_resolve_reports_missing() {
        let r = registry();
        assert_eq!(profiles_resolve(&r, &["browser-specialist"]), CheckStatus::Pass);
        assert!(matches!(
            profiles_resolve(&r, &["browser-specialist", "missing"]),
            CheckStatus::Fail { .. }
        ));
    }

    #[test]
    fn orphans_are_non_terminal_jobs() {
        let mut running = SpecialistJob::new("j1", "o", "s", "browser-specialist", JobBudget::default(), 60);
        running.apply(JobEvent::Start).unwrap();
        let mut done = SpecialistJob::new("j2", "o", "s", "browser-specialist", JobBudget::default(), 60);
        done.apply(JobEvent::Start).unwrap();
        done.apply(JobEvent::Complete).unwrap();
        let orphans = find_orphaned_jobs(&[running, done]);
        assert_eq!(orphans, vec!["j1".to_string()]);
    }
}
