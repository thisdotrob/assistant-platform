//! Docker runtime for agent runners: image naming and the `claw-agent-base`
//! contract, container mount construction/validation, spawn preparation and
//! `docker run` arg construction, idle/stale lifecycle classification, the
//! stub/Claude-OAuth auth paths, and runtime readiness checks.
//!
//! This crate owns no session or core tables and does not depend on
//! `claw-session`: the host (which does) resolves session paths and passes them
//! in. Executing Docker and the real Claude OAuth path require a Docker host and
//! credentials and are exercised only outside the sandbox; everything here —
//! mount/auth/schema gating, arg construction, lifecycle classification — is
//! host-side logic with full unit coverage.

pub mod auth;
pub mod error;
pub mod image;
pub mod lifecycle;
pub mod mount;
pub mod readiness;
#[cfg(feature = "real-docker")]
pub mod runtime_real;

pub use auth::{
    prepare_runner_env, OneCliReadiness, RunnerAuthMode, ANTHROPIC_API_KEY_ENV, OAUTH_TOKEN_ENV,
    PLACEHOLDER_TOKEN, RUNNER_MODE_ENV,
};
pub use error::{AuthError, MountError, SpawnError};
pub use image::{BaseImageContract, ImageRef, BASE_IMAGE_REPOSITORY, BASE_IMAGE_RUNTIME};
pub use lifecycle::{
    classify, docker_run_args, gate_session_schema, prepare_spawn, reap_action, ContainerId,
    ContainerRuntime, FakeRuntime, LifecyclePolicy, ReapAction, RuntimeState, SchemaRange,
    SpawnSpec,
};
pub use mount::{
    build_session_mounts, validate_mounts, MemoryMountSpec, Mount, MountMode, SessionPaths,
    CONTAINER_HEARTBEAT, CONTAINER_INBOUND_DB, CONTAINER_INBOX, CONTAINER_OUTBOUND_DB,
    CONTAINER_OUTBOX,
};
pub use readiness::{
    docker_daemon_ready, image_resolves, mount_roots_ready, skipped_no_docker, CheckStatus,
};
#[cfg(feature = "real-docker")]
pub use runtime_real::{DockerCliError, DockerCliRuntime};

pub const MODULE_ID: &str = "claw-runtime-docker";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
