//! Structured audit events for delegation and agent-to-agent routing.
//!
//! Every consequential graph action — creating a specialist, starting and
//! finishing a delegation job, routing a message between agents — emits a typed
//! event. The host owns durable storage; this crate only defines the event
//! shape and a sink trait, plus an in-memory sink for tests.

use serde::{Deserialize, Serialize};

use crate::job::JobStatus;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    SpecialistCreated {
        profile_id: String,
        specialist_group: String,
    },
    DelegationStarted {
        job_id: String,
        orchestrator_group: String,
        specialist_group: String,
        profile_id: String,
    },
    DelegationCompleted {
        job_id: String,
        status: JobStatus,
    },
    AgentRouting {
        from_group: String,
        to_group: String,
        kind: String,
    },
}

/// A sink the host implements to persist audit events.
pub trait AuditSink {
    fn record(&mut self, event: AuditEvent);
}

/// An in-memory sink, primarily for tests and dry runs.
#[derive(Clone, Debug, Default)]
pub struct VecAuditSink {
    pub events: Vec<AuditEvent>,
}

impl VecAuditSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AuditSink for VecAuditSink {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_tag_serializes_and_round_trips() {
        let e = AuditEvent::DelegationCompleted {
            job_id: "j1".into(),
            status: JobStatus::Succeeded,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"event\":\"delegation_completed\""));
        assert!(json.contains("\"status\":\"succeeded\""));
        let back: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn vec_sink_records_in_order() {
        let mut sink = VecAuditSink::new();
        sink.record(AuditEvent::SpecialistCreated {
            profile_id: "browser-specialist".into(),
            specialist_group: "browser-1".into(),
        });
        sink.record(AuditEvent::AgentRouting {
            from_group: "orchestrator".into(),
            to_group: "browser-1".into(),
            kind: "specialist_handoff".into(),
        });
        assert_eq!(sink.events.len(), 2);
        assert!(matches!(sink.events[0], AuditEvent::SpecialistCreated { .. }));
        assert!(matches!(sink.events[1], AuditEvent::AgentRouting { .. }));
    }
}
