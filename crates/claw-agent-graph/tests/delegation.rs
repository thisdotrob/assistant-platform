//! End-to-end delegation round-trip over the local session harness.
//!
//! Drives the full flow without Docker or a real browser: register the browser
//! specialist profile, create a specialist group, start a job, hand off a packet
//! into the specialist's session, have a `FakeContainer` emit a structured
//! result, collect it, complete the job, and deliver the result back to the
//! orchestrator's session — asserting policy gates and audit events along the way.

use claw_agent_graph::audit::AuditEvent;
use claw_agent_graph::handoff::{
    artifacts_within_policy, validate_handoff, MemoryFact, ResultArtifact, RetentionLabel,
    SpecialistResult, SpecialistStatus, RESULT_KIND,
};
use claw_agent_graph::job::{
    cancel_grace_exceeded, is_timed_out, JobBudget, JobEvent, JobStatus, SpecialistJob,
};
use claw_agent_graph::readiness::{no_orphaned_jobs, CheckStatus};
use claw_agent_graph::registry::{ProfileLimits, RegisteredProfile, SpecialistRegistry};
use claw_agent_graph::routing::{
    authorize_external_destination, collect_result, forward_attachment, A2aAcl, ReturnPath,
    RoutingError,
};
use claw_agent_graph::{
    create_specialist, route_handoff, route_result, start_job, store, transition_job,
    DelegationError, VecAuditSink,
};

use claw_db::{apply, baseline_migrations, baseline_owner_modules};
use claw_session::{LocalControl, SessionLayout};

use rusqlite::Connection;

fn migrated_db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    let order: Vec<String> = baseline_owner_modules()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut set = baseline_migrations(order);
    for m in store::migrations() {
        set.add(m);
    }
    apply(&mut conn, &set).unwrap();
    conn
}

fn registry() -> SpecialistRegistry {
    let mut r = SpecialistRegistry::new();
    r.register(RegisteredProfile::specialist(
        "browser-specialist",
        "0.1.0",
        ProfileLimits::new(1, 2),
    ));
    r
}

#[test]
fn full_delegation_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = migrated_db();
    let reg = registry();
    let mut audit = VecAuditSink::new();

    // 1. Create a specialist group under policy.
    let specialist_group =
        create_specialist(&conn, &reg, &mut audit, "browser-specialist", "browser-1").unwrap();
    assert_eq!(specialist_group, "browser-1");

    // 2. Stand up the two sessions: orchestrator and specialist.
    let orch_layout = SessionLayout::derive(root, "orchestrator", "sess-1").unwrap();
    let spec_layout = SessionLayout::derive(root, "browser-1", "job-1").unwrap();
    let orch_control = LocalControl::new(orch_layout.clone());
    let spec_control = LocalControl::new(spec_layout.clone());
    orch_control.init().unwrap();
    spec_control.init().unwrap();

    // 3. Policy gates: the edge must be allowed, and the specialist may not own
    //    external destinations.
    let mut acl = A2aAcl::new();
    acl.allow("orchestrator", "browser-1");
    let bp = reg.get("browser-specialist").unwrap();
    assert!(matches!(
        authorize_external_destination(bp),
        Err(RoutingError::ExternalDestinationForbidden { .. })
    ));

    // 4. Start the job, then drive it through the guarded state machine to
    //    running (queued -> running).
    let job = SpecialistJob::new(
        "job-1",
        "orchestrator",
        "browser-1",
        "browser-specialist",
        JobBudget {
            max_tokens: Some(2000),
            max_wall_secs: Some(120),
        },
        60,
    );
    start_job(&conn, &reg, &mut audit, &job).unwrap();
    assert_eq!(
        transition_job(&conn, &mut audit, "job-1", JobEvent::Start).unwrap(),
        JobStatus::Running
    );

    // 5. Route a handoff packet into the specialist's session (ACL-gated, audited).
    let mut handoff = claw_agent_graph::HandoffPacket::new("Find the latest invoice total");
    handoff.facts.push(MemoryFact {
        text: "Account email is owner@example.com".into(),
        source: Some("memory:fact-42".into()),
        retention: RetentionLabel::CiteOnly,
    });
    handoff.constraints.push("do not place orders".into());
    validate_handoff(&handoff).unwrap();
    let return_path = ReturnPath {
        orchestrator_group: "orchestrator".into(),
        session_id: "sess-1".into(),
        inbound_seq: 0,
    };
    let handoff_seq = route_handoff(
        &mut audit,
        &acl,
        &spec_layout,
        "orchestrator",
        "browser-1",
        &handoff,
        &return_path,
    )
    .unwrap();
    assert_eq!(handoff_seq % 2, 0, "host-written inbound seqs are even");

    // 6. The specialist (a fake container) wakes, reads the handoff, and emits a
    //    structured result.
    let spec_container = spec_control.fake_container();
    spec_container.start("run-1").unwrap();
    store::add_run_link(&conn, "job-1", "run-1").unwrap();
    let inbound = spec_container.read_inbound().unwrap();
    assert!(inbound.iter().any(|(_, c)| c.contains("invoice total")));

    let mut result = SpecialistResult::new(SpecialistStatus::Completed, "found the total");
    result.answer = "42".into();
    result.artifacts.push(ResultArtifact {
        artifact_id: "shot-1".into(),
        kind: "screenshot".into(),
        by_reference: true,
        size_bytes: Some(1024),
    });
    spec_container
        .emit(RESULT_KIND, &result.to_json().unwrap())
        .unwrap();

    // 7. Collect the result and check it satisfies the artifact policy.
    let collected = collect_result(&spec_layout).unwrap();
    assert_eq!(collected.status, SpecialistStatus::Completed);
    assert_eq!(collected.answer, "42");
    artifacts_within_policy(&collected, 50 * 1024 * 1024).unwrap();

    // 8. Complete the job and route the result back to the orchestrator.
    assert_eq!(
        transition_job(&conn, &mut audit, "job-1", JobEvent::Complete).unwrap(),
        JobStatus::Succeeded
    );
    assert_eq!(store::load_job(&conn, "job-1").unwrap().status, JobStatus::Succeeded);
    let result_seq = route_result(&mut audit, &orch_layout, "browser-1", "orchestrator", &collected).unwrap();
    assert_eq!(result_seq % 2, 0);

    // 9. The orchestrator session now carries the result inbound.
    let orch_container = orch_control.fake_container();
    let orch_inbound = orch_container.read_inbound().unwrap();
    assert!(orch_inbound.iter().any(|(_, c)| c.contains("\"answer\":\"42\"")));

    // 10. Audit trail captured every consequential action.
    assert!(audit.events.iter().any(|e| matches!(
        e,
        AuditEvent::SpecialistCreated { specialist_group, .. } if specialist_group == "browser-1"
    )));
    assert!(audit
        .events
        .iter()
        .any(|e| matches!(e, AuditEvent::DelegationStarted { job_id, .. } if job_id == "job-1")));
    assert!(audit.events.iter().any(|e| matches!(
        e,
        AuditEvent::DelegationCompleted { status: JobStatus::Succeeded, .. }
    )));
    // Routing the handoff and the result each emitted an agent-routing event.
    assert!(audit.events.iter().any(|e| matches!(
        e,
        AuditEvent::AgentRouting { kind, .. } if kind == "specialist_handoff"
    )));
    assert!(audit.events.iter().any(|e| matches!(
        e,
        AuditEvent::AgentRouting { kind, .. } if kind == "specialist_result"
    )));
}

#[test]
fn cancelled_job_reaches_cancelled_terminal_and_clears_orphans() {
    let conn = migrated_db();
    let reg = registry();
    let mut audit = VecAuditSink::new();

    let job = SpecialistJob::new(
        "job-c",
        "orchestrator",
        "browser-1",
        "browser-specialist",
        JobBudget::default(),
        60,
    );
    start_job(&conn, &reg, &mut audit, &job).unwrap();
    transition_job(&conn, &mut audit, "job-c", JobEvent::Start).unwrap();
    // A running job is an orphan from the readiness check's perspective.
    assert!(matches!(no_orphaned_jobs(&conn).unwrap(), CheckStatus::Fail { .. }));

    // Host requests cancel, the grace period elapses, and the job is reaped.
    store::set_cancel_requested(&conn, "job-c").unwrap();
    assert!(cancel_grace_exceeded(10, 10));
    assert_eq!(
        transition_job(&conn, &mut audit, "job-c", JobEvent::Cancel).unwrap(),
        JobStatus::Cancelled
    );

    let reloaded = store::load_job(&conn, "job-c").unwrap();
    assert!(reloaded.cancel_requested);
    assert_eq!(reloaded.status, JobStatus::Cancelled);
    assert!(reloaded.status.is_terminal());
    // No orphan remains once the job is terminal.
    assert!(matches!(no_orphaned_jobs(&conn).unwrap(), CheckStatus::Pass));
}

#[test]
fn timed_out_job_records_timeout_and_cannot_keep_running() {
    let conn = migrated_db();
    let reg = registry();
    let mut audit = VecAuditSink::new();

    let job = SpecialistJob::new(
        "job-t",
        "orchestrator",
        "browser-1",
        "browser-specialist",
        JobBudget::default(),
        30,
    );
    start_job(&conn, &reg, &mut audit, &job).unwrap();
    transition_job(&conn, &mut audit, "job-t", JobEvent::Start).unwrap();

    // Elapsed >= timeout: the host reaps it with a Timeout event.
    assert!(is_timed_out(30, 30));
    assert_eq!(
        transition_job(&conn, &mut audit, "job-t", JobEvent::Timeout).unwrap(),
        JobStatus::TimedOut
    );
    assert!(matches!(no_orphaned_jobs(&conn).unwrap(), CheckStatus::Pass));

    // A timed-out job is terminal: it cannot transition back to running/complete,
    // so it can never "keep running invisibly".
    assert!(matches!(
        transition_job(&conn, &mut audit, "job-t", JobEvent::Complete),
        Err(DelegationError::InvalidTransition(_))
    ));
}

#[test]
fn attachment_forwarding_stays_within_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let spec_layout = SessionLayout::derive(root, "browser-1", "job-1").unwrap();
    let orch_layout = SessionLayout::derive(root, "orchestrator", "sess-1").unwrap();

    // The specialist wrote an artifact into its outbox; forward it to the
    // orchestrator's inbox.
    let src_dir = spec_layout.outbox_message_dir("m1").unwrap();
    let dst_dir = orch_layout.inbox_message_dir("m1").unwrap();
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("shot.png"), b"PNGDATA").unwrap();

    let dst = forward_attachment(&src_dir, &dst_dir, "shot.png").unwrap();
    assert!(dst.starts_with(&dst_dir));
    assert_eq!(std::fs::read(&dst).unwrap(), b"PNGDATA");

    // A traversing file name is refused.
    assert!(forward_attachment(&src_dir, &dst_dir, "../escape.png").is_err());
}
