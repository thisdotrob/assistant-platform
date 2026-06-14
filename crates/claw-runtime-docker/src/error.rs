//! Runtime error types.

use std::path::PathBuf;

/// A mount that violates the container mount contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountError {
    /// A host path is not under any allowed mount root.
    OutsideAllowlist { host_path: PathBuf },
    /// A host path contains a `..` component and could traverse out of an
    /// allowed root even if it textually starts under one.
    PathTraversal { host_path: PathBuf },
    /// A blocked credential path (e.g. `.env`, private keys) was requested.
    BlockedPath { host_path: PathBuf },
    /// The inbound DB was mounted writable; it must be read-only.
    InboundNotReadOnly,
    /// Two mounts target the same container path.
    DuplicateTarget { container_path: PathBuf },
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::OutsideAllowlist { host_path } => {
                write!(f, "mount source {} is outside the allowlist", host_path.display())
            }
            MountError::PathTraversal { host_path } => {
                write!(f, "mount source {} contains a parent-dir traversal", host_path.display())
            }
            MountError::BlockedPath { host_path } => {
                write!(f, "mount source {} is a blocked credential path", host_path.display())
            }
            MountError::InboundNotReadOnly => {
                write!(f, "inbound.db must be mounted read-only")
            }
            MountError::DuplicateTarget { container_path } => {
                write!(f, "duplicate container mount target {}", container_path.display())
            }
        }
    }
}

impl std::error::Error for MountError {}

/// Why a Claude runner environment could not be prepared. On any of these the
/// host must NOT fall back to raw token injection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    OneCliProxyMissing,
    AnthropicSecretMissing,
    PlaceholderInjectionFailed,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::OneCliProxyMissing => write!(f, "OneCLI proxy/CA config is not applied"),
            AuthError::AnthropicSecretMissing => {
                write!(f, "OneCLI Anthropic secret is not present for this instance")
            }
            AuthError::PlaceholderInjectionFailed => {
                write!(f, "OneCLI placeholder injection does not work through the runner container")
            }
        }
    }
}

impl std::error::Error for AuthError {}

/// Why a spawn could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The session DB schema version is outside the runner's supported range.
    UnsupportedSessionSchema { found: u32, min: u32, max: u32 },
    Mount(MountError),
    Auth(AuthError),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::UnsupportedSessionSchema { found, min, max } => write!(
                f,
                "session schema version {found} is outside supported range {min}..={max}"
            ),
            SpawnError::Mount(e) => write!(f, "{e}"),
            SpawnError::Auth(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<MountError> for SpawnError {
    fn from(value: MountError) -> Self {
        SpawnError::Mount(value)
    }
}

impl From<AuthError> for SpawnError {
    fn from(value: AuthError) -> Self {
        SpawnError::Auth(value)
    }
}
