//! Slack readiness checks.
//!
//! The network calls a real instance makes (`auth.test`, `apps.connections.open`,
//! a Socket Mode inbound smoke) are injected as a [`SlackProbe`] so these checks
//! run deterministically offline; a host wires a real probe in production. Token
//! and Socket Mode configuration checks are pure and take no probe.
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

/// The Slack configuration the readiness checks inspect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlackConfig {
    pub bot_token: Option<String>,
    pub app_token: Option<String>,
    pub socket_mode_enabled: bool,
}

/// The bot identity a successful `auth.test` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackBotIdentity {
    pub bot_user_id: String,
    pub team: String,
    /// The bot's own `bot_id` (`B…`), as reported by `auth.test`. The bot's own
    /// `chat.postMessage` replies arrive back as `message` events carrying this
    /// `bot_id` and often no `user`, so self-author detection keys on it rather
    /// than `bot_user_id`. `None` when `auth.test` omitted it.
    pub bot_id: Option<String>,
}

/// Why a probe call failed, kept distinct so a check can phrase its detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeError {
    /// The token was rejected (e.g. `invalid_auth`).
    Auth(String),
    /// The call could not reach Slack.
    Network(String),
    /// Slack returned an API-level error.
    Api(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Auth(d) => write!(f, "auth rejected: {d}"),
            ProbeError::Network(d) => write!(f, "network error: {d}"),
            ProbeError::Api(d) => write!(f, "slack api error: {d}"),
        }
    }
}

/// The injected Slack network surface the readiness checks exercise. A real
/// implementation issues the corresponding Slack calls; tests supply a fake.
pub trait SlackProbe {
    /// `auth.test` — verify the bot token and return the bot identity.
    fn auth_test(&self) -> Result<SlackBotIdentity, ProbeError>;
    /// `apps.connections.open` — verify the app-level token can open Socket Mode.
    fn open_connection(&self) -> Result<(), ProbeError>;
    /// Confirm an inbound Socket Mode event can be received end-to-end.
    fn inbound_smoke(&self) -> Result<(), ProbeError>;
}

/// The bot token is present and shaped like a Slack bot token (`xoxb-...`).
pub fn bot_token_well_formed(cfg: &SlackConfig) -> CheckStatus {
    token_check(cfg.bot_token.as_deref(), "xoxb-", "bot token")
}

/// The app-level token is present and shaped like an `xapp-...` token.
pub fn app_token_well_formed(cfg: &SlackConfig) -> CheckStatus {
    token_check(cfg.app_token.as_deref(), "xapp-", "app-level token")
}

fn token_check(token: Option<&str>, prefix: &str, label: &str) -> CheckStatus {
    match token {
        None => CheckStatus::Fail { detail: format!("{label} is not configured") },
        Some(t) if t.starts_with(prefix) && t.len() > prefix.len() => CheckStatus::Pass,
        Some(_) => CheckStatus::Fail {
            detail: format!("{label} is not a valid {prefix}… token"),
        },
    }
}

/// Socket Mode — the initial local-first transport — is enabled.
pub fn socket_mode_enabled(cfg: &SlackConfig) -> CheckStatus {
    if cfg.socket_mode_enabled {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: "Socket Mode is disabled; it is the required initial transport".to_string(),
        }
    }
}

/// `auth.test` succeeds and, when an expected bot user id is supplied, the
/// returned identity matches it (guards against a token for the wrong app).
pub fn web_api_identity(probe: &dyn SlackProbe, expected_bot_user_id: Option<&str>) -> CheckStatus {
    match probe.auth_test() {
        Ok(identity) => match expected_bot_user_id {
            Some(expected) if expected != identity.bot_user_id => CheckStatus::Fail {
                detail: format!(
                    "bot identity mismatch: token resolves to {}, expected {expected}",
                    identity.bot_user_id
                ),
            },
            _ => CheckStatus::Pass,
        },
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// `apps.connections.open` succeeds — the app-level token can open Socket Mode.
pub fn connections_open(probe: &dyn SlackProbe) -> CheckStatus {
    match probe.open_connection() {
        Ok(()) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// An inbound Socket Mode event can be received.
pub fn inbound_event_smoke(probe: &dyn SlackProbe) -> CheckStatus {
    match probe.inbound_smoke() {
        Ok(()) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

/// Non-invasive delivery check: confirm we can authenticate rather than posting
/// a real test message into a channel. Reuses `auth.test`.
pub fn delivery_or_auth(probe: &dyn SlackProbe) -> CheckStatus {
    match probe.auth_test() {
        Ok(_) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail { detail: e.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProbe {
        identity: Result<SlackBotIdentity, ProbeError>,
        open: Result<(), ProbeError>,
        smoke: Result<(), ProbeError>,
    }

    impl Default for FakeProbe {
        fn default() -> Self {
            Self {
                identity: Ok(SlackBotIdentity {
                    bot_user_id: "U_BOT".to_string(),
                    team: "T1".to_string(),
                    bot_id: Some("B_BOT".to_string()),
                }),
                open: Ok(()),
                smoke: Ok(()),
            }
        }
    }

    impl SlackProbe for FakeProbe {
        fn auth_test(&self) -> Result<SlackBotIdentity, ProbeError> {
            self.identity.clone()
        }
        fn open_connection(&self) -> Result<(), ProbeError> {
            self.open.clone()
        }
        fn inbound_smoke(&self) -> Result<(), ProbeError> {
            self.smoke.clone()
        }
    }

    #[test]
    fn token_checks_require_presence_and_shape() {
        let good = SlackConfig {
            bot_token: Some("xoxb-abc".to_string()),
            app_token: Some("xapp-abc".to_string()),
            socket_mode_enabled: true,
        };
        assert!(bot_token_well_formed(&good).is_pass());
        assert!(app_token_well_formed(&good).is_pass());
        assert!(socket_mode_enabled(&good).is_pass());

        let bad = SlackConfig {
            bot_token: Some("nope".to_string()),
            app_token: None,
            socket_mode_enabled: false,
        };
        assert!(bot_token_well_formed(&bad).is_blocking_failure());
        assert!(app_token_well_formed(&bad).is_blocking_failure());
        assert!(socket_mode_enabled(&bad).is_blocking_failure());
    }

    #[test]
    fn identity_check_passes_and_detects_mismatch() {
        let probe = FakeProbe::default();
        assert!(web_api_identity(&probe, None).is_pass());
        assert!(web_api_identity(&probe, Some("U_BOT")).is_pass());
        assert!(web_api_identity(&probe, Some("U_OTHER")).is_blocking_failure());
    }

    #[test]
    fn identity_check_fails_on_auth_error() {
        let probe = FakeProbe {
            identity: Err(ProbeError::Auth("invalid_auth".to_string())),
            ..Default::default()
        };
        assert!(web_api_identity(&probe, None).is_blocking_failure());
        assert!(delivery_or_auth(&probe).is_blocking_failure());
    }

    #[test]
    fn connection_and_smoke_reflect_probe_results() {
        let ok = FakeProbe::default();
        assert!(connections_open(&ok).is_pass());
        assert!(inbound_event_smoke(&ok).is_pass());
        assert!(delivery_or_auth(&ok).is_pass());

        let failing = FakeProbe {
            open: Err(ProbeError::Api("not_allowed_token_type".to_string())),
            smoke: Err(ProbeError::Network("timeout".to_string())),
            ..Default::default()
        };
        assert!(connections_open(&failing).is_blocking_failure());
        assert!(inbound_event_smoke(&failing).is_blocking_failure());
    }
}
