//! Access evaluation: turn a known/unknown identity plus the configured
//! [`UnknownPolicy`] into an [`AccessDecision`], and enforce admin gates.
//!
//! Deny-by-default is structural: an unknown identity is only ever allowed when
//! the policy is explicitly [`UnknownPolicy::Public`]; everything else denies or
//! escalates to approval.

use rusqlite::Connection;

use crate::model::{AccessDecision, PermissionError, UnknownPolicy};
use crate::store;

/// The pure policy core: decide what to do with an identity given whether it is
/// already known and the configured unknown policy.
pub fn decide(known: bool, policy: UnknownPolicy) -> AccessDecision {
    if known {
        return AccessDecision::Allow;
    }
    match policy {
        UnknownPolicy::Strict => AccessDecision::Deny {
            reason: "unknown identity under strict policy".to_string(),
        },
        UnknownPolicy::RequestApproval => AccessDecision::RequestApproval {
            reason: "unknown identity awaiting approval".to_string(),
        },
        UnknownPolicy::Public => AccessDecision::Allow,
    }
}

/// Evaluate an inbound message's sender: known if a `user_dms` route matches.
pub fn evaluate_sender(
    conn: &Connection,
    channel: &str,
    address: &str,
    policy: UnknownPolicy,
) -> Result<AccessDecision, PermissionError> {
    let known = store::resolve_user_by_dm(conn, channel, address)?.is_some();
    Ok(decide(known, policy))
}

/// Evaluate an inbound message's channel. The permissions module does not own a
/// channel registry, so the caller supplies channel known-ness (the router /
/// channel adapter tracks registration); the policy core is reused.
pub fn evaluate_channel(channel_known: bool, policy: UnknownPolicy) -> AccessDecision {
    decide(channel_known, policy)
}

/// Assert that an actor may perform an administrative action. Returns
/// `NotAuthorized` otherwise; this is the single choke point so an admin gate
/// cannot be skipped by callers.
pub fn require_admin(conn: &Connection, actor: i64) -> Result<(), PermissionError> {
    if store::is_administrative(conn, actor)? {
        Ok(())
    } else {
        Err(PermissionError::NotAuthorized { actor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_identity_is_always_allowed() {
        for policy in [
            UnknownPolicy::Strict,
            UnknownPolicy::RequestApproval,
            UnknownPolicy::Public,
        ] {
            assert!(decide(true, policy).is_allow());
        }
    }

    #[test]
    fn unknown_identity_denies_by_default() {
        assert!(matches!(
            decide(false, UnknownPolicy::Strict),
            AccessDecision::Deny { .. }
        ));
        assert!(matches!(
            decide(false, UnknownPolicy::RequestApproval),
            AccessDecision::RequestApproval { .. }
        ));
        assert!(decide(false, UnknownPolicy::Public).is_allow());
    }
}
