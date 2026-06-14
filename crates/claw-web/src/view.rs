//! Serializable view models the UI renders, and the [`WebApp`] trait the host
//! implements to supply them.
//!
//! The crate links no domain code: every page reads its data through `WebApp`,
//! which the host backs with the real groups/sessions/scheduler/approvals/etc.
//! modules. View models are plain owned structs so a handler can serialize them
//! straight to JSON without borrowing host internals.

use serde::Serialize;

/// The instance at a glance.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Overview {
    pub product_id: String,
    pub product_version: String,
    pub platform_version: String,
    pub instance: Option<String>,
    pub ready: bool,
    pub counts: OverviewCounts,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Default)]
pub struct OverviewCounts {
    pub groups: u64,
    pub active_sessions: u64,
    pub pending_approvals: u64,
    pub scheduled_items: u64,
}

/// An agent group and the channels wired to it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub channels: Vec<ChannelView>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ChannelView {
    pub kind: String,
    pub identifier: String,
    pub connected: bool,
}

/// A session row in the sessions list.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionView {
    pub session_id: String,
    pub group_id: String,
    pub state: String,
    pub last_activity: Option<i64>,
}

/// A run summary.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunView {
    pub run_id: String,
    pub session_id: String,
    pub state: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// A run plus its timeline of logs/messages/tool events.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunDetail {
    pub run: RunView,
    pub timeline: Vec<TimelineEntry>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TimelineEntry {
    pub at: i64,
    /// e.g. "log", "message", "tool", "handoff".
    pub kind: String,
    pub text: String,
}

/// A queued unit of work.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QueueItem {
    pub id: String,
    pub kind: String,
    pub enqueued_at: Option<i64>,
    pub state: String,
}

/// A scheduled (one-off or recurring) item.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScheduledItem {
    pub id: String,
    pub description: String,
    pub next_run_at: Option<i64>,
    pub recurrence: Option<String>,
}

/// A user and their role.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UserView {
    pub id: String,
    pub handle: String,
    pub role: String,
}

/// A pending or resolved approval.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ApprovalView {
    pub id: String,
    pub kind: String,
    pub requested_by: String,
    pub state: String,
    pub expires_at: Option<i64>,
}

/// A capability's enablement and readiness, with any setup gaps to fix.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CapabilityView {
    pub id: String,
    pub enabled: bool,
    pub ready: bool,
    pub setup_gaps: Vec<String>,
}

/// The aggregated readiness registry, in a neutral shape the host maps each
/// module's checks into (mirroring pass/fail/skipped without coupling to any
/// one module's enum).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ReadinessReportView {
    pub ready: bool,
    pub blocking_failures: u64,
    pub checks: Vec<ReadinessCheckView>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ReadinessCheckView {
    pub module: String,
    pub name: String,
    /// "pass", "fail", or "skipped".
    pub status: String,
    pub detail: String,
}

/// A specialist agent's runtime status and any artifacts it has captured.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SpecialistStatusView {
    pub id: String,
    pub kind: String,
    pub ready: bool,
    pub state: String,
    pub artifacts: Vec<ArtifactRefView>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ArtifactRefView {
    pub name: String,
    pub path: String,
    pub captured_at: Option<i64>,
    pub size: u64,
}

/// The data source behind every read page. The host implements this over its
/// real modules; the crate only renders what it returns. Lookups that can miss
/// return `Option` so the handler can answer 404.
pub trait WebApp {
    fn overview(&self) -> Overview;
    fn groups(&self) -> Vec<GroupView>;
    fn sessions(&self) -> Vec<SessionView>;
    fn run_detail(&self, run_id: &str) -> Option<RunDetail>;
    fn queue(&self) -> Vec<QueueItem>;
    fn scheduled(&self) -> Vec<ScheduledItem>;
    fn users(&self) -> Vec<UserView>;
    fn approvals(&self) -> Vec<ApprovalView>;
    fn capabilities(&self) -> Vec<CapabilityView>;
    /// The aggregated readiness registry the UI surfaces (and that blocks
    /// "ready" when any check is a blocking failure).
    fn readiness(&self) -> ReadinessReportView;
    /// Specialist agents (e.g. the browser specialist) and their artifacts.
    fn specialists(&self) -> Vec<SpecialistStatusView>;
}
