//! Container mount construction and validation.
//!
//! The container sees only explicitly mounted paths. Session mounts grant the
//! container read-only access to `inbound.db` and the inbox, and writable
//! access to `outbound.db`, the heartbeat, and the outbox. The host owns the
//! session layout (in `claw-session`) and supplies resolved paths here, so this
//! crate stays free of a `claw-session` dependency. Mounts are validated
//! host-side against an allowlist, and blocked credential paths are rejected.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::MountError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

impl MountMode {
    pub fn is_read_only(self) -> bool {
        matches!(self, MountMode::ReadOnly)
    }
}

/// One bind mount from host to container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub mode: MountMode,
}

impl Mount {
    pub fn read_only(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            mode: MountMode::ReadOnly,
        }
    }

    pub fn read_write(host_path: impl Into<PathBuf>, container_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            mode: MountMode::ReadWrite,
        }
    }
}

/// Host-resolved session paths the runtime mounts. The host derives these from
/// a `claw-session` `SessionLayout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPaths {
    pub inbound_db: PathBuf,
    pub outbound_db: PathBuf,
    pub heartbeat: PathBuf,
    pub inbox_dir: PathBuf,
    pub outbox_dir: PathBuf,
}

/// A memory root mounted into the container at a profile-defined path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryMountSpec {
    pub host_root: PathBuf,
    pub container_path: PathBuf,
    pub read_only: bool,
}

/// The container paths session artifacts are mounted at.
pub const CONTAINER_INBOUND_DB: &str = "/session/inbound.db";
pub const CONTAINER_OUTBOUND_DB: &str = "/session/outbound.db";
pub const CONTAINER_HEARTBEAT: &str = "/session/.heartbeat";
pub const CONTAINER_INBOX: &str = "/session/inbox";
pub const CONTAINER_OUTBOX: &str = "/session/outbox";

/// Build the mount set for a session run. Inbound DB and inbox are read-only;
/// outbound DB, heartbeat, and outbox are writable. An optional memory mount is
/// appended at its profile-defined path.
pub fn build_session_mounts(session: &SessionPaths, memory: Option<&MemoryMountSpec>) -> Vec<Mount> {
    let mut mounts = vec![
        Mount::read_only(&session.inbound_db, CONTAINER_INBOUND_DB),
        Mount::read_write(&session.outbound_db, CONTAINER_OUTBOUND_DB),
        Mount::read_write(&session.heartbeat, CONTAINER_HEARTBEAT),
        Mount::read_only(&session.inbox_dir, CONTAINER_INBOX),
        Mount::read_write(&session.outbox_dir, CONTAINER_OUTBOX),
    ];
    if let Some(memory) = memory {
        mounts.push(Mount {
            host_path: memory.host_root.clone(),
            container_path: memory.container_path.clone(),
            mode: if memory.read_only {
                MountMode::ReadOnly
            } else {
                MountMode::ReadWrite
            },
        });
    }
    mounts
}

/// File/dir names that must never be mounted into a container: raw credential
/// material that OneCLI is supposed to mediate instead.
const BLOCKED_NAMES: &[&str] = &[
    ".env",
    "credentials",
    "credentials.json",
    "id_rsa",
    "id_ed25519",
    ".npmrc",
    ".netrc",
];

const BLOCKED_EXTENSIONS: &[&str] = &["pem", "key"];

fn is_blocked(host_path: &Path) -> bool {
    if host_path
        .components()
        .any(|c| c.as_os_str() == ".ssh" || c.as_os_str() == ".aws")
    {
        return true;
    }
    // Compared case-insensitively so a case-insensitive host filesystem can't
    // smuggle a credential file past the denylist (e.g. `.ENV` for `.env`).
    if let Some(name) = host_path.file_name().and_then(|n| n.to_str())
        && BLOCKED_NAMES.contains(&name.to_ascii_lowercase().as_str())
    {
        return true;
    }
    if let Some(ext) = host_path.extension().and_then(|e| e.to_str())
        && BLOCKED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    {
        return true;
    }
    false
}

/// Validate a mount set host-side:
/// - no host path contains a `..` traversal;
/// - every host path is under one of the allowed roots;
/// - no blocked credential path is mounted;
/// - the inbound DB mount is read-only;
/// - no two mounts target the same container path.
pub fn validate_mounts(mounts: &[Mount], allowed_roots: &[PathBuf]) -> Result<(), MountError> {
    let mut seen_targets: Vec<&Path> = Vec::new();
    for mount in mounts {
        // Reject `..` first: a traversal can defeat the allowlist below by
        // textually starting under an allowed root while resolving elsewhere.
        if mount
            .host_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(MountError::PathTraversal {
                host_path: mount.host_path.clone(),
            });
        }
        if is_blocked(&mount.host_path) {
            return Err(MountError::BlockedPath {
                host_path: mount.host_path.clone(),
            });
        }
        let allowed = allowed_roots
            .iter()
            .any(|root| mount.host_path.starts_with(root));
        if !allowed {
            return Err(MountError::OutsideAllowlist {
                host_path: mount.host_path.clone(),
            });
        }
        if mount.container_path == Path::new(CONTAINER_INBOUND_DB)
            && !mount.mode.is_read_only()
        {
            return Err(MountError::InboundNotReadOnly);
        }
        if seen_targets.contains(&mount.container_path.as_path()) {
            return Err(MountError::DuplicateTarget {
                container_path: mount.container_path.clone(),
            });
        }
        seen_targets.push(&mount.container_path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &Path) -> SessionPaths {
        SessionPaths {
            inbound_db: root.join("inbound.db"),
            outbound_db: root.join("outbound.db"),
            heartbeat: root.join(".heartbeat"),
            inbox_dir: root.join("inbox"),
            outbox_dir: root.join("outbox"),
        }
    }

    #[test]
    fn session_mounts_make_inbound_ro_and_outbound_rw() {
        let mounts = build_session_mounts(&session(Path::new("/data/sess")), None);
        let by_target = |t: &str| {
            mounts
                .iter()
                .find(|m| m.container_path == Path::new(t))
                .unwrap()
        };
        assert!(by_target(CONTAINER_INBOUND_DB).mode.is_read_only());
        assert!(by_target(CONTAINER_INBOX).mode.is_read_only());
        assert!(!by_target(CONTAINER_OUTBOUND_DB).mode.is_read_only());
        assert!(!by_target(CONTAINER_HEARTBEAT).mode.is_read_only());
        assert!(!by_target(CONTAINER_OUTBOX).mode.is_read_only());
    }

    #[test]
    fn valid_mounts_under_allowed_root_pass() {
        let root = PathBuf::from("/data/sess");
        let mounts = build_session_mounts(&session(&root), None);
        assert!(validate_mounts(&mounts, &[PathBuf::from("/data")]).is_ok());
    }

    #[test]
    fn mounts_outside_allowlist_are_rejected() {
        let mounts = build_session_mounts(&session(Path::new("/data/sess")), None);
        let err = validate_mounts(&mounts, &[PathBuf::from("/other")]).unwrap_err();
        assert!(matches!(err, MountError::OutsideAllowlist { .. }));
    }

    #[test]
    fn blocked_credential_paths_are_rejected() {
        for blocked in [
            "/data/.env",
            "/data/secrets/credentials.json",
            "/data/keys/server.pem",
            "/home/u/.ssh/id_rsa",
        ] {
            let mounts = vec![Mount::read_only(blocked, "/session/x")];
            let err = validate_mounts(&mounts, &[PathBuf::from("/")]).unwrap_err();
            assert!(matches!(err, MountError::BlockedPath { .. }), "{blocked} not blocked");
        }
    }

    #[test]
    fn parent_dir_traversal_is_rejected_even_under_allowed_root() {
        for traversal in ["/data/../etc/shadow", "/data/sess/../../root/.bashrc"] {
            let mounts = vec![Mount::read_only(traversal, "/session/x")];
            let err = validate_mounts(&mounts, &[PathBuf::from("/data")]).unwrap_err();
            assert!(
                matches!(err, MountError::PathTraversal { .. }),
                "{traversal} not rejected as traversal"
            );
        }
    }

    #[test]
    fn blocked_names_are_matched_case_insensitively() {
        for blocked in ["/data/.ENV", "/data/keys/SERVER.PEM", "/data/u/ID_RSA"] {
            let mounts = vec![Mount::read_only(blocked, "/session/x")];
            let err = validate_mounts(&mounts, &[PathBuf::from("/data")]).unwrap_err();
            assert!(matches!(err, MountError::BlockedPath { .. }), "{blocked} not blocked");
        }
    }

    #[test]
    fn inbound_mounted_writable_is_rejected() {
        let mounts = vec![Mount::read_write("/data/sess/inbound.db", CONTAINER_INBOUND_DB)];
        let err = validate_mounts(&mounts, &[PathBuf::from("/data")]).unwrap_err();
        assert_eq!(err, MountError::InboundNotReadOnly);
    }

    #[test]
    fn duplicate_container_targets_are_rejected() {
        let mounts = vec![
            Mount::read_only("/data/a", "/session/x"),
            Mount::read_write("/data/b", "/session/x"),
        ];
        let err = validate_mounts(&mounts, &[PathBuf::from("/data")]).unwrap_err();
        assert!(matches!(err, MountError::DuplicateTarget { .. }));
    }
}
