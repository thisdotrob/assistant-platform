//! Inbound/outbound envelopes and the per-run context shared by host, runner,
//! CLI, and web UI. These types carry no product policy; a run's behavior is
//! decided by the typed actions it returns and the context the host supplies.

use serde::{Deserialize, Serialize};

use crate::action::OutboundAction;

/// Which agent produced a run. Fallback is disabled for specialists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Orchestrator,
    Specialist,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Orchestrator => "orchestrator",
            AgentKind::Specialist => "specialist",
        }
    }
}

/// One inbound message as presented to a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundEnvelope {
    pub seq: i64,
    pub sender: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

/// Context for a single agent run. The host owns every field here; the runner
/// never derives the default destination from model prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContext {
    pub run_id: String,
    pub inbound_seq: i64,
    pub agent_kind: AgentKind,
    pub default_destination: String,
    pub observe_only: bool,
}

/// The result of an agent run: zero or more typed actions plus optional final
/// text. Final text is a fallback candidate only; it is never scanned for
/// routing tags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    #[serde(default)]
    pub actions: Vec<OutboundAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
}

impl RunResult {
    pub fn has_user_visible_send(&self) -> bool {
        self.actions.iter().any(OutboundAction::is_user_visible_send)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kind_round_trips_snake_case() {
        let json = serde_json::to_string(&AgentKind::Specialist).unwrap();
        assert_eq!(json, r#""specialist""#);
        let back: AgentKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AgentKind::Specialist);
    }

    #[test]
    fn run_result_detects_user_visible_send() {
        let mut result = RunResult::default();
        assert!(!result.has_user_visible_send());
        result.actions.push(OutboundAction::AddReaction {
            target_seq: 1,
            emoji: ":x:".into(),
        });
        assert!(!result.has_user_visible_send());
        result.actions.push(OutboundAction::SendMessage {
            destination: "d".into(),
            text: "t".into(),
        });
        assert!(result.has_user_visible_send());
    }
}
