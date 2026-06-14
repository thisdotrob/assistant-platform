//! Errors for the per-session DB protocol.

use std::path::PathBuf;

use crate::layout::DbKind;

#[derive(Debug)]
pub enum SessionError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// An identifier (agent group or session id) contained characters that
    /// could escape the sessions root.
    InvalidId(String),
    /// A registered migration's content no longer matches the recorded checksum.
    ChecksumMismatch {
        db_kind: DbKind,
        version: u32,
        recorded: String,
        computed: String,
    },
    /// Migrations were declared out of order or with a duplicate version.
    BadMigrationSequence {
        db_kind: DbKind,
        version: u32,
    },
    /// The on-disk schema version is outside the runner's supported range.
    UnsupportedSchemaVersion {
        db_kind: DbKind,
        found: u32,
        supported_min: u32,
        supported_max: u32,
    },
    /// A sequence number violated host-even / container-odd parity.
    SequenceParity {
        db_kind: DbKind,
        seq: i64,
    },
    /// A read-write outbound open (or recovery) was attempted while a container
    /// still appears to be alive.
    ContainerAlive {
        detail: String,
    },
    /// The exclusive session lock is already held.
    SessionLocked {
        path: PathBuf,
    },
    /// An attachment path escaped its session-scoped base directory.
    AttachmentEscape {
        path: PathBuf,
    },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            SessionError::Json(e) => write!(f, "json error: {e}"),
            SessionError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            SessionError::InvalidId(id) => write!(
                f,
                "invalid session identifier {id:?}: expected ascii alphanumerics, '-' or '_'"
            ),
            SessionError::ChecksumMismatch {
                db_kind,
                version,
                recorded,
                computed,
            } => write!(
                f,
                "{db_kind} migration {version} checksum mismatch: recorded {recorded}, registered {computed}"
            ),
            SessionError::BadMigrationSequence { db_kind, version } => {
                write!(f, "{db_kind} migration sequence broken at version {version}")
            }
            SessionError::UnsupportedSchemaVersion {
                db_kind,
                found,
                supported_min,
                supported_max,
            } => write!(
                f,
                "{db_kind} schema version {found} unsupported by runner (supports {supported_min}..={supported_max})"
            ),
            SessionError::SequenceParity { db_kind, seq } => {
                write!(f, "{db_kind} sequence {seq} violates parity")
            }
            SessionError::ContainerAlive { detail } => {
                write!(f, "refusing outbound recovery: container alive ({detail})")
            }
            SessionError::SessionLocked { path } => {
                write!(f, "session lock already held at {}", path.display())
            }
            SessionError::AttachmentEscape { path } => {
                write!(f, "attachment path escapes session directory: {}", path.display())
            }
        }
    }
}

impl std::error::Error for SessionError {}

impl From<rusqlite::Error> for SessionError {
    fn from(value: rusqlite::Error) -> Self {
        SessionError::Sqlite(value)
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(value: serde_json::Error) -> Self {
        SessionError::Json(value)
    }
}
