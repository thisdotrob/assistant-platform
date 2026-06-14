//! Capability-assembly events.
//!
//! claw-capabilities defines the event shape and a sink trait; the host owns
//! durable storage. The registry's own methods do NOT emit — the host
//! constructs and records an event after a successful registration or
//! assembly, mirroring the event pattern used across the platform crates.

use serde::{Deserialize, Serialize};

/// An event describing a change to the in-memory capability registry or the
/// result of assembling a profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CapabilityEvent {
    /// A capability was registered under `capability_id`, owned by `module_id`.
    CapabilityRegistered {
        capability_id: String,
        module_id: String,
    },
    /// A profile was assembled into a concrete product from `capability_ids`.
    ProfileAssembled {
        profile_id: String,
        capability_ids: Vec<String>,
    },
}

/// A sink the host implements to durably record capability events.
pub trait CapabilityEventSink {
    fn record(&mut self, event: CapabilityEvent);
}

/// An in-memory sink that collects events, for tests and local assembly.
#[derive(Debug, Default)]
pub struct VecEventSink {
    pub events: Vec<CapabilityEvent>,
}

impl VecEventSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CapabilityEventSink for VecEventSink {
    fn record(&mut self, event: CapabilityEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_collects_in_order() {
        let mut sink = VecEventSink::new();
        sink.record(CapabilityEvent::CapabilityRegistered {
            capability_id: "memory".into(),
            module_id: "claw-memory".into(),
        });
        sink.record(CapabilityEvent::ProfileAssembled {
            profile_id: "orchestrator".into(),
            capability_ids: vec!["memory".into()],
        });
        assert_eq!(sink.events.len(), 2);
    }

    #[test]
    fn event_tag_is_snake_case() {
        let json = serde_json::to_string(&CapabilityEvent::CapabilityRegistered {
            capability_id: "memory".into(),
            module_id: "claw-memory".into(),
        })
        .unwrap();
        assert!(json.contains("\"event\":\"capability_registered\""));
    }
}
