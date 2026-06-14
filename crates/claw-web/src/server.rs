//! Auth middleware and server settings — the seam between the wire and the app.
//!
//! `claw-web` carries no async runtime or socket code (the rest of the platform
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

use crate::auth::{bearer_token, query_token, strip_token_query, AuthReject, TokenStore};
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
    /// A one-time query token was valid. Return a redirect to the cleaned URL;
    /// the host attaches the durable client credential on the way out.
    Exchange(Response),
}

/// The single auth gate. Bearer header wins; a valid `?token=` is exchanged via
/// redirect; anything else is rejected. State-changing requests additionally
/// must pass the origin check.
pub fn authenticate(store: &TokenStore, settings: &ServerSettings, req: &Request) -> AuthDecision {
    if req.method.is_mutating() && !origin_allowed(settings, req) {
        return AuthDecision::Reject(Response::text(403, "origin not allowed"));
    }

    if let Some(token) = req.header("Authorization").and_then(bearer_token) {
        return match store.verify(token) {
            o if o.is_authenticated() => AuthDecision::Allow,
            _ => AuthDecision::Reject(unauthorized(AuthReject::Invalid)),
        };
    }

    if let Some(token) = query_token(&req.query) {
        return match store.verify(&token) {
            o if o.is_authenticated() => {
                let cleaned = clean_url(&req.path, &req.query);
                AuthDecision::Exchange(Response::redirect(cleaned))
            }
            _ => AuthDecision::Reject(unauthorized(AuthReject::Invalid)),
        };
    }

    AuthDecision::Reject(unauthorized(AuthReject::Missing))
}

/// On a mutating request, reject any cross-origin `Origin`. Header auth is not
/// auto-attached by browsers so CSRF does not apply to it, but a cookie-based
/// exchange could be cross-site forged — this check closes that path.
///
/// An *absent* `Origin` is allowed here, because non-browser clients (curl, the
/// platform's own tools) legitimately send none and our auth basis is the
/// Bearer header. If the host introduces a cookie credential on the exchange
/// redirect, absent-Origin must no longer be trusted: tighten this to require a
/// present, allowlisted `Origin` (or `Sec-Fetch-Site: same-origin`) for
/// cookie-authenticated mutations.
fn origin_allowed(settings: &ServerSettings, req: &Request) -> bool {
    match req.header("Origin") {
        None => true, // non-browser / same-origin tools send no Origin
        Some(origin) => settings.allowed_origins.iter().any(|o| o == origin),
    }
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
            }
            other => panic!("expected exchange, got {other:?}"),
        }
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
