//! The real Slack [`ChannelAdapter`]: Web API auth lifecycle and outbound
//! delivery.
//!
//! Outbound is the half that fits the platform's synchronous, no-extra-deps
//! model: a message is rendered to mrkdwn and POSTed to Slack's Web API by
//! shelling `curl` (the same pattern the Docker runtime and the OneCLI client
//! use — there is no async HTTP stack). The network surface is injected as a
//! [`SlackApi`] so the adapter's logic (rendering, threading, lifecycle gating,
//! error mapping) is exercised offline against a fake, never touching the
//! network in the sandbox.
//!
//! Inbound (Socket Mode) lives in [`crate::socket`], not here; this module
//! brings up the outbound Web API surface — `start` authenticates and `deliver`
//! posts.
//!
//! Security: the host never holds a real Slack token. Each call routes through
//! the OneCLI proxy ([`ProxyInjection`]) carrying only [`SLACK_TOKEN_PLACEHOLDER`];
//! the proxy swaps in the real `xoxb-`/`xapp-` on the wire, keyed by request
//! path. The placeholder, proxy URL, and CA path are fed via curl's stdin config
//! (`-K -`), so nothing sensitive lands in argv, a log, or on disk.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use assistant_router::{
    ChannelAdapter, ChannelError, ChannelHealth, DeliveryTarget, OutboundContent, SenderIdentity,
    SetupStep,
};
use serde_json::Value;

use crate::mrkdwn;
use crate::readiness::SlackBotIdentity;

/// How long a single Slack Web API call may take before curl gives up.
const SLACK_TIMEOUT_SECS: u32 = 10;

/// The non-secret stand-in the host sends in place of a real Slack token. The
/// host never holds an `xoxb-`/`xapp-`: it routes its Slack calls through the
/// OneCLI proxy, which replaces this placeholder with the real token on the wire
/// — selected by the request *path*, not this value — so the value is arbitrary
/// and only needs to be a well-formed Bearer the proxy can rewrite. Mirrors the
/// Anthropic `CLAUDE_CODE_OAUTH_TOKEN=placeholder` model.
pub const SLACK_TOKEN_PLACEHOLDER: &str = "claw-onecli-slack-placeholder";

/// OneCLI proxy injection for the host's Slack calls. The real `xoxb-`/`xapp-`
/// live only in the OneCLI vault; the host sends [`SLACK_TOKEN_PLACEHOLDER`] and
/// routes curl through this proxy, which swaps in the real token by request path
/// (`apps.connections.open` → app token, `auth.test`/`chat.postMessage` → bot
/// token). Both the Web API client and the Socket Mode opener share this.
#[derive(Clone, Debug)]
pub struct ProxyInjection {
    /// Host-facing OneCLI proxy URL, e.g. `http://127.0.0.1:10355`.
    pub proxy_url: String,
    /// Path to the OneCLI CA the host's curl must trust to verify the
    /// intercepted TLS. A public trust anchor, not a secret.
    pub ca_cert: PathBuf,
}

/// The outbound Slack Web API surface the adapter drives. A real instance issues
/// the corresponding Slack calls; tests supply a fake so the adapter is covered
/// offline.
pub trait SlackApi {
    /// `auth.test` — confirm the bot token and resolve the bot's own identity.
    fn auth_test(&self) -> Result<SlackBotIdentity, SlackApiError>;
    /// `chat.postMessage` — post mrkdwn `text` to `channel` (threaded under
    /// `thread_ts` when set), returning the posted message ts.
    fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String, SlackApiError>;
}

/// Why a Slack Web API call failed.
#[derive(Debug)]
pub enum SlackApiError {
    /// `curl` could not be launched.
    Spawn(std::io::Error),
    /// curl exited non-zero — the call could not reach Slack.
    Transport(String),
    /// Slack returned `ok: false` with this error code.
    Api(String),
    /// The response body was not valid JSON.
    Parse(String),
    /// Writing the request (stdin config) or reading the response failed.
    Io(std::io::Error),
}

impl std::fmt::Display for SlackApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to launch curl for the Slack Web API: {e}"),
            Self::Transport(d) => write!(f, "could not reach the Slack Web API: {d}"),
            Self::Api(code) => write!(f, "Slack Web API returned an error: {code}"),
            Self::Parse(e) => write!(f, "Slack Web API returned malformed JSON: {e}"),
            Self::Io(e) => write!(f, "Slack Web API request/response IO failed: {e}"),
        }
    }
}

impl std::error::Error for SlackApiError {}

/// The real curl-backed Slack Web API client. Compiles offline (std +
/// serde_json, no extra deps); only ever invoked live, outside the sandbox. The
/// real bot token never lives here — calls go through the OneCLI proxy carrying
/// only [`SLACK_TOKEN_PLACEHOLDER`].
pub struct CurlSlackApi {
    injection: ProxyInjection,
}

impl CurlSlackApi {
    /// Build a client that routes its Web API calls through the OneCLI proxy,
    /// which injects the real bot token on the wire (by request path).
    pub fn via_proxy(injection: ProxyInjection) -> Self {
        Self { injection }
    }

    /// Call a Web API method, returning the parsed body once `ok` is confirmed
    /// true. The placeholder Bearer, the proxy URL, and the CA are fed via curl's
    /// stdin config (not argv); the (non-secret) JSON body, when present, is
    /// passed inline.
    fn call(&self, method: &str, body: Option<&str>) -> Result<Value, SlackApiError> {
        let url = format!("https://slack.com/api/{method}");
        let mut cmd = Command::new("curl");
        // `-sS`: quiet, but still surface transport errors. No `-f`: Slack
        // signals app-level failures with HTTP 200 + `{ok:false}`, so success
        // is decided from the body, not the status code.
        cmd.args(["-sS", "--max-time", &SLACK_TIMEOUT_SECS.to_string(), "-K", "-", &url]);
        if let Some(json) = body {
            cmd.args(["-H", "Content-Type: application/json; charset=utf-8", "--data", json]);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(SlackApiError::Spawn)?;
        {
            let mut config = child
                .stdin
                .take()
                .ok_or_else(|| SlackApiError::Transport("curl stdin unavailable".to_string()))?;
            config
                .write_all(curl_config(&self.injection).as_bytes())
                .map_err(SlackApiError::Io)?;
        }

        let output = child.wait_with_output().map_err(SlackApiError::Io)?;
        if !output.status.success() {
            return Err(SlackApiError::Transport(format!("curl exited with {}", output.status)));
        }
        parse_ok(&output.stdout)
    }
}

/// Build curl's `-K` stdin config for an OneCLI-proxied Slack call: the proxy,
/// the CA trust anchor, and the placeholder Bearer the proxy rewrites by path.
/// Shared by the Web API client and the Socket Mode opener; kept on stdin (not
/// argv) to match the existing pattern. The values carry no quote that could
/// break a config line (proxy URL and placeholder are controlled; the CA path is
/// ours).
pub(crate) fn curl_config(injection: &ProxyInjection) -> String {
    format!(
        "proxy = \"{}\"\ncacert = \"{}\"\nheader = \"Authorization: Bearer {SLACK_TOKEN_PLACEHOLDER}\"\n",
        injection.proxy_url,
        injection.ca_cert.display(),
    )
}

/// Parse a Slack Web API body, returning it only when `ok` is true. Split out so
/// the ok/error mapping is unit-testable without a live call.
fn parse_ok(body: &[u8]) -> Result<Value, SlackApiError> {
    let value: Value = serde_json::from_slice(body).map_err(|e| SlackApiError::Parse(e.to_string()))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value)
    } else {
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Err(SlackApiError::Api(code))
    }
}

impl SlackApi for CurlSlackApi {
    fn auth_test(&self) -> Result<SlackBotIdentity, SlackApiError> {
        let value = self.call("auth.test", None)?;
        Ok(SlackBotIdentity {
            bot_user_id: value.get("user_id").and_then(Value::as_str).unwrap_or_default().to_string(),
            team: value.get("team").and_then(Value::as_str).unwrap_or_default().to_string(),
            bot_id: value.get("bot_id").and_then(Value::as_str).map(str::to_string),
        })
    }

    fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String, SlackApiError> {
        let mut payload = serde_json::json!({ "channel": channel, "text": text });
        if let Some(ts) = thread_ts {
            payload["thread_ts"] = Value::String(ts.to_string());
        }
        let body = serde_json::to_string(&payload).map_err(|e| SlackApiError::Parse(e.to_string()))?;
        let value = self.call("chat.postMessage", Some(&body))?;
        Ok(value.get("ts").and_then(Value::as_str).unwrap_or_default().to_string())
    }
}

/// The Slack channel adapter. Generic over the Web API surface so production
/// uses [`CurlSlackApi`] while tests inject a fake.
pub struct SlackChannel<A: SlackApi> {
    api: A,
    /// The bot's own user id, resolved at `start` from `auth.test`; used by
    /// callers for self-author detection and sender resolution.
    bot_user_id: Option<String>,
    /// The bot's own `bot_id` (`B…`), resolved at `start` from `auth.test`. The
    /// serve loop feeds this into `SlackIdentity.self_bot_id` so the bot's own
    /// replies (which arrive as `message` events with this `bot_id` and no
    /// `user`) are recognized and skipped rather than re-driving a turn.
    self_bot_id: Option<String>,
    connected: bool,
}

impl<A: SlackApi> SlackChannel<A> {
    pub fn new(api: A) -> Self {
        Self { api, bot_user_id: None, self_bot_id: None, connected: false }
    }

    /// The bot's own user id once `start` has authenticated.
    pub fn bot_user_id(&self) -> Option<&str> {
        self.bot_user_id.as_deref()
    }

    /// The bot's own `bot_id` once `start` has authenticated, for self-author
    /// detection of the bot's own posted replies.
    pub fn self_bot_id(&self) -> Option<&str> {
        self.self_bot_id.as_deref()
    }
}

impl SlackChannel<CurlSlackApi> {
    /// Build a production channel whose Web API calls go through the OneCLI proxy
    /// (the real bot token is injected on the wire; the host holds only a
    /// placeholder).
    pub fn via_proxy(injection: ProxyInjection) -> Self {
        Self::new(CurlSlackApi::via_proxy(injection))
    }
}

impl<A: SlackApi> ChannelAdapter for SlackChannel<A> {
    fn channel_kind(&self) -> &'static str {
        "slack"
    }

    fn start(&mut self) -> Result<(), ChannelError> {
        // Confirm the token and capture our own identity. This brings up only
        // the Web API surface; Socket Mode (inbound) is a later slice.
        let identity = self
            .api
            .auth_test()
            .map_err(|e| ChannelError::Setup { detail: e.to_string() })?;
        self.bot_user_id = Some(identity.bot_user_id);
        self.self_bot_id = identity.bot_id;
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
        // A Slack user/bot id is already stable; pass it through unchanged so it
        // matches the id `event::normalize` stamps on inbound events. The
        // `channel_kind` on the event keeps it distinct across channels.
        SenderIdentity {
            sender_id: raw_sender.trim().to_string(),
            label: None,
        }
    }

    fn deliver(
        &self,
        target: &DeliveryTarget,
        content: &OutboundContent,
    ) -> Result<String, ChannelError> {
        if !self.connected {
            return Err(ChannelError::NotConnected);
        }
        let text = mrkdwn::render(content);
        self.api
            .post_message(&target.chat_id, target.thread_root_id.as_deref(), &text)
            .map_err(|e| ChannelError::Delivery { detail: e.to_string() })
    }

    fn health(&self) -> ChannelHealth {
        if self.connected {
            ChannelHealth::Connected
        } else {
            ChannelHealth::Disconnected { detail: "not started".to_string() }
        }
    }

    fn setup_steps(&self) -> Vec<SetupStep> {
        vec![SetupStep {
            id: "slack-web-api-auth".to_string(),
            description: "Authenticate the Slack bot token (auth.test)".to_string(),
            completed: self.connected,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// An in-process Slack Web API: records the posts it receives and returns
    /// canned identity/ts or injected errors, so the adapter is driven offline.
    struct FakeApi {
        identity: Result<SlackBotIdentity, ()>,
        post_result: Result<String, String>,
        posts: RefCell<Vec<(String, Option<String>, String)>>,
    }

    impl FakeApi {
        fn ok() -> Self {
            Self {
                identity: Ok(SlackBotIdentity {
                    bot_user_id: "U_BOT".to_string(),
                    team: "T1".to_string(),
                    bot_id: Some("B_BOT".to_string()),
                }),
                post_result: Ok("1700000000.000100".to_string()),
                posts: RefCell::new(Vec::new()),
            }
        }
    }

    impl SlackApi for FakeApi {
        fn auth_test(&self) -> Result<SlackBotIdentity, SlackApiError> {
            self.identity
                .clone()
                .map_err(|_| SlackApiError::Api("invalid_auth".to_string()))
        }

        fn post_message(
            &self,
            channel: &str,
            thread_ts: Option<&str>,
            text: &str,
        ) -> Result<String, SlackApiError> {
            self.posts.borrow_mut().push((
                channel.to_string(),
                thread_ts.map(str::to_string),
                text.to_string(),
            ));
            self.post_result
                .clone()
                .map_err(SlackApiError::Api)
        }
    }

    #[test]
    fn start_authenticates_and_connects() {
        let mut ch = SlackChannel::new(FakeApi::ok());
        assert!(!ch.is_connected());
        ch.start().unwrap();
        assert!(ch.is_connected());
        assert_eq!(ch.bot_user_id(), Some("U_BOT"));
        assert_eq!(ch.self_bot_id(), Some("B_BOT"));
        assert!(ch.health().is_connected());
        assert!(ch.setup_steps()[0].completed);
    }

    #[test]
    fn start_maps_auth_failure_to_setup_error() {
        let mut api = FakeApi::ok();
        api.identity = Err(());
        let mut ch = SlackChannel::new(api);
        let err = ch.start().unwrap_err();
        assert!(matches!(err, ChannelError::Setup { .. }), "got {err:?}");
        assert!(!ch.is_connected());
    }

    #[test]
    fn deliver_before_start_is_refused() {
        let ch = SlackChannel::new(FakeApi::ok());
        let target = DeliveryTarget { chat_id: "C1".to_string(), thread_root_id: None };
        let content = OutboundContent::Text { body: "hi".to_string() };
        assert_eq!(ch.deliver(&target, &content), Err(ChannelError::NotConnected));
    }

    #[test]
    fn deliver_renders_mrkdwn_and_forwards_target() {
        let mut ch = SlackChannel::new(FakeApi::ok());
        ch.start().unwrap();
        let target = DeliveryTarget {
            chat_id: "C1".to_string(),
            thread_root_id: Some("100.1".to_string()),
        };
        // `<` and `&` must arrive entity-escaped, threaded under the root.
        let content = OutboundContent::Text { body: "1 < 2 && ok".to_string() };
        let ts = ch.deliver(&target, &content).unwrap();
        assert_eq!(ts, "1700000000.000100");

        let posts = ch.api.posts.borrow();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "C1");
        assert_eq!(posts[0].1.as_deref(), Some("100.1"));
        assert_eq!(posts[0].2, "1 &lt; 2 &amp;&amp; ok");
    }

    #[test]
    fn deliver_card_falls_back_to_mrkdwn_layout() {
        let mut ch = SlackChannel::new(FakeApi::ok());
        ch.start().unwrap();
        let target = DeliveryTarget { chat_id: "C1".to_string(), thread_root_id: None };
        let card = OutboundContent::Card {
            title: "Deploy <prod>".to_string(),
            body: "done".to_string(),
            fallback: "Deploy prod: done".to_string(),
        };
        ch.deliver(&target, &card).unwrap();
        assert_eq!(ch.api.posts.borrow()[0].2, "*Deploy &lt;prod&gt;*\ndone");
    }

    #[test]
    fn deliver_maps_api_failure_to_delivery_error() {
        let mut api = FakeApi::ok();
        api.post_result = Err("channel_not_found".to_string());
        let mut ch = SlackChannel::new(api);
        ch.start().unwrap();
        let target = DeliveryTarget { chat_id: "C_GONE".to_string(), thread_root_id: None };
        let err = ch
            .deliver(&target, &OutboundContent::Text { body: "x".to_string() })
            .unwrap_err();
        assert!(matches!(err, ChannelError::Delivery { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_sender_passes_through_slack_id() {
        let ch = SlackChannel::new(FakeApi::ok());
        let id = ch.resolve_sender("  U123 ");
        assert_eq!(id.sender_id, "U123");
        assert_eq!(id.label, None);
    }

    #[test]
    fn stop_disconnects() {
        let mut ch = SlackChannel::new(FakeApi::ok());
        ch.start().unwrap();
        ch.stop();
        assert!(!ch.is_connected());
        assert!(matches!(ch.health(), ChannelHealth::Disconnected { .. }));
    }

    #[test]
    fn parse_ok_maps_body_status() {
        assert!(parse_ok(br#"{"ok":true,"ts":"1.2"}"#).is_ok());
        let err = parse_ok(br#"{"ok":false,"error":"not_in_channel"}"#).unwrap_err();
        assert!(matches!(err, SlackApiError::Api(code) if code == "not_in_channel"));
        assert!(matches!(parse_ok(b"not json"), Err(SlackApiError::Parse(_))));
    }
}
