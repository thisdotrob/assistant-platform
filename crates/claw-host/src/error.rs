//! Errors surfaced by the host run-loop.

use claw_agent_protocol::UnsupportedProtocol;
use claw_runtime_docker::SpawnError;
use claw_session::SessionError;

/// A run-loop failure. The runtime's own error is rendered to a string at the
/// boundary so [`HostError`] stays free of the `ContainerRuntime::Error`
/// associated type and the same error type serves every runtime.
#[derive(Debug)]
pub enum HostError {
    /// Deriving the instance/session layout from config failed.
    Layout(String),
    /// A `claw-session` operation failed.
    Session(SessionError),
    /// A central-DB operation failed (opening the permissions DB, an admin
    /// write). The underlying error is rendered to a string at the boundary so
    /// `HostError` stays free of the `claw-db`/`claw-permissions` error types.
    Db(String),
    /// Preparing the spawn (schema gate, mount validation, auth) failed.
    Spawn(SpawnError),
    /// Applying the OneCLI gateway config to the Claude-path spawn failed.
    OneCli(crate::onecli::OneCliError),
    /// The container runtime failed to spawn or stop a container.
    Runtime(String),
    /// The runner declared a protocol version the shim does not support.
    Protocol(UnsupportedProtocol),
    /// The container's heartbeat went stale: the runner is presumed dead.
    ContainerDied { detail: String },
    /// No reply appeared within the turn deadline.
    Timeout { seconds: u64 },
    /// The inbound channel transport failed unrecoverably (e.g. a rejected
    /// Socket Mode app token). Per-turn failures inside the serve loop are logged
    /// and skipped; this is only for a fault that ends the whole listener.
    Channel(String),
    /// Reading terminal input failed.
    Io(std::io::Error),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Layout(detail) => write!(f, "layout error: {detail}"),
            HostError::Session(e) => write!(f, "session error: {e}"),
            HostError::Db(detail) => write!(f, "central db error: {detail}"),
            HostError::Spawn(e) => write!(f, "spawn refused: {e}"),
            HostError::OneCli(e) => write!(f, "onecli gateway error: {e}"),
            HostError::Runtime(detail) => write!(f, "container runtime error: {detail}"),
            HostError::Protocol(p) => write!(
                f,
                "runner protocol {} not supported by shim (supports: {})",
                p.declared,
                p.supported.join(", ")
            ),
            HostError::ContainerDied { detail } => write!(f, "container died: {detail}"),
            HostError::Timeout { seconds } => {
                write!(f, "no reply within {seconds}s turn deadline")
            }
            HostError::Channel(detail) => write!(f, "inbound channel error: {detail}"),
            HostError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<SessionError> for HostError {
    fn from(e: SessionError) -> Self {
        HostError::Session(e)
    }
}

impl From<SpawnError> for HostError {
    fn from(e: SpawnError) -> Self {
        HostError::Spawn(e)
    }
}

impl From<UnsupportedProtocol> for HostError {
    fn from(e: UnsupportedProtocol) -> Self {
        HostError::Protocol(e)
    }
}

impl From<crate::onecli::OneCliError> for HostError {
    fn from(e: crate::onecli::OneCliError) -> Self {
        HostError::OneCli(e)
    }
}

impl From<std::io::Error> for HostError {
    fn from(e: std::io::Error) -> Self {
        HostError::Io(e)
    }
}
