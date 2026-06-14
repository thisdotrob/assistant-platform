//! The stub agent loop drives a real session outbound DB through the
//! `OutboundSink` trait, proving the protocol works end-to-end without Docker
//! or Claude. The sink is backed by `claw-session`'s `FakeContainer`, which is a
//! dev-dependency only — production `claw-agent-protocol` depends on `claw-core`
//! alone.

use claw_agent_protocol::{
    run_once, AgentKind, Delivery, FallbackLedger, OutboundAction, OutboundSink, RunContext,
    RunResult, StubProvider,
};
use claw_agent_protocol::fragment::shared_fragments;
use claw_session::{current_outbound_compat, read_outbound, FakeContainer, LocalControl, SessionLayout};

/// Bridges processed deliveries into a session outbound DB: each delivery is
/// written as one odd-sequence outbound message whose kind is the action kind
/// and whose content is the serialized typed action.
struct ContainerSink<'a> {
    container: &'a FakeContainer,
}

impl OutboundSink for ContainerSink<'_> {
    type Error = Box<dyn std::error::Error>;

    fn emit(&mut self, delivery: &Delivery) -> Result<(), Self::Error> {
        let content = serde_json::to_string(&delivery.action)?;
        self.container.emit(&delivery.kind, &content)?;
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
fn fallback_run_writes_single_outbound_message() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(tmp.path(), "orchestrator", "sess-1").unwrap();
    let control = LocalControl::new(layout.clone());
    control.init().unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();

    let provider = StubProvider::final_text("here is your answer");
    let mut sink = ContainerSink { container: &container };
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
    assert!(processed.fallback.is_deliver());

    let outbound = read_outbound(&layout, current_outbound_compat()).unwrap();
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].kind, "send_message");
    let action: OutboundAction = serde_json::from_str(&outbound[0].content).unwrap();
    assert_eq!(
        action,
        OutboundAction::SendMessage {
            destination: "local:cli".into(),
            text: "here is your answer".into(),
        }
    );
}

#[test]
fn typed_send_plus_final_text_writes_no_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(tmp.path(), "orchestrator", "sess-1").unwrap();
    let control = LocalControl::new(layout.clone());
    control.init().unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();

    let provider = StubProvider::new(RunResult {
        actions: vec![OutboundAction::SendMessage {
            destination: "local:cli".into(),
            text: "the answer".into(),
        }],
        final_text: Some("the answer".into()),
    });
    let mut sink = ContainerSink { container: &container };
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
    assert!(!processed.fallback.is_deliver());
    assert_eq!(
        processed.archived_final_text.as_deref(),
        Some("the answer"),
        "final text archived as transcript only"
    );

    let outbound = read_outbound(&layout, current_outbound_compat()).unwrap();
    assert_eq!(outbound.len(), 1, "exactly one delivery, no fallback duplicate");
    assert_eq!(outbound[0].kind, "send_message");
}

#[test]
fn replayed_run_does_not_re_deliver() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(tmp.path(), "orchestrator", "sess-1").unwrap();
    let control = LocalControl::new(layout.clone());
    control.init().unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();

    let provider = StubProvider::final_text("idempotent reply");
    let mut ledger = FallbackLedger::new();
    let fragments = shared_fragments(AgentKind::Orchestrator, "cli");

    {
        let mut sink = ContainerSink { container: &container };
        run_once(&provider, &mut sink, &ctx(), &[], &fragments, &mut ledger).unwrap();
    }
    {
        let mut sink = ContainerSink { container: &container };
        let processed =
            run_once(&provider, &mut sink, &ctx(), &[], &fragments, &mut ledger).unwrap();
        assert!(!processed.fallback.is_deliver(), "replay must not re-deliver");
    }

    let outbound = read_outbound(&layout, current_outbound_compat()).unwrap();
    assert_eq!(outbound.len(), 1, "second run wrote nothing");
}
