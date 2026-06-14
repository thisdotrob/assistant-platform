//! Minimal local CLI channel adapter.
//!
//! Normalizes terminal/test input into inbound session messages, resolves a
//! stable local sender identity, and renders outbound messages back to plain
//! text (with a graceful fallback for card-style payloads that a terminal can't
//! render richly). It speaks to a session entirely through the `assistant-session`
//! public API; it owns no session-DB schema of its own.
//!
//! It conforms to [`assistant_router::ChannelAdapter`] so the host can drive it
//! uniformly alongside Slack/Telegram, and normalizes raw terminal input into a
//! [`assistant_router::RoutingEvent`] for the router.

use std::cell::RefCell;

use assistant_router::{
    dedupe_key, engagement_key, ChannelAdapter, ChannelError, ChannelHealth, DeliveryTarget,
    MessageState, OutboundContent, RoutingEvent, SenderIdentity, SetupStep,
};
use assistant_session::{
    current_outbound_compat, enqueue_inbound, init_session, read_outbound, safe_attachment_path,
    session_exists, InboundMessage, OutboundMessage, SessionError, SessionLayout,
};

/// Stable identity for the local terminal user. Derived from `$USER` so the
/// sender is consistent across messages in a session; falls back to a fixed
/// label when the environment doesn't expose a user name.
pub fn local_sender() -> String {
    match std::env::var("USER") {
        Ok(user) if !user.trim().is_empty() => format!("local:{}", user.trim()),
        _ => "local:cli".to_string(),
    }
}

/// A normalized inbound message before it is enqueued. Kept as a thin wrapper so
/// callers can inspect/transform before commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedInbound {
    pub message: InboundMessage,
}

/// The local CLI channel bound to one session folder.
pub struct CliChannel {
    layout: SessionLayout,
    started: bool,
    /// When set, an inbound message whose resolved sender matches this id is
    /// flagged self-authored so the router ignores the bot's own echo.
    self_sender_id: Option<String>,
    /// Host-facing outbound sink. Rich content rendered to plain terminal lines
    /// in arrival order; the host drains and prints them.
    delivered: RefCell<Vec<String>>,
}

impl CliChannel {
    pub fn new(layout: SessionLayout) -> Self {
        Self {
            layout,
            started: false,
            self_sender_id: None,
            delivered: RefCell::new(Vec::new()),
        }
    }

    pub fn layout(&self) -> &SessionLayout {
        &self.layout
    }

    /// Mark a sender id as the bot's own identity so inbound events from it are
    /// flagged self-authored (and dropped by the router as a self-echo).
    pub fn set_self_identity(&mut self, sender_id: impl Into<String>) {
        self.self_sender_id = Some(sender_id.into());
    }

    /// Stable conversation id for this channel — the session folder name.
    pub fn chat_id(&self) -> String {
        self.layout
            .dir()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cli")
            .to_string()
    }

    /// Start the channel, initializing the session folder if needed.
    pub fn start(&mut self) -> Result<(), SessionError> {
        if !session_exists(&self.layout) {
            init_session(&self.layout)?;
        }
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.started = false;
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Normalize raw terminal text into an inbound message attributed to the
    /// local sender. Trailing newline noise is trimmed; the body is preserved.
    pub fn normalize_inbound(&self, raw: &str) -> NormalizedInbound {
        NormalizedInbound {
            message: InboundMessage {
                sender: local_sender(),
                content: raw.trim_end_matches(['\n', '\r']).to_string(),
                metadata: None,
            },
        }
    }

    /// Normalize and enqueue a terminal message, returning its sequence number.
    pub fn send_text(&self, raw: &str) -> Result<i64, SessionError> {
        let normalized = self.normalize_inbound(raw);
        enqueue_inbound(&self.layout, &normalized.message)
    }

    /// Resolve a safe attachment path under this message's inbox directory.
    /// Rejects names that would escape the session folder.
    pub fn attachment_path(
        &self,
        message_id: &str,
        file_name: &str,
    ) -> Result<std::path::PathBuf, SessionError> {
        let base = self.layout.inbox_message_dir(message_id)?;
        safe_attachment_path(&base, file_name)
    }

    /// Read outbound messages and render each to a plain-terminal line.
    pub fn render_outbound(&self) -> Result<Vec<String>, SessionError> {
        let messages = read_outbound(&self.layout, current_outbound_compat())?;
        Ok(messages.iter().map(render_message).collect())
    }

    /// Normalize raw terminal input into a [`RoutingEvent`] for the router. The
    /// CLI is a single direct conversation, so every event is `is_direct` and
    /// has no thread; trailing newline noise is trimmed.
    pub fn normalize_event(&self, raw: &str, platform_message_id: &str) -> RoutingEvent {
        let identity = self.resolve_sender("");
        let chat = self.chat_id();
        let is_self = self.self_sender_id.as_deref() == Some(identity.sender_id.as_str());
        RoutingEvent {
            channel_kind: "cli".to_string(),
            chat_id: chat.clone(),
            sender_id: identity.sender_id,
            sender_label: identity.label,
            platform_message_id: platform_message_id.to_string(),
            thread_root_id: None,
            reply_target_id: None,
            engagement_key: engagement_key("cli", &chat, None),
            permalink: None,
            state: MessageState::New,
            is_mention: false,
            is_direct: true,
            is_self_author: is_self,
            dedupe_key: dedupe_key("cli", &chat, platform_message_id),
            text: raw.trim_end_matches(['\n', '\r']).to_string(),
            raw_ref: None,
        }
    }

    /// Take the buffered outbound lines delivered via [`ChannelAdapter::deliver`],
    /// leaving the sink empty. The host prints these to the terminal.
    pub fn drain_delivered(&self) -> Vec<String> {
        std::mem::take(&mut self.delivered.borrow_mut())
    }
}

impl ChannelAdapter for CliChannel {
    fn channel_kind(&self) -> &'static str {
        "cli"
    }

    fn start(&mut self) -> Result<(), ChannelError> {
        // The inherent `start` carries the richer SessionError for direct callers;
        // the uniform trait surface narrows it to a setup failure.
        CliChannel::start(self).map_err(|e| ChannelError::Setup { detail: e.to_string() })
    }

    fn stop(&mut self) {
        CliChannel::stop(self);
    }

    fn is_connected(&self) -> bool {
        self.started
    }

    fn resolve_sender(&self, raw_sender: &str) -> SenderIdentity {
        let trimmed = raw_sender.trim();
        if trimmed.is_empty() {
            SenderIdentity {
                sender_id: local_sender(),
                label: None,
            }
        } else {
            SenderIdentity {
                sender_id: format!("local:{trimmed}"),
                label: Some(trimmed.to_string()),
            }
        }
    }

    fn deliver(
        &self,
        _target: &DeliveryTarget,
        content: &OutboundContent,
    ) -> Result<String, ChannelError> {
        if !self.started {
            return Err(ChannelError::NotConnected);
        }
        let mut buf = self.delivered.borrow_mut();
        buf.push(content.fallback_text());
        Ok(format!("cli-{}", buf.len()))
    }

    fn health(&self) -> ChannelHealth {
        if self.started {
            ChannelHealth::Connected
        } else {
            ChannelHealth::Disconnected {
                detail: "not started".to_string(),
            }
        }
    }

    fn setup_steps(&self) -> Vec<SetupStep> {
        vec![SetupStep {
            id: "session-init".to_string(),
            description: "Initialize the local session folder".to_string(),
            completed: session_exists(&self.layout),
        }]
    }
}

/// Render one outbound message for a plain terminal. Rich "card" payloads fall
/// back to their text content so nothing is dropped on a terminal that can't
/// render the card.
pub fn render_message(message: &OutboundMessage) -> String {
    match message.kind.as_str() {
        "text" => message.content.clone(),
        "card" => format!("[card] {}", message.content),
        other => format!("[{other}] {}", message.content),
    }
}

pub const MODULE_ID: &str = "assistant-channel-cli";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_sender_is_stable() {
        // Whatever the environment, two reads agree.
        assert_eq!(local_sender(), local_sender());
        assert!(local_sender().starts_with("local:"));
    }

    #[test]
    fn normalize_trims_trailing_newlines_only() {
        let layout = SessionLayout::derive(std::path::Path::new("/tmp/x"), "g", "s").unwrap();
        let ch = CliChannel::new(layout);
        let n = ch.normalize_inbound("  hi there\n\n");
        assert_eq!(n.message.content, "  hi there");
    }

    #[test]
    fn card_falls_back_to_text() {
        let msg = OutboundMessage {
            seq: 1,
            kind: "card".to_string(),
            content: "Title / body".to_string(),
            metadata: None,
            created_at: "now".to_string(),
        };
        assert_eq!(render_message(&msg), "[card] Title / body");
    }

    #[test]
    fn normalize_event_is_direct_and_keys_are_stable() {
        let layout = SessionLayout::derive(std::path::Path::new("/tmp/x"), "g", "sess").unwrap();
        let ch = CliChannel::new(layout);
        let ev = ch.normalize_event("hello\n", "m1");
        assert_eq!(ev.channel_kind, "cli");
        assert!(ev.is_direct);
        assert!(!ev.is_mention);
        assert!(!ev.is_self_author);
        assert_eq!(ev.text, "hello");
        assert_eq!(ev.chat_id, "sess");
        assert_eq!(ev.engagement_key, "cli:sess");
        assert_eq!(ev.dedupe_key, "cli:sess:m1");
    }

    #[test]
    fn self_identity_flags_self_authored_events() {
        let layout = SessionLayout::derive(std::path::Path::new("/tmp/x"), "g", "sess").unwrap();
        let mut ch = CliChannel::new(layout);
        let me = ch.resolve_sender("").sender_id;
        ch.set_self_identity(me);
        assert!(ch.normalize_event("echo", "m2").is_self_author);
    }

    #[test]
    fn deliver_is_gated_and_card_falls_back_to_text() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = SessionLayout::derive(tmp.path(), "g", "sess").unwrap();
        let mut ch = CliChannel::new(layout);
        let target = DeliveryTarget { chat_id: ch.chat_id(), thread_root_id: None };
        let card = OutboundContent::Card {
            title: "T".to_string(),
            body: "B".to_string(),
            fallback: "T — B".to_string(),
        };
        // Delivery before start is refused.
        assert_eq!(ch.deliver(&target, &card), Err(ChannelError::NotConnected));
        ch.start().unwrap();
        assert!(ch.is_connected());
        assert_eq!(ch.deliver(&target, &card).unwrap(), "cli-1");
        // The rich card is delivered via its plain-text fallback; draining empties.
        assert_eq!(ch.drain_delivered(), vec!["T — B".to_string()]);
        assert!(ch.drain_delivered().is_empty());
    }
}
