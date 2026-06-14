//! Approval readiness checks: the expiry sweep is running, and no pending
//! approval is stuck past its expiry. The sweep-liveness check depends on
//! host-side runtime state, so it takes an injected probe; the stuck-approval
//! check is a pure central-DB query.
//!
//! `CheckStatus` is duplicated per crate (not shared) to honor the module
//! dependency boundary, matching the other platform crates.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::model::{ApprovalError, EpochSecs};
use crate::store::pending_past_expiry;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
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

/// The host's approval-expiry sweep is alive, via an injected liveness probe.
pub fn expiry_sweep_running(probe: impl FnOnce() -> bool) -> CheckStatus {
    if probe() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: "approval expiry sweep is not running".to_string(),
        }
    }
}

/// No pending approval is past its expiry — any such rows mean the sweep is not
/// keeping up, and an expired-but-unswept approval could be wrongly honored.
pub fn no_approvals_stuck_past_expiry(
    conn: &Connection,
    now: EpochSecs,
) -> Result<CheckStatus, ApprovalError> {
    let stuck = pending_past_expiry(conn, now)?;
    if stuck == 0 {
        Ok(CheckStatus::Pass)
    } else {
        Ok(CheckStatus::Fail {
            detail: format!("{stuck} pending approval(s) past expiry"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_pass_and_fail() {
        assert!(expiry_sweep_running(|| true).is_pass());
        assert!(expiry_sweep_running(|| false).is_blocking_failure());
    }
}
