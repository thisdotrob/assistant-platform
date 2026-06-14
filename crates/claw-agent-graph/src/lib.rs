//! Agent graph: the predefined specialist registry, the specialist-job model
//! and its status machine, structured handoff/result packets, agent-to-agent
//! routing over the session DB protocol, central-DB job persistence, and the
//! host-facing delegation flow.
//!
//! The crate stays free of any dependency on concrete specialist crates: the
//! host supplies specialist identity and limits as plain [`registry::RegisteredProfile`]
//! data, so the graph never links `claw-capabilities` or `claw-specialist-browser`.

pub mod audit;
pub mod delegation;
pub mod handoff;
pub mod job;
pub mod readiness;
pub mod registry;
pub mod routing;
pub mod store;

pub use audit::{AuditEvent, AuditSink, VecAuditSink};
pub use delegation::{
    create_specialist, route_handoff, route_result, start_job, transition_job, DelegationError,
};
pub use handoff::{
    artifacts_within_policy, validate_handoff, ArtifactPolicyError, CredentialPolicy, HandoffError,
    HandoffPacket, MemoryFact, ResultArtifact, RetentionLabel, ScopeLabels, SpecialistResult,
    SpecialistStatus, HANDOFF_KIND, RESULT_KIND,
};
pub use job::{
    cancel_grace_exceeded, is_timed_out, transition, JobBudget, JobError, JobEvent, JobStatus,
    SpecialistJob,
};
pub use readiness::{find_orphaned_jobs, no_orphaned_jobs, profiles_resolve, CheckStatus};
pub use registry::{
    authorize_create, authorize_job_start, PolicyError, ProfileLimits, RegisteredProfile,
    SpecialistRegistry,
};
pub use routing::{
    authorize_a2a, authorize_external_destination, collect_result, deliver_handoff,
    deliver_result, forward_attachment, A2aAcl, ReturnPath, RoutingError,
};
pub use store::{StoreError, AGENT_GRAPH_JOBS_VERSION};

pub const MODULE_ID: &str = "claw-agent-graph";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
