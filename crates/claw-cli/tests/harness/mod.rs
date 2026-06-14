//! Shared end-to-end harness for the product verticals.
//!
//! Every helper here composes the REAL platform crates through their public
//! APIs and existing reusable fakes (`FakeContainer`, `FakeQmd`, the channel
//! normalizers, the injected runtime readiness probes). Nothing is
//! reimplemented: a bootstrapped temp instance is driven exactly as a product
//! host would drive it.
//!
//! One faithful detail this surfaces: `claw_cli`'s bootstrap applies only the
//! baseline central migration set, so a live instance gains each owning
//! module's later migrations through the upgrade runner. [`open_full_central`]
//! reproduces that by layering the modules' own `migrations()` over the
//! bootstrapped DB — `claw_db::apply` is idempotent, so the baseline rows are
//! checksum-verified and skipped while the module migrations advance the schema.
#![allow(dead_code)]

use std::path::Path;

use claw_agent_graph::{
    collect_result, create_specialist, route_handoff, route_result, start_job, store,
    transition_job, A2aAcl, HandoffPacket, JobBudget, JobEvent, JobStatus, RegisteredProfile,
    ReturnPath, SpecialistJob, SpecialistRegistry, SpecialistResult, SpecialistStatus, VecAuditSink,
    RESULT_KIND,
};
use claw_cli::{setup, BootstrapRequest};
use claw_config::InstanceLayout;
use claw_db::{apply, baseline_migrations, baseline_owner_modules, open_central};
use claw_memory::{
    upsert_entry, Confidence, FakeQmd, IndexDoc, MemoryBackend, MemoryFrontMatter, Retention,
    ReusePolicy, Scope, SearchOutcome, SourceType,
};
use claw_permissions::{
    add_user_dm, bootstrap_owner, create_user, evaluate_sender, grant_role, AccessDecision, Role,
    UnknownPolicy,
};
use claw_scheduler::{
    claim_due, complete_occurrence, list_items, upsert_item, ContextPolicy, Lease, ProjectedItem,
    Recurrence, ScheduleIntent, ScheduledMessageMeta,
};
use claw_session::{
    current_outbound_compat, enqueue_inbound, read_outbound, InboundMessage, LocalControl,
    OutboundMessage, SessionLayout,
};
use assistant_specialist_browser::{BrowserSpecialistProfile, NetworkPolicy, BROWSER_PROFILE_ID};
use claw_web::{
    ApprovalView, CapabilityView, GroupView, Overview, OverviewCounts, QueueItem,
    ReadinessReportView, RunDetail, ScheduledItem, SessionView, SpecialistStatusView, UserView,
    WebApp,
};
use rusqlite::Connection;

/// A bootstrapped instance under a temp HOME. The `TempDir` is held so the
/// instance root outlives the test.
pub struct Instance {
    pub home: tempfile::TempDir,
    pub layout: InstanceLayout,
    pub namespace: String,
    pub product_id: String,
    pub product_version: String,
}

/// Build a product bootstrap request for a temp HOME at the given namespace.
pub fn bootstrap_request(home: &Path, namespace: &str, product_id: &str) -> BootstrapRequest {
    BootstrapRequest {
        namespace: namespace.to_string(),
        product_id: product_id.to_string(),
        product_version: "0.1.0".to_string(),
        instance: None,
        enabled_modules: Vec::new(),
        home: Some(home.to_path_buf()),
        protected_roots: Vec::new(),
        dry_run: false,
    }
}

impl Instance {
    /// Run the product setup entry point (foundational bootstrap, no extra
    /// steps) under a fresh temp HOME and return the derived layout.
    pub fn bootstrap(namespace: &str, product_id: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let product_version = "0.1.0".to_string();
        let code = setup(bootstrap_request(home.path(), namespace, product_id), Vec::new());
        assert_eq!(code, 0, "foundational bootstrap must succeed");
        let layout = InstanceLayout::derive(home.path(), namespace, None).unwrap();
        assert!(layout.config_path().exists(), "config.toml written");
        assert!(layout.central_db_path().exists(), "central db written");
        Self {
            home,
            layout,
            namespace: namespace.to_string(),
            product_id: product_id.to_string(),
            product_version,
        }
    }

    /// Open the bootstrapped central DB and advance it to the full module
    /// schema (scheduler lease columns, memory catalog, agent-graph jobs).
    pub fn full_central(&self) -> Connection {
        open_full_central(&self.layout)
    }

    /// The instance's sessions root, under which agent-group sessions live.
    pub fn sessions_base(&self) -> std::path::PathBuf {
        self.layout.sessions_dir()
    }
}

/// The running platform version, as the manifest records it.
pub fn platform_version() -> String {
    claw_core::platform_metadata().version.to_string()
}

/// Open the central DB and layer the owning modules' own migrations onto the
/// baseline the bootstrap wrote. Idempotent: baseline rows are skipped.
pub fn open_full_central(layout: &InstanceLayout) -> Connection {
    let mut conn = open_central(&layout.central_db_path()).unwrap();
    let order: Vec<String> = baseline_owner_modules()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut set = baseline_migrations(order);
    for m in claw_scheduler::migrations() {
        set.add(m);
    }
    for m in claw_memory::migrations() {
        set.add(m);
    }
    for m in store::migrations() {
        set.add(m);
    }
    apply(&mut conn, &set).unwrap();
    conn
}

// --- Permissions: owner / member seeding and a sender gate ------------------

/// Bootstrap the instance owner, returning the owner user id.
pub fn seed_owner(conn: &Connection, handle: &str) -> i64 {
    bootstrap_owner(conn, handle, Some(handle)).unwrap()
}

/// "Pair" a user on a channel: register them and the DM address that channel
/// traffic resolves through, grant them membership, and return the user id.
pub fn pair_member(conn: &Connection, owner: i64, handle: &str, channel: &str, address: &str) -> i64 {
    let uid = create_user(conn, handle, None).unwrap();
    add_user_dm(conn, uid, channel, address).unwrap();
    grant_role(conn, owner, uid, Role::Member).unwrap();
    uid
}

/// The access decision for a sender on a channel under the given policy.
pub fn sender_decision(
    conn: &Connection,
    channel: &str,
    address: &str,
    policy: UnknownPolicy,
) -> AccessDecision {
    evaluate_sender(conn, channel, address, policy).unwrap()
}

// --- Session message loop ---------------------------------------------------

/// Create and initialize a session under the instance's sessions root.
pub fn open_session(sessions_base: &Path, group: &str, session: &str) -> SessionLayout {
    let sl = SessionLayout::derive(sessions_base, group, session).unwrap();
    LocalControl::new(sl.clone()).init().unwrap();
    sl
}

/// Drive one full host -> container -> host turn: the host enqueues an inbound
/// message, a `FakeContainer` wakes, reads it, emits an echo reply, stops, and
/// the host reads the outbound transcript back.
pub fn run_echo_turn(sl: &SessionLayout, sender: &str, text: &str) -> Vec<OutboundMessage> {
    enqueue_inbound(
        sl,
        &InboundMessage {
            sender: sender.to_string(),
            content: text.to_string(),
            metadata: None,
        },
    )
    .unwrap();

    let container = LocalControl::new(sl.clone()).fake_container();
    container.start("run-1").unwrap();
    let inbound = container.read_inbound().unwrap();
    let reply = inbound
        .last()
        .map(|(_, c)| format!("echo: {c}"))
        .unwrap_or_default();
    container.emit("text", &reply).unwrap();
    container.stop().unwrap();

    read_outbound(sl, current_outbound_compat()).unwrap()
}

// --- Memory: catalog + qmd backend over FakeQmd -----------------------------

/// A memory backend over `FakeQmd` plus the central catalog projection. Each
/// write upserts the catalog row and re-indexes the markdown corpus, so a
/// search reflects everything written for the agent.
pub struct MemoryStore {
    docs: Vec<IndexDoc>,
    backend: FakeQmd,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            backend: FakeQmd::new(),
        }
    }

    /// Write one memory entry for an agent group: project its front matter into
    /// the catalog and add its body to the searchable index.
    pub fn write(
        &mut self,
        conn: &Connection,
        agent_group_id: i64,
        memory_id: &str,
        scope: Scope,
        rel_path: &str,
        body: &str,
    ) {
        let front_matter = MemoryFrontMatter {
            memory_id: memory_id.to_string(),
            owner_agent_group_id: format!("ag_{agent_group_id}"),
            scope,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: None,
            captured_at: Some("2026-06-01T10:00:00Z".to_string()),
            confidence: Confidence::High,
            reuse_policy: ReusePolicy::SameScope,
            retention: Retention::Normal,
        };
        upsert_entry(conn, agent_group_id, rel_path, &front_matter).unwrap();
        self.docs.push(IndexDoc {
            memory_id: memory_id.to_string(),
            rel_path: rel_path.to_string(),
            body: body.to_string(),
        });
        self.backend.reindex(&self.docs);
    }

    /// Search the agent's indexed memory.
    pub fn search(&self, query: &str, limit: usize) -> SearchOutcome {
        self.backend.search(query, limit)
    }
}

// --- Scheduler: project and fire reminders ----------------------------------

/// Project a one-off reminder for an agent group at `process_after`.
pub fn schedule_one_off(
    conn: &Connection,
    agent_group_id: i64,
    summary: &str,
    process_after: i64,
) -> ScheduledMessageMeta {
    let meta = ScheduledMessageMeta::create(
        agent_group_id,
        ScheduleIntent {
            created_by: "owner".to_string(),
            summary: summary.to_string(),
            created_at: process_after - 1,
        },
        process_after,
        None,
        ContextPolicy::CurrentMemory,
    )
    .unwrap();
    upsert_item(conn, &meta, None).unwrap();
    meta
}

/// Project a recurring reminder for an agent group, repeating every `seconds`.
pub fn schedule_recurring(
    conn: &Connection,
    agent_group_id: i64,
    summary: &str,
    process_after: i64,
    seconds: i64,
) -> ScheduledMessageMeta {
    let meta = ScheduledMessageMeta::create(
        agent_group_id,
        ScheduleIntent {
            created_by: "owner".to_string(),
            summary: summary.to_string(),
            created_at: process_after - 1,
        },
        process_after,
        Some(Recurrence::Every { seconds }),
        ContextPolicy::CurrentMemory,
    )
    .unwrap();
    upsert_item(conn, &meta, None).unwrap();
    meta
}

/// Re-project a (possibly advanced) scheduled item into the central index.
pub fn reproject(conn: &Connection, meta: &ScheduledMessageMeta) {
    upsert_item(conn, meta, None).unwrap();
}

/// Claim every occurrence due at `now` under a sweeper lease.
pub fn claim_due_now(conn: &Connection, now: i64) -> Vec<Lease> {
    claim_due(conn, now, "sweeper", 60).unwrap()
}

/// Complete a claimed occurrence as fired.
pub fn complete(conn: &Connection, lease: &Lease, fired_at: i64) {
    complete_occurrence(conn, &lease.occurrence, fired_at).unwrap();
}

/// Items projected for an agent group (newest schedule order).
pub fn scheduled_items(conn: &Connection, agent_group_id: i64) -> Vec<ProjectedItem> {
    list_items(conn, agent_group_id, None).unwrap()
}

// --- Agent graph: the browser specialist ------------------------------------

/// The host-owned browser profile, allow-listed to `example.com`.
pub fn browser_profile() -> BrowserSpecialistProfile {
    BrowserSpecialistProfile::new(NetworkPolicy::allowlist(["example.com"]))
}

/// The browser profile bridged into the agent graph as plain registry data.
pub fn browser_registered() -> RegisteredProfile {
    browser_profile().registered_profile()
}

/// Run a full browser delegation: register the profile, create the specialist
/// group, stand up orchestrator + specialist sessions, drive the job through
/// its guarded state machine, route a handoff in, have a `FakeContainer` emit a
/// structured result, collect it, complete the job, and route the result back
/// to the orchestrator. Returns the collected result.
pub fn delegate_browser(
    conn: &Connection,
    sessions_base: &Path,
    orchestrator_group: &str,
    job_id: &str,
    prompt: &str,
    answer: &str,
) -> SpecialistResult {
    let mut audit = VecAuditSink::new();
    let mut reg = SpecialistRegistry::new();
    reg.register(browser_registered());

    let specialist_group = "browser-1";
    create_specialist(conn, &reg, &mut audit, BROWSER_PROFILE_ID, specialist_group).unwrap();

    let orch_layout = SessionLayout::derive(sessions_base, orchestrator_group, "sess-1").unwrap();
    let spec_layout = SessionLayout::derive(sessions_base, specialist_group, job_id).unwrap();
    LocalControl::new(orch_layout.clone()).init().unwrap();
    LocalControl::new(spec_layout.clone()).init().unwrap();

    let mut acl = A2aAcl::new();
    acl.allow(orchestrator_group, specialist_group);

    let job = SpecialistJob::new(
        job_id,
        orchestrator_group,
        specialist_group,
        BROWSER_PROFILE_ID,
        JobBudget {
            max_tokens: Some(2000),
            max_wall_secs: Some(120),
        },
        60,
    );
    start_job(conn, &reg, &mut audit, &job).unwrap();
    transition_job(conn, &mut audit, job_id, JobEvent::Start).unwrap();

    let handoff = HandoffPacket::new(prompt);
    let return_path = ReturnPath {
        orchestrator_group: orchestrator_group.to_string(),
        session_id: "sess-1".to_string(),
        inbound_seq: 0,
    };
    route_handoff(
        &mut audit,
        &acl,
        &spec_layout,
        orchestrator_group,
        specialist_group,
        &handoff,
        &return_path,
    )
    .unwrap();

    let spec_container = LocalControl::new(spec_layout.clone()).fake_container();
    spec_container.start("run-1").unwrap();
    store::add_run_link(conn, job_id, "run-1").unwrap();

    let mut result = SpecialistResult::new(SpecialistStatus::Completed, "browser task complete");
    result.answer = answer.to_string();
    spec_container
        .emit(RESULT_KIND, &result.to_json().unwrap())
        .unwrap();

    let collected = collect_result(&spec_layout).unwrap();
    assert_eq!(
        transition_job(conn, &mut audit, job_id, JobEvent::Complete).unwrap(),
        JobStatus::Succeeded
    );
    route_result(
        &mut audit,
        &orch_layout,
        specialist_group,
        orchestrator_group,
        &collected,
    )
    .unwrap();
    collected
}

// --- Web: a read surface over the real instance state -----------------------

/// A thin [`WebApp`] over the real instance: scheduled items come straight from
/// the scheduler projection; sessions and queue rows are the ones the scenario
/// observed; the remaining pages return their empty/default shapes (the product
/// host fills them from modules the verticals do not exercise).
pub struct InstanceWebApp<'a> {
    pub conn: &'a Connection,
    pub product_id: String,
    pub product_version: String,
    pub platform_version: String,
    pub agent_group_id: i64,
    pub group_count: u64,
    pub sessions: Vec<SessionView>,
    pub queue: Vec<QueueItem>,
    pub ready: bool,
}

impl WebApp for InstanceWebApp<'_> {
    fn overview(&self) -> Overview {
        Overview {
            product_id: self.product_id.clone(),
            product_version: self.product_version.clone(),
            platform_version: self.platform_version.clone(),
            instance: None,
            ready: self.ready,
            counts: OverviewCounts {
                groups: self.group_count,
                active_sessions: self.sessions.len() as u64,
                pending_approvals: 0,
                scheduled_items: self.scheduled().len() as u64,
            },
        }
    }

    fn groups(&self) -> Vec<GroupView> {
        Vec::new()
    }

    fn sessions(&self) -> Vec<SessionView> {
        self.sessions.clone()
    }

    fn run_detail(&self, _run_id: &str) -> Option<RunDetail> {
        None
    }

    fn queue(&self) -> Vec<QueueItem> {
        self.queue.clone()
    }

    fn scheduled(&self) -> Vec<ScheduledItem> {
        scheduled_items(self.conn, self.agent_group_id)
            .into_iter()
            .map(|item| ScheduledItem {
                id: item.id,
                description: item.intent,
                next_run_at: item.process_after,
                recurrence: item
                    .recurrence
                    .map(|Recurrence::Every { seconds }| format!("every {seconds}s")),
            })
            .collect()
    }

    fn users(&self) -> Vec<UserView> {
        Vec::new()
    }

    fn approvals(&self) -> Vec<ApprovalView> {
        Vec::new()
    }

    fn capabilities(&self) -> Vec<CapabilityView> {
        Vec::new()
    }

    fn readiness(&self) -> ReadinessReportView {
        ReadinessReportView {
            ready: self.ready,
            blocking_failures: 0,
            checks: Vec::new(),
        }
    }

    fn specialists(&self) -> Vec<SpecialistStatusView> {
        Vec::new()
    }
}
