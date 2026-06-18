//! Auth middleware and server settings — the seam between the wire and the app.
//!
//! `assistant-web` carries no async runtime or socket code (the rest of the platform
//! is synchronous and transport-free, deferring real I/O behind seams). The host
//! owns the listener; before it dispatches a parsed [`Request`] it runs it past
//! [`authenticate`], which is the single auth choke point. Everything is denied
//! unless a valid token is presented, so a new route is private by default.
//!
//! Two credential paths are honored: an `Authorization: Bearer` header (the
//! steady state) and a one-time `?token=` query param. A valid query token is
//! never served content directly — it is *exchanged*: the middleware returns a
//! redirect to the same path with the token stripped, so the secret leaves the
//! URL before anything logs or stores it. The host issues the client-side
//! credential (e.g. a cookie) on that redirect.

use crate::auth::{
    bearer_token, cookie_token, query_token, session_cookie, strip_token_query, AuthReject,
    TokenStore,
};
use crate::http::{Request, Response};

/// Where the UI listens. Loopback by default: the operational UI is
/// single-instance and must not be reachable off-box without an explicit,
/// deliberate change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSettings {
    pub bind: String,
    pub port: u16,
    /// Origins permitted on state-changing requests, for defense-in-depth when
    /// a cookie credential is in play. Empty means "same-origin only" — any
    /// request carrying an `Origin` header is rejected on mutating verbs.
    pub allowed_origins: Vec<String>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8787,
            allowed_origins: Vec::new(),
        }
    }
}

impl ServerSettings {
    pub fn is_loopback(&self) -> bool {
        self.bind == "127.0.0.1" || self.bind == "::1" || self.bind == "localhost"
    }
}

/// What the auth middleware decided about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    /// Credential valid — proceed to dispatch.
    Allow,
    /// No valid credential — return this 401 instead of dispatching.
    Reject(Response),
    /// A one-time query token was valid. Return a redirect to the cleaned URL,
    /// already carrying the `Set-Cookie` that establishes the browser session.
    Exchange(Response),
}

/// The single auth gate, tried in order: an `Authorization: Bearer` header (the
/// steady state for tools), then the `web_session` cookie (the browser session),
/// then a one-time `?token=` query param (exchanged into a cookie via redirect).
/// Anything else is rejected, so a new route is private by default.
///
/// The two credentials differ in CSRF exposure, so mutating requests get a
/// credential-specific origin check: a Bearer is never auto-attached by a
/// browser (no CSRF), whereas the cookie is — so cookie-authenticated mutations
/// must additionally present an allowlisted `Origin`.
pub fn authenticate(store: &TokenStore, settings: &ServerSettings, req: &Request) -> AuthDecision {
    if let Some(token) = req.header("Authorization").and_then(bearer_token) {
        if !store.verify(token).is_authenticated() {
            return AuthDecision::Reject(unauthorized(AuthReject::Invalid));
        }
        if req.method.is_mutating() && !bearer_origin_ok(settings, req) {
            return AuthDecision::Reject(forbidden_origin());
        }
        return AuthDecision::Allow;
    }

    if let Some(token) = req.header("Cookie").and_then(cookie_token) {
        if !store.verify(token).is_authenticated() {
            return AuthDecision::Reject(unauthorized(AuthReject::Invalid));
        }
        if req.method.is_mutating() && !cookie_origin_ok(settings, req) {
            return AuthDecision::Reject(forbidden_origin());
        }
        return AuthDecision::Allow;
    }

    if let Some(token) = query_token(&req.query) {
        return match store.verify(&token) {
            o if o.is_authenticated() => {
                let cleaned = clean_url(&req.path, &req.query);
                AuthDecision::Exchange(
                    Response::redirect(cleaned).with_header("Set-Cookie", session_cookie(&token)),
                )
            }
            _ => AuthDecision::Reject(unauthorized(AuthReject::Invalid)),
        };
    }

    AuthDecision::Reject(unauthorized(AuthReject::Missing))
}

/// Bearer mutations: an *absent* `Origin` is allowed because non-browser clients
/// (curl, the platform's own tools) legitimately send none, and a Bearer is
/// never auto-attached by a browser so it cannot be cross-site forged. A
/// *present* `Origin` must still be allowlisted, as defense in depth.
fn bearer_origin_ok(settings: &ServerSettings, req: &Request) -> bool {
    match req.header("Origin") {
        None => true,
        Some(origin) => settings.allowed_origins.iter().any(|o| o == origin),
    }
}

/// Cookie mutations: the credential rides on the browser automatically, so an
/// absent `Origin` can no longer be trusted. Require a present, allowlisted
/// `Origin` — the server-side half of the `SameSite=Strict` CSRF defense.
fn cookie_origin_ok(settings: &ServerSettings, req: &Request) -> bool {
    match req.header("Origin") {
        Some(origin) => settings.allowed_origins.iter().any(|o| o == origin),
        None => false,
    }
}

fn forbidden_origin() -> Response {
    Response::text(403, "origin not allowed")
}

fn clean_url(path: &str, query: &str) -> String {
    let stripped = strip_token_query(query);
    if stripped.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{stripped}")
    }
}

fn unauthorized(reason: AuthReject) -> Response {
    let detail = match reason {
        AuthReject::Missing => "missing credential",
        AuthReject::Invalid => "invalid credential",
        AuthReject::Revoked => "credential revoked",
    };
    Response::text(401, detail).with_header("WWW-Authenticate", "Bearer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::WebToken;
    use crate::http::Method;

    fn store() -> TokenStore {
        TokenStore::for_token(&WebToken::from_secret("good-token"), 1)
    }

    #[test]
    fn loopback_is_the_default_bind() {
        let s = ServerSettings::default();
        assert!(s.is_loopback());
        assert_eq!(s.port, 8787);
    }

    #[test]
    fn valid_bearer_is_allowed() {
        let req = Request::new(Method::Get, "/api/overview")
            .with_header("Authorization", "Bearer good-token");
        assert_eq!(
            authenticate(&store(), &ServerSettings::default(), &req),
            AuthDecision::Allow
        );
    }

    #[test]
    fn missing_and_invalid_credentials_are_401() {
        let settings = ServerSettings::default();
        let none = Request::new(Method::Get, "/x");
        let bad = Request::new(Method::Get, "/x").with_header("Authorization", "Bearer nope");
        for req in [none, bad] {
            match authenticate(&store(), &settings, &req) {
                AuthDecision::Reject(resp) => assert_eq!(resp.status, 401),
                other => panic!("expected reject, got {other:?}"),
            }
        }
    }

    #[test]
    fn revoked_store_rejects_a_previously_good_token() {
        let mut s = store();
        s.revoke();
        let req = Request::new(Method::Get, "/x").with_header("Authorization", "Bearer good-token");
        match authenticate(&s, &ServerSettings::default(), &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 401),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn one_time_query_token_is_exchanged_and_stripped() {
        let req = Request::new(Method::Get, "/dashboard").with_query("token=good-token&tab=runs");
        match authenticate(&store(), &ServerSettings::default(), &req) {
            AuthDecision::Exchange(resp) => {
                assert_eq!(resp.status, 303);
                let loc = resp.header("location").unwrap();
                assert_eq!(loc, "/dashboard?tab=runs");
                assert!(!loc.contains("good-token"), "secret must leave the URL");
                // The redirect establishes the browser session in one hop.
                let cookie = resp.header("set-cookie").expect("exchange sets a cookie");
                assert!(cookie.contains("web_session=good-token"));
                assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
            }
            other => panic!("expected exchange, got {other:?}"),
        }
    }

    #[test]
    fn a_valid_session_cookie_authenticates_a_read() {
        let req = Request::new(Method::Get, "/api/overview")
            .with_header("Cookie", "web_session=good-token");
        assert_eq!(
            authenticate(&store(), &ServerSettings::default(), &req),
            AuthDecision::Allow
        );
    }

    #[test]
    fn an_invalid_session_cookie_is_401() {
        let req =
            Request::new(Method::Get, "/api/overview").with_header("Cookie", "web_session=nope");
        match authenticate(&store(), &ServerSettings::default(), &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 401),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn cookie_mutation_without_an_origin_is_blocked_as_csrf() {
        // A foreign site can drive a cookie-bearing POST but cannot forge the
        // Origin; an absent Origin is no longer trusted once a cookie is in play.
        let settings = ServerSettings {
            allowed_origins: vec!["http://127.0.0.1:8787".to_string()],
            ..ServerSettings::default()
        };
        let req = Request::new(Method::Post, "/api/memory/file")
            .with_header("Cookie", "web_session=good-token");
        match authenticate(&store(), &settings, &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 403),
            other => panic!("expected 403, got {other:?}"),
        }
    }

    #[test]
    fn cookie_mutation_from_a_foreign_origin_is_blocked() {
        let settings = ServerSettings {
            allowed_origins: vec!["http://127.0.0.1:8787".to_string()],
            ..ServerSettings::default()
        };
        let req = Request::new(Method::Post, "/api/memory/file")
            .with_header("Cookie", "web_session=good-token")
            .with_header("Origin", "https://evil.example");
        match authenticate(&store(), &settings, &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 403),
            other => panic!("expected 403, got {other:?}"),
        }
    }

    #[test]
    fn cookie_mutation_from_the_allowed_origin_is_permitted() {
        let settings = ServerSettings {
            allowed_origins: vec!["http://127.0.0.1:8787".to_string()],
            ..ServerSettings::default()
        };
        let req = Request::new(Method::Post, "/api/memory/file")
            .with_header("Cookie", "web_session=good-token")
            .with_header("Origin", "http://127.0.0.1:8787");
        assert_eq!(authenticate(&store(), &settings, &req), AuthDecision::Allow);
    }

    #[test]
    fn invalid_query_token_is_rejected_not_exchanged() {
        let req = Request::new(Method::Get, "/dashboard").with_query("token=nope");
        match authenticate(&store(), &ServerSettings::default(), &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 401),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn mutating_request_from_a_foreign_origin_is_blocked() {
        let settings = ServerSettings::default(); // no allowed origins
        let req = Request::new(Method::Post, "/api/approvals/1/grant")
            .with_header("Authorization", "Bearer good-token")
            .with_header("Origin", "https://evil.example");
        match authenticate(&store(), &settings, &req) {
            AuthDecision::Reject(resp) => assert_eq!(resp.status, 403),
            other => panic!("expected 403, got {other:?}"),
        }
    }

    #[test]
    fn mutating_request_from_an_allowed_origin_passes_origin_check() {
        let settings = ServerSettings {
            allowed_origins: vec!["http://127.0.0.1:8787".to_string()],
            ..ServerSettings::default()
        };
        let req = Request::new(Method::Post, "/api/approvals/1/grant")
            .with_header("Authorization", "Bearer good-token")
            .with_header("Origin", "http://127.0.0.1:8787");
        assert_eq!(authenticate(&store(), &settings, &req), AuthDecision::Allow);
    }
}
