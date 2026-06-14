//! Agent-to-agent routing over the per-session DB protocol.
//!
//! Delegation traffic rides the same `claw-session` inbound/outbound DBs as
//! channel traffic, but between two agent groups: the orchestrator enqueues a
//! handoff packet into the specialist's session, the specialist emits a
//! structured result on its outbound DB, and the orchestrator collects it and
//! enqueues the result back into its own session. Two policy gates guard the
//! path: an explicit A2A allow-list (no implicit edges) and the rule that
//! specialists may never own external channel destinations.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use claw_session::{
    current_outbound_compat, enqueue_inbound, read_outbound, safe_attachment_path, InboundMessage,
    SessionLayout,
};

use crate::handoff::{HandoffPacket, SpecialistResult, HANDOFF_KIND, RESULT_KIND};
use crate::registry::RegisteredProfile;

/// Where a specialist should send its result back to: the orchestrator session
/// and the inbound sequence that prompted the delegation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnPath {
    pub orchestrator_group: String,
    pub session_id: String,
    pub inbound_seq: i64,
}

/// The metadata envelope carried alongside delegation messages so the receiver
/// can tell handoffs/results from ordinary chat and find the return path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DelegationEnvelope {
    kind: String,
    #[serde(default)]
    return_path: Option<ReturnPath>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingError {
    AclDenied { from: String, to: String },
    CyclicAcl { node: String },
    ExternalDestinationForbidden { profile_id: String },
    Session(String),
    Encode(String),
    NoResult,
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingError::AclDenied { from, to } => {
                write!(f, "agent-to-agent routing from {from:?} to {to:?} is not allowed")
            }
            RoutingError::CyclicAcl { node } => write!(
                f,
                "agent-to-agent routing allow-list has a cycle reachable from {node:?}"
            ),
            RoutingError::ExternalDestinationForbidden { profile_id } => write!(
                f,
                "profile {profile_id:?} may not own external channel destinations"
            ),
            RoutingError::Session(e) => write!(f, "session error: {e}"),
            RoutingError::Encode(e) => write!(f, "delegation encode error: {e}"),
            RoutingError::NoResult => write!(f, "no specialist result present on outbound"),
        }
    }
}

impl std::error::Error for RoutingError {}

impl From<claw_session::SessionError> for RoutingError {
    fn from(value: claw_session::SessionError) -> Self {
        RoutingError::Session(value.to_string())
    }
}

impl From<serde_json::Error> for RoutingError {
    fn from(value: serde_json::Error) -> Self {
        RoutingError::Encode(value.to_string())
    }
}

/// An explicit allow-list of directed agent-to-agent routing edges. There are no
/// implicit edges: an orchestrator may only hand off to a specialist whose edge
/// has been registered.
#[derive(Clone, Debug, Default)]
pub struct A2aAcl {
    edges: HashSet<(String, String)>,
}

impl A2aAcl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow(&mut self, from: impl Into<String>, to: impl Into<String>) -> &mut Self {
        self.edges.insert((from.into(), to.into()));
        self
    }

    pub fn is_allowed(&self, from: &str, to: &str) -> bool {
        self.edges.contains(&(from.to_string(), to.to_string()))
    }

    /// True when the allow-list contains no directed cycle. The delegation graph
    /// must be acyclic so a chain of handoffs always terminates rather than
    /// looping an orchestrator and a specialist back into each other.
    pub fn is_acyclic(&self) -> bool {
        self.first_cycle_node().is_none()
    }

    /// The first node from which a directed cycle is reachable, if any. Useful
    /// for surfacing *which* edge made the graph cyclic.
    pub fn first_cycle_node(&self) -> Option<String> {
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut nodes: BTreeSet<&str> = BTreeSet::new();
        for (from, to) in &self.edges {
            adjacency.entry(from).or_default().push(to);
            nodes.insert(from);
            nodes.insert(to);
        }

        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }

        fn visit<'a>(
            node: &'a str,
            adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
            marks: &mut BTreeMap<&'a str, Mark>,
        ) -> bool {
            marks.insert(node, Mark::Visiting);
            if let Some(neighbours) = adjacency.get(node) {
                for &next in neighbours {
                    match marks.get(next) {
                        Some(Mark::Visiting) => return true,
                        Some(Mark::Done) => {}
                        None => {
                            if visit(next, adjacency, marks) {
                                return true;
                            }
                        }
                    }
                }
            }
            marks.insert(node, Mark::Done);
            false
        }

        let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
        for node in nodes {
            if !marks.contains_key(node) && visit(node, &adjacency, &mut marks) {
                return Some(node.to_string());
            }
        }
        None
    }
}

/// Gate a handoff on the A2A allow-list. Refuses if the edge is not registered,
/// and refuses if the allow-list as a whole contains a cycle — the contract
/// requires that agent-to-agent routing always terminates.
pub fn authorize_a2a(acl: &A2aAcl, from: &str, to: &str) -> Result<(), RoutingError> {
    if let Some(node) = acl.first_cycle_node() {
        return Err(RoutingError::CyclicAcl { node });
    }
    if acl.is_allowed(from, to) {
        Ok(())
    } else {
        Err(RoutingError::AclDenied {
            from: from.to_string(),
            to: to.to_string(),
        })
    }
}

/// Refuse to treat a profile as owning external destinations. Specialists are
/// internal-only; only orchestrator-class profiles may face channels.
pub fn authorize_external_destination(profile: &RegisteredProfile) -> Result<(), RoutingError> {
    if profile.allows_external_destinations {
        Ok(())
    } else {
        Err(RoutingError::ExternalDestinationForbidden {
            profile_id: profile.profile_id.clone(),
        })
    }
}

/// Enqueue a handoff packet into the specialist's session. The packet rides the
/// message content as JSON; the metadata envelope marks it a handoff and carries
/// the return path. Returns the assigned inbound sequence.
pub fn deliver_handoff(
    specialist_layout: &SessionLayout,
    sender: &str,
    handoff: &HandoffPacket,
    return_path: &ReturnPath,
) -> Result<i64, RoutingError> {
    let envelope = DelegationEnvelope {
        kind: HANDOFF_KIND.to_string(),
        return_path: Some(return_path.clone()),
    };
    let message = InboundMessage {
        sender: sender.to_string(),
        content: handoff.to_json()?,
        metadata: Some(serde_json::to_string(&envelope)?),
    };
    Ok(enqueue_inbound(specialist_layout, &message)?)
}

/// Read the specialist's outbound DB and parse the last `specialist_result`
/// message it emitted. Errors with `NoResult` if none is present.
pub fn collect_result(specialist_layout: &SessionLayout) -> Result<SpecialistResult, RoutingError> {
    let outbound = read_outbound(specialist_layout, current_outbound_compat())?;
    let last = outbound
        .iter()
        .rev()
        .find(|m| m.kind == RESULT_KIND)
        .ok_or(RoutingError::NoResult)?;
    Ok(SpecialistResult::from_json(&last.content)?)
}

/// Enqueue a collected specialist result back into the orchestrator's session.
pub fn deliver_result(
    orchestrator_layout: &SessionLayout,
    sender: &str,
    result: &SpecialistResult,
) -> Result<i64, RoutingError> {
    let envelope = DelegationEnvelope {
        kind: RESULT_KIND.to_string(),
        return_path: None,
    };
    let message = InboundMessage {
        sender: sender.to_string(),
        content: result.to_json()?,
        metadata: Some(serde_json::to_string(&envelope)?),
    };
    Ok(enqueue_inbound(orchestrator_layout, &message)?)
}

/// Copy an attachment from one session message directory to another, validating
/// the file name against both bases so it can never escape either session.
pub fn forward_attachment(
    from_msg_dir: &Path,
    to_msg_dir: &Path,
    file_name: &str,
) -> Result<PathBuf, RoutingError> {
    let src = safe_attachment_path(from_msg_dir, file_name)?;
    let dst = safe_attachment_path(to_msg_dir, file_name)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RoutingError::Session(e.to_string()))?;
    }
    std::fs::copy(&src, &dst).map_err(|e| RoutingError::Session(e.to_string()))?;
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ProfileLimits, RegisteredProfile};

    #[test]
    fn acl_has_no_implicit_edges() {
        let mut acl = A2aAcl::new();
        acl.allow("orchestrator", "browser-1");
        assert!(authorize_a2a(&acl, "orchestrator", "browser-1").is_ok());
        assert_eq!(
            authorize_a2a(&acl, "orchestrator", "other"),
            Err(RoutingError::AclDenied {
                from: "orchestrator".into(),
                to: "other".into()
            })
        );
        // Edges are directed.
        assert!(authorize_a2a(&acl, "browser-1", "orchestrator").is_err());
    }

    #[test]
    fn acl_detects_cycles() {
        let mut acyclic = A2aAcl::new();
        acyclic.allow("orchestrator", "browser-1");
        acyclic.allow("orchestrator", "researcher-1");
        acyclic.allow("researcher-1", "browser-1");
        assert!(acyclic.is_acyclic());
        assert_eq!(acyclic.first_cycle_node(), None);

        let mut cyclic = A2aAcl::new();
        cyclic.allow("orchestrator", "browser-1");
        cyclic.allow("browser-1", "orchestrator");
        assert!(!cyclic.is_acyclic());
        assert!(cyclic.first_cycle_node().is_some());
        // A cyclic allow-list is refused even for an edge that exists in it.
        assert!(matches!(
            authorize_a2a(&cyclic, "orchestrator", "browser-1"),
            Err(RoutingError::CyclicAcl { .. })
        ));
    }

    #[test]
    fn specialists_may_not_own_external_destinations() {
        let specialist = RegisteredProfile::specialist("browser-specialist", "0.1.0", ProfileLimits::new(1, 1));
        assert_eq!(
            authorize_external_destination(&specialist),
            Err(RoutingError::ExternalDestinationForbidden {
                profile_id: "browser-specialist".into()
            })
        );

        let orchestrator = RegisteredProfile {
            profile_id: "personal-orchestrator".into(),
            profile_version: "0.1.0".into(),
            kind: "orchestrator".into(),
            allows_external_destinations: true,
            limits: ProfileLimits::new(1, 1),
        };
        assert!(authorize_external_destination(&orchestrator).is_ok());
    }

    #[test]
    fn return_path_round_trips_through_envelope() {
        let rp = ReturnPath {
            orchestrator_group: "orchestrator".into(),
            session_id: "sess-1".into(),
            inbound_seq: 4,
        };
        let env = DelegationEnvelope {
            kind: HANDOFF_KIND.to_string(),
            return_path: Some(rp.clone()),
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: DelegationEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, HANDOFF_KIND);
        assert_eq!(back.return_path, Some(rp));
    }
}
