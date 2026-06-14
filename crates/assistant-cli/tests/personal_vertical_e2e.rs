//! Milestone 14: the personal product vertical, driven end to end over the real
//! platform crates.
//!
//! The script: bootstrap a fresh instance, pair an owner over Telegram, accept
//! and engage a direct message, run a session turn, write and retrieve a
//! household memory, schedule and fire a one-off reminder, delegate a task to
//! the browser specialist, and finally inspect the instance through the web read
//! surface. Every step composes a real module through its public API.

mod harness;

use assistant_channel_telegram::{normalize, parse_update, TelegramIdentity};
use assistant_memory::{Scope, SearchOutcome};
use assistant_permissions::{AccessDecision, UnknownPolicy};
use assistant_router::{
    EngagementContext, EngagementDecision, EngagementMode, IgnoredMessagePolicy, SenderScope,
};
use assistant_web::{SessionView, WebApp};

use harness::{
    claim_due_now, complete, delegate_browser, open_session, pair_member, run_echo_turn,
    schedule_one_off, sender_decision, seed_owner, Instance, InstanceWebApp, MemoryStore,
};

const HOUSEHOLD_AGENT_GROUP: i64 = 1;

fn bot_identity() -> TelegramIdentity {
    TelegramIdentity {
        bot_id: 42,
        bot_username: "assistantbot".to_string(),
    }
}

#[test]
fn personal_vertical_runs_end_to_end() {
    // 1. Bootstrap the personal instance and bring its central DB to the full
    //    module schema.
    let instance = Instance::bootstrap("personalns", "assistant");
    let conn = instance.full_central();

    // 2. Seed the owner and pair them over a Telegram DM (address = chat id).
    let owner = seed_owner(&conn, "rob");
    pair_member(&conn, owner, "robuser", "telegram", "555");

    // 3. A direct Telegram message arrives and is normalized to a routing event.
    let update = parse_update(
        r#"{"update_id":1,"message":{"message_id":10,"from":{"id":555,"username":"robuser"},"chat":{"id":555,"type":"private"},"text":"remind me to call the plumber"}}"#,
    )
    .unwrap();
    let event = normalize(&update, &bot_identity()).expect("a private message routes");
    assert_eq!(event.channel_kind, "telegram");
    assert!(event.is_direct);
    assert_eq!(event.sender_id, "555");

    // 4. The paired sender is known and allowed under the strict default policy.
    let access = sender_decision(&conn, "telegram", &event.sender_id, UnknownPolicy::Strict);
    assert!(matches!(access, AccessDecision::Allow));

    // 5. A direct message engages the agent (member sender, known scope).
    let decision = engage(&event, true);
    assert_eq!(decision, EngagementDecision::Engage);

    // 6. The engaged message runs a full session turn and the agent replies.
    let session = open_session(&instance.sessions_base(), "household", "sess-1");
    let outbound = run_echo_turn(&session, &event.sender_id, &event.text);
    assert!(
        outbound.iter().any(|m| m.content.contains("call the plumber")),
        "the agent's reply echoes the request: {outbound:?}"
    );

    // 7. Write a household memory and retrieve it from the indexed store.
    let mut memory = MemoryStore::new();
    memory.write(
        &conn,
        HOUSEHOLD_AGENT_GROUP,
        "mem_plumber",
        Scope::AllChats,
        "people/contacts.md",
        "Rob's plumber is Joe, reachable at 555-1234.",
    );
    let SearchOutcome::Hits(hits) = memory.search("plumber", 10) else {
        panic!("memory search should return hits, not a degraded outcome");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id, "mem_plumber");

    // 8. Schedule a one-off reminder; it is claimable exactly once.
    schedule_one_off(&conn, HOUSEHOLD_AGENT_GROUP, "call the plumber", 1_000);
    let due = claim_due_now(&conn, 1_000);
    assert_eq!(due.len(), 1, "the reminder is due and claimed once");
    complete(&conn, &due[0], 1_000);
    assert!(
        claim_due_now(&conn, 1_000).is_empty(),
        "a fired one-off is not re-claimed"
    );

    // 9. Delegate a browser task and receive a structured result.
    let result = delegate_browser(
        &conn,
        &instance.sessions_base(),
        "household",
        "job-1",
        "find the plumber's rating on example.com",
        "4.5 stars",
    );
    assert_eq!(result.answer, "4.5 stars");

    // 10. Inspect the instance through the web read surface.
    let app = InstanceWebApp {
        conn: &conn,
        product_id: instance.product_id.clone(),
        product_version: instance.product_version.clone(),
        platform_version: harness::platform_version(),
        agent_group_id: HOUSEHOLD_AGENT_GROUP,
        group_count: 1,
        sessions: vec![SessionView {
            session_id: "sess-1".to_string(),
            group_id: "household".to_string(),
            state: "idle".to_string(),
            last_activity: None,
        }],
        queue: Vec::new(),
        ready: true,
    };
    let overview = app.overview();
    assert_eq!(overview.product_id, "assistant");
    assert_eq!(overview.counts.scheduled_items, 1);
    assert_eq!(overview.counts.active_sessions, 1);
    let scheduled = app.scheduled();
    assert!(
        scheduled.iter().any(|s| s.description == "call the plumber"),
        "the web surface lists the scheduled reminder: {scheduled:?}"
    );
}

fn engage(event: &assistant_router::RoutingEvent, sender_is_member: bool) -> EngagementDecision {
    assistant_router::evaluate_engagement(
        event,
        &EngagementMode::Mention,
        SenderScope::Known,
        IgnoredMessagePolicy::Drop,
        EngagementContext {
            sender_is_member,
            has_active_sticky: false,
        },
    )
}
