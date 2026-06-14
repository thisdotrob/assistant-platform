//! Stub agent loop.
//!
//! Drives one run: render the prompt, ask the provider for a result, process it
//! into deliveries, and emit them through an [`OutboundSink`]. The sink is an
//! abstraction so this crate never depends on `claw-session` in production —
//! the session-backed sink lives in tests as a dev-dependency. Keeping the loop
//! behind the sink trait is what lets `claw-agent-protocol` stay
//! `depends_on = ["claw-core"]` in the platform manifest.

use crate::action::OutboundAction;
use crate::envelope::{InboundEnvelope, RunContext};
use crate::fallback::{FallbackDecision, FallbackLedger};
use crate::fragment::{render_prompt, RenderableFragment};
use crate::process::{process_run, Delivery, ProcessedRun};
use crate::provider::AgentProvider;

/// Where processed deliveries are written (in production, the session outbound
/// DB; in tests, a fake container).
pub trait OutboundSink {
    type Error;
    fn emit(&mut self, delivery: &Delivery) -> Result<(), Self::Error>;
}

/// Render the prompt, run the provider, process the result, and emit every
/// delivery plus a single fallback message when the policy fires. The processed
/// run is returned so callers can inspect deliveries, fallback, and archived
/// transcript text.
pub fn run_once<P, S>(
    provider: &P,
    sink: &mut S,
    ctx: &RunContext,
    inbound: &[InboundEnvelope],
    fragments: &[RenderableFragment],
    ledger: &mut FallbackLedger,
) -> Result<ProcessedRun, S::Error>
where
    P: AgentProvider,
    S: OutboundSink,
{
    let prompt = render_prompt(fragments);
    let result = provider.run(ctx, inbound, &prompt);
    let processed = process_run(ctx, &result, ledger);

    for delivery in &processed.deliveries {
        sink.emit(delivery)?;
    }
    if let FallbackDecision::Deliver { destination, text } = &processed.fallback {
        let action = OutboundAction::SendMessage {
            destination: destination.clone(),
            text: text.clone(),
        };
        sink.emit(&Delivery::new(action))?;
    }
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{AgentKind, RunResult};
    use crate::fragment::shared_fragments;
    use crate::provider::StubProvider;

    #[derive(Default)]
    struct VecSink {
        emitted: Vec<Delivery>,
    }

    impl OutboundSink for VecSink {
        type Error = std::convert::Infallible;
        fn emit(&mut self, delivery: &Delivery) -> Result<(), Self::Error> {
            self.emitted.push(delivery.clone());
            Ok(())
        }
    }

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
    fn fallback_run_emits_single_message() {
        let provider = StubProvider::final_text("hello");
        let mut sink = VecSink::default();
        let mut ledger = FallbackLedger::new();
        let processed = run_once(
            &provider,
            &mut sink,
            &ctx(),
            &[],
            &shared_fragments(AgentKind::Orchestrator, "cli"),
            &mut ledger,
        )
        .unwrap();

        assert_eq!(sink.emitted.len(), 1);
        assert_eq!(sink.emitted[0].kind, "send_message");
        assert!(processed.fallback.is_deliver());
    }

    #[test]
    fn typed_send_run_emits_no_fallback() {
        let provider = StubProvider::new(RunResult {
            actions: vec![OutboundAction::SendMessage {
                destination: "local:cli".into(),
                text: "answer".into(),
            }],
            final_text: Some("answer".into()),
        });
        let mut sink = VecSink::default();
        let mut ledger = FallbackLedger::new();
        run_once(
            &provider,
            &mut sink,
            &ctx(),
            &[],
            &shared_fragments(AgentKind::Orchestrator, "cli"),
            &mut ledger,
        )
        .unwrap();

        assert_eq!(sink.emitted.len(), 1, "exactly the typed send, no fallback");
    }
}
