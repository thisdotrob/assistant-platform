//! The run-loop composition root for a local live turn.
//!
//! `assistant-cli` stays a domain-free framework and the channel crate stays out of
//! container lifecycle; this crate is where a terminal message, a real Docker
//! container, the session DB protocol, and the OneCLI-gated Claude auth path are
//! wired into a single turn. Both products inherit it through a thin `run`
//! subcommand and by appending [`setup_steps`] to their setup pipeline.
//!
//! The run-loop ([`run`]) builds the real CLI-backed Docker runtime; the generic
//! [`Host`] is what the offline test drives with `FakeRuntime` and an in-process
//! fake shim, so the full enqueue→spawn→poll→deliver path is covered without
//! Docker. Executing real Docker and the real Claude path happens only outside
//! the sandbox.

use std::io::BufRead;
use std::path::PathBuf;

use assistant_config::{home_dir, InstanceLayout};
use assistant_runtime_docker::{base_image_ref, DockerCliRuntime};
use assistant_session::SessionLayout;

pub mod admin;
pub mod delegation;
pub mod error;
pub mod memory;
pub mod onecli;
pub mod qmd;
pub mod run;
pub mod scheduler;
pub mod setup_steps;
#[cfg(feature = "socket-mode")]
mod signal;
pub mod slack;

pub use admin::{
    create_scheduled_message, register_user, RegisterUserOptions, ScheduleMessageOptions,
};
pub use error::HostError;
pub use qmd::{NodeQmd, QmdSidecar};
pub use run::{Host, HostConfig};
pub use scheduler::{sweep_once, SweepReport};
pub use setup_steps::setup_steps;
pub use slack::{serve_slack, SchedulerTickConfig, SlackServeOptions};
#[cfg(feature = "socket-mode")]
pub use slack::serve_slack_live;

/// Re-exported so products can select the run auth mode without taking a direct
/// dependency on `assistant-runtime-docker`.
pub use assistant_runtime_docker::RunnerAuthMode;

/// Re-exported so products can register specialists (and tests can build specs)
/// without taking a direct dependency on `assistant-specialist-spec`.
pub use assistant_specialist_spec::SpecialistSpec;

/// Re-exported so products and tests can set the Slack serve gate without taking
/// direct dependencies on `assistant-router`/`assistant-permissions`.
pub use assistant_permissions::UnknownPolicy;
pub use assistant_router::EngagementMode;

pub const MODULE_ID: &str = "assistant-host";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// This composition runs a single host agent, so every agent-scoped central-DB
/// row (sticky-engagement windows, the memory catalog) is keyed under one agent
/// group. The tables are keyed by `agent_group_id` to support multiple wired
/// agents sharing a DB; here there is exactly one.
pub(crate) const HOST_AGENT_GROUP: i64 = 1;

/// The single host agent group's logical string id — its `owner_agent_group_id`
/// in memory front matter, which selects the orchestrator memory root on disk
/// (`groups/orchestrator/memory`). Paired with the integer [`HOST_AGENT_GROUP`]
/// that keys the central-DB catalog.
pub(crate) const HOST_AGENT_OWNER: &str = "ag_orchestrator";

/// Cap on catalog memories injected into a turn (matches the v1 pre-reply RAG
/// top-N). The limit is applied after eligibility filtering.
pub(crate) const MEMORY_INJECTION_LIMIT: usize = 5;

/// Product-supplied inputs for a run. The product knows its namespace and the
/// session to attach to; the platform derives the on-disk layout and wires the
/// runtime/auth path.
pub struct RunOptions {
    pub namespace: String,
    pub instance: Option<String>,
    pub home: Option<PathBuf>,
    pub group: String,
    pub session: String,
    /// Process a single terminal line then exit (vs. serve until EOF).
    pub once: bool,
    /// Stub (echo, no creds) or real Claude OAuth (OneCLI-gated).
    pub mode: RunnerAuthMode,
}

/// Serve terminal turns until stdin closes. Equivalent to [`run`] with
/// `once = false`.
pub fn serve(opts: RunOptions) -> i32 {
    run(RunOptions { once: false, ..opts })
}

/// Run the terminal turn loop, returning a process exit code: 0 on a clean
/// exit, 1 on any error.
pub fn run(opts: RunOptions) -> i32 {
    match run_inner(opts) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("run error: {e}");
            1
        }
    }
}

fn run_inner(opts: RunOptions) -> Result<(), HostError> {
    let home = opts
        .home
        .clone()
        .or_else(home_dir)
        .ok_or_else(|| HostError::Layout("HOME is not set; pass --home <path>".to_string()))?;

    let instance_layout = InstanceLayout::derive(&home, &opts.namespace, opts.instance.as_deref())
        .map_err(|e| HostError::Layout(e.to_string()))?;
    let sessions_dir = instance_layout.sessions_dir();
    let session_layout = SessionLayout::derive(&sessions_dir, &opts.group, &opts.session)?;

    let onecli = onecli::probe(&instance_layout);
    let onecli_agent = onecli::agent_identifier(&instance_layout);
    let onecli_ca_dir = onecli::OneCliPaths::for_instance(&instance_layout).dir;
    let image = base_image_ref(env!("CARGO_PKG_VERSION"));
    let config = HostConfig::new(image, vec![sessions_dir], opts.mode, onecli)
        .with_onecli_agent(onecli_agent, onecli_ca_dir)
        .with_memory(
            instance_layout.central_db_path(),
            HOST_AGENT_GROUP,
            MEMORY_INJECTION_LIMIT,
            instance_layout.groups_dir(),
            HOST_AGENT_OWNER.to_string(),
        );
    let runtime = DockerCliRuntime::new();
    let mut host = Host::new(session_layout, runtime, config);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    if opts.once {
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? > 0 {
            host.process_turn(line.trim_end_matches(['\n', '\r']), &mut stdout)?;
        }
    } else {
        // A serve session must outlive a single bad turn. A recoverable failure
        // (the container died, or a turn timed out) is reported and the loop
        // keeps reading: a death has already reaped the handle, so the next line
        // respawns a fresh container. Fatal failures (spawn refused, protocol
        // mismatch, layout, session, or IO errors) still abort the session.
        for line in stdin.lock().lines() {
            if let Err(e) = host.process_turn(&line?, &mut stdout) {
                if is_recoverable(&e) {
                    eprintln!("turn failed ({e}); ready for the next message");
                    continue;
                }
                return Err(e);
            }
        }
    }

    host.shutdown()
}

/// Product-supplied inputs for a Slack Socket Mode serve session. Mirrors
/// [`RunOptions`], but every Slack channel maps to its own per-channel session
/// under `group` (the session id is the Slack channel id). The real Slack tokens
/// are never held here: they live in the OneCLI vault and are injected on the
/// wire via `proxy_url` (the host-facing gateway proxy, e.g.
/// `http://127.0.0.1:10355`).
#[cfg(feature = "socket-mode")]
pub struct SlackRunOptions {
    pub namespace: String,
    pub instance: Option<String>,
    pub home: Option<PathBuf>,
    pub group: String,
    /// Stub (echo, no creds) or real Claude OAuth (OneCLI-gated).
    pub mode: RunnerAuthMode,
    /// Host-facing OneCLI proxy URL the Slack calls route through.
    pub proxy_url: String,
    /// The specialists this product registers. The orchestrator may delegate to
    /// any of them by `route_name`; each runs in its own custom image. Empty
    /// disables delegation (the orchestrator gets no `delegate` tool).
    pub specialists: Vec<SpecialistSpec>,
}

/// Serve Slack turns over Socket Mode until the process is signalled, returning a
/// process exit code (0 clean, 1 on error). Derives the on-disk layout and host
/// config exactly like [`run`], then drives the live transport with both Slack
/// surfaces routed through the OneCLI proxy, so no Slack token is held here.
#[cfg(feature = "socket-mode")]
pub fn run_slack(opts: SlackRunOptions) -> i32 {
    match run_slack_inner(opts) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("slack serve error: {e}");
            1
        }
    }
}

#[cfg(feature = "socket-mode")]
fn run_slack_inner(opts: SlackRunOptions) -> Result<(), HostError> {
    let home = opts
        .home
        .clone()
        .or_else(home_dir)
        .ok_or_else(|| HostError::Layout("HOME is not set; pass --home <path>".to_string()))?;
    let instance_layout = InstanceLayout::derive(&home, &opts.namespace, opts.instance.as_deref())
        .map_err(|e| HostError::Layout(e.to_string()))?;
    let sessions_dir = instance_layout.sessions_dir();

    let onecli = onecli::probe(&instance_layout);
    let onecli_agent = onecli::agent_identifier(&instance_layout);
    // Stable per-installation lease owner: a restarted daemon reclaims its own
    // stale leases rather than treating them as another claimer's.
    let scheduler_owner = onecli_agent.clone();
    let paths = onecli::OneCliPaths::for_instance(&instance_layout);

    // The host's own Slack calls go through the OneCLI proxy, which scopes
    // injection per agent and identifies the agent by a token in the proxy URL.
    // Derive that authenticated URL (rebound onto the host-reachable endpoint)
    // before the container-facing config consumes `onecli_agent`.
    let proxy_url = onecli::host_proxy_url(&onecli_agent, &opts.proxy_url)?;

    let image = base_image_ref(env!("CARGO_PKG_VERSION"));
    let config = HostConfig::new(image, vec![sessions_dir.clone()], opts.mode, onecli)
        .with_onecli_agent(onecli_agent, paths.dir.clone())
        .with_memory(
            instance_layout.central_db_path(),
            HOST_AGENT_GROUP,
            MEMORY_INJECTION_LIMIT,
            instance_layout.groups_dir(),
            HOST_AGENT_OWNER.to_string(),
        );

    // Optional qmd semantic ranking on top of the catalog floor. Enabled only
    // when `ASSISTANT_QMD_SIDECAR` points at the host-side sidecar; absent it, memory
    // stays catalog-only (so the default daemon and the offline gate are
    // unaffected). When enabled, kick off a background reindex so the index is
    // warm by the first turn — the serve loop never blocks on it.
    let mut config = match qmd_sidecar_config(&instance_layout) {
        Some(sidecar) => {
            spawn_qmd_reindex(&instance_layout, sidecar.clone());
            config.with_qmd_sidecar(sidecar)
        }
        None => config,
    };

    // The orchestrator container builds its dynamic `delegate` routing menu from
    // this: a JSON array of `{name, description}`, one per registered specialist.
    // `run_specialist_turn` overwrites `extra_env`, so a specialist never inherits
    // `ASSISTANT_SPECIALISTS` (and so cannot re-delegate from its own image).
    let menu: Vec<_> = opts.specialists.iter().map(|s| s.menu_entry()).collect();
    let specialists_json = serde_json::to_string(&menu)
        .map_err(|e| HostError::Layout(format!("serializing the specialist menu failed: {e}")))?;
    config.extra_env = vec![("ASSISTANT_SPECIALISTS".to_string(), specialists_json)];

    let slack_opts = SlackServeOptions {
        sessions_dir,
        group: opts.group,
        config,
        // The gate reads this installation's central DB. Deny-by-default sender
        // permissions with mention-sticky engagement: the bot only acts on a
        // registered user (see the `register-user` admin path) who @mentions or
        // DMs it, then stays engaged for follow-ups in that conversation.
        central_db_path: instance_layout.central_db_path(),
        engagement: EngagementMode::MentionSticky,
        policy: UnknownPolicy::Strict,
        // Fire due scheduled items from the serve loop's idle windows. 300s lease
        // comfortably exceeds a turn; 30s sweep keeps firing responsive without
        // hammering the central DB.
        scheduler: Some(SchedulerTickConfig {
            owner: scheduler_owner,
            lease_ttl_secs: 300,
            tick_interval: std::time::Duration::from_secs(30),
        }),
        // The specialists this product registered. The orchestrator may delegate
        // to any of them by `route_name`; each runs in its own custom image as a
        // real Claude turn. Empty disables delegation.
        specialists: opts.specialists,
    };
    // Run until the process is signalled (Ctrl-C / SIGTERM). The handler flips an
    // atomic that this stop predicate reads; the serve loop notices within one
    // idle read window and returns, draining its per-channel containers on the
    // way out instead of orphaning them to default-disposition process death.
    let stop = signal::install_shutdown_handler();
    serve_slack_live(proxy_url, paths.ca_cert, slack_opts, &stop)
}

/// Build the qmd sidecar config from the environment, or `None` to stay
/// catalog-only. `ASSISTANT_QMD_SIDECAR` is the path to the host-side sidecar script
/// (`qmd-sidecar.mjs`); `ASSISTANT_QMD_NODE` optionally overrides the `node` binary.
/// The index db and corpus live under the orchestrator memory root's `qmd/`
/// directory, which the catalog walk already skips as derived state.
#[cfg(feature = "socket-mode")]
fn qmd_sidecar_config(layout: &InstanceLayout) -> Option<qmd::QmdSidecar> {
    let sidecar_path = PathBuf::from(std::env::var_os("ASSISTANT_QMD_SIDECAR")?);
    if sidecar_path.as_os_str().is_empty() {
        return None;
    }
    let qmd_dir = assistant_memory::MemoryRoot::orchestrator(&layout.groups_dir(), HOST_AGENT_OWNER)
        .path()
        .join("qmd");
    let mut sidecar =
        qmd::QmdSidecar::new(sidecar_path, qmd_dir.join("index.sqlite"), qmd_dir.join("corpus"));
    if let Some(node_bin) = std::env::var_os("ASSISTANT_QMD_NODE")
        && !node_bin.is_empty()
    {
        sidecar.node_bin = node_bin.to_string_lossy().into_owned();
    }
    Some(sidecar)
}

/// Reindex the orchestrator memory root into the qmd corpus + index on a detached
/// thread so the serve loop never blocks on embedding (which can download models
/// on first run). Fail-open: every failure is logged and the daemon proceeds on
/// the catalog-only floor.
#[cfg(feature = "socket-mode")]
fn spawn_qmd_reindex(layout: &InstanceLayout, sidecar: qmd::QmdSidecar) {
    let central_db_path = layout.central_db_path();
    let groups_dir = layout.groups_dir();
    std::thread::spawn(move || {
        let conn = match assistant_db::open_central(&central_db_path) {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("qmd startup reindex skipped (open central db: {e})");
                return;
            }
        };
        let root = assistant_memory::MemoryRoot::orchestrator(&groups_dir, HOST_AGENT_OWNER);
        let mut backend = qmd::NodeQmd::new(sidecar);
        match assistant_memory::reindex_root(&conn, &root, HOST_AGENT_GROUP, &mut backend) {
            Ok(report) => eprintln!(
                "qmd startup reindex complete: {} entries indexed, health {:?}",
                report.indexed, report.health
            ),
            Err(e) => eprintln!("qmd startup reindex failed (continuing catalog-only): {e}"),
        }
    });
}

/// Whether a per-turn failure should let the serve loop continue rather than
/// tear down the whole session. A dead container is recoverable (the handle has
/// been reaped; the next turn respawns) and a turn timeout is transient. Every
/// other failure is a configuration, protocol, or IO fault that a retry would
/// only repeat, so it is fatal.
fn is_recoverable(err: &HostError) -> bool {
    matches!(err, HostError::ContainerDied { .. } | HostError::Timeout { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_session::SessionError;

    #[test]
    fn only_death_and_timeout_are_recoverable() {
        assert!(is_recoverable(&HostError::ContainerDied {
            detail: "x".to_string()
        }));
        assert!(is_recoverable(&HostError::Timeout { seconds: 1 }));

        assert!(!is_recoverable(&HostError::Layout("x".to_string())));
        assert!(!is_recoverable(&HostError::Runtime("x".to_string())));
        assert!(!is_recoverable(&HostError::Session(SessionError::InvalidId(
            "x".to_string()
        ))));
    }
}
