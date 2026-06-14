pub mod event;
pub mod fragment;
pub mod model;
pub mod readiness;
pub mod store;

pub use event::{ApprovalEvent, ApprovalEventSink, VecEventSink};
pub use fragment::{
    approval_policy_body, APPROVAL_POLICY_ID, APPROVAL_POLICY_ORDER, APPROVAL_POLICY_PARAMS,
};
pub use model::{
    Approval, ApprovalError, ApprovalKind, ApprovalStatus, EpochSecs, Question, QuestionStatus,
};
pub use readiness::{expiry_sweep_running, no_approvals_stuck_past_expiry, CheckStatus};
pub use store::{
    ask, deny, get_approval, get_question, grant, pending_past_expiry, request_approval, respond,
    sweep_expired,
};

pub const MODULE_ID: &str = "assistant-approvals";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};
    use assistant_permissions::{bootstrap_owner, create_user, grant_role, Role};
    use rusqlite::Connection;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let set = baseline_migrations(order);
        apply(&mut conn, &set).unwrap();
        conn
    }

    #[test]
    fn only_authorized_approver_can_grant() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let member = create_user(&conn, "member", None).unwrap();
        grant_role(&conn, owner, member, Role::Member).unwrap();

        let id = request_approval(&conn, ApprovalKind::Generic, "do the thing", Some(owner), None)
            .unwrap();

        // A member cannot grant; the approval stays pending.
        assert!(matches!(
            grant(&conn, member, id, 100),
            Err(ApprovalError::NotAuthorized { .. })
        ));
        assert_eq!(get_approval(&conn, id).unwrap().unwrap().status, ApprovalStatus::Pending);

        // The owner can.
        grant(&conn, owner, id, 100).unwrap();
        assert_eq!(get_approval(&conn, id).unwrap().unwrap().status, ApprovalStatus::Granted);
    }

    #[test]
    fn expired_approval_is_never_honored_even_before_sweep() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        // Expires at t=50.
        let id =
            request_approval(&conn, ApprovalKind::Generic, "x", Some(owner), Some(50)).unwrap();

        // Granting at t=60 (past expiry) is refused even though the sweep has
        // not run; the approval is marked expired.
        assert!(matches!(grant(&conn, owner, id, 60), Err(ApprovalError::Expired { .. })));
        assert_eq!(get_approval(&conn, id).unwrap().unwrap().status, ApprovalStatus::Expired);
    }

    #[test]
    fn sweep_marks_expired_and_readiness_reflects_backlog() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        request_approval(&conn, ApprovalKind::Generic, "a", Some(owner), Some(10)).unwrap();
        request_approval(&conn, ApprovalKind::Credential, "b", Some(owner), Some(10)).unwrap();
        request_approval(&conn, ApprovalKind::Generic, "c", Some(owner), None).unwrap();

        // At t=20 two have lapsed; readiness flags the backlog before sweeping.
        assert!(no_approvals_stuck_past_expiry(&conn, 20).unwrap().is_blocking_failure());
        assert_eq!(sweep_expired(&conn, 20).unwrap(), 2);
        // After sweeping, nothing is stuck and the no-expiry one is untouched.
        assert!(no_approvals_stuck_past_expiry(&conn, 20).unwrap().is_pass());
    }

    #[test]
    fn response_matches_originating_question_only() {
        let conn = db();
        let q = ask(&conn, Some("sess-1"), "approve plan?").unwrap();

        // Responding to a non-existent question id is rejected.
        assert!(matches!(respond(&conn, q + 999), Err(ApprovalError::NotFound { .. })));

        // Responding to the real question succeeds once; a second response is
        // rejected (it is no longer open).
        respond(&conn, q).unwrap();
        assert_eq!(get_question(&conn, q).unwrap().unwrap().status, QuestionStatus::Answered);
        assert!(matches!(respond(&conn, q), Err(ApprovalError::NotDecidable { .. })));
    }

    #[test]
    fn malformed_expiry_errors_rather_than_never_expiring() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let id = request_approval(&conn, ApprovalKind::Generic, "x", Some(owner), None).unwrap();
        // Corrupt the stored expiry to a non-numeric string.
        conn.execute(
            "UPDATE pending_approvals SET expires_at = 'soon' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();

        // Reading must surface the bad value, not silently treat it as
        // never-expiring; granting through the same read path is refused too.
        assert!(matches!(get_approval(&conn, id), Err(ApprovalError::UnknownEnum(_))));
        assert!(matches!(grant(&conn, owner, id, 100), Err(ApprovalError::UnknownEnum(_))));
    }

    #[test]
    fn unknown_question_status_errors_rather_than_reopening() {
        let conn = db();
        let q = ask(&conn, None, "approve?").unwrap();
        respond(&conn, q).unwrap();
        // Corrupt the stored status to an unrecognized value.
        conn.execute(
            "UPDATE pending_questions SET status = 'weird' WHERE id = ?1",
            rusqlite::params![q],
        )
        .unwrap();

        // It must error, not default to Open (which would let it be answered
        // again).
        assert!(matches!(get_question(&conn, q), Err(ApprovalError::UnknownEnum(_))));
        assert!(matches!(respond(&conn, q), Err(ApprovalError::UnknownEnum(_))));
    }

    #[test]
    fn credential_approval_kind_round_trips() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let id = request_approval(
            &conn,
            ApprovalKind::Credential,
            "OneCLI: api.anthropic.com",
            Some(owner),
            None,
        )
        .unwrap();
        assert_eq!(get_approval(&conn, id).unwrap().unwrap().kind, ApprovalKind::Credential);
    }
}
