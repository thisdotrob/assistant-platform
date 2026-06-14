//! Readiness registry keyed by enabled module/profile/capability.
//!
//! Only enabled items get a check; disabled modules never appear. Later
//! milestones register real check logic — for the skeleton each check starts
//! `Pending` and results are persisted for CLI/web display.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SetupError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Module,
    Profile,
    Capability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessCheck {
    pub id: String,
    pub kind: CheckKind,
    pub status: CheckStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessRegistry {
    pub checks: Vec<ReadinessCheck>,
}

/// Items enabled for this instance — the only things that get readiness checks.
#[derive(Debug, Clone, Default)]
pub struct EnabledSurface {
    pub modules: Vec<String>,
    pub profile: Option<(String, String)>,
    pub capabilities: Vec<String>,
}

impl ReadinessRegistry {
    /// Build a registry with one pending check per enabled item. Nothing is
    /// registered for items outside the enabled surface.
    pub fn for_enabled(surface: &EnabledSurface) -> Self {
        let mut checks = Vec::new();
        for module in &surface.modules {
            checks.push(ReadinessCheck {
                id: format!("module:{module}"),
                kind: CheckKind::Module,
                status: CheckStatus::Pending,
                detail: None,
            });
        }
        if let Some((id, version)) = &surface.profile {
            checks.push(ReadinessCheck {
                id: format!("profile:{id}@{version}"),
                kind: CheckKind::Profile,
                status: CheckStatus::Pending,
                detail: None,
            });
        }
        for capability in &surface.capabilities {
            checks.push(ReadinessCheck {
                id: format!("capability:{capability}"),
                kind: CheckKind::Capability,
                status: CheckStatus::Pending,
                detail: None,
            });
        }
        Self { checks }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.checks.iter().any(|c| c.id == id)
    }

    /// Record a result for a known check id. Returns false if no such check is
    /// registered (i.e. the item was not enabled).
    pub fn record(&mut self, id: &str, status: CheckStatus, detail: Option<String>) -> bool {
        if let Some(check) = self.checks.iter_mut().find(|c| c.id == id) {
            check.status = status;
            check.detail = detail;
            true
        } else {
            false
        }
    }

    pub fn all_pass(&self) -> bool {
        self.checks
            .iter()
            .all(|c| matches!(c.status, CheckStatus::Pass | CheckStatus::Skipped))
    }
}

pub fn load_registry(path: &Path) -> Result<ReadinessRegistry, SetupError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| SetupError::State {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReadinessRegistry::default()),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn save_registry(path: &Path, registry: &ReadinessRegistry) -> Result<(), SetupError> {
    let text = serde_json::to_string_pretty(registry).map_err(|source| SetupError::State {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(path, text).map_err(|source| SetupError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_enabled_modules_get_checks() {
        let surface = EnabledSurface {
            modules: vec!["claw-core".to_string(), "claw-session".to_string()],
            profile: Some(("assistant".to_string(), "0.1.0".to_string())),
            capabilities: vec![],
        };
        let registry = ReadinessRegistry::for_enabled(&surface);
        assert!(registry.contains("module:claw-core"));
        assert!(registry.contains("module:claw-session"));
        assert!(registry.contains("profile:assistant@0.1.0"));
        // A module that is not enabled has no check.
        assert!(!registry.contains("module:claw-channel-slack"));
        assert_eq!(registry.checks.len(), 3);
    }

    #[test]
    fn record_unknown_check_is_rejected() {
        let mut registry = ReadinessRegistry::for_enabled(&EnabledSurface {
            modules: vec!["claw-core".to_string()],
            profile: None,
            capabilities: vec![],
        });
        assert!(registry.record("module:claw-core", CheckStatus::Pass, None));
        assert!(!registry.record("module:not-enabled", CheckStatus::Pass, None));
    }
}
