//! Permission domain types: roles, the unknown-sender/channel policy, and the
//! access decision an inbound message resolves to.
//!
//! The crate never reads config or the wall clock itself — callers pass the
//! configured [`UnknownPolicy`] in. Roles are stored one row per (user, role)
//! in the baseline `user_roles` table; the enum here is the typed view of that
//! column.

use serde::{Deserialize, Serialize};

/// A platform role. Ordered by privilege: an owner outranks an admin, which
/// outranks a member. Owner is unique per instance (the bootstrap identity);
/// admins and members are not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Member,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "member" => Some(Role::Member),
            "admin" => Some(Role::Admin),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }

    /// Whether holding this role satisfies an administrative gate. Owners and
    /// admins administer; members do not.
    pub fn is_administrative(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }
}

/// What to do with a message from a sender or on a channel the instance does
/// not yet know. The default is the most restrictive, [`UnknownPolicy::Strict`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownPolicy {
    /// Deny outright. Deny-by-default.
    #[default]
    Strict,
    /// Hold for human approval before acting.
    RequestApproval,
    /// Allow, where the product's own policy permits public access.
    Public,
}

impl UnknownPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            UnknownPolicy::Strict => "strict",
            UnknownPolicy::RequestApproval => "request_approval",
            UnknownPolicy::Public => "public",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "strict" => Some(UnknownPolicy::Strict),
            "request_approval" => Some(UnknownPolicy::RequestApproval),
            "public" => Some(UnknownPolicy::Public),
            _ => None,
        }
    }
}

/// The outcome of evaluating an inbound message against the known-identity set
/// and the configured unknown policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AccessDecision {
    /// The sender/channel is known, or policy allows public access.
    Allow,
    /// Unknown under a request-approval policy: hold for human approval.
    RequestApproval { reason: String },
    /// Unknown under a strict policy, or an explicit denial.
    Deny { reason: String },
}

impl AccessDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, AccessDecision::Allow)
    }
}

/// A persisted user identity (the baseline `users` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub handle: String,
    pub display_name: Option<String>,
}

#[derive(Debug)]
pub enum PermissionError {
    Sqlite(rusqlite::Error),
    /// An administrative action was attempted by a non-administrative actor.
    NotAuthorized { actor: i64 },
    /// Bootstrapping an owner when one already exists.
    OwnerAlreadyExists,
    /// A referenced user does not exist.
    UnknownUser { user_id: i64 },
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionError::Sqlite(e) => write!(f, "permissions sqlite error: {e}"),
            PermissionError::NotAuthorized { actor } => {
                write!(f, "user {actor} is not authorized for this administrative action")
            }
            PermissionError::OwnerAlreadyExists => write!(f, "an owner already exists"),
            PermissionError::UnknownUser { user_id } => write!(f, "unknown user {user_id}"),
        }
    }
}

impl std::error::Error for PermissionError {}

impl From<rusqlite::Error> for PermissionError {
    fn from(value: rusqlite::Error) -> Self {
        PermissionError::Sqlite(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_and_orders_by_privilege() {
        for role in [Role::Member, Role::Admin, Role::Owner] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert!(Role::Owner > Role::Admin);
        assert!(Role::Admin > Role::Member);
        assert!(Role::Owner.is_administrative());
        assert!(Role::Admin.is_administrative());
        assert!(!Role::Member.is_administrative());
    }

    #[test]
    fn unknown_policy_defaults_to_strict_and_round_trips() {
        assert_eq!(UnknownPolicy::default(), UnknownPolicy::Strict);
        for p in [
            UnknownPolicy::Strict,
            UnknownPolicy::RequestApproval,
            UnknownPolicy::Public,
        ] {
            assert_eq!(UnknownPolicy::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn access_decision_is_json_tagged() {
        let json = serde_json::to_string(&AccessDecision::Deny { reason: "x".into() }).unwrap();
        assert!(json.contains("\"decision\":\"deny\""));
    }
}
