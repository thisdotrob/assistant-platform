//! The host-facing delegation flow: create a specialist and start/complete its
//! jobs, threading registry policy, central-DB persistence, and audit together.
//!
//! These functions are the single place that combines the three concerns so a
//! host never has to re-derive the order: check policy, then mutate the store,
//! then emit the audit event. Policy and store both feed `DelegationError`.

use rusqlite::Connection;

use claw_session::SessionLayout;

use crate::audit::{AuditEvent, AuditSink};
use crate::handoff::{HandoffPacket, SpecialistResult, HANDOFF_KIND, RESULT_KIND};
use crate::job::{transition, JobError, JobEvent, JobStatus, SpecialistJob};
use crate::registry::{authorize_create, authorize_job_start, PolicyError, SpecialistRegistry};
use crate::routing::{
    authorize_a2a, deliver_handoff, deliver_result, A2aAcl, ReturnPath, RoutingError,
};
use crate::store::{self, StoreError};

#[derive(Debug)]
pub enum DelegationError {
    Policy(PolicyError),
    Store(StoreError),
    Routing(RoutingError),
    InvalidTransition(JobError),
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegationError::Policy(e) => write!(f, "{e}"),
            DelegationError::Store(e) => write!(f, "{e}"),
            DelegationError::Routing(e) => write!(f, "{e}"),
            DelegationError::InvalidTransition(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DelegationError {}

impl From<PolicyError> for DelegationError {
    fn from(value: PolicyError) -> Self {
        DelegationError::Policy(value)
    }
}

impl From<StoreError> for DelegationError {
    fn from(value: StoreError) -> Self {
        DelegationError::Store(value)
    }
}

impl From<RoutingError> for DelegationError {
    fn from(value: RoutingError) -> Self {
        DelegationError::Routing(value)
    }
}

impl From<JobError> for DelegationError {
    fn from(value: JobError) -> Self {
        DelegationError::InvalidTransition(value)
    }
}

/// Authorize, then create a specialist agent group for `profile_id`, recording
/// a `SpecialistCreated` audit event. Returns the created group slug.
pub fn create_specialist(
    conn: &Connection,
    registry: &SpecialistRegistry,
    audit: &mut dyn AuditSink,
    profile_id: &str,
    slug: &str,
) -> Result<String, DelegationError> {
    let existing = store::specialist_group_count(conn, profile_id)?;
    let profile = authorize_create(registry, profile_id, existing)?;
    store::create_specialist_group(conn, slug, profile_id, &profile.profile_version)?;
    audit.record(AuditEvent::SpecialistCreated {
        profile_id: profile_id.to_string(),
        specialist_group: slug.to_string(),
    });
    Ok(slug.to_string())
}

/// Authorize, then persist a queued job, recording a `DelegationStarted` audit
/// event. The job's profile must be registered and within its concurrency limit.
pub fn start_job(
    conn: &Connection,
    registry: &SpecialistRegistry,
    audit: &mut dyn AuditSink,
    job: &SpecialistJob,
) -> Result<(), DelegationError> {
    let profile = registry
        .get(&job.profile_id)
        .ok_or_else(|| PolicyError::UnknownProfile {
            profile_id: job.profile_id.clone(),
        })?;
    let in_flight = store::running_or_queued_job_count(conn, &job.profile_id)?;
    authorize_job_start(profile, in_flight)?;
    store::insert_job(conn, job)?;
    audit.record(AuditEvent::DelegationStarted {
        job_id: job.job_id.clone(),
        orchestrator_group: job.orchestrator_group.clone(),
        specialist_group: job.specialist_group.clone(),
        profile_id: job.profile_id.clone(),
    });
    Ok(())
}

/// Drive a job's status machine by applying an event, validating the transition
/// against the loaded current status before persisting it. This is the guarded
/// path to the store: the low-level `store::update_status` does not itself
/// enforce the state machine, so all lifecycle changes should go through here.
/// Emits `DelegationCompleted` when the resulting status is terminal.
pub fn transition_job(
    conn: &Connection,
    audit: &mut dyn AuditSink,
    job_id: &str,
    event: JobEvent,
) -> Result<JobStatus, DelegationError> {
    let current = store::load_job(conn, job_id)?.status;
    let next = transition(current, event)?;
    store::update_status(conn, job_id, next)?;
    if next.is_terminal() {
        audit.record(AuditEvent::DelegationCompleted {
            job_id: job_id.to_string(),
            status: next,
        });
    }
    Ok(next)
}

/// Authorize and route a handoff packet to a specialist's session, recording an
/// `AgentRouting` audit event. The edge must be in the allow-list and the
/// allow-list must be acyclic.
pub fn route_handoff(
    audit: &mut dyn AuditSink,
    acl: &A2aAcl,
    specialist_layout: &SessionLayout,
    from: &str,
    to: &str,
    handoff: &HandoffPacket,
    return_path: &ReturnPath,
) -> Result<i64, DelegationError> {
    authorize_a2a(acl, from, to)?;
    let seq = deliver_handoff(specialist_layout, from, handoff, return_path)?;
    audit.record(AuditEvent::AgentRouting {
        from_group: from.to_string(),
        to_group: to.to_string(),
        kind: HANDOFF_KIND.to_string(),
    });
    Ok(seq)
}

/// Route a collected specialist result back to the orchestrator's session,
/// recording an `AgentRouting` audit event.
pub fn route_result(
    audit: &mut dyn AuditSink,
    orchestrator_layout: &SessionLayout,
    from: &str,
    to: &str,
    result: &SpecialistResult,
) -> Result<i64, DelegationError> {
    let seq = deliver_result(orchestrator_layout, from, result)?;
    audit.record(AuditEvent::AgentRouting {
        from_group: from.to_string(),
        to_group: to.to_string(),
        kind: RESULT_KIND.to_string(),
    });
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::VecAuditSink;
    use crate::job::JobBudget;
    use crate::registry::{ProfileLimits, RegisteredProfile};
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};

    fn migrated_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut set = baseline_migrations(order);
        for m in store::migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn registry() -> SpecialistRegistry {
        let mut r = SpecialistRegistry::new();
        r.register(RegisteredProfile::specialist(
            "browser-specialist",
            "0.1.0",
            ProfileLimits::new(1, 1),
        ));
        r
    }

    #[test]
    fn create_specialist_enforces_count_limit() {
        let conn = migrated_db();
        let reg = registry();
        let mut audit = VecAuditSink::new();
        create_specialist(&conn, &reg, &mut audit, "browser-specialist", "browser-1").unwrap();
        // max_specialists == 1, so a second is rejected.
        assert!(matches!(
            create_specialist(&conn, &reg, &mut audit, "browser-specialist", "browser-2"),
            Err(DelegationError::Policy(PolicyError::SpecialistLimitReached { .. }))
        ));
    }

    #[test]
    fn start_job_enforces_concurrency_limit() {
        let conn = migrated_db();
        let reg = registry();
        let mut audit = VecAuditSink::new();
        let j1 = SpecialistJob::new("j1", "o", "browser-1", "browser-specialist", JobBudget::default(), 60);
        start_job(&conn, &reg, &mut audit, &j1).unwrap();
        // max_concurrent_jobs == 1; second queued job is over the limit.
        let j2 = SpecialistJob::new("j2", "o", "browser-1", "browser-specialist", JobBudget::default(), 60);
        assert!(matches!(
            start_job(&conn, &reg, &mut audit, &j2),
            Err(DelegationError::Policy(PolicyError::ConcurrencyLimitReached { .. }))
        ));
    }

    #[test]
    fn transition_job_enforces_state_machine() {
        let conn = migrated_db();
        let reg = registry();
        let mut audit = VecAuditSink::new();
        let job = SpecialistJob::new("j1", "o", "browser-1", "browser-specialist", JobBudget::default(), 60);
        start_job(&conn, &reg, &mut audit, &job).unwrap();
        // A queued job cannot be completed directly.
        assert!(matches!(
            transition_job(&conn, &mut audit, "j1", JobEvent::Complete),
            Err(DelegationError::InvalidTransition(_))
        ));
        // Start then complete is valid, and only completion is audited.
        transition_job(&conn, &mut audit, "j1", JobEvent::Start).unwrap();
        assert_eq!(
            transition_job(&conn, &mut audit, "j1", JobEvent::Complete).unwrap(),
            JobStatus::Succeeded
        );
        assert!(audit
            .events
            .iter()
            .any(|e| matches!(e, AuditEvent::DelegationCompleted { .. })));
    }

    #[test]
    fn start_job_rejects_unknown_profile() {
        let conn = migrated_db();
        let reg = registry();
        let mut audit = VecAuditSink::new();
        let job = SpecialistJob::new("j1", "o", "s", "ghost", JobBudget::default(), 60);
        assert!(matches!(
            start_job(&conn, &reg, &mut audit, &job),
            Err(DelegationError::Policy(PolicyError::UnknownProfile { .. }))
        ));
    }
}
