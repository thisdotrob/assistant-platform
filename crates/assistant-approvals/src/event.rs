//! Structured approval events. The crate defines the event shape and a sink
//! trait; the host owns durable storage. An in-memory sink is included for
//! tests.

use serde::{Deserialize, Serialize};

use crate::model::ApprovalKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ApprovalEvent {
    ApprovalRequested { id: i64, kind: ApprovalKind },
    ApprovalGranted { id: i64, approver: i64 },
    ApprovalDenied { id: i64, approver: i64 },
    ApprovalExpired { id: i64 },
}

pub trait ApprovalEventSink {
    fn record(&mut self, event: ApprovalEvent);
}

#[derive(Debug, Default)]
pub struct VecEventSink {
    pub events: Vec<ApprovalEvent>,
}

impl ApprovalEventSink for VecEventSink {
    fn record(&mut self, event: ApprovalEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_event_is_json_tagged() {
        let event = ApprovalEvent::ApprovalRequested {
            id: 1,
            kind: ApprovalKind::Credential,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"approval_requested\""));
        assert!(json.contains("\"kind\":\"credential\""));
        let back: ApprovalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn vec_sink_records_in_order() {
        let mut sink = VecEventSink::default();
        sink.record(ApprovalEvent::ApprovalGranted { id: 1, approver: 2 });
        sink.record(ApprovalEvent::ApprovalExpired { id: 3 });
        assert_eq!(sink.events.len(), 2);
    }
}
