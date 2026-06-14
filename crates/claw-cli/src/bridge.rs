//! The in-container CLI request/response bridge.
//!
//! An agent inside a container submits a CLI command by emitting an outbound
//! message of kind [`REQUEST_KIND`] whose content is a JSON [`CommandRequest`].
//! The host drains those, dispatches each under the access policy as an agent
//! caller, and enqueues the [`BridgeResponse`] back on the inbound stream tagged
//! [`RESPONSE_KIND`] and correlated by the request's sequence number. This crate
//! owns no tables: it rides claw-session's existing message streams.

use serde::{Deserialize, Serialize};

use claw_session::{
    current_outbound_compat, enqueue_inbound, mark_delivered, read_outbound, InboundMessage,
    SessionError, SessionLayout,
};

use crate::access::{dispatch, AccessPolicy};
use crate::command::{Caller, CommandOutcome, CommandRequest};
use crate::registry::CommandRegistry;

/// Outbound message kind an agent uses to submit a CLI command.
pub const REQUEST_KIND: &str = "cli.request";
/// Inbound message kind the host uses to return a CLI result.
pub const RESPONSE_KIND: &str = "cli.response";
/// The sender stamped on bridge responses in the inbound stream.
const BRIDGE_SENDER: &str = "claw-cli";

/// Largest request payload we will attempt to decode. A CLI command is small;
/// anything larger is rejected before parsing so an agent cannot force the host
/// to allocate against an oversized JSON document.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Most requests we will drain in one serving pass. Bounds the work (and the
/// response writes) a single agent can drive per serve; the rest wait for the
/// next pass.
const MAX_BATCH: usize = 256;

/// A CLI request as the host sees it: the parsed command, the group the agent
/// is scoped to, and the outbound sequence to correlate the response with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeRequest {
    pub request_seq: i64,
    pub agent_group_id: String,
    pub command: CommandRequest,
}

/// The result returned to the agent, correlated to the request it answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub in_reply_to: i64,
    pub outcome: CommandOutcome,
}

/// The metadata stamped on a response's inbound row so the container can tell a
/// CLI response apart from an ordinary message.
#[derive(Serialize, Deserialize)]
struct ResponseMeta {
    kind: String,
    in_reply_to: i64,
}

/// Encode a command for submission over the bridge (container side).
pub fn encode_request(command: &CommandRequest) -> serde_json::Result<String> {
    serde_json::to_string(command)
}

/// Decode a bridge response delivered to the container (container side).
pub fn decode_response(content: &str) -> serde_json::Result<BridgeResponse> {
    serde_json::from_str(content)
}

/// A bridge operation failed.
#[derive(Debug)]
pub enum BridgeError {
    /// A session-DB read or write failed.
    Session(SessionError),
    /// A request payload could not be decoded; `seq` names the offending message.
    BadRequest { seq: i64, detail: String },
    /// A response payload could not be encoded.
    BadResponse { detail: String },
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Session(e) => write!(f, "session error: {e}"),
            BridgeError::BadRequest { seq, detail } => {
                write!(f, "cli request at seq {seq} is malformed: {detail}")
            }
            BridgeError::BadResponse { detail } => {
                write!(f, "cli response could not be encoded: {detail}")
            }
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BridgeError::Session(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SessionError> for BridgeError {
    fn from(e: SessionError) -> Self {
        BridgeError::Session(e)
    }
}

/// The transport the bridge round-trips over. The host wires the
/// session-backed [`SessionBridge`]; tests use an in-memory fake.
pub trait BridgeTransport {
    /// CLI requests an agent has submitted and the host has not yet answered,
    /// oldest first.
    fn pending_requests(&self) -> Result<Vec<BridgeRequest>, BridgeError>;
    /// Deliver a response back to the agent and mark its request handled.
    fn send_response(&mut self, response: &BridgeResponse) -> Result<(), BridgeError>;
}

/// Drain every pending CLI request, dispatch it under the access policy as an
/// agent caller, and send each response back. Returns the number handled.
pub fn serve_pending(
    transport: &mut dyn BridgeTransport,
    registry: &CommandRegistry,
    policy: &dyn AccessPolicy,
) -> Result<usize, BridgeError> {
    let pending = transport.pending_requests()?;
    let count = pending.len();
    for req in pending {
        let caller = Caller::Agent { agent_group_id: req.agent_group_id.clone() };
        let outcome = dispatch(registry, policy, &caller, &req.command);
        transport.send_response(&BridgeResponse { in_reply_to: req.request_seq, outcome })?;
    }
    Ok(count)
}

/// A [`BridgeTransport`] backed by a session's DBs. Reads CLI requests from the
/// outbound stream and writes responses to the inbound stream. The in-memory
/// high-water mark de-duplicates within a serving instance; across host
/// restarts a request could be re-served, so resources behind writes should be
/// idempotent (writes are also approval-gated by policy).
pub struct SessionBridge<'a> {
    layout: &'a SessionLayout,
    agent_group_id: String,
    high_water: i64,
}

impl<'a> SessionBridge<'a> {
    pub fn new(layout: &'a SessionLayout, agent_group_id: impl Into<String>) -> Self {
        Self { layout, agent_group_id: agent_group_id.into(), high_water: 0 }
    }
}

impl BridgeTransport for SessionBridge<'_> {
    fn pending_requests(&self) -> Result<Vec<BridgeRequest>, BridgeError> {
        let messages = read_outbound(self.layout, current_outbound_compat())?;
        let mut out = Vec::new();
        for m in messages {
            if m.kind != REQUEST_KIND || m.seq <= self.high_water {
                continue;
            }
            if out.len() >= MAX_BATCH {
                break;
            }
            if m.content.len() > MAX_REQUEST_BYTES {
                return Err(BridgeError::BadRequest {
                    seq: m.seq,
                    detail: format!(
                        "payload of {} bytes exceeds the {MAX_REQUEST_BYTES}-byte limit",
                        m.content.len()
                    ),
                });
            }
            let command: CommandRequest = serde_json::from_str(&m.content)
                .map_err(|e| BridgeError::BadRequest { seq: m.seq, detail: e.to_string() })?;
            out.push(BridgeRequest {
                request_seq: m.seq,
                agent_group_id: self.agent_group_id.clone(),
                command,
            });
        }
        Ok(out)
    }

    fn send_response(&mut self, response: &BridgeResponse) -> Result<(), BridgeError> {
        let content = serde_json::to_string(response)
            .map_err(|e| BridgeError::BadResponse { detail: e.to_string() })?;
        let metadata = serde_json::to_string(&ResponseMeta {
            kind: RESPONSE_KIND.to_string(),
            in_reply_to: response.in_reply_to,
        })
        .ok();
        enqueue_inbound(
            self.layout,
            &InboundMessage { sender: BRIDGE_SENDER.to_string(), content, metadata },
        )?;
        mark_delivered(self.layout, response.in_reply_to)?;
        if response.in_reply_to > self.high_water {
            self.high_water = response.in_reply_to;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::OperatorOnly;
    use crate::command::ResultTable;
    use crate::registry::{ActionSpec, Resource};

    struct EchoResource;

    impl Resource for EchoResource {
        fn name(&self) -> &str {
            "echo"
        }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec::read("say")]
        }
        fn execute(&self, request: &CommandRequest) -> CommandOutcome {
            let mut table = ResultTable::new(["arg"]);
            table.push_row([request.args.first().cloned().unwrap_or_default()]);
            CommandOutcome::table(table)
        }
    }

    /// An in-memory transport: it holds queued requests and records responses,
    /// so the round-trip can be exercised without any DB.
    #[derive(Default)]
    struct FakeTransport {
        requests: Vec<BridgeRequest>,
        responses: Vec<BridgeResponse>,
    }

    impl BridgeTransport for FakeTransport {
        fn pending_requests(&self) -> Result<Vec<BridgeRequest>, BridgeError> {
            Ok(self.requests.clone())
        }
        fn send_response(&mut self, response: &BridgeResponse) -> Result<(), BridgeError> {
            self.responses.push(response.clone());
            Ok(())
        }
    }

    fn registry() -> CommandRegistry {
        let mut reg = CommandRegistry::new();
        reg.register(Box::new(EchoResource)).unwrap();
        reg
    }

    #[test]
    fn request_content_round_trips_through_the_wire_helpers() {
        let cmd = CommandRequest::new("echo", "say").with_args(["hi"]);
        let encoded = encode_request(&cmd).unwrap();
        let response = BridgeResponse {
            in_reply_to: 3,
            outcome: CommandOutcome::table(ResultTable::new(["arg"])),
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert_eq!(decode_response(&response_json).unwrap(), response);
        // The encoded request decodes back to the same command.
        let back: CommandRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn serve_pending_dispatches_each_request_and_correlates_responses() {
        let mut transport = FakeTransport {
            requests: vec![
                BridgeRequest {
                    request_seq: 1,
                    agent_group_id: "g1".to_string(),
                    command: CommandRequest::new("echo", "say").with_args(["first"]),
                },
                BridgeRequest {
                    request_seq: 3,
                    agent_group_id: "g1".to_string(),
                    command: CommandRequest::new("echo", "say").with_args(["second"]),
                },
            ],
            responses: Vec::new(),
        };

        // OperatorOnly denies agents, so each response is an access-denied error
        // — but the round-trip and correlation still hold.
        let handled = serve_pending(&mut transport, &registry(), &OperatorOnly).unwrap();
        assert_eq!(handled, 2);
        assert_eq!(transport.responses.len(), 2);
        assert_eq!(transport.responses[0].in_reply_to, 1);
        assert_eq!(transport.responses[1].in_reply_to, 3);
        assert!(matches!(transport.responses[0].outcome, CommandOutcome::Error { .. }));
    }

    #[test]
    fn serve_pending_runs_the_command_when_policy_allows_the_agent() {
        struct AllowAll;
        impl AccessPolicy for AllowAll {
            fn authorize(&self, _: &crate::access::AccessRequest<'_>) -> crate::access::AccessDecision {
                crate::access::AccessDecision::Allow
            }
        }

        let mut transport = FakeTransport {
            requests: vec![BridgeRequest {
                request_seq: 5,
                agent_group_id: "g1".to_string(),
                command: CommandRequest::new("echo", "say").with_args(["pong"]),
            }],
            responses: Vec::new(),
        };
        serve_pending(&mut transport, &registry(), &AllowAll).unwrap();
        match &transport.responses[0].outcome {
            CommandOutcome::Ok { table, .. } => assert_eq!(table.rows[0][0], "pong"),
            other => panic!("expected ok, got {other:?}"),
        }
    }
}
