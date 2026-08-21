//! Build the per-turn [`HostConfig`] for running a specialist agent in its own
//! container.
//!
//! The host is specialist-agnostic: given a registered [`SpecialistSpec`] it runs
//! the spec's custom image as a real Claude turn ([`RunnerAuthMode::Specialist`])
//! credentialed through the OneCLI proxy. The spec carries the complete
//! in-container turn config (system prompt, tools, limits, env); the host hands it
//! to the generic shim harness via the `ASSISTANT_SPECIALIST_*` env. No browser-
//! or specialist-specific knowledge lives here.
//!
//! [`agent_host_config`] is shared by the scheduler's `run_as` path (a scheduled
//! item that runs as a named agent rather than the orchestrator) and the
//! agent-to-agent `send_message` path, so both produce byte-identical container
//! configuration for the same spec.

use std::path::Path;

use assistant_runtime_docker::{ImageRef, LifecyclePolicy, RunnerAuthMode};
use assistant_specialist_spec::SpecialistSpec;

use crate::run::HostConfig;

/// Build the per-turn [`HostConfig`] for running `spec`'s agent in a container:
/// its custom image, credentialed auth mode, own OneCLI identity, no orchestrator
/// memory injection, a stable per-group workspace, and the generic
/// `ASSISTANT_SPECIALIST_*` turn config the shim harness reads. Shared by the
/// scheduler's `run_as` path (a scheduled item that runs as a named agent rather
/// than the orchestrator) and the agent-to-agent `send_message` path, so both
/// produce byte-identical container configuration for the same spec.
pub(crate) fn agent_host_config(
    base_config: &HostConfig,
    spec: &SpecialistSpec,
    sessions_dir: &Path,
) -> Result<HostConfig, String> {
    let tools_json = serde_json::to_string(&spec.tools)
        .map_err(|e| format!("serializing the specialist tools failed: {e}"))?;
    let allowed_tools_json = serde_json::to_string(&spec.allowed_tools)
        .map_err(|e| format!("serializing the specialist allowed-tools failed: {e}"))?;
    let mcp_tools_json = serde_json::to_string(&spec.mcp_tools)
        .map_err(|e| format!("serializing the specialist MCP tools failed: {e}"))?;
    // The `send_message` destination menu the shim shows the model: each declared
    // destination name plus a short description. Peer descriptions aren't resolved
    // here (this builder sees only one spec), so a generic hint is used; the
    // orchestrator destination gets a human-facing hint.
    let destination_entries: Vec<serde_json::Value> = spec
        .destinations
        .iter()
        .map(|name| {
            let description = if name == "orchestrator" {
                "the human-facing orchestrator; send here to report a result or reach the human"
                    .to_string()
            } else {
                format!("the {name} agent")
            };
            serde_json::json!({ "name": name, "description": description })
        })
        .collect();
    let destinations_json = serde_json::to_string(&destination_entries)
        .map_err(|e| format!("serializing the specialist destinations failed: {e}"))?;

    // The specialist runs its own custom image (carrying the binaries it needs),
    // its own auth mode, and no orchestrator memory injection; mounts and cadence
    // are inherited from the base config. Its per-image `extra_env` is preserved
    // and the generic `ASSISTANT_SPECIALIST_*` turn config is layered on top.
    let image = match &spec.image_digest {
        Some(digest) => ImageRef::pinned(&spec.image_repository, &spec.image_tag, digest),
        None => ImageRef::new(&spec.image_repository, &spec.image_tag),
    };
    let mut config = base_config.clone();
    config.image = image;
    // A specialist runs the credentialed equivalent of the orchestrator's mode: a
    // stub orchestrator (the offline gate) spawns a stub specialist that needs no
    // OneCLI gateway, while any credentialed orchestrator spawns a real
    // `Specialist` turn (`ASSISTANT_RUNNER_MODE=specialist`, OneCLI-gated).
    config.auth_mode = match base_config.auth_mode {
        RunnerAuthMode::Stub => RunnerAuthMode::Stub,
        _ => RunnerAuthMode::Specialist,
    };
    config.onecli_agent = spec.onecli_agent.clone();
    config.memory = None;
    // Long-running agents (e.g. an SE agent implementing an issue) raise the
    // per-turn deadline well above the host default; such turns run in the
    // background so the long wall-clock never blocks the serve loop.
    if let Some(secs) = spec.turn_timeout_secs {
        let timeout = std::time::Duration::from_secs(secs);
        config.turn_timeout = timeout;
        // The shim refreshes the heartbeat only between turns, so a single long
        // implementation turn (clone the monorepo, build, test) would otherwise
        // trip the default 300s heartbeat-staleness reaper and be killed mid-work
        // long before its turn deadline (observed: GP-589 reaped at exactly 5m
        // after flipping to In Progress). Let staleness track the turn deadline so
        // the reaper only fires once the turn itself has timed out.
        config.policy = LifecyclePolicy::new(config.policy.idle_after.min(timeout), timeout);
    }
    // Each specialist gets its own persistent workspace under the instance root,
    // keyed by group_slug (stable across runs). Derived from sessions_dir's parent
    // (the instance root: sessions_dir = <root>/sessions/).
    if let Some(instance_root) = sessions_dir.parent() {
        let workspace_dir = instance_root.join("workspaces").join(&spec.group_slug);
        config = config.with_workspace(workspace_dir);
    }
    config.extra_env = spec.extra_env.clone();
    // Raw named-volume mounts (e.g. the shared board DB) the specialist's
    // turn containers get, so a work-turn can record state the gate also reads.
    config.extra_volumes = spec.volumes.clone();
    config.extra_env.extend([
        (
            "ASSISTANT_SPECIALIST_SYSTEM_PROMPT".to_string(),
            spec.system_prompt.clone(),
        ),
        ("ASSISTANT_SPECIALIST_TOOLS".to_string(), tools_json),
        ("ASSISTANT_SPECIALIST_ALLOWED_TOOLS".to_string(), allowed_tools_json),
        ("ASSISTANT_SPECIALIST_MCP_TOOLS".to_string(), mcp_tools_json),
        ("ASSISTANT_SPECIALIST_DESTINATIONS".to_string(), destinations_json),
        (
            "ASSISTANT_SPECIALIST_MAX_TURNS".to_string(),
            spec.max_turns.to_string(),
        ),
    ]);
    Ok(config)
}
