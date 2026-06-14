//! The shared channel boundary: the normalized inbound routing event every
//! channel produces, and the host-facing `ChannelAdapter` trait every channel
//! implements.
//!
//! assistant-router owns these because it is the one consumer that evaluates routing
//! and engagement against normalized events, and every channel crate already
//! depends on assistant-router. The types are self-contained (no assistant-agent-protocol
//! dependency) — channels translate their platform payloads into a
//! `RoutingEvent` on the way in and translate router/session outbound content
//! into `OutboundContent` on the way out.

use serde::{Deserialize, Serialize};

/// The lifecycle state an inbound event represents. A plain message is `New`;
/// edits, deletes, and reactions are normalized so the router can audit them
/// without re-running task intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MessageState {
    New,
    Edited,
    /// A tombstone: the source message was deleted. Kept for audit visibility.
    Deleted,
    Reaction {
        emoji: String,
    },
}

/// A stable sender identity resolved from a platform-specific raw sender.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderIdentity {
    pub sender_id: String,
    pub label: Option<String>,
}

/// A normalized inbound routing event — the single shape every channel produces
/// regardless of platform. Carries the fields the router needs to route, dedupe,
/// and audit a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingEvent {
    /// Which channel produced this, e.g. "slack" / "telegram" / "cli".
    pub channel_kind: String,
    /// Stable channel/chat id.
    pub chat_id: String,
    /// Stable sender id and optional display label.
    pub sender_id: String,
    pub sender_label: Option<String>,
    /// Platform message id.
    pub platform_message_id: String,
    /// Thread/root id when the platform supports threads.
    pub thread_root_id: Option<String>,
    /// Reply target message id where applicable.
    pub reply_target_id: Option<String>,
    /// Normalized engagement-state key — the conversation scope engagement is
    /// tracked against (top-level vs. a specific thread are distinct).
    pub engagement_key: String,
    /// Permalink or source reference where the platform supports it.
    pub permalink: Option<String>,
    /// Edit/delete/reaction state.
    pub state: MessageState,
    /// Platform-confirmed mention of the bot.
    pub is_mention: bool,
    /// The event is a direct message.
    pub is_direct: bool,
    /// The event originates from the bot's own identity (ignored unless an
    /// explicit host-generated echo).
    pub is_self_author: bool,
    /// Delivery dedupe key — retries of the same platform message collapse here.
    pub dedupe_key: String,
    /// Normalized text body.
    pub text: String,
    /// Channel-specific raw reference, for debugging only.
    pub raw_ref: Option<String>,
}

/// Build the delivery dedupe key for a platform message. Retries of the same
/// message (e.g. Slack retry headers) must produce the same key, so it is a
/// pure function of the channel, chat, and platform message id — never of
/// receipt time or a retry counter.
pub fn dedupe_key(channel_kind: &str, chat_id: &str, platform_message_id: &str) -> String {
    format!("{channel_kind}:{chat_id}:{platform_message_id}")
}

/// Build the engagement-state key for a conversation scope. A thread reply is
/// scoped to its root so it stays distinct from the channel's top level.
pub fn engagement_key(channel_kind: &str, chat_id: &str, thread_root_id: Option<&str>) -> String {
    match thread_root_id {
        Some(root) => format!("{channel_kind}:{chat_id}:{root}"),
        None => format!("{channel_kind}:{chat_id}"),
    }
}

/// Where an outbound message is delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    pub chat_id: String,
    /// When set, deliver into this thread.
    pub thread_root_id: Option<String>,
}

/// Outbound content to deliver. Rich kinds carry an explicit `fallback` so a
/// channel that cannot render a card/question never drops the message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboundContent {
    Text {
        body: String,
    },
    Card {
        title: String,
        body: String,
        fallback: String,
    },
    Question {
        prompt: String,
        options: Vec<String>,
        fallback: String,
    },
}

impl OutboundContent {
    /// Plain-text rendering, used by channels that cannot render the rich form.
    pub fn fallback_text(&self) -> String {
        match self {
            OutboundContent::Text { body } => body.clone(),
            OutboundContent::Card { fallback, .. } => fallback.clone(),
            OutboundContent::Question { fallback, .. } => fallback.clone(),
        }
    }
}

/// A file to deliver alongside or instead of text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub file_name: String,
    pub caption: Option<String>,
}

/// Channel-specific health, surfaced to operators and readiness reporting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChannelHealth {
    Connected,
    Degraded { detail: String },
    Disconnected { detail: String },
}

impl ChannelHealth {
    pub fn is_connected(&self) -> bool {
        matches!(self, ChannelHealth::Connected)
    }
}

/// A discrete channel setup step, for setup progression reporting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStep {
    pub id: String,
    pub description: String,
    pub completed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// An operation was attempted while the channel was not connected.
    NotConnected,
    /// The channel does not support this operation.
    Unsupported { op: &'static str },
    /// Bringing the channel up (lifecycle start / connect / auth) failed.
    Setup { detail: String },
    /// Delivery failed for a channel-specific reason.
    Delivery { detail: String },
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::NotConnected => write!(f, "channel is not connected"),
            ChannelError::Unsupported { op } => write!(f, "channel does not support {op}"),
            ChannelError::Setup { detail } => write!(f, "channel setup failed: {detail}"),
            ChannelError::Delivery { detail } => write!(f, "delivery failed: {detail}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// The host-facing boundary every channel module implements. Inbound
/// normalization is platform-specific (each channel maps its own payloads into
/// a [`RoutingEvent`]); this trait is the uniform surface the host drives for
/// lifecycle, sender resolution, delivery, and health. Optional operations
/// default to [`ChannelError::Unsupported`] so a channel implements only what
/// its platform offers.
///
/// The trait is object-safe so the host can hold `Box<dyn ChannelAdapter>`.
pub trait ChannelAdapter {
    /// Stable channel kind, e.g. "slack".
    fn channel_kind(&self) -> &'static str;

    fn start(&mut self) -> Result<(), ChannelError>;
    fn stop(&mut self);
    fn is_connected(&self) -> bool;

    /// Resolve a platform-specific raw sender into a stable identity.
    fn resolve_sender(&self, raw_sender: &str) -> SenderIdentity;

    /// Deliver outbound content, returning the platform message id.
    fn deliver(
        &self,
        target: &DeliveryTarget,
        content: &OutboundContent,
    ) -> Result<String, ChannelError>;

    fn send_file(
        &self,
        _target: &DeliveryTarget,
        _file: &FileRef,
    ) -> Result<String, ChannelError> {
        Err(ChannelError::Unsupported { op: "send_file" })
    }

    fn edit(
        &self,
        _target: &DeliveryTarget,
        _message_id: &str,
        _content: &OutboundContent,
    ) -> Result<(), ChannelError> {
        Err(ChannelError::Unsupported { op: "edit" })
    }

    fn react(
        &self,
        _target: &DeliveryTarget,
        _message_id: &str,
        _emoji: &str,
    ) -> Result<(), ChannelError> {
        Err(ChannelError::Unsupported { op: "react" })
    }

    fn health(&self) -> ChannelHealth;

    fn setup_steps(&self) -> Vec<SetupStep> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A test double exercising the trait offline. Records delivered content and
    /// only supports text delivery, so the default Unsupported behavior is
    /// observable.
    struct FakeChannel {
        connected: bool,
        delivered: RefCell<Vec<(DeliveryTarget, OutboundContent)>>,
    }

    impl FakeChannel {
        fn new() -> Self {
            Self { connected: false, delivered: RefCell::new(Vec::new()) }
        }
    }

    impl ChannelAdapter for FakeChannel {
        fn channel_kind(&self) -> &'static str {
            "fake"
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
            SenderIdentity { sender_id: format!("fake:{raw_sender}"), label: None }
        }
        fn deliver(
            &self,
            target: &DeliveryTarget,
            content: &OutboundContent,
        ) -> Result<String, ChannelError> {
            if !self.connected {
                return Err(ChannelError::NotConnected);
            }
            self.delivered.borrow_mut().push((target.clone(), content.clone()));
            Ok(format!("msg-{}", self.delivered.borrow().len()))
        }
        fn health(&self) -> ChannelHealth {
            if self.connected {
                ChannelHealth::Connected
            } else {
                ChannelHealth::Disconnected { detail: "not started".into() }
            }
        }
    }

    #[test]
    fn adapter_is_object_safe_and_lifecycle_gates_delivery() {
        let mut ch = FakeChannel::new();
        let adapter: &mut dyn ChannelAdapter = &mut ch;
        let target = DeliveryTarget { chat_id: "C1".into(), thread_root_id: None };
        let content = OutboundContent::Text { body: "hi".into() };

        // Delivery before start is refused.
        assert_eq!(adapter.deliver(&target, &content), Err(ChannelError::NotConnected));
        adapter.start().unwrap();
        assert!(adapter.is_connected());
        assert_eq!(adapter.deliver(&target, &content).unwrap(), "msg-1");

        // Unsupported optional ops fall back to the default.
        assert_eq!(
            adapter.react(&target, "msg-1", "+1"),
            Err(ChannelError::Unsupported { op: "react" })
        );
        assert!(adapter.health().is_connected());
    }

    #[test]
    fn dedupe_key_is_stable_across_retries() {
        // The same platform message id always yields the same dedupe key,
        // regardless of how many times it is delivered.
        let a = dedupe_key("slack", "C1", "1700000000.000100");
        let b = dedupe_key("slack", "C1", "1700000000.000100");
        assert_eq!(a, b);
        // A different message id differs.
        assert_ne!(a, dedupe_key("slack", "C1", "1700000000.000200"));
    }

    #[test]
    fn engagement_key_distinguishes_thread_from_top_level() {
        let top = engagement_key("slack", "C1", None);
        let thread = engagement_key("slack", "C1", Some("1700.1"));
        assert_ne!(top, thread);
        assert_eq!(top, "slack:C1");
        assert_eq!(thread, "slack:C1:1700.1");
    }

    #[test]
    fn rich_content_falls_back_to_text() {
        let card = OutboundContent::Card {
            title: "T".into(),
            body: "B".into(),
            fallback: "T — B".into(),
        };
        assert_eq!(card.fallback_text(), "T — B");
    }
}
