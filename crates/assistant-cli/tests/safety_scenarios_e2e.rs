//! Milestone 14: the cross-cutting safety properties, driven end to end over the
//! real platform crates.
//!
//! Four guarantees the platform must never relax:
//!   1. an unknown sender is denied (or escalated to approval), never silently
//!      admitted — deny-by-default;
//!   2. the browser specialist is internal-only and cannot face an external
//!      destination;
//!   3. a credential-capable container will not spawn while the OneCLI auth path
//!      is not ready, and no raw token is ever emitted as a fallback;
//!   4. setup cannot complete while a hard readiness gate fails, and resumes to
//!      completion once the cause is fixed.

mod harness;

use assistant_agent_graph::{authorize_external_destination, RoutingError};
use assistant_cli::{setup, FnStep, SetupError};
use assistant_permissions::{AccessDecision, UnknownPolicy};
use assistant_runtime_docker::{
    prepare_spawn, AuthError, ImageRef, OneCliReadiness, RunnerAuthMode, SchemaRange, SpawnError,
};

use harness::{
    bootstrap_request, browser_registered, pair_member, sender_decision, seed_owner, Instance,
};

#[test]
fn unknown_sender_is_blocked_or_approval_routed() {
    let instance = Instance::bootstrap("safetyns", "assistant");
    let conn = instance.full_central();
    let owner = seed_owner(&conn, "rob");
    pair_member(&conn, owner, "robuser", "telegram", "555");

    // A paired member is known and admitted even under the strictest policy.
    assert!(matches!(
        sender_decision(&conn, "telegram", "555", UnknownPolicy::Strict),
        AccessDecision::Allow
    ));

    // An unknown sender is denied outright under the deny-by-default policy.
    assert!(matches!(
        sender_decision(&conn, "telegram", "999", UnknownPolicy::Strict),
        AccessDecision::Deny { .. }
    ));

    // Under request-approval the same unknown sender is escalated, not admitted.
    assert!(matches!(
        sender_decision(&conn, "telegram", "999", UnknownPolicy::RequestApproval),
        AccessDecision::RequestApproval { .. }
    ));
}

#[test]
fn browser_specialist_cannot_send_externally() {
    // The browser profile is a specialist: it never owns an external
    // destination, so authorizing one is refused.
    assert!(matches!(
        authorize_external_destination(&browser_registered()),
        Err(RoutingError::ExternalDestinationForbidden { .. })
    ));
}

#[test]
fn onecli_gateway_failure_blocks_credential_capable_spawn() {
    let image = ImageRef::new("assistant-base", "0.1.0");
    let not_ready = OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: false,
    };

    // A credential-capable (ClaudeOAuth) runner is refused while the OneCLI auth
    // path is not ready; the error names the failed probe and no token leaks.
    let blocked = prepare_spawn(
        "runner-1",
        image.clone(),
        Vec::new(),
        &[],
        1,
        SchemaRange::new(1, 1),
        RunnerAuthMode::ClaudeOAuth,
        not_ready,
    );
    assert!(
        matches!(
            blocked,
            Err(SpawnError::Auth(AuthError::PlaceholderInjectionFailed))
        ),
        "credential-capable spawn must be blocked: {blocked:?}"
    );

    // A credential-less stub runner is unaffected by OneCLI readiness.
    let stub = prepare_spawn(
        "runner-stub",
        image.clone(),
        Vec::new(),
        &[],
        1,
        SchemaRange::new(1, 1),
        RunnerAuthMode::Stub,
        not_ready,
    );
    assert!(stub.is_ok(), "stub containers spawn without OneCLI: {stub:?}");

    // Once OneCLI is ready, the credential-capable runner spawns.
    let ready = OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: true,
    };
    let ok = prepare_spawn(
        "runner-1",
        image,
        Vec::new(),
        &[],
        1,
        SchemaRange::new(1, 1),
        RunnerAuthMode::ClaudeOAuth,
        ready,
    );
    assert!(ok.is_ok(), "a ready OneCLI auth path permits the spawn: {ok:?}");
}

#[test]
fn setup_fails_until_readiness_passes() {
    let home = tempfile::tempdir().unwrap();

    // The host probes the OneCLI auth path as a hard setup gate. While it is not
    // ready, setup cannot complete.
    let not_ready = OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: false,
        placeholder_injection_ok: false,
    };
    let gate_fail = FnStep::gate("onecli_ready", "OneCLI auth path must be ready", move |_ctx| {
        if not_ready.is_ready() {
            Ok("onecli ready".to_string())
        } else {
            Err(SetupError::Gate {
                id: "onecli_ready".to_string(),
                detail: "OneCLI auth path not ready".to_string(),
            })
        }
    })
    .boxed();
    let code = setup(
        bootstrap_request(home.path(), "safetyns", "assistant"),
        vec![gate_fail],
    );
    assert_eq!(code, 1, "setup is blocked while the readiness gate fails");

    // Fix the cause and resume on the same instance: the gate now passes and
    // setup runs to completion.
    let ready = OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: true,
    };
    let gate_pass = FnStep::gate("onecli_ready", "OneCLI auth path must be ready", move |_ctx| {
        if ready.is_ready() {
            Ok("onecli ready".to_string())
        } else {
            Err(SetupError::Gate {
                id: "onecli_ready".to_string(),
                detail: "OneCLI auth path not ready".to_string(),
            })
        }
    })
    .boxed();
    let code = setup(
        bootstrap_request(home.path(), "safetyns", "assistant"),
        vec![gate_pass],
    );
    assert_eq!(code, 0, "setup completes once the readiness gate passes");
}
