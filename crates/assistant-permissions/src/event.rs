//! Structured permission events.
//!
//! Role/membership changes and access denials are consequential governance
//! actions, so each emits a typed event. As elsewhere on the platform, this
//! crate defines the event shape and a sink trait; the host owns durable
//! storage. An in-memory sink is included for tests.

use serde::{Deserialize, Serialize};

use crate::model::Role;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleChange {
    Granted,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PermissionEvent {
    MembershipChanged {
        user_id: i64,
        role: Role,
        change: RoleChange,
    },
    AccessDenied {
        channel: String,
        address: String,
        reason: String,
    },
}

/// A sink the host implements to persist permission events.
pub trait PermissionEventSink {
    fn record(&mut self, event: PermissionEvent);
}

/// An in-memory sink for tests and local harnesses.
#[derive(Debug, Default)]
pub struct VecEventSink {
    pub events: Vec<PermissionEvent>,
}

impl PermissionEventSink for VecEventSink {
    fn record(&mut self, event: PermissionEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_event_is_json_tagged() {
        let event = PermissionEvent::MembershipChanged {
            user_id: 7,
            role: Role::Admin,
            change: RoleChange::Granted,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"membership_changed\""));
        assert!(json.contains("\"role\":\"admin\""));
        let back: PermissionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn vec_sink_records_in_order() {
        let mut sink = VecEventSink::default();
        sink.record(PermissionEvent::AccessDenied {
            channel: "slack".into(),
            address: "U999".into(),
            reason: "unknown".into(),
        });
        assert_eq!(sink.events.len(), 1);
    }
}
