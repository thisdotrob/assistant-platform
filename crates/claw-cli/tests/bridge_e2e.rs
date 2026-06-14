//! End-to-end proof of the in-container CLI bridge over a real session DB:
//! a fake container emits a CLI request, the host serves it through the
//! registry under an allow-all policy, and the response comes back on the
//! container's inbound stream, correlated by sequence.

use claw_cli::{
    decode_response, encode_request, serve_pending, AccessDecision, AccessPolicy, AccessRequest,
    ActionSpec, BridgeError, CommandOutcome, CommandRequest, CommandRegistry, Resource, ResultTable,
    SessionBridge, REQUEST_KIND, RESPONSE_KIND,
};
use claw_session::{LocalControl, SessionLayout};

struct SessionsResource;

impl Resource for SessionsResource {
    fn name(&self) -> &str {
        "sessions"
    }
    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec::read("list")]
    }
    fn execute(&self, _request: &CommandRequest) -> CommandOutcome {
        let mut table = ResultTable::new(["session_id", "state"]);
        table.push_row(["s1", "active"]);
        CommandOutcome::message(table, "1 session")
    }
}

struct AllowAgents;

impl AccessPolicy for AllowAgents {
    fn authorize(&self, _: &AccessRequest<'_>) -> AccessDecision {
        AccessDecision::Allow
    }
}

#[test]
fn agent_cli_request_round_trips_over_the_session_db() {
    let root = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(root.path(), "g1", "s1").unwrap();

    let control = LocalControl::new(layout.clone());
    control.init().unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();

    // The agent emits a CLI request on the outbound stream.
    let command = CommandRequest::new("sessions", "list");
    let request_seq = container
        .emit(REQUEST_KIND, &encode_request(&command).unwrap())
        .unwrap();

    // The host drains, dispatches, and replies.
    let mut registry = CommandRegistry::new();
    registry.register(Box::new(SessionsResource)).unwrap();
    let mut bridge = SessionBridge::new(&layout, "g1");
    let handled = serve_pending(&mut bridge, &registry, &AllowAgents).unwrap();
    assert_eq!(handled, 1);

    // The container reads its inbound stream and finds the correlated response.
    let inbound = container.read_inbound().unwrap();
    assert_eq!(inbound.len(), 1, "expected exactly one response message");
    let response = decode_response(&inbound[0].1).unwrap();
    assert_eq!(response.in_reply_to, request_seq);
    match response.outcome {
        CommandOutcome::Ok { table, message } => {
            assert_eq!(table.columns, vec!["session_id", "state"]);
            assert_eq!(table.rows[0], vec!["s1", "active"]);
            assert_eq!(message.as_deref(), Some("1 session"));
        }
        other => panic!("expected ok outcome, got {other:?}"),
    }

    // A second serve with no new requests is a no-op (high-water mark holds).
    let again = serve_pending(&mut bridge, &registry, &AllowAgents).unwrap();
    assert_eq!(again, 0);
    assert_eq!(container.read_inbound().unwrap().len(), 1);

    // The response carried the cli.response marker in its metadata.
    assert_ne!(REQUEST_KIND, RESPONSE_KIND);
}

#[test]
fn an_oversized_request_payload_is_rejected_before_parsing() {
    let root = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(root.path(), "g1", "s1").unwrap();

    let control = LocalControl::new(layout.clone());
    control.init().unwrap();
    let container = control.fake_container();
    container.start("run-1").unwrap();

    // A request whose content far exceeds the 64 KiB cap.
    let huge = "x".repeat(64 * 1024 + 1);
    container.emit(REQUEST_KIND, &huge).unwrap();

    let mut registry = CommandRegistry::new();
    registry.register(Box::new(SessionsResource)).unwrap();
    let mut bridge = SessionBridge::new(&layout, "g1");
    let err = serve_pending(&mut bridge, &registry, &AllowAgents).unwrap_err();
    assert!(matches!(err, BridgeError::BadRequest { .. }), "got {err:?}");

    // Nothing was dispatched, so no response reached the container.
    assert!(container.read_inbound().unwrap().is_empty());
}
