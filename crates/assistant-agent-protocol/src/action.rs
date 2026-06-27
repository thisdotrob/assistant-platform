//! Typed outbound actions: the canonical delivery mechanism.
//!
//! `send_message` and the other typed actions below are the only way an agent
//! run delivers user-visible output. Tagged or XML-like text in model prose is
//! never parsed for routing (see [`crate::process`]); it can only ever be
//! archived as transcript text. Every variant round-trips through serde so the
//! shim, host, CLI, and web UI serialize against one schema.

use serde::{Deserialize, Serialize};

/// A typed action emitted by an agent run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OutboundAction {
    SendMessage {
        destination: String,
        text: String,
    },
    SendFile {
        destination: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    SendCard {
        destination: String,
        title: String,
        body: String,
    },
    AskUserQuestion {
        destination: String,
        question: String,
        options: Vec<String>,
    },
    EditMessage {
        target_seq: i64,
        text: String,
    },
    AddReaction {
        target_seq: i64,
        emoji: String,
    },
    /// Schedule a future turn in the current session. Not a user-visible send:
    /// it records a scheduled item (fired later by the host's scheduler tick),
    /// so the run's final text still delivers the user-facing confirmation.
    /// `every_seconds` recurs on a fixed interval; absent = fire once.
    ScheduleMessage {
        text: String,
        after_seconds: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every_seconds: Option<i64>,
    },
    /// Persist a memory for reuse in later turns. Not a user-visible send: the
    /// host writes a catalog-indexed markdown note as a side effect, so the
    /// run's final text still delivers the user-facing confirmation. `title` is
    /// an optional human label; absent = the host derives one.
    SaveMemory {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Cancel a previously scheduled item by its `scheduled_item_id` (which the
    /// agent reads from the host-injected `<active_schedules>` context block).
    /// Not a user-visible send: the host marks the item cancelled as a side
    /// effect, so the run's final text still delivers the user-facing
    /// confirmation.
    CancelSchedule {
        scheduled_item_id: String,
    },
}

impl OutboundAction {
    /// Whether this action delivers or alters user-visible content for a run.
    ///
    /// A user-visible send suppresses final-text fallback so a run never both
    /// sends a typed reply and re-delivers its final text. Edits count because
    /// they replace visible content; a bare reaction does not, since a reaction
    /// alone is an acknowledgement rather than the run's reply.
    pub fn is_user_visible_send(&self) -> bool {
        matches!(
            self,
            OutboundAction::SendMessage { .. }
                | OutboundAction::SendFile { .. }
                | OutboundAction::SendCard { .. }
                | OutboundAction::AskUserQuestion { .. }
                | OutboundAction::EditMessage { .. }
        )
    }

    /// The stable wire "kind" written into the session outbound DB.
    pub fn kind(&self) -> &'static str {
        match self {
            OutboundAction::SendMessage { .. } => "send_message",
            OutboundAction::SendFile { .. } => "send_file",
            OutboundAction::SendCard { .. } => "send_card",
            OutboundAction::AskUserQuestion { .. } => "ask_user_question",
            OutboundAction::EditMessage { .. } => "edit_message",
            OutboundAction::AddReaction { .. } => "add_reaction",
            OutboundAction::ScheduleMessage { .. } => "schedule_message",
            OutboundAction::SaveMemory { .. } => "save_memory",
            OutboundAction::CancelSchedule { .. } => "cancel_schedule",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_message_round_trips() {
        let action = OutboundAction::SendMessage {
            destination: "local:cli".to_string(),
            text: "hi".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#"{"action":"send_message","destination":"local:cli","text":"hi"}"#);
        let back: OutboundAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn every_variant_round_trips() {
        let actions = vec![
            OutboundAction::SendMessage {
                destination: "d".into(),
                text: "t".into(),
            },
            OutboundAction::SendFile {
                destination: "d".into(),
                path: "/p".into(),
                caption: Some("c".into()),
            },
            OutboundAction::SendFile {
                destination: "d".into(),
                path: "/p".into(),
                caption: None,
            },
            OutboundAction::SendCard {
                destination: "d".into(),
                title: "ti".into(),
                body: "bo".into(),
            },
            OutboundAction::AskUserQuestion {
                destination: "d".into(),
                question: "q".into(),
                options: vec!["a".into(), "b".into()],
            },
            OutboundAction::EditMessage {
                target_seq: 3,
                text: "x".into(),
            },
            OutboundAction::AddReaction {
                target_seq: 3,
                emoji: ":+1:".into(),
            },
            OutboundAction::ScheduleMessage {
                text: "stretch".into(),
                after_seconds: 60,
                every_seconds: Some(3600),
            },
            OutboundAction::ScheduleMessage {
                text: "once".into(),
                after_seconds: 60,
                every_seconds: None,
            },
            OutboundAction::SaveMemory {
                content: "user prefers terse replies".into(),
                title: Some("reply style".into()),
            },
            OutboundAction::SaveMemory {
                content: "no title here".into(),
                title: None,
            },
            OutboundAction::CancelSchedule {
                scheduled_item_id: "sched_abc123".into(),
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let back: OutboundAction = serde_json::from_str(&json).unwrap();
            assert_eq!(back, action, "round trip failed for {}", action.kind());
        }
    }

    #[test]
    fn user_visible_send_classification() {
        assert!(OutboundAction::SendMessage {
            destination: "d".into(),
            text: "t".into()
        }
        .is_user_visible_send());
        assert!(OutboundAction::EditMessage {
            target_seq: 1,
            text: "t".into()
        }
        .is_user_visible_send());
        assert!(!OutboundAction::AddReaction {
            target_seq: 1,
            emoji: ":x:".into()
        }
        .is_user_visible_send());
        // Scheduling is a side effect, not the run's visible reply.
        assert!(!OutboundAction::ScheduleMessage {
            text: "t".into(),
            after_seconds: 60,
            every_seconds: None,
        }
        .is_user_visible_send());
        // Saving a memory is a side effect, not the run's visible reply.
        assert!(!OutboundAction::SaveMemory {
            content: "c".into(),
            title: None,
        }
        .is_user_visible_send());
        // Cancelling a schedule is a side effect, not the run's visible reply.
        assert!(!OutboundAction::CancelSchedule {
            scheduled_item_id: "sched_abc123".into(),
        }
        .is_user_visible_send());
    }

    #[test]
    fn schedule_message_omits_absent_recurrence() {
        let action = OutboundAction::ScheduleMessage {
            text: "once".into(),
            after_seconds: 90,
            every_seconds: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(
            json,
            r#"{"action":"schedule_message","text":"once","after_seconds":90}"#
        );
        let back: OutboundAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn save_memory_omits_absent_title() {
        let action = OutboundAction::SaveMemory {
            content: "remember this".into(),
            title: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(
            json,
            r#"{"action":"save_memory","content":"remember this"}"#
        );
        let back: OutboundAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, action);
    }

    #[test]
    fn skips_absent_caption_in_json() {
        let action = OutboundAction::SendFile {
            destination: "d".into(),
            path: "/p".into(),
            caption: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(!json.contains("caption"), "absent caption must not serialize: {json}");
    }
}
