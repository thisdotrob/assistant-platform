//! Milestone 14: the work product vertical, driven end to end over the real
//! platform crates.
//!
//! The script: bootstrap a fresh instance, pair a member over Slack, engage on
//! an `@`-mention in a channel, follow up in the thread under a sticky session,
//! write and retrieve a squad memory, schedule and advance a recurring check,
//! delegate a task to the browser specialist, and inspect the work queue and
//! schedule through the web read surface.

mod harness;

use assistant_channel_slack::{normalize, parse_event, SlackIdentity};
use assistant_memory::{Scope, SearchOutcome};
use assistant_permissions::{AccessDecision, UnknownPolicy};
use assistant_router::{
    EngagementContext, EngagementDecision, EngagementMode, IgnoredMessagePolicy, RoutingEvent,
    SenderScope,
};
use assistant_web::{QueueItem, SessionView, WebApp};

use harness::{
    claim_due_now, complete, delegate_browser, open_session, pair_member, reproject,
    run_echo_turn, schedule_recurring, sender_decision, seed_owner, Instance, InstanceWebApp,
    MemoryStore,
};

const SQUAD_AGENT_GROUP: i64 = 2;

fn bot_identity() -> SlackIdentity {
    SlackIdentity {
        bot_user_id: "U_BOT".to_string(),
        self_bot_id: Some("B_SELF".to_string()),
    }
}

#[test]
fn work_vertical_runs_end_to_end() {
    // 1. Bootstrap the work instance and bring its central DB to the full schema.
    let instance = Instance::bootstrap("workns", "cleoclaw");
    let conn = instance.full_central();

    // 2. Seed the owner and pair a teammate over Slack (address = Slack user id).
    let owner = seed_owner(&conn, "admin");
    pair_member(&conn, owner, "alice", "slack", "U1");

    // 3. An @-mention arrives in a channel and normalizes to a routing event.
    let mention = normalize(
        &parse_event(
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"100.1","text":"<@U_BOT> ship the release"}"#,
        )
        .unwrap(),
        &bot_identity(),
    )
    .expect("an app mention routes");
    assert_eq!(mention.channel_kind, "slack");
    assert!(mention.is_mention);
    assert!(!mention.is_direct);
    assert_eq!(mention.engagement_key, "slack:C1");

    // 4. The paired sender is known and allowed; the mention engages.
    assert!(matches!(
        sender_decision(&conn, "slack", &mention.sender_id, UnknownPolicy::Strict),
        AccessDecision::Allow
    ));
    assert_eq!(engage(&mention, false, true), EngagementDecision::Engage);

    // 5. The engaged mention runs a session turn.
    let session = open_session(&instance.sessions_base(), "squad", "sess-1");
    let outbound = run_echo_turn(&session, &mention.sender_id, &mention.text);
    assert!(outbound.iter().any(|m| m.content.contains("ship the release")));

    // 6. A thread reply (no fresh mention) still engages under an active sticky
    //    session scoped to the thread root.
    let reply = normalize(
        &parse_event(
            r#"{"type":"message","channel":"C1","user":"U1","ts":"200.2","thread_ts":"100.1","text":"any update?"}"#,
        )
        .unwrap(),
        &bot_identity(),
    )
    .expect("a thread reply routes");
    assert_eq!(reply.thread_root_id.as_deref(), Some("100.1"));
    assert_eq!(reply.engagement_key, "slack:C1:100.1");
    assert!(!reply.is_mention);
    assert_eq!(engage_sticky(&reply, true), EngagementDecision::Engage);

    // 7. Write a squad memory and retrieve it from the indexed store.
    let mut memory = MemoryStore::new();
    memory.write(
        &conn,
        SQUAD_AGENT_GROUP,
        "mem_release",
        Scope::Channel,
        "decisions/release.md",
        "The release ships on Fridays after the on-call sign-off.",
    );
    let SearchOutcome::Hits(hits) = memory.search("release", 10) else {
        panic!("squad memory search should return hits");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id, "mem_release");

    // 8. Schedule a recurring check, fire it once, advance it, and confirm the
    //    next occurrence is claimable only at its later time.
    let mut check = schedule_recurring(&conn, SQUAD_AGENT_GROUP, "standup reminder", 1_000, 3_600);
    let first = claim_due_now(&conn, 1_000);
    assert_eq!(first.len(), 1, "the recurring check is due and claimed once");
    complete(&conn, &first[0], 1_000);
    check.record_fired(&first[0].occurrence);
    reproject(&conn, &check);
    assert!(
        claim_due_now(&conn, 1_000).is_empty(),
        "the advanced recurrence is not due at the old time"
    );
    let second = claim_due_now(&conn, 4_600);
    assert_eq!(second.len(), 1, "the next occurrence claims at its later time");
    assert_eq!(second[0].occurrence.sequence, 2);

    // 9. Delegate a browser task and receive a structured result.
    let result = delegate_browser(
        &conn,
        &instance.sessions_base(),
        "squad",
        "job-1",
        "check the release notes on example.com",
        "notes look good",
    );
    assert_eq!(result.answer, "notes look good");

    // 10. Inspect the work queue and schedule through the web read surface.
    let app = InstanceWebApp {
        conn: &conn,
        product_id: instance.product_id.clone(),
        product_version: instance.product_version.clone(),
        platform_version: harness::platform_version(),
        agent_group_id: SQUAD_AGENT_GROUP,
        group_count: 1,
        sessions: vec![SessionView {
            session_id: "sess-1".to_string(),
            group_id: "squad".to_string(),
            state: "idle".to_string(),
            last_activity: None,
        }],
        queue: vec![QueueItem {
            id: "job-1".to_string(),
            kind: "specialist_job".to_string(),
            enqueued_at: None,
            state: "succeeded".to_string(),
        }],
        ready: true,
    };
    let queue = app.queue();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, "job-1");
    let scheduled = app.scheduled();
    let standup = scheduled
        .iter()
        .find(|s| s.description == "standup reminder")
        .expect("the web surface lists the recurring check");
    assert_eq!(standup.recurrence.as_deref(), Some("every 3600s"));
}

fn engage(event: &RoutingEvent, sticky: bool, sender_is_member: bool) -> EngagementDecision {
    assistant_router::evaluate_engagement(
        event,
        &EngagementMode::Mention,
        SenderScope::Known,
        IgnoredMessagePolicy::Drop,
        EngagementContext {
            sender_is_member,
            has_active_sticky: sticky,
        },
    )
}

fn engage_sticky(event: &RoutingEvent, has_active_sticky: bool) -> EngagementDecision {
    assistant_router::evaluate_engagement(
        event,
        &EngagementMode::MentionSticky,
        SenderScope::Known,
        IgnoredMessagePolicy::Drop,
        EngagementContext {
            sender_is_member: true,
            has_active_sticky,
        },
    )
}
