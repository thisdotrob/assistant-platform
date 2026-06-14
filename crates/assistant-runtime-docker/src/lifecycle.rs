//! Container lifecycle: spawn-arg construction, schema gating, idle/stale
//! classification and reaping, and a runtime abstraction with a fake for tests.
//!
//! Building the `docker run` argv and gating spawns is host-side logic that is
//! fully testable here. Actually executing Docker lives behind the
//! [`ContainerRuntime`] trait; the real CLI-backed implementation runs only
//! outside the sandbox.

use std::collections::HashMap;
use std::time::Duration;

use crate::auth::{prepare_runner_env, OneCliReadiness, RunnerAuthMode};
use crate::error::SpawnError;
use crate::image::ImageRef;
use crate::mount::{validate_mounts, Mount, MountMode};

/// The session DB schema versions a runner supports. The host passes the
/// session's actual version to [`gate_session_schema`]; this crate does not
/// depend on `assistant-session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaRange {
    pub min: u32,
    pub max: u32,
}

impl SchemaRange {
    pub fn new(min: u32, max: u32) -> Self {
        Self { min, max }
    }

    pub fn supports(self, found: u32) -> bool {
        found >= self.min && found <= self.max
    }
}

/// Refuse to start a runner against a session schema outside the supported
/// range.
pub fn gate_session_schema(found: u32, range: SchemaRange) -> Result<(), SpawnError> {
    if range.supports(found) {
        Ok(())
    } else {
        Err(SpawnError::UnsupportedSessionSchema {
            found,
            min: range.min,
            max: range.max,
        })
    }
}

/// A validated, ready-to-run container spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub name: String,
    pub image: ImageRef,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
}

/// Prepare a spawn: gate the session schema, validate mounts against the
/// allowlist, and prepare the runner env via the auth path. Any failure aborts
/// the spawn before a container is started.
#[allow(clippy::too_many_arguments)]
pub fn prepare_spawn(
    name: impl Into<String>,
    image: ImageRef,
    mounts: Vec<Mount>,
    allowed_roots: &[std::path::PathBuf],
    session_schema_found: u32,
    schema_range: SchemaRange,
    auth_mode: RunnerAuthMode,
    onecli: OneCliReadiness,
) -> Result<SpawnSpec, SpawnError> {
    gate_session_schema(session_schema_found, schema_range)?;
    validate_mounts(&mounts, allowed_roots)?;
    let env = prepare_runner_env(auth_mode, onecli)?;
    Ok(SpawnSpec {
        name: name.into(),
        image,
        mounts,
        env,
    })
}

/// Build the `docker run` argument vector for a prepared spawn. Detached and
/// `--rm` (so an exited container — whether cleanly stopped or crashed — removes
/// itself, freeing its deterministic `--name` for a respawn after death or a
/// later same-session run), with each mount expressed as a `--mount` bind
/// (read-only mounts carry `,readonly`), each env as `--env KEY=VALUE`, and the
/// image reference last.
pub fn docker_run_args(spec: &SpawnSpec) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--detach".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        spec.name.clone(),
    ];
    for mount in &spec.mounts {
        let mut spec_str = format!(
            "type=bind,source={},target={}",
            mount.host_path.display(),
            mount.container_path.display()
        );
        if mount.mode == MountMode::ReadOnly {
            spec_str.push_str(",readonly");
        }
        args.push("--mount".to_string());
        args.push(spec_str);
    }
    for (key, value) in &spec.env {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(spec.image.reference());
    args
}

/// Runtime-state projection for a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeState {
    Running,
    Idle,
    Stale,
    Stopped,
}

/// Idle/stale thresholds, measured against time since the last heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecyclePolicy {
    pub idle_after: Duration,
    pub stale_after: Duration,
}

impl LifecyclePolicy {
    pub fn new(idle_after: Duration, stale_after: Duration) -> Self {
        Self {
            idle_after,
            stale_after,
        }
    }
}

/// Classify a container from time since its last heartbeat. `None` means no
/// live heartbeat (stopped/never-started).
pub fn classify(since_heartbeat: Option<Duration>, policy: LifecyclePolicy) -> RuntimeState {
    match since_heartbeat {
        None => RuntimeState::Stopped,
        Some(since) if since <= policy.idle_after => RuntimeState::Running,
        Some(since) if since <= policy.stale_after => RuntimeState::Idle,
        Some(_) => RuntimeState::Stale,
    }
}

/// What the reaper should do for a state. Idle containers are stopped to free
/// resources; stale containers are force-reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapAction {
    Keep,
    StopIdle,
    ForceReap,
}

pub fn reap_action(state: RuntimeState) -> ReapAction {
    match state {
        RuntimeState::Running | RuntimeState::Stopped => ReapAction::Keep,
        RuntimeState::Idle => ReapAction::StopIdle,
        RuntimeState::Stale => ReapAction::ForceReap,
    }
}

/// Identifier returned by the runtime for a spawned container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerId(pub String);

/// Executes container operations. The real implementation shells out to Docker;
/// [`FakeRuntime`] records calls for host-side-logic tests.
pub trait ContainerRuntime {
    type Error;
    fn spawn(&mut self, spec: &SpawnSpec) -> Result<ContainerId, Self::Error>;
    fn stop(&mut self, id: &ContainerId) -> Result<(), Self::Error>;
}

/// In-memory runtime for tests: records spawns/stops, never touches Docker.
#[derive(Default)]
pub struct FakeRuntime {
    pub spawned: Vec<SpawnSpec>,
    pub stopped: Vec<ContainerId>,
    running: HashMap<String, SpawnSpec>,
    next_id: u64,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self, id: &ContainerId) -> bool {
        self.running.contains_key(&id.0)
    }
}

impl ContainerRuntime for FakeRuntime {
    type Error = std::convert::Infallible;

    fn spawn(&mut self, spec: &SpawnSpec) -> Result<ContainerId, Self::Error> {
        self.next_id += 1;
        let id = ContainerId(format!("{}-{}", spec.name, self.next_id));
        self.spawned.push(spec.clone());
        self.running.insert(id.0.clone(), spec.clone());
        Ok(id)
    }

    fn stop(&mut self, id: &ContainerId) -> Result<(), Self::Error> {
        self.running.remove(&id.0);
        self.stopped.push(id.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ANTHROPIC_API_KEY_ENV, OAUTH_TOKEN_ENV};
    use crate::mount::{build_session_mounts, SessionPaths, CONTAINER_INBOUND_DB};
    use std::path::{Path, PathBuf};

    fn session() -> SessionPaths {
        let root = Path::new("/data/sess");
        SessionPaths {
            inbound_db: root.join("inbound.db"),
            outbound_db: root.join("outbound.db"),
            heartbeat: root.join(".heartbeat"),
            inbox_dir: root.join("inbox"),
            outbox_dir: root.join("outbox"),
        }
    }

    fn ready() -> OneCliReadiness {
        OneCliReadiness {
            proxy_configured: true,
            anthropic_secret_present: true,
            placeholder_injection_ok: true,
        }
    }

    #[test]
    fn schema_gate_refuses_out_of_range() {
        assert!(gate_session_schema(2, SchemaRange::new(1, 2)).is_ok());
        let err = gate_session_schema(3, SchemaRange::new(1, 2)).unwrap_err();
        assert_eq!(
            err,
            SpawnError::UnsupportedSessionSchema {
                found: 3,
                min: 1,
                max: 2
            }
        );
    }

    #[test]
    fn stub_spawn_succeeds_without_credentials() {
        let mounts = build_session_mounts(&session(), None);
        let spec = prepare_spawn(
            "sess-1",
            ImageRef::new("assistant-base", "0.1.0"),
            mounts,
            &[PathBuf::from("/data")],
            2,
            SchemaRange::new(1, 2),
            RunnerAuthMode::Stub,
            ready(),
        )
        .unwrap();

        let mut runtime = FakeRuntime::new();
        let id = runtime.spawn(&spec).unwrap();
        assert!(runtime.is_running(&id));
        assert!(!spec.env.iter().any(|(k, _)| k == OAUTH_TOKEN_ENV));
    }

    #[test]
    fn claude_spawn_blocked_when_onecli_not_ready() {
        let mounts = build_session_mounts(&session(), None);
        let result = prepare_spawn(
            "sess-1",
            ImageRef::new("assistant-base", "0.1.0"),
            mounts,
            &[PathBuf::from("/data")],
            2,
            SchemaRange::new(1, 2),
            RunnerAuthMode::ClaudeOAuth,
            OneCliReadiness {
                placeholder_injection_ok: false,
                ..ready()
            },
        );
        assert!(matches!(result, Err(SpawnError::Auth(_))));
    }

    #[test]
    fn docker_args_mark_readonly_and_carry_only_placeholder() {
        let mounts = build_session_mounts(&session(), None);
        let spec = prepare_spawn(
            "sess-1",
            ImageRef::new("assistant-base", "0.1.0"),
            mounts,
            &[PathBuf::from("/data")],
            2,
            SchemaRange::new(1, 2),
            RunnerAuthMode::ClaudeOAuth,
            ready(),
        )
        .unwrap();
        let args = docker_run_args(&spec);
        let joined = args.join(" ");

        // Detached and self-removing on exit so the deterministic name is freed
        // for a respawn-after-death or a later same-session run.
        assert!(args.contains(&"--detach".to_string()));
        assert!(args.contains(&"--rm".to_string()));

        // Inbound DB mount is read-only; outbound is not.
        let inbound = args
            .iter()
            .find(|a| a.contains(CONTAINER_INBOUND_DB))
            .unwrap();
        assert!(inbound.ends_with(",readonly"), "inbound must be readonly: {inbound}");
        let outbound = args
            .iter()
            .find(|a| a.contains("/session/outbound.db"))
            .unwrap();
        assert!(!outbound.contains("readonly"));

        // Only the placeholder token, never a raw token, appears in args.
        assert!(joined.contains(&format!("{OAUTH_TOKEN_ENV}=placeholder")));
        assert!(joined.contains(&format!("{ANTHROPIC_API_KEY_ENV}=")));
        assert!(!joined.contains("real-token"));
        // Image reference is last.
        assert_eq!(args.last().unwrap(), "assistant-base:0.1.0");
    }

    #[test]
    fn classify_and_reap_follow_thresholds() {
        let policy = LifecyclePolicy::new(Duration::from_secs(60), Duration::from_secs(300));
        assert_eq!(classify(None, policy), RuntimeState::Stopped);
        assert_eq!(classify(Some(Duration::from_secs(10)), policy), RuntimeState::Running);
        assert_eq!(classify(Some(Duration::from_secs(120)), policy), RuntimeState::Idle);
        assert_eq!(classify(Some(Duration::from_secs(600)), policy), RuntimeState::Stale);

        assert_eq!(reap_action(RuntimeState::Running), ReapAction::Keep);
        assert_eq!(reap_action(RuntimeState::Idle), ReapAction::StopIdle);
        assert_eq!(reap_action(RuntimeState::Stale), ReapAction::ForceReap);
    }

    #[test]
    fn fake_runtime_tracks_spawn_and_stop() {
        let mounts = build_session_mounts(&session(), None);
        let spec = prepare_spawn(
            "sess-1",
            ImageRef::new("assistant-base", "0.1.0"),
            mounts,
            &[PathBuf::from("/data")],
            2,
            SchemaRange::new(1, 2),
            RunnerAuthMode::Stub,
            ready(),
        )
        .unwrap();
        let mut runtime = FakeRuntime::new();
        let id = runtime.spawn(&spec).unwrap();
        assert!(runtime.is_running(&id));
        runtime.stop(&id).unwrap();
        assert!(!runtime.is_running(&id));
        assert_eq!(runtime.stopped.len(), 1);
    }
}
