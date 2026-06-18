//! Token-based auth for the single-instance web UI.
//!
//! The instance holds one bearer secret. We never persist the secret itself —
//! only its SHA-256 hash, in a file written with `0600` perms — and we compare
//! in constant time so a wrong guess leaks no timing signal. The plaintext is
//! returned exactly once, at generation or rotation, for the operator to use;
//! after that only the hash survives. Logs get a non-reversible fingerprint
//! (`redact`) so a token can be correlated across lines without ever appearing.
//!
//! A token may arrive two ways: as an `Authorization: Bearer <token>` header
//! (the normal path) or as a `?token=<token>` query param for a one-time link.
//! The query form is meant to be exchanged immediately and stripped from the
//! URL (see [`strip_token_query`]) so the secret does not linger in history,
//! referrers, or server logs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;

/// Width of a freshly generated secret, in random bytes (hex-encoded to 64
/// chars). 256 bits of entropy.
const TOKEN_BYTES: usize = 32;

/// A plaintext web secret. Held only transiently — generation and rotation hand
/// one back, and it is never serialized or logged. `Debug` is redacted so it
/// cannot leak through a stray `{:?}`.
#[derive(Clone)]
pub struct WebToken(String);

impl WebToken {
    /// Generate a fresh 256-bit secret from the OS CSPRNG (`/dev/urandom`).
    pub fn generate() -> io::Result<Self> {
        let bytes = os_random(TOKEN_BYTES)?;
        Ok(Self(to_hex(&bytes)))
    }

    /// Wrap an existing secret string (used by callers holding a known token,
    /// and by tests). The value is treated as opaque.
    pub fn from_secret(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret as a string, for placing in an `Authorization` header or link.
    /// Handle with care: this is the live credential.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The non-reversible fingerprint of this token, for logging.
    pub fn fingerprint(&self) -> String {
        redact(&self.0)
    }
}

impl std::fmt::Debug for WebToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebToken({})", redact(&self.0))
    }
}

/// The persisted auth state: the hash of the active secret plus its lifecycle.
/// Never contains the plaintext.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenStore {
    /// Hex SHA-256 of the active secret.
    token_hash: String,
    created_at: i64,
    rotated_at: Option<i64>,
    revoked: bool,
}

/// Why a presented token was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthReject {
    /// No credential was presented at all.
    Missing,
    /// A credential was presented but did not match the active secret.
    Invalid,
    /// The active secret has been revoked; no token authenticates.
    Revoked,
}

/// The result of checking a presented credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    Authenticated,
    Rejected(AuthReject),
}

impl AuthOutcome {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthOutcome::Authenticated)
    }
}

impl TokenStore {
    /// Build a store for a known token (hashing it at rest).
    pub fn for_token(token: &WebToken, now: i64) -> Self {
        Self {
            token_hash: hash_hex(token.expose()),
            created_at: now,
            rotated_at: None,
            revoked: false,
        }
    }

    /// Generate a new secret and a store wrapping its hash. The returned
    /// [`WebToken`] is the only time the plaintext exists — persist the store
    /// and surface the token to the operator, then drop it.
    pub fn create(now: i64) -> io::Result<(Self, WebToken)> {
        let token = WebToken::generate()?;
        Ok((Self::for_token(&token, now), token))
    }

    /// Check a presented secret against the active hash in constant time.
    pub fn verify(&self, presented: &str) -> AuthOutcome {
        if self.revoked {
            return AuthOutcome::Rejected(AuthReject::Revoked);
        }
        if ct_eq(hash_hex(presented).as_bytes(), self.token_hash.as_bytes()) {
            AuthOutcome::Authenticated
        } else {
            AuthOutcome::Rejected(AuthReject::Invalid)
        }
    }

    /// Replace the active secret with a fresh one. The old secret stops
    /// verifying immediately. Returns the new plaintext (handle once).
    pub fn rotate(&mut self, now: i64) -> io::Result<WebToken> {
        let token = WebToken::generate()?;
        self.token_hash = hash_hex(token.expose());
        self.rotated_at = Some(now);
        self.revoked = false;
        Ok(token)
    }

    /// Revoke the active secret. After this no token authenticates until a
    /// rotation issues a new one.
    pub fn revoke(&mut self) {
        self.revoked = true;
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked
    }

    /// Persist atomically with `0600` perms (temp file in the same dir, then
    /// rename) so the secret-bearing state is never world-readable, even mid-write.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private(path, &body)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// A non-reversible fingerprint of a secret: the first 8 hex chars of its
/// SHA-256. Safe to log — it identifies a token across lines without revealing
/// it (the full hash is 64 chars; 8 is enough to disambiguate in practice).
pub fn redact(secret: &str) -> String {
    let h = hash_hex(secret);
    format!("tok:{}", &h[..8])
}

/// Extract the bearer token from an `Authorization` header value, if it is a
/// well-formed `Bearer <token>`. Case-insensitive on the scheme.
pub fn bearer_token(authorization: &str) -> Option<&str> {
    let rest = authorization.strip_prefix("Bearer ").or_else(|| {
        authorization
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("bearer "))
            .map(|_| &authorization[7..])
    })?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Pull the `token` param out of a raw query string (`a=1&token=XYZ&b=2`).
pub fn query_token(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == "token")
        .map(|(_, v)| v.to_string())
}

/// The cookie that carries the browser session credential after a one-time
/// `?token=` link is exchanged. Steady-state browser auth rides on this; the
/// secret never appears in a URL again.
pub const SESSION_COOKIE: &str = "web_session";

/// Pull the session token out of a `Cookie` header value
/// (`a=1; web_session=XYZ; b=2`). Cookie pairs are `;`-separated.
pub fn cookie_token(cookie_header: &str) -> Option<&str> {
    cookie_header
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// Build the `Set-Cookie` value that establishes the browser session on the
/// exchange redirect. `HttpOnly` keeps it out of JavaScript, `SameSite=Strict`
/// stops it riding cross-site requests (the CSRF basis), `Path=/` scopes it to
/// the whole UI, and `Max-Age` bounds its life so a stale tab re-auths. No
/// `Secure`: the UI is loopback-only HTTP, where `Secure` would suppress the
/// cookie entirely.
pub fn session_cookie(token: &str) -> String {
    const MAX_AGE_SECS: u32 = 12 * 60 * 60;
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={MAX_AGE_SECS}")
}

/// Rebuild a query string with any `token` param removed, so a one-time link
/// can be stripped before it is stored anywhere. Returns `""` when nothing else
/// remains.
pub fn strip_token_query(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| p.split_once('=').map(|(k, _)| k != "token").unwrap_or(true))
        .collect::<Vec<_>>()
        .join("&")
}

/// Read `n` bytes from the OS CSPRNG. The platform uses no RNG crate, so we read
/// `/dev/urandom` directly (available on the unix hosts we target).
fn os_random(n: usize) -> io::Result<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn hash_hex(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// Length-aware constant-time byte compare: no early return on the first
/// differing byte, so timing does not reveal how much of a guess was right.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write `body` to `path` atomically and privately: a temp sibling created with
/// `0600`, written, then renamed over the target.
fn write_private(path: &Path, body: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "token path has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "token path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.tmp"));

    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    // Re-assert perms in case the file pre-existed with looser bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_256_bit() {
        let a = WebToken::generate().unwrap();
        let b = WebToken::generate().unwrap();
        assert_eq!(a.expose().len(), 64, "32 bytes hex-encoded");
        assert_ne!(a.expose(), b.expose());
    }

    #[test]
    fn verify_accepts_the_active_secret_and_rejects_others() {
        let token = WebToken::from_secret("s3cr3t");
        let store = TokenStore::for_token(&token, 100);
        assert_eq!(store.verify("s3cr3t"), AuthOutcome::Authenticated);
        assert_eq!(
            store.verify("wrong"),
            AuthOutcome::Rejected(AuthReject::Invalid)
        );
    }

    #[test]
    fn rotation_invalidates_the_old_secret() {
        let mut store = TokenStore::for_token(&WebToken::from_secret("old"), 100);
        let new = store.rotate(200).unwrap();
        assert_eq!(
            store.verify("old"),
            AuthOutcome::Rejected(AuthReject::Invalid)
        );
        assert_eq!(store.verify(new.expose()), AuthOutcome::Authenticated);
    }

    #[test]
    fn revocation_rejects_every_token_until_rotation() {
        let token = WebToken::from_secret("live");
        let mut store = TokenStore::for_token(&token, 100);
        store.revoke();
        assert_eq!(
            store.verify("live"),
            AuthOutcome::Rejected(AuthReject::Revoked)
        );
        // A rotation re-enables auth with the fresh secret.
        let fresh = store.rotate(300).unwrap();
        assert_eq!(store.verify(fresh.expose()), AuthOutcome::Authenticated);
    }

    #[test]
    fn the_secret_never_appears_in_debug_or_persisted_form() {
        let token = WebToken::from_secret("super-secret-value");
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("super-secret-value"));
        assert!(dbg.contains("tok:"));

        let store = TokenStore::for_token(&token, 1);
        let json = serde_json::to_string(&store).unwrap();
        assert!(!json.contains("super-secret-value"));
        assert!(json.contains("token_hash"));
    }

    #[test]
    fn redaction_is_stable_and_non_reversible() {
        assert_eq!(redact("abc"), redact("abc"));
        assert_ne!(redact("abc"), redact("abd"));
        assert!(!redact("abc").contains("abc"));
    }

    #[test]
    fn bearer_parsing_handles_scheme_case_and_whitespace() {
        assert_eq!(bearer_token("Bearer xyz"), Some("xyz"));
        assert_eq!(bearer_token("bearer xyz"), Some("xyz"));
        assert_eq!(bearer_token("Bearer   xyz  "), Some("xyz"));
        assert_eq!(bearer_token("Basic xyz"), None);
        assert_eq!(bearer_token("Bearer "), None);
    }

    #[test]
    fn query_token_extracted_and_stripped() {
        assert_eq!(query_token("a=1&token=XYZ&b=2"), Some("XYZ".to_string()));
        assert_eq!(query_token("a=1&b=2"), None);
        assert_eq!(strip_token_query("a=1&token=XYZ&b=2"), "a=1&b=2");
        assert_eq!(strip_token_query("token=XYZ"), "");
        assert_eq!(strip_token_query(""), "");
    }

    #[test]
    fn cookie_token_is_parsed_from_the_session_pair() {
        assert_eq!(cookie_token("web_session=XYZ"), Some("XYZ"));
        assert_eq!(cookie_token("a=1; web_session=XYZ; b=2"), Some("XYZ"));
        assert_eq!(cookie_token("a=1; b=2"), None);
        assert_eq!(cookie_token("web_session="), None);
        assert_eq!(cookie_token(""), None);
    }

    #[test]
    fn session_cookie_is_httponly_samesite_strict_and_path_scoped() {
        let c = session_cookie("the-secret");
        assert!(c.starts_with("web_session=the-secret;"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("Max-Age="));
        // Loopback HTTP: must NOT be Secure or the browser drops it.
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn store_round_trips_through_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-token.json");
        let (store, token) = TokenStore::create(1234).unwrap();
        store.save(&path).unwrap();

        let loaded = TokenStore::load(&path).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.verify(token.expose()), AuthOutcome::Authenticated);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }
    }

    #[test]
    fn ct_eq_is_length_aware() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }
}
