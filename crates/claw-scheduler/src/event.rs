//! Structured scheduler events.
//!
//! Every consequential scheduling action emits a typed event: a due occurrence
//! becoming runnable, an occurrence finishing, and the pause/resume/cancel
//! lifecycle transitions. As with the rest of the platform, this crate only
//! defines the event shape and a sink trait — the host owns durable storage and
//! provides the sink; an in-memory sink is included for tests.

use serde::{Deserialize, Serialize};

use crate::model::LifecycleTransition;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SchedulerEvent {
    /// A scheduled occurrence is due and was claimed for execution.
    WorkDue {
        scheduled_item_id: String,
        sequence: u64,
        idempotency_key: String,
    },
    /// A scheduled occurrence finished.
    WorkCompleted {
        scheduled_item_id: String,
        sequence: u64,
    },
    Paused {
        scheduled_item_id: String,
    },
    Resumed {
        scheduled_item_id: String,
    },
    Cancelled {
        scheduled_item_id: String,
    },
}

impl SchedulerEvent {
    /// The lifecycle event a transition emits, or `None` for transitions that do
    /// not map to a lifecycle event (a one-off `Complete` is reported as
    /// `WorkCompleted` against its occurrence, not as a lifecycle event).
    pub fn for_transition(transition: LifecycleTransition, scheduled_item_id: &str) -> Option<Self> {
        let id = scheduled_item_id.to_string();
        match transition {
            LifecycleTransition::Pause => Some(SchedulerEvent::Paused { scheduled_item_id: id }),
            LifecycleTransition::Resume => Some(SchedulerEvent::Resumed { scheduled_item_id: id }),
            LifecycleTransition::Cancel => Some(SchedulerEvent::Cancelled { scheduled_item_id: id }),
            LifecycleTransition::Complete => None,
        }
    }
}

/// A sink the host implements to persist scheduler events.
pub trait SchedulerEventSink {
    fn record(&mut self, event: SchedulerEvent);
}

/// An in-memory sink for tests and local harnesses.
#[derive(Debug, Default)]
pub struct VecEventSink {
    pub events: Vec<SchedulerEvent>,
}

impl SchedulerEventSink for VecEventSink {
    fn record(&mut self, event: SchedulerEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_json_tagged() {
        let event = SchedulerEvent::WorkDue {
            scheduled_item_id: "sched_x".into(),
            sequence: 3,
            idempotency_key: "ab12".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"work_due\""));
        let back: SchedulerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn transition_maps_to_lifecycle_events() {
        assert_eq!(
            SchedulerEvent::for_transition(LifecycleTransition::Pause, "s"),
            Some(SchedulerEvent::Paused { scheduled_item_id: "s".into() })
        );
        assert_eq!(
            SchedulerEvent::for_transition(LifecycleTransition::Resume, "s"),
            Some(SchedulerEvent::Resumed { scheduled_item_id: "s".into() })
        );
        assert_eq!(
            SchedulerEvent::for_transition(LifecycleTransition::Cancel, "s"),
            Some(SchedulerEvent::Cancelled { scheduled_item_id: "s".into() })
        );
        // Complete is reported per-occurrence, not as a lifecycle event.
        assert_eq!(SchedulerEvent::for_transition(LifecycleTransition::Complete, "s"), None);
    }

    #[test]
    fn vec_sink_records_in_order() {
        let mut sink = VecEventSink::default();
        sink.record(SchedulerEvent::Paused { scheduled_item_id: "a".into() });
        sink.record(SchedulerEvent::Resumed { scheduled_item_id: "a".into() });
        assert_eq!(sink.events.len(), 2);
        assert!(matches!(sink.events[0], SchedulerEvent::Paused { .. }));
        assert!(matches!(sink.events[1], SchedulerEvent::Resumed { .. }));
    }
}
