//! Agent provider abstraction and a stub implementation.
//!
//! The provider is the seam where the real Claude Agent SDK plugs in. Local
//! runner tests use [`StubProvider`], which needs no credentials, so the host
//! side of the protocol (prompt assembly, action processing, fallback) can be
//! exercised end-to-end without Docker or Claude.

use crate::envelope::{InboundEnvelope, RunContext, RunResult};

/// Runs a single agent turn from a rendered prompt and inbound messages.
pub trait AgentProvider {
    fn id(&self) -> &str;
    fn run(&self, ctx: &RunContext, inbound: &[InboundEnvelope], prompt: &str) -> RunResult;
}

/// A deterministic provider for tests. It returns a canned [`RunResult`],
/// independent of the prompt, so callers control exactly which actions and
/// final text a run produces.
pub struct StubProvider {
    canned: RunResult,
}

impl StubProvider {
    pub fn new(canned: RunResult) -> Self {
        Self { canned }
    }

    /// A provider that produces only final text (no typed actions), exercising
    /// the fallback path.
    pub fn final_text(text: impl Into<String>) -> Self {
        Self::new(RunResult {
            actions: vec![],
            final_text: Some(text.into()),
        })
    }
}

impl AgentProvider for StubProvider {
    fn id(&self) -> &str {
        "stub"
    }

    fn run(&self, _ctx: &RunContext, _inbound: &[InboundEnvelope], _prompt: &str) -> RunResult {
        self.canned.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::OutboundAction;
    use crate::envelope::AgentKind;

    fn ctx() -> RunContext {
        RunContext {
            run_id: "run-1".into(),
            inbound_seq: 2,
            agent_kind: AgentKind::Orchestrator,
            default_destination: "local:cli".into(),
            observe_only: false,
        }
    }

    #[test]
    fn stub_returns_canned_result() {
        let provider = StubProvider::new(RunResult {
            actions: vec![OutboundAction::SendMessage {
                destination: "local:cli".into(),
                text: "hi".into(),
            }],
            final_text: None,
        });
        let result = provider.run(&ctx(), &[], "prompt");
        assert!(result.has_user_visible_send());
        assert_eq!(provider.id(), "stub");
    }

    #[test]
    fn final_text_helper_produces_no_actions() {
        let provider = StubProvider::final_text("just text");
        let result = provider.run(&ctx(), &[], "prompt");
        assert!(result.actions.is_empty());
        assert_eq!(result.final_text.as_deref(), Some("just text"));
    }
}
