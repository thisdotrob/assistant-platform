//! Aggregating and rendering readiness across modules.
//!
//! Each module defines its own `CheckStatus` to keep module boundaries clean,
//! so this crate carries a neutral [`CheckState`] the host maps those into when
//! collecting a [`ReadinessReport`]. The report renders through the same
//! [`crate::output`] path as any command, and `doctor` exits non-zero when any
//! check is a blocking failure.

use serde::{Deserialize, Serialize};

use crate::command::{CommandOutcome, ResultTable};
use crate::output::{render, OutputFormat, Verbosity};

/// One check's outcome, in a module-neutral form mirroring every module's own
/// `CheckStatus`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckState {
    Pass,
    Fail { detail: String },
    Skipped { detail: String },
}

impl CheckState {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckState::Pass)
    }

    /// A failure that should hold back readiness. `Skipped` does not block.
    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckState::Fail { .. })
    }

    fn label(&self) -> &'static str {
        match self {
            CheckState::Pass => "pass",
            CheckState::Fail { .. } => "fail",
            CheckState::Skipped { .. } => "skipped",
        }
    }

    fn detail(&self) -> &str {
        match self {
            CheckState::Pass => "",
            CheckState::Fail { detail } | CheckState::Skipped { detail } => detail,
        }
    }
}

/// One named check belonging to a module.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub module: String,
    pub name: String,
    pub state: CheckState,
}

/// The aggregated readiness of every registered module. claw-cli registers no
/// checks of its own; the host feeds in each module's results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessReport {
    checks: Vec<CheckResult>,
}

impl ReadinessReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one check's result.
    pub fn add(&mut self, module: impl Into<String>, name: impl Into<String>, state: CheckState) {
        self.checks.push(CheckResult {
            module: module.into(),
            name: name.into(),
            state,
        });
    }

    pub fn checks(&self) -> &[CheckResult] {
        &self.checks
    }

    pub fn blocking_failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.state.is_blocking_failure())
            .count()
    }

    /// Ready when no check is a blocking failure.
    pub fn is_ready(&self) -> bool {
        self.blocking_failures() == 0
    }

    /// Render the report as a command result, so it flows through the shared
    /// JSON/table renderer.
    pub fn to_outcome(&self) -> CommandOutcome {
        let mut table = ResultTable::new(["module", "check", "status", "detail"]);
        for c in &self.checks {
            table.push_row([
                c.module.clone(),
                c.name.clone(),
                c.state.label().to_string(),
                c.state.detail().to_string(),
            ]);
        }
        let failures = self.blocking_failures();
        let summary = if failures == 0 {
            format!("ready: {} check(s) passed", self.checks.len())
        } else {
            format!("not ready: {failures} of {} check(s) failing", self.checks.len())
        };
        CommandOutcome::message(table, summary)
    }
}

/// Render the readiness report in the requested format.
pub fn render_readiness(
    report: &ReadinessReport,
    format: OutputFormat,
    verbosity: Verbosity,
) -> String {
    render(&report.to_outcome(), format, verbosity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ReadinessReport {
        let mut r = ReadinessReport::new();
        r.add("claw-db", "schema_current", CheckState::Pass);
        r.add("claw-session", "parity", CheckState::Skipped { detail: "no sessions".to_string() });
        r.add("claw-router", "channels_connected", CheckState::Fail { detail: "slack down".to_string() });
        r
    }

    #[test]
    fn skipped_does_not_block_but_fail_does() {
        let r = report();
        assert_eq!(r.blocking_failures(), 1);
        assert!(!r.is_ready());

        let mut clean = ReadinessReport::new();
        clean.add("claw-db", "schema_current", CheckState::Pass);
        clean.add("claw-session", "parity", CheckState::Skipped { detail: "n/a".to_string() });
        assert!(clean.is_ready());
    }

    #[test]
    fn table_render_shows_every_check_and_the_failure_summary() {
        let rendered = render_readiness(&report(), OutputFormat::Table, Verbosity::Normal);
        assert!(rendered.contains("claw-db"));
        assert!(rendered.contains("channels_connected"));
        assert!(rendered.contains("slack down"));
        assert!(rendered.contains("not ready: 1 of 3"));
    }

    #[test]
    fn json_render_reflects_aggregated_state() {
        let json = render_readiness(&report(), OutputFormat::Json, Verbosity::Normal);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["status"], "ok"); // the render itself succeeds
        assert!(value["message"].as_str().unwrap().contains("not ready"));
    }

    #[test]
    fn empty_report_is_ready() {
        assert!(ReadinessReport::new().is_ready());
    }
}
