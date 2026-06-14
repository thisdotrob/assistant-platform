//! Host-side channel registration and health reporting.
//!
//! The host registers each product channel's [`ChannelAdapter`] here, keyed by
//! its channel kind, so routing and operator tooling have one place to resolve a
//! channel and to read aggregate health. The registry holds trait objects, so it
//! never depends on the concrete channel crates — they depend on it.

use serde::{Deserialize, Serialize};

use crate::channel::{ChannelAdapter, ChannelHealth};

/// A channel's kind paired with its current health, for operator/readiness
/// reporting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStatus {
    pub channel_kind: String,
    pub health: ChannelHealth,
}

/// Registering a channel failed.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// A channel of this kind is already registered.
    Duplicate { channel_kind: &'static str },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Duplicate { channel_kind } => {
                write!(f, "channel kind {channel_kind:?} is already registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// The set of channels the host has wired, keyed by channel kind.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: Vec<Box<dyn ChannelAdapter>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel. Rejects a second channel of the same kind so the
    /// host never has two adapters racing for one platform.
    pub fn register(&mut self, channel: Box<dyn ChannelAdapter>) -> Result<(), RegistryError> {
        let kind = channel.channel_kind();
        if self.channels.iter().any(|c| c.channel_kind() == kind) {
            return Err(RegistryError::Duplicate { channel_kind: kind });
        }
        self.channels.push(channel);
        Ok(())
    }

    /// The registered channel kinds, in registration order.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.channels.iter().map(|c| c.channel_kind()).collect()
    }

    pub fn get(&self, channel_kind: &str) -> Option<&dyn ChannelAdapter> {
        self.channels
            .iter()
            .find(|c| c.channel_kind() == channel_kind)
            .map(|c| c.as_ref())
    }

    pub fn get_mut(&mut self, channel_kind: &str) -> Option<&mut (dyn ChannelAdapter + 'static)> {
        self.channels
            .iter_mut()
            .find(|c| c.channel_kind() == channel_kind)
            .map(|c| c.as_mut())
    }

    /// Health for every registered channel, in registration order.
    pub fn health_report(&self) -> Vec<ChannelStatus> {
        self.channels
            .iter()
            .map(|c| ChannelStatus {
                channel_kind: c.channel_kind().to_string(),
                health: c.health(),
            })
            .collect()
    }

    /// Whether every registered channel reports connected.
    pub fn all_connected(&self) -> bool {
        self.channels.iter().all(|c| c.health().is_connected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelError, DeliveryTarget, OutboundContent, SenderIdentity};

    /// A minimal adapter whose connected state and kind are fixed at construction.
    struct StubChannel {
        kind: &'static str,
        connected: bool,
    }

    impl ChannelAdapter for StubChannel {
        fn channel_kind(&self) -> &'static str {
            self.kind
        }
        fn start(&mut self) -> Result<(), ChannelError> {
            self.connected = true;
            Ok(())
        }
        fn stop(&mut self) {
            self.connected = false;
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn resolve_sender(&self, raw_sender: &str) -> SenderIdentity {
            SenderIdentity { sender_id: raw_sender.to_string(), label: None }
        }
        fn deliver(
            &self,
            _target: &DeliveryTarget,
            _content: &OutboundContent,
        ) -> Result<String, ChannelError> {
            Ok("ok".to_string())
        }
        fn health(&self) -> ChannelHealth {
            if self.connected {
                ChannelHealth::Connected
            } else {
                ChannelHealth::Disconnected { detail: "stub".to_string() }
            }
        }
    }

    #[test]
    fn registers_distinct_kinds_and_rejects_duplicates() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(StubChannel { kind: "cli", connected: true })).unwrap();
        reg.register(Box::new(StubChannel { kind: "slack", connected: false })).unwrap();
        assert_eq!(reg.kinds(), vec!["cli", "slack"]);

        let dup = reg.register(Box::new(StubChannel { kind: "slack", connected: true }));
        assert_eq!(dup, Err(RegistryError::Duplicate { channel_kind: "slack" }));
    }

    #[test]
    fn resolves_a_channel_by_kind() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(StubChannel { kind: "cli", connected: true })).unwrap();
        assert!(reg.get("cli").is_some());
        assert!(reg.get("telegram").is_none());
    }

    #[test]
    fn health_report_and_all_connected_reflect_each_channel() {
        let mut reg = ChannelRegistry::new();
        reg.register(Box::new(StubChannel { kind: "cli", connected: true })).unwrap();
        reg.register(Box::new(StubChannel { kind: "slack", connected: false })).unwrap();

        let report = reg.health_report();
        assert_eq!(report[0].channel_kind, "cli");
        assert_eq!(report[0].health, ChannelHealth::Connected);
        assert_eq!(report[1].channel_kind, "slack");
        assert!(!reg.all_connected());

        // Starting the lagging channel brings the fleet fully connected.
        reg.get_mut("slack").unwrap().start().unwrap();
        assert!(reg.all_connected());
    }
}
