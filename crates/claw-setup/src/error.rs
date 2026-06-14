use std::path::PathBuf;

use claw_config::ConfigError;
use claw_db::DbError;

#[derive(Debug)]
pub enum SetupError {
    Config(ConfigError),
    Db(DbError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    State {
        path: PathBuf,
        source: serde_json::Error,
    },
    SourceMutation {
        path: PathBuf,
    },
    /// A hard setup gate did not pass, so setup cannot complete. The pipeline
    /// stops here; a resume reruns from this gate once the cause is fixed.
    Gate {
        id: String,
        detail: String,
    },
    /// A step id is not a safe identifier. Ids are used as resumable-state keys
    /// and per-step log filenames, so they must be `[a-z0-9_-]+` to keep them
    /// from escaping the logs dir via path traversal.
    InvalidStepId {
        id: String,
    },
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Config(e) => write!(f, "config error: {e}"),
            SetupError::Db(e) => write!(f, "database error: {e}"),
            SetupError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            SetupError::State { path, source } => {
                write!(f, "setup-state error at {}: {source}", path.display())
            }
            SetupError::SourceMutation { path } => write!(
                f,
                "refusing to write {}: path is inside a protected source repo",
                path.display()
            ),
            SetupError::Gate { id, detail } => {
                write!(f, "setup gate {id} did not pass: {detail}")
            }
            SetupError::InvalidStepId { id } => {
                write!(f, "invalid setup step id {id:?}: must be non-empty [a-z0-9_-]")
            }
        }
    }
}

impl std::error::Error for SetupError {}

impl From<ConfigError> for SetupError {
    fn from(value: ConfigError) -> Self {
        SetupError::Config(value)
    }
}

impl From<DbError> for SetupError {
    fn from(value: DbError) -> Self {
        SetupError::Db(value)
    }
}
