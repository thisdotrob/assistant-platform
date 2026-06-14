//! Resumable setup-step state, persisted as JSON under the instance's setup dir.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::SetupError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupState {
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl SetupState {
    pub fn is_completed(&self, step: &str) -> bool {
        self.completed.iter().any(|s| s == step)
    }

    pub fn mark_completed(&mut self, step: &str) {
        if !self.is_completed(step) {
            self.completed.push(step.to_string());
        }
        self.last_error = None;
        self.touch();
    }

    pub fn record_error(&mut self, message: impl Into<String>) {
        self.last_error = Some(message.into());
        self.touch();
    }

    fn touch(&mut self) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.updated_at = Some(secs.to_string());
    }
}

/// Load setup state. A missing file means "fresh start" and yields the default.
pub fn load_state(path: &Path) -> Result<SetupState, SetupError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| SetupError::State {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SetupState::default()),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn save_state(path: &Path, state: &SetupState) -> Result<(), SetupError> {
    let text = serde_json::to_string_pretty(state).map_err(|source| SetupError::State {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(path, text).map_err(|source| SetupError::Io {
        path: path.to_path_buf(),
        source,
    })
}
