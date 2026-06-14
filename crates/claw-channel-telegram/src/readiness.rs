//! Telegram readiness checks.
//!
//! The Bot API calls a real instance makes (`getMe`, `getWebhookInfo`, an
//! inbound pairing smoke, a delivery smoke) are injected as a [`TelegramProbe`]
//! so these checks run deterministically offline; a host wires a real probe in
//! production. Token and polling/webhook configuration checks are pure where
//! they can be, and consult the probe only for live webhook state.
//!
//! `CheckStatus` is duplicated per crate (not shared) to honor the module
//! dependency boundary, matching the other platform crates.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    Skipped { detail: String },
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass)
    }

    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckStatus::Fail { .. })
    }
}

/// Which inbound transport the bot is configured to use. Polling is the default
/// local-first transport; webhook is opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportMode {
    #[default]
    Polling,
    Webhook,
}

/// The Telegram configuration the readiness checks inspect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub mode: TransportMode,
    /// Required when `mode` is [`TransportMode::Webhook`].
    pub webhook_url: Option<String>,
    /// Whether the bot is expected to see all group messages (privacy mode off).
    /// A mention-sticky group bot needs this true; a DM-only bot leaves it false.
    pub expect_group_message_visibility: bool,
}

/// What `getMe` returns about our bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotIdentity {
    pub bot_id: i64,
    pub username: String,
    /// Telegram's privacy-mode inverse: true means the bot receives all group
    /// messages, false means only commands/mentions/replies.
    pub can_read_all_group_messages: bool,
}

/// What `getWebhookInfo` returns about the currently registered webhook.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookInfo {
    /// Empty when no webhook is set (polling territory).
    pub url: String,
    pub pending_update_count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeError {
    Auth(String),
    Network(String),
    Api(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Auth(d) => write!(f, "auth rejected: {d}"),
            ProbeError::Network(d) => write!(f, "network error: {d}"),
            ProbeError::Api(d) => write!(f, "telegram api error: {d}"),
        }
    }
}

/// The injected Telegram network surface the readiness checks exercise.
pub trait TelegramProbe {
    /// `getMe` — verify the bot token and return the bot identity.
    fn get_me(&self) -> Result<BotIdentity, ProbeError>;
    /// `getWebhookInfo` — the currently registered webhook, if any.
    fn webhook_info(&self) -> Result<WebhookInfo, ProbeError>;
    /// Confirm an inbound pairing message can be received end-to-end.
    fn pairing_smoke(&self) -> Result<(), ProbeError>;
    /// Non-invasive confirmation that message/file delivery would succeed.
    fn delivery_smoke(&self) -> Result<(), ProbeError>;
}

/// The bot token is present and shaped like a Telegram token (`<digits>:<rest>`).
pub fn bot_token_well_formed(cfg: &TelegramConfig) -> CheckStatus {
    match cfg.bot_token.as_deref() {
        None => CheckStatus::Fail { detail: "bot token is not configured".to_string() },
        Some(t) => match t.split_once(':') {
            Some((digits, rest))
                if !digits.is_empty()
                    && digits.chars().all(|c| c.is_ascii_digit())
                    && !rest.is_empty() =>
            {
                CheckStatus::Pass
            }
            _ => CheckStatus::Fail {
                detail: "bot token is not a valid <id>:<secret> token".to_string(),
            },
        },
    }
}

/// `getMe` succeeds and, when an expected username is supplied, matches it.
pub fn bot_identity(probe: &dyn TelegramProbe, expected_username: Option<&str>) -> CheckStatus {
    match probe.get_me() {
        Ok(identity) => match expected_username {
            Some(expected) if expected != identity.username => CheckStatus::Fail {
                detail: format!(
                    "bot identity mismatch: token resolves to @{}, expected @{expected}",
                    identity.username
                ),
            },
            _ => CheckStatus::Pass,
        },
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// Polling and webhook are mutually exclusive: a webhook registered while in
/// polling mode silently steals updates, and webhook mode needs a matching
/// registered URL.
pub fn transport_exclusivity(cfg: &TelegramConfig, probe: &dyn TelegramProbe) -> CheckStatus {
    let info = match probe.webhook_info() {
        Ok(i) => i,
        Err(e) => return CheckStatus::Fail { detail: e.to_string() },
    };
    match cfg.mode {
        TransportMode::Polling => {
            if info.url.is_empty() {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail {
                    detail: format!(
                        "polling mode but a webhook is registered at {}; delete it first",
                        info.url
                    ),
                }
            }
        }
        TransportMode::Webhook => match cfg.webhook_url.as_deref() {
            None | Some("") => CheckStatus::Fail {
                detail: "webhook mode but no webhook_url is configured".to_string(),
            },
            Some(want) if want == info.url => CheckStatus::Pass,
            Some(want) => CheckStatus::Fail {
                detail: format!("webhook mismatch: registered {:?}, configured {want:?}", info.url),
            },
        },
    }
}

/// An inbound pairing message can be received.
pub fn inbound_pairing_smoke(probe: &dyn TelegramProbe) -> CheckStatus {
    match probe.pairing_smoke() {
        Ok(()) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// The bot's group-message visibility matches what the product expects. A
/// mismatch is a configuration problem in BotFather, not a transient error.
pub fn group_privacy_matches(cfg: &TelegramConfig, probe: &dyn TelegramProbe) -> CheckStatus {
    match probe.get_me() {
        Ok(identity) => {
            if identity.can_read_all_group_messages == cfg.expect_group_message_visibility {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail {
                    detail: format!(
                        "group privacy mismatch: bot can_read_all_group_messages={}, expected {}",
                        identity.can_read_all_group_messages, cfg.expect_group_message_visibility
                    ),
                }
            }
        }
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// Non-invasive message/file delivery check.
pub fn delivery_smoke(probe: &dyn TelegramProbe) -> CheckStatus {
    match probe.delivery_smoke() {
        Ok(()) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbe {
        identity: Result<BotIdentity, ProbeError>,
        webhook: Result<WebhookInfo, ProbeError>,
        pairing: Result<(), ProbeError>,
        delivery: Result<(), ProbeError>,
    }

    impl FakeProbe {
        fn new() -> Self {
            Self {
                identity: Ok(BotIdentity {
                    bot_id: 999,
                    username: "claw_bot".to_string(),
                    can_read_all_group_messages: true,
                }),
                webhook: Ok(WebhookInfo::default()),
                pairing: Ok(()),
                delivery: Ok(()),
            }
        }
    }

    impl TelegramProbe for FakeProbe {
        fn get_me(&self) -> Result<BotIdentity, ProbeError> {
            self.identity.clone()
        }
        fn webhook_info(&self) -> Result<WebhookInfo, ProbeError> {
            self.webhook.clone()
        }
        fn pairing_smoke(&self) -> Result<(), ProbeError> {
            self.pairing.clone()
        }
        fn delivery_smoke(&self) -> Result<(), ProbeError> {
            self.delivery.clone()
        }
    }

    #[test]
    fn token_shape_is_validated() {
        let good = TelegramConfig {
            bot_token: Some("123456:ABC-def".to_string()),
            ..Default::default()
        };
        assert!(bot_token_well_formed(&good).is_pass());

        for bad in ["", "nope", "abc:def", "123456:"] {
            let cfg = TelegramConfig { bot_token: Some(bad.to_string()), ..Default::default() };
            assert!(bot_token_well_formed(&cfg).is_blocking_failure(), "{bad:?}");
        }
        assert!(bot_token_well_formed(&TelegramConfig::default()).is_blocking_failure());
    }

    #[test]
    fn identity_passes_and_detects_mismatch_or_auth_error() {
        let probe = FakeProbe::new();
        assert!(bot_identity(&probe, None).is_pass());
        assert!(bot_identity(&probe, Some("claw_bot")).is_pass());
        assert!(bot_identity(&probe, Some("other")).is_blocking_failure());

        let failing = FakeProbe { identity: Err(ProbeError::Auth("401".to_string())), ..FakeProbe::new() };
        assert!(bot_identity(&failing, None).is_blocking_failure());
    }

    #[test]
    fn polling_mode_rejects_a_registered_webhook() {
        let cfg = TelegramConfig { mode: TransportMode::Polling, ..Default::default() };
        // No webhook registered -> ok.
        assert!(transport_exclusivity(&cfg, &FakeProbe::new()).is_pass());
        // A registered webhook conflicts with polling.
        let conflict = FakeProbe {
            webhook: Ok(WebhookInfo { url: "https://h/hook".to_string(), pending_update_count: 0 }),
            ..FakeProbe::new()
        };
        assert!(transport_exclusivity(&cfg, &conflict).is_blocking_failure());
    }

    #[test]
    fn webhook_mode_requires_a_matching_registered_url() {
        let probe = FakeProbe {
            webhook: Ok(WebhookInfo { url: "https://h/hook".to_string(), pending_update_count: 0 }),
            ..FakeProbe::new()
        };
        let matching = TelegramConfig {
            mode: TransportMode::Webhook,
            webhook_url: Some("https://h/hook".to_string()),
            ..Default::default()
        };
        assert!(transport_exclusivity(&matching, &probe).is_pass());

        let unconfigured = TelegramConfig { mode: TransportMode::Webhook, ..Default::default() };
        assert!(transport_exclusivity(&unconfigured, &probe).is_blocking_failure());

        let mismatched = TelegramConfig {
            mode: TransportMode::Webhook,
            webhook_url: Some("https://other/hook".to_string()),
            ..Default::default()
        };
        assert!(transport_exclusivity(&mismatched, &probe).is_blocking_failure());
    }

    #[test]
    fn group_privacy_expectation_is_enforced() {
        // Probe reports visibility on; expectation on -> pass.
        let cfg_on = TelegramConfig { expect_group_message_visibility: true, ..Default::default() };
        assert!(group_privacy_matches(&cfg_on, &FakeProbe::new()).is_pass());
        // Expectation off but bot can read all -> mismatch.
        let cfg_off = TelegramConfig { expect_group_message_visibility: false, ..Default::default() };
        assert!(group_privacy_matches(&cfg_off, &FakeProbe::new()).is_blocking_failure());
    }

    #[test]
    fn pairing_and_delivery_smokes_reflect_probe() {
        let ok = FakeProbe::new();
        assert!(inbound_pairing_smoke(&ok).is_pass());
        assert!(delivery_smoke(&ok).is_pass());

        let failing = FakeProbe {
            pairing: Err(ProbeError::Network("no update".to_string())),
            delivery: Err(ProbeError::Api("chat_not_found".to_string())),
            ..FakeProbe::new()
        };
        assert!(inbound_pairing_smoke(&failing).is_blocking_failure());
        assert!(delivery_smoke(&failing).is_blocking_failure());
    }
}
