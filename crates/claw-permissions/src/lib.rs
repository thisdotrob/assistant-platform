pub mod access;
pub mod event;
pub mod model;
pub mod readiness;
pub mod standing;
pub mod store;

pub use access::{decide, evaluate_channel, evaluate_sender, require_admin};
pub use event::{PermissionEvent, PermissionEventSink, RoleChange, VecEventSink};
pub use model::{AccessDecision, PermissionError, Role, UnknownPolicy, User};
pub use readiness::{admin_exists, is_admin, references_intact, CheckStatus};
pub use standing::{
    active_standing_instructions, capture, capture_as, classify, deactivate, migrations,
    CaptureOutcome, InstructionClass, StandingInstruction,
};
pub use store::{
    add_user_dm, bootstrap_owner, create_user, find_user_by_handle, get_user, grant_role,
    has_role, is_administrative, owner_exists, resolve_user_by_dm, revoke_role, roles_of,
};

pub const MODULE_ID: &str = "claw-permissions";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};
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
    fn role_access_matrix() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", Some("Owner")).unwrap();
        let alice = create_user(&conn, "alice", None).unwrap();
        let bob = create_user(&conn, "bob", None).unwrap();

        // Owner is administrative; fresh members are not.
        assert!(is_administrative(&conn, owner).unwrap());
        assert!(!is_administrative(&conn, alice).unwrap());

        // Owner promotes alice to admin; alice can then administer.
        grant_role(&conn, owner, alice, Role::Admin).unwrap();
        assert!(is_administrative(&conn, alice).unwrap());
        require_admin(&conn, alice).unwrap();

        // Member bob cannot administer.
        grant_role(&conn, owner, bob, Role::Member).unwrap();
        assert!(require_admin(&conn, bob).is_err());
    }

    #[test]
    fn admin_gate_cannot_be_bypassed_and_writes_nothing_when_denied() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let member = create_user(&conn, "member", None).unwrap();
        grant_role(&conn, owner, member, Role::Member).unwrap();
        let victim = create_user(&conn, "victim", None).unwrap();

        // A member trying to grant admin is rejected.
        let err = grant_role(&conn, member, victim, Role::Admin);
        assert!(matches!(err, Err(PermissionError::NotAuthorized { .. })));
        // And nothing was written: victim has no admin role.
        assert!(!has_role(&conn, victim, Role::Admin).unwrap());
    }

    #[test]
    fn admin_cannot_grant_or_revoke_the_owner_role() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let admin = create_user(&conn, "admin", None).unwrap();
        grant_role(&conn, owner, admin, Role::Admin).unwrap();
        let target = create_user(&conn, "target", None).unwrap();

        // An admin cannot mint a second owner (no self- or other-promotion).
        assert!(matches!(
            grant_role(&conn, admin, target, Role::Owner),
            Err(PermissionError::NotAuthorized { .. })
        ));
        assert!(!has_role(&conn, target, Role::Owner).unwrap());
        assert!(owner_exists(&conn).unwrap());

        // Nor can an admin demote the instance owner.
        assert!(matches!(
            revoke_role(&conn, admin, owner, Role::Owner),
            Err(PermissionError::NotAuthorized { .. })
        ));
        assert!(has_role(&conn, owner, Role::Owner).unwrap());

        // The owner itself also cannot grant a second owner via this path —
        // ownership is bootstrap-only.
        assert!(matches!(
            grant_role(&conn, owner, target, Role::Owner),
            Err(PermissionError::NotAuthorized { .. })
        ));
        assert!(!has_role(&conn, target, Role::Owner).unwrap());
    }

    #[test]
    fn second_owner_bootstrap_is_rejected() {
        let conn = db();
        bootstrap_owner(&conn, "owner1", None).unwrap();
        assert!(matches!(
            bootstrap_owner(&conn, "owner2", None),
            Err(PermissionError::OwnerAlreadyExists)
        ));
    }

    #[test]
    fn unknown_sender_denied_known_sender_allowed() {
        let conn = db();
        let alice = create_user(&conn, "alice", None).unwrap();
        add_user_dm(&conn, alice, "slack", "U-ALICE").unwrap();

        // Unknown sender under the default strict policy is denied.
        let unknown = evaluate_sender(&conn, "slack", "U-STRANGER", UnknownPolicy::Strict).unwrap();
        assert!(matches!(unknown, AccessDecision::Deny { .. }));

        // The registered DM resolves to a known sender and is allowed.
        let known = evaluate_sender(&conn, "slack", "U-ALICE", UnknownPolicy::Strict).unwrap();
        assert!(known.is_allow());
        assert_eq!(resolve_user_by_dm(&conn, "slack", "U-ALICE").unwrap(), Some(alice));
    }

    #[test]
    fn unknown_channel_policy_escalates_to_approval() {
        // An unknown channel under request-approval policy escalates rather than
        // allowing or hard-denying.
        let decision = evaluate_channel(false, UnknownPolicy::RequestApproval);
        assert!(matches!(decision, AccessDecision::RequestApproval { .. }));
    }

    #[test]
    fn revoke_is_atomic_and_admin_gated() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let alice = create_user(&conn, "alice", None).unwrap();
        grant_role(&conn, owner, alice, Role::Admin).unwrap();
        assert!(has_role(&conn, alice, Role::Admin).unwrap());

        // A non-admin cannot revoke.
        let member = create_user(&conn, "member", None).unwrap();
        assert!(revoke_role(&conn, member, alice, Role::Admin).is_err());
        assert!(has_role(&conn, alice, Role::Admin).unwrap());

        // The owner can.
        revoke_role(&conn, owner, alice, Role::Admin).unwrap();
        assert!(!has_role(&conn, alice, Role::Admin).unwrap());
    }
}
