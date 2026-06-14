//! Approval domain types over the baseline `pending_approvals` and
//! `pending_questions` tables.
//!
//! The crate never reads the wall clock — callers pass `now`. Approval expiry
//! is stored in the baseline TEXT `expires_at` column as a decimal epoch string
//! (the platform convention for epoch values in TEXT columns), compared via
//! `CAST(expires_at AS INTEGER)`.

use serde::{Deserialize, Serialize};

use assistant_permissions::PermissionError;

pub type EpochSecs = i64;

/// What an approval is for. `Credential` is the OneCLI credential approval the
/// gateway integration requires before container-side external credential use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Generic,
    Credential,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalKind::Generic => "generic",
            ApprovalKind::Credential => "credential",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "generic" => Some(ApprovalKind::Generic),
            "credential" => Some(ApprovalKind::Credential),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Granted => "granted",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalStatus::Pending),
            "granted" => Some(ApprovalStatus::Granted),
            "denied" => Some(ApprovalStatus::Denied),
            "expired" => Some(ApprovalStatus::Expired),
            _ => None,
        }
    }
}

/// A pending approval card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub id: i64,
    pub kind: ApprovalKind,
    pub subject: String,
    pub status: ApprovalStatus,
    pub requested_by: Option<i64>,
    pub expires_at: Option<EpochSecs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Open,
    Answered,
}

impl QuestionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionStatus::Open => "open",
            QuestionStatus::Answered => "answered",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(QuestionStatus::Open),
            "answered" => Some(QuestionStatus::Answered),
            _ => None,
        }
    }
}

/// A pending free-form question awaiting a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: i64,
    pub session_id: Option<String>,
    pub prompt: String,
    pub status: QuestionStatus,
}

#[derive(Debug)]
pub enum ApprovalError {
    Sqlite(rusqlite::Error),
    /// The approver is not authorized to decide approvals.
    NotAuthorized { actor: i64 },
    /// No such approval/question.
    NotFound { id: i64 },
    /// The approval/question is not in a decidable (pending/open) state.
    NotDecidable { id: i64 },
    /// The approval has expired and may not be honored.
    Expired { id: i64 },
    /// An unknown enum string was read from the DB.
    UnknownEnum(String),
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalError::Sqlite(e) => write!(f, "approvals sqlite error: {e}"),
            ApprovalError::NotAuthorized { actor } => {
                write!(f, "user {actor} is not authorized to decide approvals")
            }
            ApprovalError::NotFound { id } => write!(f, "no approval/question {id}"),
            ApprovalError::NotDecidable { id } => write!(f, "approval/question {id} is not decidable"),
            ApprovalError::Expired { id } => write!(f, "approval {id} has expired"),
            ApprovalError::UnknownEnum(s) => write!(f, "unknown enum value {s:?}"),
        }
    }
}

impl std::error::Error for ApprovalError {}

impl From<rusqlite::Error> for ApprovalError {
    fn from(value: rusqlite::Error) -> Self {
        ApprovalError::Sqlite(value)
    }
}

impl From<PermissionError> for ApprovalError {
    fn from(value: PermissionError) -> Self {
        match value {
            PermissionError::NotAuthorized { actor } => ApprovalError::NotAuthorized { actor },
            PermissionError::Sqlite(e) => ApprovalError::Sqlite(e),
            other => ApprovalError::UnknownEnum(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_status_round_trip() {
        for k in [ApprovalKind::Generic, ApprovalKind::Credential] {
            assert_eq!(ApprovalKind::parse(k.as_str()), Some(k));
        }
        for s in [
            ApprovalStatus::Pending,
            ApprovalStatus::Granted,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
        ] {
            assert_eq!(ApprovalStatus::parse(s.as_str()), Some(s));
        }
    }
}
