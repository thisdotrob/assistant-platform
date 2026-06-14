//! The shared vocabulary for declaring a specialist sub-agent.
//!
//! A [`SpecialistSpec`] is the one piece of data a product hands the host to
//! register a specialist: its routing name, its agent-graph identity and
//! concurrency limits, the custom container image carrying its binaries, and the
//! in-container turn configuration (system prompt, allowed tools, env). It is
//! deliberately plain data with no dependency on the host, the Docker runtime, or
//! the agent-graph engine, so a specialist crate (e.g. `claw-specialist-browser`)
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The custom image's repository (e.g. `"claw-specialist-browser"`). The host
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
    /// The auto-approve permission patterns (e.g. `["Bash(agent-browser:*)"]`);
    /// anything outside these is denied rather than prompted.
    pub allowed_tools: Vec<String>,
    /// The per-turn step ceiling, bounding a stuck or looping specialist turn.
    pub max_turns: u32,
    /// Extra environment variables passed straight into the specialist's
    /// container, for any in-image tooling the specialist reads (the host adds the
    /// generic `CLAW_SPECIALIST_*` turn-config vars on top of these).
    pub extra_env: Vec<(String, String)>,
}

/// A `{ name, description }` pair the host hands the orchestrator (as JSON in
/// `CLAW_SPECIALISTS`) so it can build the dynamic `delegate` routing menu.
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

pub const MODULE_ID: &str = "claw-specialist-spec";
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
            image_repository: "claw-specialist-browser".to_string(),
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
