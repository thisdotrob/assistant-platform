//! Audit model for messages the router declined to deliver.
//!
//! These types describe *why* an inbound message was dropped and the row shape
//! recorded in the central `dropped_messages` table. Routing itself is out of
//! scope here — this slice only records and reports drops for audit.

use serde::{Deserialize, Serialize};

/// Why an inbound message was dropped instead of routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// The sender could not be resolved to a known user.
    UnknownSender,
    /// The originating channel is not recognized.
    UnknownChannel,
    /// The sender is known but not permitted on this channel.
    NotAuthorized,
    /// The payload could not be parsed into a routable message.
    Malformed,
    /// No destination matched the message.
    NoRoute,
    /// A duplicate of an already-handled message (idempotency).
    Duplicate,
    /// No wired agent engaged and the ignored-message policy is drop.
    NotEngaged,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::UnknownSender => "unknown_sender",
            DropReason::UnknownChannel => "unknown_channel",
            DropReason::NotAuthorized => "not_authorized",
            DropReason::Malformed => "malformed",
            DropReason::NoRoute => "no_route",
            DropReason::Duplicate => "duplicate",
            DropReason::NotEngaged => "not_engaged",
        }
    }

    pub fn parse(s: &str) -> Result<Self, RouterError> {
        match s {
            "unknown_sender" => Ok(DropReason::UnknownSender),
            "unknown_channel" => Ok(DropReason::UnknownChannel),
            "not_authorized" => Ok(DropReason::NotAuthorized),
            "malformed" => Ok(DropReason::Malformed),
            "no_route" => Ok(DropReason::NoRoute),
            "duplicate" => Ok(DropReason::Duplicate),
            "not_engaged" => Ok(DropReason::NotEngaged),
            other => Err(RouterError::UnknownEnum { value: other.to_string() }),
        }
    }
}

/// A recorded drop, as read back from `dropped_messages`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedMessage {
    pub id: i64,
    pub channel: String,
    pub sender: Option<String>,
    pub reason: DropReason,
    pub payload: Option<String>,
    pub created_at: String,
}

#[derive(Debug)]
pub enum RouterError {
    Sqlite(rusqlite::Error),
    /// A stored value did not parse into a known enum variant.
    UnknownEnum { value: String },
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouterError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            RouterError::UnknownEnum { value } => {
                write!(f, "unrecognized stored value {value:?}")
            }
        }
    }
}

impl std::error::Error for RouterError {}

impl From<rusqlite::Error> for RouterError {
    fn from(e: rusqlite::Error) -> Self {
        RouterError::Sqlite(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_round_trips_through_str() {
        for r in [
            DropReason::UnknownSender,
            DropReason::UnknownChannel,
            DropReason::NotAuthorized,
            DropReason::Malformed,
            DropReason::NoRoute,
            DropReason::Duplicate,
            DropReason::NotEngaged,
        ] {
            assert_eq!(DropReason::parse(r.as_str()).unwrap(), r);
        }
    }

    #[test]
    fn unknown_reason_string_is_rejected() {
        assert!(matches!(
            DropReason::parse("teleported"),
            Err(RouterError::UnknownEnum { .. })
        ));
    }
}
