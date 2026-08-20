//! The shared vocabulary for declaring a specialist sub-agent.
//!
//! A [`SpecialistSpec`] is the one piece of data a product hands the host to
//! register a specialist: its routing name, its agent-graph identity and
//! concurrency limits, the custom container image carrying its binaries, and the
//! in-container turn configuration (system prompt, allowed tools, env). It is
//! deliberately plain data with no dependency on the host, the Docker runtime, or
//! the agent-graph engine, so a specialist crate (e.g. `assistant-specialist-browser`)
//! can build one without pulling in core internals — the host translates the
//! plain fields into an `ImageRef`, a `RegisteredProfile`, and container env at
//! registration time.
//!
//! This crate is the boundary that lets specialists ship as self-contained,
//! importable units (their own crate + their own image) instead of being
//! hard-wired into the host.

use serde::{Deserialize, Serialize};

/// A declarative description of a specialist sub-agent the orchestrator may
/// delegate to. Every field is owned data so the spec is `Send + 'static` and can
/// cross thread boundaries (the host runs specialist jobs on background workers).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistSpec {
    /// The name the orchestrator routes by — the `delegate` tool's `specialist`
    /// enum value (e.g. `"browser"`). Must be unique across registered specialists.
    pub route_name: String,
    /// A one-line description of what this specialist does, surfaced to the
    /// orchestrator so it can decide when to delegate here.
    pub description: String,
    /// The agent-graph profile identity (e.g. `"browser-specialist"`). The host
    /// admits the specialist into the job graph under this id.
    pub profile_id: String,
    /// The profile version, recorded with the registered profile.
    pub profile_version: String,
    /// The session-group slug the specialist's job containers live under
    /// (e.g. `"browser-1"`); jobs run at `{sessions}/{group_slug}/{job_id}`.
    pub group_slug: String,
    /// The custom image's repository (e.g. `"assistant-specialist-browser"`). The host
    /// builds an `ImageRef` from this plus the tag (and optional digest).
    pub image_repository: String,
    /// The image tag (e.g. the specialist crate's version).
    pub image_tag: String,
    /// An optional content digest; when set the host pins the image by digest
    /// rather than by tag.
    pub image_digest: Option<String>,
    /// The maximum number of concurrent instances of this specialist the host
    /// will create (browsing is session-stateful, so typically `1`).
    pub max_specialists: u32,
    /// The per-instance concurrent-job ceiling the host's policy enforces.
    pub max_concurrent_jobs: u32,
    /// The size ceiling for a single returned artifact, in bytes.
    pub max_artifact_bytes: u64,
    /// The complete system prompt for the specialist's in-container Claude turn.
    /// The builder folds any guardrails (e.g. a network allowlist) into this
    /// string; the generic shim harness uses it verbatim.
    pub system_prompt: String,
    /// The Agent SDK built-in tools to enable for the turn (e.g. `["Bash"]`).
    pub tools: Vec<String>,
    /// The in-process assistant MCP tools this agent may use, by name — a subset
    /// of `schedule_message`, `cancel_schedule`, `pause_schedule`,
    /// `resume_schedule`, `save_memory`, `send_message`. Empty (the default) is a
    /// leaf specialist with no scheduling or messaging tools. `send_message` also
    /// requires a non-empty `destinations`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_tools: Vec<String>,
    /// The auto-approve permission patterns (e.g. `["Bash(agent-browser:*)"]`);
    /// anything outside these is denied rather than prompted.
    pub allowed_tools: Vec<String>,
    /// The per-turn step ceiling, bounding a stuck or looping specialist turn.
    pub max_turns: u32,
    /// Wall-clock deadline for a single turn, in seconds. `None` uses the host
    /// default (short — fine for quick turns). Long-running work (e.g. an SE agent
    /// implementing an issue) must raise this well above the default, and such
    /// turns should run in the background so they do not block the serve loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_timeout_secs: Option<u64>,
    /// Extra environment variables passed straight into the specialist's
    /// container, for any in-image tooling the specialist reads (the host adds the
    /// generic `ASSISTANT_SPECIALIST_*` turn-config vars on top of these).
    pub extra_env: Vec<(String, String)>,
    /// The OneCLI agent identity this specialist's containers authenticate as.
    /// The host queries `GET /api/container-config?agent=<onecli_agent>` when
    /// spawning the specialist's container, giving it its own scoped credentials
    /// rather than sharing the orchestrator's identity. Required: every specialist
    /// must declare its own identity so credential scoping is always explicit.
    pub onecli_agent: String,
    /// Recurring tasks the host creates when this specialist is registered.
    /// Each fires into the orchestrator's `standing` session so the orchestrator
    /// can delegate back to this specialist. Idempotent: already-created tasks
    /// (any status) are never recreated, even if the specialist is re-registered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standing_tasks: Vec<StandingTask>,
    /// Whether this agent is the Slack-wired orchestrator. Exactly one registered
    /// agent should set this `true`. Only a Slack-wired agent may `send_message`
    /// to a channel (Slack) destination; every other agent's text is internal and
    /// returns to its caller. Defaults to `false` (an ordinary specialist).
    #[serde(default)]
    pub slack_wired: bool,
    /// The names this agent may `send_message` to — other agents' `route_name`s
    /// and (for the Slack-wired orchestrator) channel destination names. The host
    /// enforces this allow-list: a `send_message` to a name outside this set is
    /// dropped. Empty means the agent talks to no one (the default for a leaf
    /// specialist that only reports back to whoever invoked it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
}

/// A recurring task that fires automatically into the orchestrator on the
/// declared interval. Declared in a [`SpecialistSpec`]'s `standing_tasks` (or at
/// the product level); the host creates the schedule on startup, idempotently.
/// Once created, the task is never recreated — even if cancelled by the operator.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingTask {
    /// Stable slug used as the idempotency key (e.g. `"jira-board-sync"`).
    /// Changing this slug orphans the old item and creates a new one.
    pub id: String,
    /// Turn content fired into the orchestrator when this task is due.
    pub summary: String,
    /// How often to fire, in seconds (e.g. `600` for every 10 minutes).
    pub interval_secs: u64,
    /// Optional host-side gate command. Empty stdout or non-zero exit skips
    /// the turn (advances the schedule without inference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_command: Option<String>,
    /// Docker volume mounts passed as `-v <spec>` when the gate runs in a
    /// container (requires `gate_onecli_agent` to be set). Each entry is a
    /// `source:target` or `source:target:options` string — named volumes or
    /// bind mounts are both accepted. Ignored when the gate runs on the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_volumes: Vec<String>,
    /// Container image to use when running the gate in a container (requires
    /// `gate_onecli_agent` to be set). When omitted the host's base image is
    /// used. Set this when the gate command requires tools not present in the
    /// base image (e.g. Python in a specialist image).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_image: Option<String>,
    /// The agent (`route_name`) whose container runs this task's turn. `None`
    /// runs the turn as the orchestrator. Set this to the owning specialist's
    /// route so the recurring task fires as that specialist rather than being
    /// dispatched by the orchestrator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
}

/// A `{ name, description }` pair the host hands the orchestrator (as JSON in
/// `ASSISTANT_SPECIALISTS`) so it can build the dynamic `delegate` routing menu.
/// Derived from the registered specs; the orchestrator never sees the full spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistMenuEntry {
    pub name: String,
    pub description: String,
}

impl SpecialistSpec {
    /// The routing-menu entry for this spec (`route_name` + `description`), the
    /// only part of a spec the orchestrator needs to decide where to delegate.
    pub fn menu_entry(&self) -> SpecialistMenuEntry {
        SpecialistMenuEntry {
            name: self.route_name.clone(),
            description: self.description.clone(),
        }
    }
}

pub const MODULE_ID: &str = "assistant-specialist-spec";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SpecialistSpec {
        SpecialistSpec {
            route_name: "browser".to_string(),
            description: "browses the web and reads pages".to_string(),
            profile_id: "browser-specialist".to_string(),
            profile_version: "0.1.0".to_string(),
            group_slug: "browser-1".to_string(),
            image_repository: "assistant-specialist-browser".to_string(),
            image_tag: "0.1.0".to_string(),
            image_digest: None,
            max_specialists: 1,
            max_concurrent_jobs: 8,
            max_artifact_bytes: 50 * 1024 * 1024,
            system_prompt: "You are a web browsing specialist.".to_string(),
            tools: vec!["Bash".to_string()],
            allowed_tools: vec!["Bash(agent-browser:*)".to_string()],
            max_turns: 40,
            extra_env: vec![],
            onecli_agent: "test-agent-browser".to_string(),
            standing_tasks: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn menu_entry_projects_route_and_description() {
        let entry = sample().menu_entry();
        assert_eq!(entry.name, "browser");
        assert_eq!(entry.description, "browses the web and reads pages");
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: SpecialistSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn menu_entries_round_trip_as_a_list() {
        let entries = vec![sample().menu_entry()];
        let json = serde_json::to_string(&entries).expect("serialize");
        let back: Vec<SpecialistMenuEntry> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entries, back);
    }
}
