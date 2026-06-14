//! assistant-channel-slack: the Slack product channel.
//!
//! Offline, deterministic core: normalizing Slack event payloads into the
//! platform's neutral [`assistant_router::RoutingEvent`] and rendering outbound
//! content into Slack mrkdwn.
//!
//! Real transport: the [`adapter`] module implements the outbound half of the
//! `ChannelAdapter` boundary — Web API auth lifecycle and `chat.postMessage`
//! delivery, shelling `curl` so it stays within the synchronous, no-extra-deps
//! model. The inbound half lives in [`socket`]: the Socket Mode protocol
//! (envelope ACK, reconnect, hand-off to [`event::normalize`]) is offline and
//! testable in the default build behind an injected transport; the live
//! websocket dial is a thin `tungstenite` shell behind the `socket-mode`
//! feature. Wiring an inbound event into a live turn is the next slice.

pub mod adapter;
pub mod event;
pub mod mrkdwn;
pub mod readiness;
pub mod socket;

pub use adapter::{
    CurlSlackApi, ProxyInjection, SlackApi, SlackApiError, SlackChannel, SLACK_TOKEN_PLACEHOLDER,
};
pub use event::{normalize, parse_event, SlackEnvelope, SlackIdentity, SlackReactionItem};
pub use mrkdwn::{escape, render};
pub use readiness::{
    app_token_well_formed, bot_token_well_formed, connections_open, delivery_or_auth,
    inbound_event_smoke, socket_mode_enabled, web_api_identity, CheckStatus, ProbeError,
    SlackBotIdentity, SlackConfig, SlackProbe,
};
pub use socket::{parse_incoming, run_listener, Incoming, SocketConn, SocketError, SocketOpener};
#[cfg(feature = "socket-mode")]
pub use socket::TungsteniteOpener;

pub const MODULE_ID: &str = "assistant-channel-slack";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
