//! Runtime readiness checks.
//!
//! Per the contract, the runtime verifies the Docker daemon is reachable, the
//! required image tags/digests resolve, and mount roots exist and are writable.
//! The mount-root check is a real filesystem probe. The Docker daemon and image
//! checks depend on a Docker host, so they take an injected probe: real wiring
//! supplies a `docker info` / `docker image inspect` probe, and environments
//! without Docker (e.g. this sandbox) record them as skipped.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::image::ImageRef;

/// The outcome of one readiness check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    /// Not evaluated here (e.g. requires a Docker host); the caller must run it
    /// in the target environment.
    Skipped { detail: String },
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass)
    }

    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckStatus::Fail { .. })
    }
}

/// Check that every mount root exists and is writable. Writability is probed by
/// creating and removing a temp file in the root.
pub fn mount_roots_ready(roots: &[&Path]) -> CheckStatus {
    for root in roots {
        if !root.exists() {
            return CheckStatus::Fail {
                detail: format!("mount root {} does not exist", root.display()),
            };
        }
        let probe = root.join(".claw-write-probe");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
            }
            Err(e) => {
                return CheckStatus::Fail {
                    detail: format!("mount root {} is not writable: {e}", root.display()),
                };
            }
        }
    }
    CheckStatus::Pass
}

/// Check the Docker daemon via an injected probe (returns true when reachable).
pub fn docker_daemon_ready(probe: impl FnOnce() -> bool) -> CheckStatus {
    if probe() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: "Docker daemon is not reachable".to_string(),
        }
    }
}

/// Check that an image reference resolves via an injected probe (e.g.
/// `docker image inspect <ref>`).
pub fn image_resolves(image: &ImageRef, probe: impl FnOnce(&str) -> bool) -> CheckStatus {
    if probe(&image.reference()) {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: format!("image {} does not resolve", image.reference()),
        }
    }
}

/// Marker for a check that requires a Docker host not available here.
pub fn skipped_no_docker(check: &str) -> CheckStatus {
    CheckStatus::Skipped {
        detail: format!("{check} requires a Docker host; run in the target environment"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_existing_root_passes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mount_roots_ready(&[dir.path()]).is_pass());
    }

    #[test]
    fn missing_root_fails() {
        let status = mount_roots_ready(&[Path::new("/no/such/root/claw")]);
        assert!(status.is_blocking_failure());
    }

    #[test]
    fn docker_probe_pass_and_fail() {
        assert!(docker_daemon_ready(|| true).is_pass());
        assert!(docker_daemon_ready(|| false).is_blocking_failure());
    }

    #[test]
    fn image_resolution_uses_probe() {
        let image = ImageRef::new("assistant-base", "0.1.0");
        assert!(image_resolves(&image, |_| true).is_pass());
        assert!(image_resolves(&image, |_| false).is_blocking_failure());
    }

    #[test]
    fn skipped_is_not_a_blocking_failure() {
        let status = skipped_no_docker("docker daemon");
        assert!(!status.is_blocking_failure());
        assert!(!status.is_pass());
    }

    #[test]
    fn check_status_round_trips_json() {
        let status = CheckStatus::Fail {
            detail: "x".into(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: CheckStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, back);
    }
}
