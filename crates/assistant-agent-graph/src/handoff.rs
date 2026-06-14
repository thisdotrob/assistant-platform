//! Host-shaped specialist handoff packets and structured results.
//!
//! Delegation is modeled as explicit jobs, not ordinary chat. The orchestrator
//! hands a specialist a structured packet (goal, selected facts with provenance
//! and retention labels, scope/privacy, attachments, constraints, non-goals,
//! credential policy, budget hints) and receives a structured result (status,
//! summary, answer, evidence, by-reference artifacts, credential/session-state
//! changes, follow-ups). Both serialize to JSON for transport over the session
//! DB protocol.

use serde::{Deserialize, Serialize};

use crate::job::JobBudget;

/// Message kinds used on the session DB protocol for delegation traffic.
pub const HANDOFF_KIND: &str = "specialist_handoff";
pub const RESULT_KIND: &str = "specialist_result";

/// Retention label that travels with every orchestrator-derived fact. Default
/// is the most restrictive (`Ephemeral`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionLabel {
    #[default]
    Ephemeral,
    CiteOnly,
    MayPersist,
}

/// A fact copied from orchestrator context, with provenance and retention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFact {
    pub text: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub retention: RetentionLabel,
}

/// The user/channel/group scope and privacy labels for a handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeLabels {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub privacy: Vec<String>,
}

/// Which credential scopes a specialist may use for this job.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialPolicy {
    #[serde(default)]
    pub allowed_scopes: Vec<String>,
    #[serde(default)]
    pub browser_session_allowed: bool,
}

/// The structured packet the orchestrator sends to a specialist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPacket {
    pub goal: String,
    #[serde(default)]
    pub expected_result_schema: Option<String>,
    #[serde(default)]
    pub facts: Vec<MemoryFact>,
    #[serde(default)]
    pub scope: ScopeLabels,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub credential_policy: CredentialPolicy,
    #[serde(default)]
    pub budget_hint: JobBudget,
}

impl HandoffPacket {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            expected_result_schema: None,
            facts: Vec::new(),
            scope: ScopeLabels::default(),
            attachments: Vec::new(),
            constraints: Vec::new(),
            non_goals: Vec::new(),
            credential_policy: CredentialPolicy::default(),
            budget_hint: JobBudget::default(),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Terminal status reported by a specialist result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialistStatus {
    Completed,
    Partial,
    Failed,
    Cancelled,
    TimedOut,
}

fn default_true() -> bool {
    true
}

/// An artifact produced by a specialist, returned to the orchestrator by
/// reference (never inline payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultArtifact {
    pub artifact_id: String,
    pub kind: String,
    #[serde(default = "default_true")]
    pub by_reference: bool,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// The structured result a specialist returns to the orchestrator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistResult {
    pub status: SpecialistStatus,
    pub summary: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<ResultArtifact>,
    #[serde(default)]
    pub credential_state_changes: Vec<String>,
    #[serde(default)]
    pub follow_ups: Vec<String>,
}

impl SpecialistResult {
    pub fn new(status: SpecialistStatus, summary: impl Into<String>) -> Self {
        Self {
            status,
            summary: summary.into(),
            answer: String::new(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            credential_state_changes: Vec::new(),
            follow_ups: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Why a handoff packet was rejected before delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffError {
    EmptyGoal,
    EmptyFactText,
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffError::EmptyGoal => write!(f, "handoff goal must not be empty"),
            HandoffError::EmptyFactText => write!(f, "handoff facts must not have empty text"),
        }
    }
}

impl std::error::Error for HandoffError {}

/// Validate a handoff packet's required content before it is sent.
pub fn validate_handoff(packet: &HandoffPacket) -> Result<(), HandoffError> {
    if packet.goal.trim().is_empty() {
        return Err(HandoffError::EmptyGoal);
    }
    if packet.facts.iter().any(|f| f.text.trim().is_empty()) {
        return Err(HandoffError::EmptyFactText);
    }
    Ok(())
}

/// Enforce the artifact return policy on a result: every artifact must be
/// returned by reference, and none may exceed `max_bytes` when a size is known.
pub fn artifacts_within_policy(result: &SpecialistResult, max_bytes: u64) -> Result<(), ArtifactPolicyError> {
    for a in &result.artifacts {
        if !a.by_reference {
            return Err(ArtifactPolicyError::NotByReference {
                artifact_id: a.artifact_id.clone(),
            });
        }
        if let Some(size) = a.size_bytes
            && size > max_bytes
        {
            return Err(ArtifactPolicyError::TooLarge {
                artifact_id: a.artifact_id.clone(),
                size,
                max: max_bytes,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactPolicyError {
    NotByReference { artifact_id: String },
    TooLarge { artifact_id: String, size: u64, max: u64 },
}

impl std::fmt::Display for ArtifactPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactPolicyError::NotByReference { artifact_id } => {
                write!(f, "result artifact {artifact_id:?} must be returned by reference")
            }
            ArtifactPolicyError::TooLarge { artifact_id, size, max } => {
                write!(f, "result artifact {artifact_id:?} size {size} exceeds maximum {max}")
            }
        }
    }
}

impl std::error::Error for ArtifactPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rich_packet() -> HandoffPacket {
        let mut p = HandoffPacket::new("Find the latest invoice total on the billing portal");
        p.expected_result_schema = Some("{ total: number }".into());
        p.facts.push(MemoryFact {
            text: "Account email is owner@example.com".into(),
            source: Some("memory:fact-42".into()),
            retention: RetentionLabel::CiteOnly,
        });
        p.scope = ScopeLabels {
            user: Some("u-1".into()),
            channel: Some("dm".into()),
            group: Some("g-personal".into()),
            privacy: vec!["personal".into()],
        };
        p.attachments = vec!["att-1".into()];
        p.constraints = vec!["do not place orders".into()];
        p.non_goals = vec!["do not change account settings".into()];
        p.credential_policy = CredentialPolicy {
            allowed_scopes: vec!["billing.read".into()],
            browser_session_allowed: true,
        };
        p
    }

    #[test]
    fn handoff_carries_required_context_and_round_trips() {
        let p = rich_packet();
        let json = p.to_json().unwrap();
        let back = HandoffPacket::from_json(&json).unwrap();
        assert_eq!(p, back);
        // Provenance, scope, constraints, credential policy, non-goals present.
        assert_eq!(back.facts[0].source.as_deref(), Some("memory:fact-42"));
        assert_eq!(back.facts[0].retention, RetentionLabel::CiteOnly);
        assert_eq!(back.scope.user.as_deref(), Some("u-1"));
        assert!(!back.constraints.is_empty());
        assert!(!back.non_goals.is_empty());
        assert_eq!(back.credential_policy.allowed_scopes, vec!["billing.read".to_string()]);
    }

    #[test]
    fn fact_retention_defaults_to_ephemeral() {
        // A fact deserialized without an explicit retention is ephemeral.
        let f: MemoryFact = serde_json::from_str(r#"{"text":"x"}"#).unwrap();
        assert_eq!(f.retention, RetentionLabel::Ephemeral);
    }

    #[test]
    fn empty_goal_and_empty_fact_are_rejected() {
        assert_eq!(validate_handoff(&HandoffPacket::new("  ")), Err(HandoffError::EmptyGoal));
        let mut p = HandoffPacket::new("ok");
        p.facts.push(MemoryFact { text: " ".into(), source: None, retention: RetentionLabel::Ephemeral });
        assert_eq!(validate_handoff(&p), Err(HandoffError::EmptyFactText));
        assert!(validate_handoff(&rich_packet()).is_ok());
    }

    #[test]
    fn result_round_trips_and_defaults_artifacts_by_reference() {
        let mut r = SpecialistResult::new(SpecialistStatus::Completed, "done");
        r.answer = "total is 42".into();
        r.artifacts.push(ResultArtifact {
            artifact_id: "shot-1".into(),
            kind: "screenshot".into(),
            by_reference: true,
            size_bytes: Some(1024),
        });
        let json = r.to_json().unwrap();
        let back = SpecialistResult::from_json(&json).unwrap();
        assert_eq!(r, back);
        // by_reference defaults true when omitted.
        let a: ResultArtifact =
            serde_json::from_str(r#"{"artifact_id":"a","kind":"trace"}"#).unwrap();
        assert!(a.by_reference);
    }

    #[test]
    fn artifact_policy_rejects_inline_and_oversized() {
        let mut r = SpecialistResult::new(SpecialistStatus::Completed, "done");
        r.artifacts.push(ResultArtifact { artifact_id: "a".into(), kind: "download".into(), by_reference: false, size_bytes: None });
        assert!(matches!(
            artifacts_within_policy(&r, 1000),
            Err(ArtifactPolicyError::NotByReference { .. })
        ));

        let mut r2 = SpecialistResult::new(SpecialistStatus::Completed, "done");
        r2.artifacts.push(ResultArtifact { artifact_id: "big".into(), kind: "trace".into(), by_reference: true, size_bytes: Some(5000) });
        assert!(matches!(
            artifacts_within_policy(&r2, 1000),
            Err(ArtifactPolicyError::TooLarge { .. })
        ));
        assert!(artifacts_within_policy(&r2, 10_000).is_ok());
    }
}
