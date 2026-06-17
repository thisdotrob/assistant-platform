//! Composition for the operator web UI: build the route table over
//! [`HostWebApp`] and issue/load the bearer token.
//!
//! The page routes ([`assistant_web::pages`]) and the memory browser/editor
//! routes ([`assistant_web::memory_api`]) register onto one `Router<HostWebApp>`
//! because the host type satisfies both `WebApp` and `MemoryApp`.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use assistant_web::{Router, TokenStore, WebToken};

use crate::web::HostWebApp;

/// Build the full route table: read pages + the memory browser/editor, all over
/// the host's central-DB- and memory-backed [`HostWebApp`].
pub fn build_router() -> Router<HostWebApp> {
    let mut router = Router::new();
    assistant_web::pages::register(&mut router);
    assistant_web::memory_api::register(&mut router);
    router
}

/// Load the persisted auth state, or mint it on first run.
///
/// The plaintext bearer secret exists exactly once — when first created — so the
/// caller can surface a one-time `?token=` link. On every later start the file
/// already holds the hash, so `None` is returned and the operator reuses the
/// secret they were given (or rotates it deliberately).
pub fn ensure_token(path: &Path) -> io::Result<(TokenStore, Option<WebToken>)> {
    if path.exists() {
        Ok((TokenStore::load(path)?, None))
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (store, token) = TokenStore::create(now)?;
        store.save(path)?;
        Ok((store, Some(token)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_web::AuthOutcome;

    #[test]
    fn token_is_minted_once_then_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-token.json");

        // First call mints a fresh secret and persists its hash.
        let (store, minted) = ensure_token(&path).unwrap();
        let secret = minted.expect("first run returns the plaintext once");
        assert_eq!(store.verify(secret.expose()), AuthOutcome::Authenticated);
        assert!(path.exists());

        // A later call loads the same store and returns no new plaintext.
        let (loaded, again) = ensure_token(&path).unwrap();
        assert!(again.is_none(), "the secret is only surfaced at creation");
        assert_eq!(loaded.verify(secret.expose()), AuthOutcome::Authenticated);
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-token.json");
        ensure_token(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
