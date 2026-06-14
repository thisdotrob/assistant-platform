//! Final-text fallback policy.
//!
//! A normal orchestrator run that completes without a user-visible typed send
//! may have its final text delivered once, to the run's default destination, as
//! a safety net. The policy enforces the architecture's constraints: disabled
//! for observe-only and specialist runs, never delivered twice for the same
//! (run, inbound message), the destination always taken from the run context
//! (never parsed from prose), and suppressed entirely when a typed send already
//! occurred (in which case the final text is archived as transcript only).

use std::collections::HashSet;

use crate::envelope::{AgentKind, RunContext, RunResult};

/// Why a final-text fallback was not delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressReason {
    /// A typed send already delivered; final text is archived, not re-sent.
    UserVisibleSendOccurred,
    /// Observe-only runs never reply.
    ObserveOnly,
    /// Specialist runs return to the orchestrator, not to users.
    Specialist,
    /// The run produced no final text to fall back to.
    NoFinalText,
    /// This (run, inbound message) already triggered a fallback.
    AlreadyDelivered,
}

/// The fallback outcome for a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FallbackDecision {
    Deliver { destination: String, text: String },
    Suppress { reason: SuppressReason },
}

impl FallbackDecision {
    pub fn is_deliver(&self) -> bool {
        matches!(self, FallbackDecision::Deliver { .. })
    }
}

/// Pure fallback decision, ignoring idempotency. [`FallbackLedger::decide`]
/// layers idempotency on top and is the entry point callers should use.
pub fn decide_fallback(ctx: &RunContext, result: &RunResult) -> FallbackDecision {
    if ctx.agent_kind == AgentKind::Specialist {
        return FallbackDecision::Suppress {
            reason: SuppressReason::Specialist,
        };
    }
    if ctx.observe_only {
        return FallbackDecision::Suppress {
            reason: SuppressReason::ObserveOnly,
        };
    }
    if result.has_user_visible_send() {
        return FallbackDecision::Suppress {
            reason: SuppressReason::UserVisibleSendOccurred,
        };
    }
    match result.final_text.as_deref() {
        Some(text) if !text.trim().is_empty() => FallbackDecision::Deliver {
            destination: ctx.default_destination.clone(),
            text: text.to_string(),
        },
        _ => FallbackDecision::Suppress {
            reason: SuppressReason::NoFinalText,
        },
    }
}

/// Records which (run_id, inbound_seq) pairs have already fired a fallback so a
/// retried or replayed run never re-delivers its final text.
#[derive(Clone, Debug, Default)]
pub struct FallbackLedger {
    fired: HashSet<(String, i64)>,
}

impl FallbackLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_fired(&self, run_id: &str, inbound_seq: i64) -> bool {
        self.fired.contains(&(run_id.to_string(), inbound_seq))
    }

    /// Decide fallback and, when delivering, record the (run, message) so a
    /// later call for the same pair suppresses as `AlreadyDelivered`.
    pub fn decide(&mut self, ctx: &RunContext, result: &RunResult) -> FallbackDecision {
        if self.has_fired(&ctx.run_id, ctx.inbound_seq) {
            return FallbackDecision::Suppress {
                reason: SuppressReason::AlreadyDelivered,
            };
        }
        let decision = decide_fallback(ctx, result);
        if decision.is_deliver() {
            self.fired.insert((ctx.run_id.clone(), ctx.inbound_seq));
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::OutboundAction;

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
    fn delivers_final_text_to_default_destination() {
        let result = RunResult {
            actions: vec![],
            final_text: Some("hello".into()),
        };
        assert_eq!(
            decide_fallback(&ctx(), &result),
            FallbackDecision::Deliver {
                destination: "local:cli".into(),
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn suppresses_when_typed_send_present() {
        let result = RunResult {
            actions: vec![OutboundAction::SendMessage {
                destination: "elsewhere".into(),
                text: "sent".into(),
            }],
            final_text: Some("hello".into()),
        };
        assert_eq!(
            decide_fallback(&ctx(), &result),
            FallbackDecision::Suppress {
                reason: SuppressReason::UserVisibleSendOccurred,
            }
        );
    }

    #[test]
    fn suppresses_for_observe_only_and_specialist() {
        let result = RunResult {
            actions: vec![],
            final_text: Some("hello".into()),
        };
        let mut observe = ctx();
        observe.observe_only = true;
        assert_eq!(
            decide_fallback(&observe, &result),
            FallbackDecision::Suppress {
                reason: SuppressReason::ObserveOnly,
            }
        );
        let mut specialist = ctx();
        specialist.agent_kind = AgentKind::Specialist;
        assert_eq!(
            decide_fallback(&specialist, &result),
            FallbackDecision::Suppress {
                reason: SuppressReason::Specialist,
            }
        );
    }

    #[test]
    fn suppresses_blank_or_missing_final_text() {
        let blank = RunResult {
            actions: vec![],
            final_text: Some("   \n".into()),
        };
        assert_eq!(
            decide_fallback(&ctx(), &blank),
            FallbackDecision::Suppress {
                reason: SuppressReason::NoFinalText,
            }
        );
    }

    #[test]
    fn idempotent_by_run_and_message() {
        let result = RunResult {
            actions: vec![],
            final_text: Some("hello".into()),
        };
        let mut ledger = FallbackLedger::new();
        assert!(ledger.decide(&ctx(), &result).is_deliver());
        // Same run + message: suppressed as already delivered.
        assert_eq!(
            ledger.decide(&ctx(), &result),
            FallbackDecision::Suppress {
                reason: SuppressReason::AlreadyDelivered,
            }
        );
        // A different inbound message in the same run still delivers.
        let mut next = ctx();
        next.inbound_seq = 4;
        assert!(ledger.decide(&next, &result).is_deliver());
    }
}
