//! Per-agent memory roots and the hard isolation boundary between them.
//!
//! Each agent group owns exactly one memory root on disk. The orchestrator's
//! lives at `groups/orchestrator/memory`; a specialist's at
//! `groups/specialists/<folder>/memory`. One agent may never mount, search, or
//! write another agent's root: every path the host resolves for an agent is
//! confined lexically to that agent's root (rejecting `..`, absolute, and
//! prefix escapes), and every entry written must declare the owning agent as
//! its `owner_agent_group_id`. Confinement here mirrors the attachment-path
//! safety in `assistant-session` and the artifact confinement in the browser
//! specialist, so the three boundaries enforce the same rule.

use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsolationError {
    /// A relative path tried to escape the root (empty, absolute, or `..`).
    Escape { path: PathBuf },
    /// A resolved path lands outside the agent's own memory root — i.e. it would
    /// touch another agent's memory.
    OutsideRoot { path: PathBuf },
    /// A specialist folder name was not a single safe path segment.
    InvalidSpecialistFolder { folder: String },
    /// An entry's declared owner does not match the root it is being written to.
    OwnerMismatch { expected: String, found: String },
}

impl std::fmt::Display for IsolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsolationError::Escape { path } => {
                write!(f, "memory path {path:?} escapes the agent memory root")
            }
            IsolationError::OutsideRoot { path } => {
                write!(f, "memory path {path:?} is outside this agent's memory root")
            }
            IsolationError::InvalidSpecialistFolder { folder } => {
                write!(f, "specialist folder {folder:?} is not a single safe path segment")
            }
            IsolationError::OwnerMismatch { expected, found } => write!(
                f,
                "entry owner {found:?} does not match memory root owner {expected:?}"
            ),
        }
    }
}

impl std::error::Error for IsolationError {}

/// True when `candidate` is a single, safe path segment (no separators, no `..`,
/// not absolute, not empty).
fn is_safe_segment(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let p = Path::new(candidate);
    let mut comps = p.components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// Reject relative candidates that could escape a root: empty, absolute, or
/// containing `..`/root/prefix components.
fn lexically_contained(candidate: &Path) -> bool {
    if candidate.as_os_str().is_empty() || candidate.is_absolute() {
        return false;
    }
    !candidate.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// One agent group's memory root: the logical owning agent-group ID plus the
/// on-disk directory that no other agent may touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRoot {
    owner_agent_group_id: String,
    root: PathBuf,
}

impl MemoryRoot {
    /// The orchestrator's memory root under `groups/orchestrator/memory`.
    pub fn orchestrator(groups_dir: &Path, owner_agent_group_id: impl Into<String>) -> Self {
        Self {
            owner_agent_group_id: owner_agent_group_id.into(),
            root: groups_dir.join("orchestrator").join("memory"),
        }
    }

    /// A specialist's memory root under `groups/specialists/<folder>/memory`. The
    /// folder must be a single safe path segment so it cannot itself escape.
    pub fn specialist(
        groups_dir: &Path,
        owner_agent_group_id: impl Into<String>,
        specialist_folder: &str,
    ) -> Result<Self, IsolationError> {
        if !is_safe_segment(specialist_folder) {
            return Err(IsolationError::InvalidSpecialistFolder {
                folder: specialist_folder.to_string(),
            });
        }
        Ok(Self {
            owner_agent_group_id: owner_agent_group_id.into(),
            root: groups_dir
                .join("specialists")
                .join(specialist_folder)
                .join("memory"),
        })
    }

    pub fn owner_agent_group_id(&self) -> &str {
        &self.owner_agent_group_id
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Resolve a relative path (e.g. `people/alice.md`) inside this root,
    /// rejecting anything that could escape it.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, IsolationError> {
        let rel = Path::new(relative);
        if !lexically_contained(rel) {
            return Err(IsolationError::Escape {
                path: rel.to_path_buf(),
            });
        }
        let joined = self.root.join(rel);
        if joined.starts_with(&self.root) {
            Ok(joined)
        } else {
            Err(IsolationError::Escape { path: joined })
        }
    }

    /// Confirm an already-resolved path stays inside this agent's root. This is
    /// the cross-agent guard: a path under a *different* agent's root is
    /// rejected as `OutsideRoot`. Also rejects any `..` component that could
    /// traverse out even if it textually starts under the root.
    pub fn confine(&self, candidate: &Path) -> Result<PathBuf, IsolationError> {
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(IsolationError::Escape {
                path: candidate.to_path_buf(),
            });
        }
        if candidate.starts_with(&self.root) {
            Ok(candidate.to_path_buf())
        } else {
            Err(IsolationError::OutsideRoot {
                path: candidate.to_path_buf(),
            })
        }
    }

    /// Guard a write: the entry's declared owner must be this root's owner.
    /// Stops an entry stamped for agent A from being written into agent B's
    /// root (a metadata-level cross-agent leak), independent of path checks.
    pub fn authorize_owner(&self, entry_owner_agent_group_id: &str) -> Result<(), IsolationError> {
        if entry_owner_agent_group_id == self.owner_agent_group_id {
            Ok(())
        } else {
            Err(IsolationError::OwnerMismatch {
                expected: self.owner_agent_group_id.clone(),
                found: entry_owner_agent_group_id.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_and_specialist_roots_are_distinct_paths() {
        let groups = Path::new("/data/groups");
        let orch = MemoryRoot::orchestrator(groups, "ag_orchestrator");
        let spec = MemoryRoot::specialist(groups, "ag_browser_1", "browser-1").unwrap();
        assert_eq!(orch.path(), Path::new("/data/groups/orchestrator/memory"));
        assert_eq!(
            spec.path(),
            Path::new("/data/groups/specialists/browser-1/memory")
        );
    }

    #[test]
    fn resolve_joins_relative_under_root() {
        let orch = MemoryRoot::orchestrator(Path::new("/data/groups"), "ag_orchestrator");
        let p = orch.resolve("people/alice.md").unwrap();
        assert_eq!(p, Path::new("/data/groups/orchestrator/memory/people/alice.md"));
        assert!(p.starts_with(orch.path()));
    }

    #[test]
    fn traversal_absolute_and_empty_relatives_are_rejected() {
        let orch = MemoryRoot::orchestrator(Path::new("/data/groups"), "ag_orchestrator");
        for bad in ["../escape.md", "a/../../escape", "/etc/passwd", ""] {
            assert!(
                matches!(orch.resolve(bad), Err(IsolationError::Escape { .. })),
                "{bad} not rejected"
            );
        }
    }

    #[test]
    fn one_agent_cannot_reach_another_agents_root() {
        let groups = Path::new("/data/groups");
        let orch = MemoryRoot::orchestrator(groups, "ag_orchestrator");
        let spec = MemoryRoot::specialist(groups, "ag_browser_1", "browser-1").unwrap();
        // A path that lives under the specialist's root is outside the
        // orchestrator's root, and vice versa.
        let in_spec = spec.resolve("notes.md").unwrap();
        assert!(matches!(
            orch.confine(&in_spec),
            Err(IsolationError::OutsideRoot { .. })
        ));
        let in_orch = orch.resolve("people/alice.md").unwrap();
        assert!(matches!(
            spec.confine(&in_orch),
            Err(IsolationError::OutsideRoot { .. })
        ));
        // Each agent can confine its own resolved paths.
        assert!(orch.confine(&in_orch).is_ok());
        assert!(spec.confine(&in_spec).is_ok());
    }

    #[test]
    fn specialist_folder_must_be_a_single_safe_segment() {
        let groups = Path::new("/data/groups");
        for bad in ["../evil", "a/b", "/abs", "", "."] {
            assert!(
                matches!(
                    MemoryRoot::specialist(groups, "ag_x", bad),
                    Err(IsolationError::InvalidSpecialistFolder { .. })
                ),
                "{bad} accepted as folder"
            );
        }
    }

    #[test]
    fn confine_rejects_parent_dir_even_under_root() {
        let orch = MemoryRoot::orchestrator(Path::new("/data/groups"), "ag_orchestrator");
        let sneaky = Path::new("/data/groups/orchestrator/memory/../../specialists/x/memory/n.md");
        assert!(matches!(
            orch.confine(sneaky),
            Err(IsolationError::Escape { .. })
        ));
    }

    #[test]
    fn owner_guard_blocks_foreign_entries() {
        let orch = MemoryRoot::orchestrator(Path::new("/data/groups"), "ag_orchestrator");
        assert!(orch.authorize_owner("ag_orchestrator").is_ok());
        assert!(matches!(
            orch.authorize_owner("ag_browser_1"),
            Err(IsolationError::OwnerMismatch { .. })
        ));
    }
}
