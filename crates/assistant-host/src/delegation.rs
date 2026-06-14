//! Host-side specialist delegation: run a `delegate` action emitted by an
//! orchestrator turn as a real specialist sub-agent run, then hand its result
//! back to the orchestrator.
//!
//! This is the composition point the agent-graph engine and the per-session
//! [`Host`] machinery meet. The engine ([`assistant_agent_graph`]) owns the job
//! lifecycle, the per-profile creation/concurrency policy, the structured
//! handoff/result packets, and the audit trail; the [`Host`] owns spawning and
//! driving the specialist's container exactly like an inbound turn. A specialist
//! runs in its own job-keyed container under the specialist group, so it never
//! collides with an orchestrator channel's container.
//!
//! Delegation is asynchronous from the orchestrator's point of view, and runs
//! off the serve thread: the orchestrator turn that calls `delegate` returns
//! immediately (acknowledging the user), then the serve loop splits the run into
//! three phases so a browsing specialist never blocks inbound handling —
//! [`begin_specialist`] (serve thread: open and start the job under the
//! concurrency cap), [`run_specialist_turn`] (a background worker thread: drive
//! the specialist's container to a result), and [`finish_specialist`] (serve
//! thread, from the idle drain: terminalize the job and build the re-injection
//! text). The result is re-injected as a fresh follow-up orchestrator turn by the
//! serve loop (see [`crate::slack`]).
//!
//! The host is specialist-agnostic: it routes a `delegate` action to one of the
//! registered [`SpecialistSpec`]s by `route_name`, admits that spec's profile
//! into the graph, and runs the spec's custom image as a real Claude turn
//! ([`RunnerAuthMode::Specialist`]) credentialed through the OneCLI proxy. The
//! spec carries the complete in-container turn config (system prompt, tools,
//! limits, env); the host hands it to the generic shim harness via the
//! `ASSISTANT_SPECIALIST_*` env. No browser- or specialist-specific knowledge lives
//! here.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use assistant_agent_graph::{
    artifacts_within_policy, create_specialist, start_job, store, transition_job, validate_handoff,
    AuditEvent, AuditSink, HandoffPacket, JobBudget, JobEvent, MemoryFact, ProfileLimits,
    RegisteredProfile, RetentionLabel, SpecialistJob, SpecialistRegistry, SpecialistResult,
    SpecialistStatus,
};
use assistant_runtime_docker::{ContainerRuntime, ImageRef, RunnerAuthMode};
use assistant_session::{InboundMessage, SessionLayout};
use assistant_specialist_spec::SpecialistSpec;

use crate::run::{Host, HostConfig};

/// The orchestrator group label recorded on a job (audit/provenance only).
const ORCHESTRATOR_GROUP: &str = "orchestrator";

/// Backstop a specialist job records as its timeout. Not actively enforced here
/// (no timer drives a `Timeout` event in this first cut); the per-turn
/// `HostConfig::turn_timeout` bounds the actual run.
const SPECIALIST_TIMEOUT_SECS: u64 = 300;

/// A started specialist job, handed off to a background worker to run.
///
/// [`begin_specialist`] has already opened the job and driven it to `Running`
/// under the concurrency cap; this carries the per-job data the worker needs
/// ([`run_specialist_turn`]) plus the resolved [`SpecialistSpec`] (so the worker
/// runs the right image/turn-config and the drain reads `max_artifact_bytes`
/// without re-resolving). The shared inputs (base host config, sessions dir,
/// runtime factory) stay on the dispatcher and are cloned into the worker
/// separately. `Send + 'static` (all owned data) so it crosses the dispatch
/// boundary onto a worker thread.
pub struct SpecialistTicket {
    /// The job id the agent-graph lifecycle is keyed on. The worker runs the
    /// container; the drain calls [`finish_specialist`] with this id to
    /// terminalize the job.
    pub job_id: String,
    /// The validated handoff (goal + facts + constraints) the worker renders into
    /// the specialist's inbound turn.
    pub packet: HandoffPacket,
    /// The resolved specialist this job runs as: its image, profile identity, and
    /// in-container turn config.
    pub spec: SpecialistSpec,
}

/// The wire payload a `delegate` action carries in its outbound `content` (the
/// serde body the shim's `delegate` tool emits).
#[derive(serde::Deserialize)]
struct DelegatePayload {
    specialist: String,
    goal: String,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
}

/// Audit sink that logs each delegation event to stderr as JSON. The host has no
/// durable audit store yet; the structured event shape is preserved so a real
/// sink can replace this without touching the flow.
struct LogAuditSink;

impl AuditSink for LogAuditSink {
    fn record(&mut self, event: AuditEvent) {
        match serde_json::to_string(&event) {
            Ok(json) => eprintln!("delegation audit: {json}"),
            Err(_) => eprintln!("delegation audit: {event:?}"),
        }
    }
}

/// A process-unique job id, monotonic within a run and across restarts (the
/// nanosecond clock advances). Digits and a hyphen only, so it is a valid
/// session-folder name.
fn next_job_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{n}")
}

/// Open and start a delegated job on the serve thread, returning a [`SpecialistTicket`]
/// the caller hands to a background worker (which runs [`run_specialist_turn`]).
///
/// This is the synchronous, DB-touching first phase: parse and validate the
/// payload, resolve the requested specialist against `specialists` by
/// `route_name`, admit its profile, create the specialist group on first use,
/// validate the handoff, then `start_job` (which enforces the concurrency cap and
/// audits) and drive the job to `Running`. Because every `begin_specialist` runs
/// on the single serve thread, the cap check is race-free. `Err` carries a
/// human-readable reason the caller surfaces to the user before any worker is
/// spawned (bad payload, unknown specialist, or cap reached).
pub(crate) fn begin_specialist(
    conn: &Connection,
    specialists: &[SpecialistSpec],
    payload_json: &str,
) -> Result<SpecialistTicket, String> {
    let payload: DelegatePayload =
        serde_json::from_str(payload_json).map_err(|e| format!("bad delegate payload: {e}"))?;

    let spec = specialists
        .iter()
        .find(|s| s.route_name == payload.specialist)
        .ok_or_else(|| {
            let available = specialists
                .iter()
                .map(|s| s.route_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "unknown specialist {:?}; available: [{available}]",
                payload.specialist
            )
        })?;

    // The resolved specialist's identity and limits, admitted into the graph as
    // plain registered-profile data (the graph never links any specialist crate).
    let mut registry = SpecialistRegistry::new();
    registry.register(RegisteredProfile::specialist(
        spec.profile_id.clone(),
        spec.profile_version.clone(),
        ProfileLimits::new(spec.max_specialists, spec.max_concurrent_jobs),
    ));

    let mut audit = LogAuditSink;

    // Create the specialist group on first use; later jobs reuse it (when
    // max_specialists == 1, a second create would be rejected by policy).
    if store::specialist_group_count(conn, &spec.profile_id).map_err(|e| e.to_string())? == 0 {
        create_specialist(conn, &registry, &mut audit, &spec.profile_id, &spec.group_slug)
            .map_err(|e| format!("creating the {:?} specialist failed: {e}", spec.route_name))?;
    }

    // Structured handoff: validate the goal, carry facts/constraints with the
    // most restrictive (ephemeral) retention.
    let mut packet = HandoffPacket::new(payload.goal.clone());
    packet.facts = payload
        .facts
        .iter()
        .map(|text| MemoryFact {
            text: text.clone(),
            source: None,
            retention: RetentionLabel::Ephemeral,
        })
        .collect();
    packet.constraints = payload.constraints.clone();
    validate_handoff(&packet).map_err(|e| format!("invalid handoff: {e}"))?;

    // Open and start the job (enforces the concurrency limit, persists, audits),
    // then drive it to Running. The job stays `running` — counting toward the cap —
    // until the drain calls `finish_specialist`, so the cap bounds in-flight jobs.
    let job_id = next_job_id();
    let job = SpecialistJob::new(
        job_id.clone(),
        ORCHESTRATOR_GROUP,
        spec.group_slug.clone(),
        spec.profile_id.clone(),
        JobBudget::default(),
        SPECIALIST_TIMEOUT_SECS,
    );
    start_job(conn, &registry, &mut audit, &job)
        .map_err(|e| format!("starting the specialist job failed: {e}"))?;
    transition_job(conn, &mut audit, &job_id, JobEvent::Start)
        .map_err(|e| format!("starting the specialist job failed: {e}"))?;

    Ok(SpecialistTicket {
        job_id,
        packet,
        spec: spec.clone(),
    })
}

/// Terminalize a delegated job on the serve thread from the worker's `outcome`,
/// returning the specialist's result text for re-injection as a follow-up turn.
///
/// The third phase, run from the serve loop's drain once a worker reports back.
/// On a worker error the job is transitioned to `Failed` and the reason is
/// returned (the caller surfaces it to the user). On success it records the
/// run-link provenance (additive — a link-write failure is logged, not fatal),
/// enforces the artifact policy (against the spec's `max_artifact_bytes`), marks
/// the job `Complete`, and returns the re-injection text built from the original
/// `goal`.
pub(crate) fn finish_specialist(
    conn: &Connection,
    job_id: &str,
    goal: &str,
    max_artifact_bytes: u64,
    outcome: Result<(String, Option<String>), String>,
) -> Result<String, String> {
    let mut audit = LogAuditSink;

    let (answer, container_id) = match outcome {
        Ok(result) => result,
        Err(err) => {
            let _ = transition_job(conn, &mut audit, job_id, JobEvent::Fail);
            return Err(err);
        }
    };

    // Provenance: link the container that ran the job to the job row. Additive
    // (the `specialist_jobs` lifecycle is authoritative), so a link-write failure
    // is logged, not fatal.
    if let Some(run_link) = container_id
        && let Err(e) = store::add_run_link(conn, job_id, &run_link)
    {
        eprintln!("delegation: recording run link for job {job_id} failed: {e}");
    }

    // The turn succeeded: enforce the artifact policy on the structured result
    // (defensive — the turn path produces no inline/oversized artifacts), then
    // mark the job complete.
    let result = SpecialistResult::new(SpecialistStatus::Completed, answer.clone());
    if let Err(e) = artifacts_within_policy(&result, max_artifact_bytes) {
        let _ = transition_job(conn, &mut audit, job_id, JobEvent::Fail);
        return Err(format!("specialist result violated artifact policy: {e}"));
    }
    transition_job(conn, &mut audit, job_id, JobEvent::Complete)
        .map_err(|e| format!("completing the specialist job failed: {e}"))?;

    Ok(format!(
        "[A delegated sub-task finished for the user's request: \"{goal}\". Below is the result it returned — share the relevant findings with the user in your own words, as your own work. Do not ask what they wanted; this IS the answer to relay:]\n{answer}"
    ))
}

/// Spawn the specialist's job-keyed container, run one turn over the handoff, and
/// return its joined text plus the id of the container that ran it (for run-link
/// provenance). The container is stopped before returning regardless of the turn
/// outcome.
///
/// This is the slow middle phase, run on a background worker thread so it never
/// blocks the serve loop. It touches no central [`Connection`]: the job is already
/// `Running` (see [`begin_specialist`]) and is terminalized later by
/// [`finish_specialist`]. The error is already a `String`, so the returned
/// outcome is `Send` and can travel back to the serve thread over a channel.
pub(crate) fn run_specialist_turn<R, F>(
    base_config: &HostConfig,
    sessions_dir: &Path,
    spec: &SpecialistSpec,
    runtime_factory: &F,
    job_id: &str,
    packet: &HandoffPacket,
) -> Result<(String, Option<String>), String>
where
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
{
    let layout = SessionLayout::derive(sessions_dir, &spec.group_slug, job_id)
        .map_err(|e| format!("deriving the specialist session failed: {e}"))?;

    // Turn config the generic shim harness reads from the container env.
    let tools_json = serde_json::to_string(&spec.tools)
        .map_err(|e| format!("serializing the specialist tools failed: {e}"))?;
    let allowed_tools_json = serde_json::to_string(&spec.allowed_tools)
        .map_err(|e| format!("serializing the specialist allowed-tools failed: {e}"))?;

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
    config.memory = None;
    config.extra_env = spec.extra_env.clone();
    config.extra_env.extend([
        (
            "ASSISTANT_SPECIALIST_SYSTEM_PROMPT".to_string(),
            spec.system_prompt.clone(),
        ),
        ("ASSISTANT_SPECIALIST_TOOLS".to_string(), tools_json),
        ("ASSISTANT_SPECIALIST_ALLOWED_TOOLS".to_string(), allowed_tools_json),
        (
            "ASSISTANT_SPECIALIST_MAX_TURNS".to_string(),
            spec.max_turns.to_string(),
        ),
    ]);

    let inbound = InboundMessage {
        sender: ORCHESTRATOR_GROUP.to_string(),
        content: specialist_inbound(packet),
        metadata: None,
    };

    let mut host = Host::new(layout, runtime_factory(), config);
    let outcome = host.run_turn(&inbound);
    // Capture the container id while it is still set, before shutdown clears it.
    let container_id = host.container_id().map(|id| id.0.clone());
    // Best-effort stop regardless of the turn outcome (the next job gets a fresh
    // container under a fresh job id).
    let _ = host.shutdown();
    let replies = outcome.map_err(|e| format!("the specialist turn failed: {e}"))?;

    let answer = replies
        .iter()
        .filter(|m| m.kind == "text")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if answer.trim().is_empty() {
        return Err("the specialist produced no result".to_string());
    }
    Ok((answer, container_id))
}

/// Render the specialist's inbound content from the handoff: the goal, plus any
/// facts and constraints as labelled context (so a real specialist receives them
/// and the stub echoes something meaningful).
fn specialist_inbound(packet: &HandoffPacket) -> String {
    let mut content = packet.goal.clone();
    if !packet.facts.is_empty() {
        content.push_str("\n\nContext:");
        for fact in &packet.facts {
            content.push_str("\n- ");
            content.push_str(&fact.text);
        }
    }
    if !packet.constraints.is_empty() {
        content.push_str("\n\nConstraints:");
        for constraint in &packet.constraints {
            content.push_str("\n- ");
            content.push_str(constraint);
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data that crosses the dispatch boundary onto a background worker must
    /// be `Send + 'static`: the worker is `std::thread::spawn`ed with a clone of
    /// the base [`HostConfig`] and the resolved [`SpecialistSpec`], and is handed a
    /// [`SpecialistTicket`]. If any of these grow a non-`Send` field (an `Rc`, a
    /// borrowed reference), background delegation stops compiling — this catches it.
    #[test]
    fn dispatched_types_are_send_and_static() {
        fn assert_send_static<T: Send + 'static>() {}
        assert_send_static::<HostConfig>();
        assert_send_static::<SpecialistSpec>();
        assert_send_static::<SpecialistTicket>();
        assert_send_static::<HandoffPacket>();
    }
}
