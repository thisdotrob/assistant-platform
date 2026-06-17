//! Product namespace and per-instance path derivation.
//!
//! An instance lives under the user's home directory at `~/.<namespace>` for
//! the default instance, or `~/.<namespace>-<instance>` for a named instance.
//! Namespaces forbid hyphens, so the derived directory name is unambiguous: a
//! default-instance directory never collides with a named-instance directory of
//! a different product.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    InvalidNamespace(String),
    InvalidInstance(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::InvalidNamespace(ns) => write!(
                f,
                "invalid product namespace {ns:?}: expected lowercase ascii starting with a letter, digits allowed, no hyphens"
            ),
            PathError::InvalidInstance(name) => write!(
                f,
                "invalid instance name {name:?}: expected lowercase ascii alphanumerics with single internal hyphens"
            ),
        }
    }
}

impl std::error::Error for PathError {}

/// A product namespace must be lowercase ascii, start with a letter, contain
/// only letters and digits, and contain no hyphens. Forbidding hyphens is what
/// keeps instance-directory derivation injective.
pub fn validate_namespace(namespace: &str) -> Result<(), PathError> {
    let mut chars = namespace.chars();
    let starts_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = namespace
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if starts_ok && rest_ok {
        Ok(())
    } else {
        Err(PathError::InvalidNamespace(namespace.to_string()))
    }
}

/// An instance name is lowercase ascii alphanumerics with optional single
/// internal hyphens (no leading/trailing/double hyphens).
pub fn validate_instance(name: &str) -> Result<(), PathError> {
    let charset_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let shape_ok = !name.starts_with('-') && !name.ends_with('-') && !name.contains("--");
    if charset_ok && shape_ok {
        Ok(())
    } else {
        Err(PathError::InvalidInstance(name.to_string()))
    }
}

/// The leaf directory name (e.g. `.assistant` or `.cleoclaw-work`).
pub fn instance_dir_name(namespace: &str, instance: Option<&str>) -> Result<String, PathError> {
    validate_namespace(namespace)?;
    match instance {
        None => Ok(format!(".{namespace}")),
        Some(name) => {
            validate_instance(name)?;
            Ok(format!(".{namespace}-{name}"))
        }
    }
}

/// The fully derived on-disk layout for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLayout {
    pub root: PathBuf,
}

impl InstanceLayout {
    pub fn derive(home: &Path, namespace: &str, instance: Option<&str>) -> Result<Self, PathError> {
        let name = instance_dir_name(namespace, instance)?;
        Ok(Self {
            root: home.join(name),
        })
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn central_db_path(&self) -> PathBuf {
        self.root.join("main.db")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Root for per-agent-group memory trees. A single agent group's memory
    /// markdown lives under `groups/orchestrator/memory` (see
    /// `assistant_memory::MemoryRoot::orchestrator`).
    pub fn groups_dir(&self) -> PathBuf {
        self.root.join("groups")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn setup_dir(&self) -> PathBuf {
        self.root.join("setup")
    }

    pub fn setup_state_path(&self) -> PathBuf {
        self.setup_dir().join("state.json")
    }

    pub fn readiness_path(&self) -> PathBuf {
        self.setup_dir().join("readiness.json")
    }

    /// Where the operator web UI's auth state (the hash of the active bearer
    /// secret, never the plaintext) is persisted, written `0600`.
    pub fn web_token_path(&self) -> PathBuf {
        self.root.join("web-token.json")
    }

    pub fn setup_log_path(&self) -> PathBuf {
        self.logs_dir().join("setup.log")
    }

    /// Directories the bootstrap step is allowed to create, in creation order.
    pub fn managed_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.root.clone(),
            self.sessions_dir(),
            self.logs_dir(),
            self.setup_dir(),
        ]
    }
}

/// The user's home directory from the environment, if set.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_instance_has_no_suffix() {
        assert_eq!(instance_dir_name("assistant", None).unwrap(), ".assistant");
        assert_eq!(instance_dir_name("cleoclaw", None).unwrap(), ".cleoclaw");
    }

    #[test]
    fn named_instance_appends_suffix() {
        assert_eq!(
            instance_dir_name("assistant", Some("work")).unwrap(),
            ".assistant-work"
        );
    }

    #[test]
    fn namespace_with_hyphen_is_rejected() {
        assert!(matches!(
            validate_namespace("assistant-work"),
            Err(PathError::InvalidNamespace(_))
        ));
        assert!(matches!(
            instance_dir_name("assistant-work", None),
            Err(PathError::InvalidNamespace(_))
        ));
    }

    #[test]
    fn instance_name_shape_is_validated() {
        assert!(validate_instance("work").is_ok());
        assert!(validate_instance("work-2").is_ok());
        assert!(matches!(
            validate_instance("-work"),
            Err(PathError::InvalidInstance(_))
        ));
        assert!(matches!(
            validate_instance("work-"),
            Err(PathError::InvalidInstance(_))
        ));
        assert!(matches!(
            validate_instance("a--b"),
            Err(PathError::InvalidInstance(_))
        ));
    }

    #[test]
    fn distinct_products_and_instances_never_collide() {
        // Because namespaces forbid hyphens, no default-instance dir can equal a
        // named-instance dir of any product.
        let cases = [
            ("assistant", None),
            ("assistant", Some("work")),
            ("cleoclaw", None),
            ("cleoclaw", Some("work")),
            ("assistant", Some("staging")),
        ];
        let mut names: Vec<String> = cases
            .iter()
            .map(|(ns, inst)| instance_dir_name(ns, *inst).unwrap())
            .collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "derived instance dir names collided");
    }

    #[test]
    fn layout_paths_are_under_root() {
        let home = Path::new("/home/test");
        let layout = InstanceLayout::derive(home, "assistant", Some("work")).unwrap();
        assert_eq!(layout.root, Path::new("/home/test/.assistant-work"));
        assert_eq!(
            layout.central_db_path(),
            Path::new("/home/test/.assistant-work/main.db")
        );
        assert_eq!(
            layout.groups_dir(),
            Path::new("/home/test/.assistant-work/groups")
        );
        assert!(layout.config_path().starts_with(&layout.root));
        assert!(layout.setup_state_path().starts_with(&layout.root));
        assert!(layout.managed_dirs().iter().all(|d| d.starts_with(&layout.root)));
    }
}
