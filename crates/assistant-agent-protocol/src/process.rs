//! Turn a run result into concrete deliveries.
//!
//! Deliveries are built from typed actions only. The run's `final_text` is
//! never parsed for routing: malformed, nested, or escaped tags in prose can
//! never produce a delivery. Final text becomes at most one fallback
//! `send_message` (under [`crate::fallback`] rules); otherwise it is archived as
//! transcript text. A run therefore never both sends a typed reply and
//! re-delivers the same content.

use crate::action::OutboundAction;
use crate::envelope::{RunContext, RunResult};
use crate::fallback::{FallbackDecision, FallbackLedger};

/// One outbound message to write to the session outbound DB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub kind: String,
    pub action: OutboundAction,
}

impl Delivery {
    pub fn new(action: OutboundAction) -> Self {
        Self {
            kind: action.kind().to_string(),
            action,
        }
    }
}

/// The outcome of processing a run: the ordered deliveries to emit, the
/// fallback decision, and any final text retained as transcript-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedRun {
    pub deliveries: Vec<Delivery>,
    pub fallback: FallbackDecision,
    pub archived_final_text: Option<String>,
}

impl ProcessedRun {
    /// Total messages actually delivered to a destination, including the
    /// fallback send when one fired. Used to prove non-duplication in tests.
    pub fn delivered_count(&self) -> usize {
        self.deliveries.len() + usize::from(self.fallback.is_deliver())
    }
}

/// Process a run against the fallback ledger. Records the fallback when it
/// fires so repeated processing of the same (run, message) is idempotent.
pub fn process_run(
    ctx: &RunContext,
    result: &RunResult,
    ledger: &mut FallbackLedger,
) -> ProcessedRun {
    let deliveries: Vec<Delivery> = result
        .actions
        .iter()
        .cloned()
        .map(Delivery::new)
        .collect();

    let fallback = ledger.decide(ctx, result);

    // When the fallback does not deliver, retain non-empty final text as
    // transcript-only. When it delivers, the text went out as the fallback
    // message, so there is nothing extra to archive.
    let archived_final_text = match &fallback {
        FallbackDecision::Deliver { .. } => None,
        FallbackDecision::Suppress { .. } => result
            .final_text
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string),
    };

    ProcessedRun {
        deliveries,
        fallback,
        archived_final_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn tags_in_final_text_never_route() {
        // Malformed, nested, and escaped tags in prose must produce zero
        // deliveries on their own. With no typed action, only the single
        // fallback message is delivered, carrying the prose verbatim.
        let prose = "<send_message dest=\"#general\">hi</send> \
                     <send_message><send_message>x</send_message></send_message> \
                     &lt;send_message&gt;escaped&lt;/send_message&gt;";
        let result = RunResult {
            actions: vec![],
            final_text: Some(prose.into()),
        };
        let mut ledger = FallbackLedger::new();
        let processed = process_run(&ctx(), &result, &mut ledger);

        assert!(processed.deliveries.is_empty(), "tags must not create deliveries");
        assert_eq!(processed.delivered_count(), 1, "only the fallback delivers");
        match &processed.fallback {
            FallbackDecision::Deliver { destination, text } => {
                assert_eq!(destination, "local:cli");
                assert_eq!(text, prose, "fallback delivers prose verbatim, unparsed");
            }
            other => panic!("expected fallback delivery, got {other:?}"),
        }
    }

    #[test]
    fn typed_send_plus_final_text_does_not_duplicate() {
        let result = RunResult {
            actions: vec![OutboundAction::SendMessage {
                destination: "local:cli".into(),
                text: "the answer".into(),
            }],
            final_text: Some("the answer".into()),
        };
        let mut ledger = FallbackLedger::new();
        let processed = process_run(&ctx(), &result, &mut ledger);

        assert_eq!(processed.deliveries.len(), 1);
        assert!(!processed.fallback.is_deliver());
        assert_eq!(processed.delivered_count(), 1, "no duplicate delivery");
        assert_eq!(
            processed.archived_final_text.as_deref(),
            Some("the answer"),
            "final text archived as transcript only"
        );
    }

    #[test]
    fn reprocessing_same_run_message_is_idempotent() {
        let result = RunResult {
            actions: vec![],
            final_text: Some("hello".into()),
        };
        let mut ledger = FallbackLedger::new();
        let first = process_run(&ctx(), &result, &mut ledger);
        assert_eq!(first.delivered_count(), 1);

        let second = process_run(&ctx(), &result, &mut ledger);
        assert_eq!(second.delivered_count(), 0, "no re-delivery on replay");
        // On the suppressed replay the text is retained as transcript only.
        assert_eq!(second.archived_final_text.as_deref(), Some("hello"));
    }
}
