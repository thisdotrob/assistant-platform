//! The specialist-job model and its status state machine.
//!
//! A delegation job moves through a small, explicit state machine. The distinct
//! terminal states (`Cancelled`, `TimedOut`, `Succeeded`, `PartialResult`,
//! `Failed`) let the host and orchestrator tell apart "finished with an answer"
//! from "stopped without one" — the architecture requires these be separable.
//! Timeout and cancel-grace decisions are pure functions of elapsed time so the
//! host can drive them from whatever clock it owns.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Cancelled,
    TimedOut,
    Succeeded,
    PartialResult,
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Cancelled => "cancelled",
            JobStatus::TimedOut => "timed_out",
            JobStatus::Succeeded => "succeeded",
            JobStatus::PartialResult => "partial_result",
            JobStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobStatus::Queued,
            "running" => JobStatus::Running,
            "cancelled" => JobStatus::Cancelled,
            "timed_out" => JobStatus::TimedOut,
            "succeeded" => JobStatus::Succeeded,
            "partial_result" => JobStatus::PartialResult,
            "failed" => JobStatus::Failed,
            _ => return None,
        })
    }

    /// Terminal states accept no further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Cancelled
                | JobStatus::TimedOut
                | JobStatus::Succeeded
                | JobStatus::PartialResult
                | JobStatus::Failed
        )
    }

    /// A non-terminal job is "in flight" — it should have a live container.
    pub fn is_in_flight(self) -> bool {
        matches!(self, JobStatus::Queued | JobStatus::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEvent {
    Start,
    Complete,
    CompletePartial,
    Cancel,
    Timeout,
    Fail,
}

impl JobEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            JobEvent::Start => "start",
            JobEvent::Complete => "complete",
            JobEvent::CompletePartial => "complete_partial",
            JobEvent::Cancel => "cancel",
            JobEvent::Timeout => "timeout",
            JobEvent::Fail => "fail",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobError {
    InvalidTransition { from: JobStatus, event: JobEvent },
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::InvalidTransition { from, event } => write!(
                f,
                "invalid job transition: {} cannot accept {}",
                from.as_str(),
                event.as_str()
            ),
        }
    }
}

impl std::error::Error for JobError {}

/// Compute the next status for an event, or reject the transition.
pub fn transition(from: JobStatus, event: JobEvent) -> Result<JobStatus, JobError> {
    use JobEvent::*;
    use JobStatus::*;
    let next = match (from, event) {
        (Queued, Start) => Running,
        (Queued, Cancel) | (Running, Cancel) => Cancelled,
        (Queued, Timeout) | (Running, Timeout) => TimedOut,
        (Queued, Fail) | (Running, Fail) => Failed,
        (Running, Complete) => Succeeded,
        (Running, CompletePartial) => PartialResult,
        _ => return Err(JobError::InvalidTransition { from, event }),
    };
    Ok(next)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBudget {
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_wall_secs: Option<u64>,
}

/// A specialist delegation job. Identity and the agent groups it links are
/// fixed at creation; `status`, `cancel_requested`, and `run_links` evolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecialistJob {
    pub job_id: String,
    pub orchestrator_group: String,
    pub specialist_group: String,
    pub profile_id: String,
    pub status: JobStatus,
    pub budget: JobBudget,
    pub timeout_secs: u64,
    pub cancel_requested: bool,
    pub run_links: Vec<String>,
}

impl SpecialistJob {
    pub fn new(
        job_id: impl Into<String>,
        orchestrator_group: impl Into<String>,
        specialist_group: impl Into<String>,
        profile_id: impl Into<String>,
        budget: JobBudget,
        timeout_secs: u64,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            orchestrator_group: orchestrator_group.into(),
            specialist_group: specialist_group.into(),
            profile_id: profile_id.into(),
            status: JobStatus::Queued,
            budget,
            timeout_secs,
            cancel_requested: false,
            run_links: Vec::new(),
        }
    }

    /// Drive the status machine in place.
    pub fn apply(&mut self, event: JobEvent) -> Result<(), JobError> {
        self.status = transition(self.status, event)?;
        Ok(())
    }

    /// Record that cancellation was requested. The job is not terminal until a
    /// `Cancel` (or `Timeout`/`Fail`) transition is applied.
    pub fn request_cancel(&mut self) {
        self.cancel_requested = true;
    }
}

/// Whether a running job has exceeded its timeout. A `timeout_secs` of 0 means
/// "no timeout".
pub fn is_timed_out(elapsed_running_secs: u64, timeout_secs: u64) -> bool {
    timeout_secs > 0 && elapsed_running_secs >= timeout_secs
}

/// Whether a cancel-requested job has outlived its stop grace period and should
/// be force-stopped.
pub fn cancel_grace_exceeded(elapsed_since_cancel_secs: u64, grace_secs: u64) -> bool {
    elapsed_since_cancel_secs >= grace_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_queued_running_succeeded() {
        let mut job = SpecialistJob::new("j1", "orch", "spec", "browser-specialist", JobBudget::default(), 60);
        assert_eq!(job.status, JobStatus::Queued);
        job.apply(JobEvent::Start).unwrap();
        assert_eq!(job.status, JobStatus::Running);
        job.apply(JobEvent::Complete).unwrap();
        assert_eq!(job.status, JobStatus::Succeeded);
        assert!(job.status.is_terminal());
    }

    #[test]
    fn partial_and_cancel_and_timeout_are_distinct_terminals() {
        assert_eq!(transition(JobStatus::Running, JobEvent::CompletePartial).unwrap(), JobStatus::PartialResult);
        assert_eq!(transition(JobStatus::Running, JobEvent::Cancel).unwrap(), JobStatus::Cancelled);
        assert_eq!(transition(JobStatus::Queued, JobEvent::Cancel).unwrap(), JobStatus::Cancelled);
        assert_eq!(transition(JobStatus::Running, JobEvent::Timeout).unwrap(), JobStatus::TimedOut);
        // All four are terminal and not equal to Succeeded.
        for s in [JobStatus::PartialResult, JobStatus::Cancelled, JobStatus::TimedOut, JobStatus::Failed] {
            assert!(s.is_terminal());
            assert_ne!(s, JobStatus::Succeeded);
        }
    }

    #[test]
    fn terminal_states_reject_further_events() {
        for terminal in [JobStatus::Succeeded, JobStatus::Cancelled, JobStatus::TimedOut, JobStatus::Failed, JobStatus::PartialResult] {
            assert!(matches!(
                transition(terminal, JobEvent::Start),
                Err(JobError::InvalidTransition { .. })
            ));
            assert!(matches!(
                transition(terminal, JobEvent::Complete),
                Err(JobError::InvalidTransition { .. })
            ));
        }
    }

    #[test]
    fn cannot_complete_a_queued_job() {
        assert!(matches!(
            transition(JobStatus::Queued, JobEvent::Complete),
            Err(JobError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn timeout_and_cancel_grace_helpers() {
        assert!(!is_timed_out(30, 60));
        assert!(is_timed_out(60, 60));
        assert!(is_timed_out(61, 60));
        assert!(!is_timed_out(10_000, 0)); // 0 == no timeout
        assert!(!cancel_grace_exceeded(5, 10));
        assert!(cancel_grace_exceeded(10, 10));
    }

    #[test]
    fn status_string_round_trips() {
        for s in [
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Cancelled,
            JobStatus::TimedOut,
            JobStatus::Succeeded,
            JobStatus::PartialResult,
            JobStatus::Failed,
        ] {
            assert_eq!(JobStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(JobStatus::parse("bogus"), None);
    }
}
