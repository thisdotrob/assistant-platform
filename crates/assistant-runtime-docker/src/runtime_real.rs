//! The real, CLI-backed [`ContainerRuntime`]: a thin shell over `docker run` /
//! `docker stop` built from the already-validated [`SpawnSpec`].
//!
//! This is the production runtime and runs only outside the sandbox (it needs a
//! Docker host). It is feature-gated (`real-docker`) so offline builds, tests,
//! and conformance always use [`crate::FakeRuntime`] and never invoke Docker.
//!
//! It introduces no new dependency — only `std::process::Command`. The argv is
//! produced entirely by [`docker_run_args`], which by contract carries only the
//! `CLAUDE_CODE_OAUTH_TOKEN=placeholder`, never a raw token, so nothing secret is
//! ever passed on the command line.

use std::process::Command;

use crate::lifecycle::{docker_run_args, ContainerId, ContainerRuntime, SpawnSpec};

/// Failure modes of the real Docker CLI runtime.
#[derive(Debug)]
pub enum DockerCliError {
    /// The `docker` binary could not be launched at all.
    Launch(std::io::Error),
    /// `docker run` exited non-zero.
    Run { code: Option<i32>, stderr: String },
    /// `docker stop` exited non-zero.
    Stop { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for DockerCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DockerCliError::Launch(e) => write!(f, "failed to launch docker: {e}"),
            DockerCliError::Run { code, stderr } => {
                write!(f, "docker run failed ({}): {stderr}", describe_code(*code))
            }
            DockerCliError::Stop { code, stderr } => {
                write!(f, "docker stop failed ({}): {stderr}", describe_code(*code))
            }
        }
    }
}

impl std::error::Error for DockerCliError {}

fn describe_code(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit {c}"),
        None => "terminated by signal".to_string(),
    }
}

/// Runs containers by shelling out to the `docker` CLI.
pub struct DockerCliRuntime {
    docker_bin: String,
}

impl DockerCliRuntime {
    pub fn new() -> Self {
        Self {
            docker_bin: "docker".to_string(),
        }
    }

    /// Override the binary (e.g. an absolute path, or `podman`). Mainly for
    /// hosts where `docker` is not on `PATH`.
    pub fn with_binary(bin: impl Into<String>) -> Self {
        Self {
            docker_bin: bin.into(),
        }
    }
}

impl Default for DockerCliRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerRuntime for DockerCliRuntime {
    type Error = DockerCliError;

    fn spawn(&mut self, spec: &SpawnSpec) -> Result<ContainerId, Self::Error> {
        let output = Command::new(&self.docker_bin)
            .args(docker_run_args(spec))
            .output()
            .map_err(DockerCliError::Launch)?;
        if !output.status.success() {
            return Err(DockerCliError::Run {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        // `docker run --detach` prints the full container id on stdout; fall back
        // to the deterministic `--name` (which `docker stop` also accepts).
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let id = if printed.is_empty() {
            spec.name.clone()
        } else {
            printed
        };
        Ok(ContainerId(id))
    }

    fn stop(&mut self, id: &ContainerId) -> Result<(), Self::Error> {
        let output = Command::new(&self.docker_bin)
            .arg("stop")
            .arg(&id.0)
            .output()
            .map_err(DockerCliError::Launch)?;
        if !output.status.success() {
            return Err(DockerCliError::Stop {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercised offline: a missing binary surfaces a Launch error without
    // touching a Docker daemon or the network.
    #[test]
    fn missing_binary_is_a_launch_error() {
        let mut runtime = DockerCliRuntime::with_binary("claw-no-such-docker-binary");
        let spec = SpawnSpec {
            name: "sess-1".to_string(),
            image: crate::image::ImageRef::new("assistant-base", "0.1.0"),
            mounts: Vec::new(),
            env: Vec::new(),
        };
        assert!(matches!(runtime.spawn(&spec), Err(DockerCliError::Launch(_))));
        assert!(matches!(
            runtime.stop(&ContainerId("x".to_string())),
            Err(DockerCliError::Launch(_))
        ));
    }
}
