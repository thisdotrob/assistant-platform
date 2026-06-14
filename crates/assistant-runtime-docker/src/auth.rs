//! Runner authentication: the stub path and the OneCLI-gated Claude OAuth path.
//!
//! The stub runner wakes with no Claude credentials. The Claude OAuth runner
//! never receives a raw token: the host applies OneCLI proxy/CA config, OneCLI
//! holds the `claude setup-token` OAuth token as an Anthropic secret, and the
//! container is given only `CLAUDE_CODE_OAUTH_TOKEN=placeholder`. OneCLI rewrites
//! the placeholder on outbound `api.anthropic.com` traffic.
//!
//! The hard rule (architecture: Claude SDK authentication): if OneCLI proxy
//! config, the Anthropic secret, or placeholder injection is not ready, the
//! runner must refuse — it must NOT fall back to raw token env injection.

use crate::error::AuthError;

/// The placeholder value the container receives in place of a real token.
pub const PLACEHOLDER_TOKEN: &str = "placeholder";

pub const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const RUNNER_MODE_ENV: &str = "ASSISTANT_RUNNER_MODE";

/// Which provider a runner container uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerAuthMode {
    /// Local stub provider; no credentials, no network to Anthropic.
    Stub,
    /// Real Claude Agent SDK via OneCLI placeholder rewrite.
    ClaudeOAuth,
    /// Specialist sub-agent that runs its own real Claude turn (the browser
    /// specialist drives `agent-browser` as a tool). Credentialed exactly like
    /// [`RunnerAuthMode::ClaudeOAuth`] — OneCLI-gated placeholder token, no raw
    /// credential — but tagged with its own runner mode so the shim loads the
    /// specialist responder instead of the orchestrator one.
    Specialist,
}

/// Readiness of the OneCLI-mediated Claude auth path, as probed by setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneCliReadiness {
    /// The host applied OneCLI proxy/CA config to the container.
    pub proxy_configured: bool,
    /// OneCLI holds the Anthropic OAuth secret for this instance.
    pub anthropic_secret_present: bool,
    /// Placeholder injection works through the runner container (the readiness
    /// dry-run succeeded).
    pub placeholder_injection_ok: bool,
}

impl OneCliReadiness {
    /// All three conditions hold.
    pub fn is_ready(self) -> bool {
        self.proxy_configured && self.anthropic_secret_present && self.placeholder_injection_ok
    }

    fn first_failure(self) -> Option<AuthError> {
        if !self.proxy_configured {
            Some(AuthError::OneCliProxyMissing)
        } else if !self.anthropic_secret_present {
            Some(AuthError::AnthropicSecretMissing)
        } else if !self.placeholder_injection_ok {
            Some(AuthError::PlaceholderInjectionFailed)
        } else {
            None
        }
    }
}

/// Prepare the credential-related environment for a runner container.
///
/// - Stub: returns no Claude credentials at all.
/// - ClaudeOAuth / Specialist: both run a real Claude turn, so both require
///   OneCLI readiness; on success they inject only the placeholder token and
///   explicitly clear `ANTHROPIC_API_KEY`. On any readiness failure they return
///   an error and never emit a raw token. They differ only in the runner mode
///   tag (`claude_oauth` vs `specialist`), which selects the shim responder.
pub fn prepare_runner_env(
    mode: RunnerAuthMode,
    readiness: OneCliReadiness,
) -> Result<Vec<(String, String)>, AuthError> {
    match mode {
        RunnerAuthMode::Stub => Ok(vec![(RUNNER_MODE_ENV.to_string(), "stub".to_string())]),
        RunnerAuthMode::ClaudeOAuth => claude_credentialed_env("claude_oauth", readiness),
        RunnerAuthMode::Specialist => claude_credentialed_env("specialist", readiness),
    }
}

/// The OneCLI-gated Claude credential env, shared by every mode that runs a real
/// Claude turn. Requires readiness; on success injects only the placeholder
/// token (the proxy swaps it for the real one) and clears `ANTHROPIC_API_KEY` so
/// no raw OAuth token is ever present in container env. `runner_mode` tags the
/// row the shim dispatches on.
fn claude_credentialed_env(
    runner_mode: &str,
    readiness: OneCliReadiness,
) -> Result<Vec<(String, String)>, AuthError> {
    if let Some(failure) = readiness.first_failure() {
        return Err(failure);
    }
    Ok(vec![
        (RUNNER_MODE_ENV.to_string(), runner_mode.to_string()),
        (OAUTH_TOKEN_ENV.to_string(), PLACEHOLDER_TOKEN.to_string()),
        (ANTHROPIC_API_KEY_ENV.to_string(), String::new()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> OneCliReadiness {
        OneCliReadiness {
            proxy_configured: true,
            anthropic_secret_present: true,
            placeholder_injection_ok: true,
        }
    }

    #[test]
    fn stub_wakes_without_claude_credentials() {
        let env = prepare_runner_env(RunnerAuthMode::Stub, ready()).unwrap();
        assert!(!env.iter().any(|(k, _)| k == OAUTH_TOKEN_ENV));
        assert!(!env.iter().any(|(k, _)| k == ANTHROPIC_API_KEY_ENV));
        assert_eq!(env, vec![(RUNNER_MODE_ENV.to_string(), "stub".to_string())]);
    }

    #[test]
    fn specialist_is_credentialed_like_oauth_but_tagged_specialist() {
        let env = prepare_runner_env(RunnerAuthMode::Specialist, ready()).unwrap();
        // Same OneCLI placeholder credential surface as ClaudeOAuth: placeholder
        // token injected, ANTHROPIC_API_KEY cleared, no raw token anywhere.
        let oauth = env.iter().find(|(k, _)| k == OAUTH_TOKEN_ENV).unwrap();
        assert_eq!(oauth.1, PLACEHOLDER_TOKEN);
        let api_key = env.iter().find(|(k, _)| k == ANTHROPIC_API_KEY_ENV).unwrap();
        assert_eq!(api_key.1, "");
        // Only the runner-mode tag differs from the orchestrator path.
        let mode = env.iter().find(|(k, _)| k == RUNNER_MODE_ENV).unwrap();
        assert_eq!(mode.1, "specialist");
    }

    #[test]
    fn specialist_refuses_when_onecli_unready() {
        let unready = OneCliReadiness {
            proxy_configured: false,
            ..ready()
        };
        assert_eq!(
            prepare_runner_env(RunnerAuthMode::Specialist, unready),
            Err(AuthError::OneCliProxyMissing)
        );
    }

    #[test]
    fn stub_ignores_unready_onecli() {
        let unready = OneCliReadiness {
            proxy_configured: false,
            anthropic_secret_present: false,
            placeholder_injection_ok: false,
        };
        assert!(prepare_runner_env(RunnerAuthMode::Stub, unready).is_ok());
    }

    #[test]
    fn claude_oauth_injects_only_placeholder() {
        let env = prepare_runner_env(RunnerAuthMode::ClaudeOAuth, ready()).unwrap();
        let oauth = env.iter().find(|(k, _)| k == OAUTH_TOKEN_ENV).unwrap();
        assert_eq!(oauth.1, PLACEHOLDER_TOKEN);
        // ANTHROPIC_API_KEY explicitly cleared.
        let api_key = env.iter().find(|(k, _)| k == ANTHROPIC_API_KEY_ENV).unwrap();
        assert_eq!(api_key.1, "");
        // No value anywhere is anything but the placeholder.
        assert!(env.iter().all(|(_, v)| v != "real-token"));
    }

    #[test]
    fn claude_oauth_refuses_when_proxy_missing() {
        let r = OneCliReadiness {
            proxy_configured: false,
            ..ready()
        };
        assert_eq!(
            prepare_runner_env(RunnerAuthMode::ClaudeOAuth, r),
            Err(AuthError::OneCliProxyMissing)
        );
    }

    #[test]
    fn claude_oauth_refuses_when_secret_missing() {
        let r = OneCliReadiness {
            anthropic_secret_present: false,
            ..ready()
        };
        assert_eq!(
            prepare_runner_env(RunnerAuthMode::ClaudeOAuth, r),
            Err(AuthError::AnthropicSecretMissing)
        );
    }

    #[test]
    fn claude_oauth_refuses_when_placeholder_injection_fails_and_never_falls_back() {
        let r = OneCliReadiness {
            placeholder_injection_ok: false,
            ..ready()
        };
        let result = prepare_runner_env(RunnerAuthMode::ClaudeOAuth, r);
        // Must be a refusal — never a raw-token env.
        assert_eq!(result, Err(AuthError::PlaceholderInjectionFailed));
    }
}
