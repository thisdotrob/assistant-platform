//! Errors for the upgrade and conformance flow.

use std::path::PathBuf;

use claw_core::LoadError;
use claw_db::DbError;
use claw_session::SessionError;

#[derive(Debug)]
pub enum UpgradeError {
    /// A central DB operation (open, read meta, migrate) failed.
    Db(DbError),
    /// A per-session DB operation (open, read schema version, migrate) failed.
    Session(SessionError),
    /// Loading or parsing a platform/product manifest failed.
    Load(LoadError),
    /// A filesystem operation against the instance tree failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Upgrade was asked to run against an instance whose central DB is absent.
    InstanceNotInitialized { path: PathBuf },
    /// A write target resolved outside the instance root (a corrupt layout or a
    /// traversal attempt). Refused before any write.
    WriteOutsideRoot { path: PathBuf },
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::Db(e) => write!(f, "central db error: {e}"),
            UpgradeError::Session(e) => write!(f, "session db error: {e}"),
            UpgradeError::Load(e) => write!(f, "manifest load error: {e}"),
            UpgradeError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            UpgradeError::InstanceNotInitialized { path } => write!(
                f,
                "no instance to upgrade: central db {} does not exist",
                path.display()
            ),
            UpgradeError::WriteOutsideRoot { path } => write!(
                f,
                "refusing write outside instance root: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for UpgradeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UpgradeError::Db(e) => Some(e),
            UpgradeError::Session(e) => Some(e),
            UpgradeError::Load(e) => Some(e),
            UpgradeError::Io { source, .. } => Some(source),
            UpgradeError::InstanceNotInitialized { .. } | UpgradeError::WriteOutsideRoot { .. } => {
                None
            }
        }
    }
}

impl From<DbError> for UpgradeError {
    fn from(value: DbError) -> Self {
        UpgradeError::Db(value)
    }
}

impl From<SessionError> for UpgradeError {
    fn from(value: SessionError) -> Self {
        UpgradeError::Session(value)
    }
}

impl From<LoadError> for UpgradeError {
    fn from(value: LoadError) -> Self {
        UpgradeError::Load(value)
    }
}
