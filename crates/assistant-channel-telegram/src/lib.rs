//! assistant-channel-telegram: the Telegram product channel.
//!
//! This slice implements the offline, deterministic core: normalizing Telegram
//! `Update` payloads into the platform's neutral [`assistant_router::RoutingEvent`]
//! and rendering outbound content into Telegram-safe MarkdownV2. The long-poll /
//! webhook transport and its readiness probes land behind the `ChannelAdapter`
//! boundary in a later slice.

pub mod event;
pub mod format;
pub mod readiness;

pub use event::{normalize, parse_update, Chat, Entity, Message, TelegramIdentity, Update, User};
pub use format::{escape_markdown_v2, render};
pub use readiness::{
    bot_identity, bot_token_well_formed, delivery_smoke, group_privacy_matches,
    inbound_pairing_smoke, transport_exclusivity, BotIdentity, CheckStatus, ProbeError,
    TelegramConfig, TelegramProbe, TransportMode, WebhookInfo,
};

pub const MODULE_ID: &str = "assistant-channel-telegram";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
