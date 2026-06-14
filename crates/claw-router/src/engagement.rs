//! Pure routing-engagement decision logic: given a normalized [`RoutingEvent`]
//! and a wired agent's engagement mode, sender scope, and ignored-message
//! policy, decide whether to engage the agent, accumulate the message as
//! context, drop it (and why), or silently ignore it.
//!
//! This module holds NO state and touches NO database. Sticky-engagement state
//! (the persisted root-conversation key for `MentionSticky`) is supplied by the
//! caller as `has_active_sticky`; the persistence behind it lands separately.

use crate::channel::RoutingEvent;
use crate::model::DropReason;

/// How a wired agent decides whether a message engages it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngagementMode {
    /// Engage when the text matches `trigger`. `"."` means always engage;
    /// otherwise this is a case-insensitive substring match.
    ///
    /// NOTE: full regex matching is deferred — no regex crate is vendored for
    /// offline builds — so the trigger is currently a literal substring (with
    /// `"."` reserved for always-on). Swapping in real regex later does not
    /// change this module's interface.
    Pattern { trigger: String },
    /// Engage only on a platform-confirmed mention or a direct message.
    Mention,
    /// First mention/DM engages; while a sticky session is active for the
    /// conversation, follow-ups stay engaged.
    MentionSticky,
}

/// Which senders an agent will accept messages from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderScope {
    /// Any sender (still subject to the messaging-group access policy).
    All,
    /// Sender must be a member of the agent group.
    Known,
}

/// What to do with a message a wired agent does not engage on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoredMessagePolicy {
    /// The agent never sees ignored messages.
    Drop,
    /// Store ignored messages as context; the next engaging message pulls them
    /// into the batch.
    Accumulate,
}

/// The decision for one wired agent and one event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngagementDecision {
    /// Engage the agent: wake its session with this message.
    Engage,
    /// Store as trigger=0 context; do not wake.
    Accumulate,
    /// Record a dropped message with this reason.
    Drop { reason: DropReason },
    /// Silently ignore — not audited (e.g. the bot's own non-echo event).
    Ignore,
}

/// Inputs the caller resolves outside this pure function.
#[derive(Clone, Copy, Debug)]
pub struct EngagementContext {
    /// The sender is a member of the agent group (only consulted under
    /// [`SenderScope::Known`]).
    pub sender_is_member: bool,
    /// An active sticky session exists for this event's engagement key (only
    /// consulted under [`EngagementMode::MentionSticky`]).
    pub has_active_sticky: bool,
}

/// Decide how a wired agent should handle an event.
pub fn evaluate_engagement(
    event: &RoutingEvent,
    mode: &EngagementMode,
    scope: SenderScope,
    policy: IgnoredMessagePolicy,
    ctx: EngagementContext,
) -> EngagementDecision {
    // The bot's own messages never engage and are not audited as drops.
    if event.is_self_author {
        return EngagementDecision::Ignore;
    }

    if is_engaged(event, mode, ctx.has_active_sticky) {
        // Engaged: apply the sender-scope gate.
        if scope == SenderScope::Known && !ctx.sender_is_member {
            return EngagementDecision::Drop { reason: DropReason::NotAuthorized };
        }
        return EngagementDecision::Engage;
    }

    // Not engaged: the ignored-message policy decides.
    match policy {
        IgnoredMessagePolicy::Accumulate => EngagementDecision::Accumulate,
        IgnoredMessagePolicy::Drop => EngagementDecision::Drop { reason: DropReason::NotEngaged },
    }
}

fn is_engaged(event: &RoutingEvent, mode: &EngagementMode, has_active_sticky: bool) -> bool {
    match mode {
        EngagementMode::Pattern { trigger } => {
            trigger == "." || text_matches(&event.text, trigger)
        }
        EngagementMode::Mention => event.is_mention || event.is_direct,
        EngagementMode::MentionSticky => event.is_mention || event.is_direct || has_active_sticky,
    }
}

fn text_matches(text: &str, trigger: &str) -> bool {
    text.to_lowercase().contains(&trigger.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{dedupe_key, engagement_key, MessageState};

    fn event(text: &str, is_mention: bool, is_direct: bool) -> RoutingEvent {
        RoutingEvent {
            channel_kind: "slack".into(),
            chat_id: "C1".into(),
            sender_id: "U1".into(),
            sender_label: None,
            platform_message_id: "m1".into(),
            thread_root_id: None,
            reply_target_id: None,
            engagement_key: engagement_key("slack", "C1", None),
            permalink: None,
            state: MessageState::New,
            is_mention,
            is_direct,
            is_self_author: false,
            dedupe_key: dedupe_key("slack", "C1", "m1"),
            text: text.into(),
            raw_ref: None,
        }
    }

    const CTX: EngagementContext =
        EngagementContext { sender_is_member: true, has_active_sticky: false };

    #[test]
    fn pattern_dot_always_engages() {
        let d = evaluate_engagement(
            &event("anything", false, false),
            &EngagementMode::Pattern { trigger: ".".into() },
            SenderScope::All,
            IgnoredMessagePolicy::Drop,
            CTX,
        );
        assert_eq!(d, EngagementDecision::Engage);
    }

    #[test]
    fn pattern_substring_is_case_insensitive() {
        let mode = EngagementMode::Pattern { trigger: "Deploy".into() };
        assert_eq!(
            evaluate_engagement(&event("please deploy now", false, false), &mode, SenderScope::All, IgnoredMessagePolicy::Drop, CTX),
            EngagementDecision::Engage
        );
        // No match -> not engaged -> dropped as not-engaged under drop policy.
        assert_eq!(
            evaluate_engagement(&event("hello", false, false), &mode, SenderScope::All, IgnoredMessagePolicy::Drop, CTX),
            EngagementDecision::Drop { reason: DropReason::NotEngaged }
        );
    }

    #[test]
    fn mention_engages_on_mention_or_dm_else_accumulates() {
        assert_eq!(
            evaluate_engagement(&event("hi", true, false), &EngagementMode::Mention, SenderScope::All, IgnoredMessagePolicy::Accumulate, CTX),
            EngagementDecision::Engage
        );
        assert_eq!(
            evaluate_engagement(&event("hi", false, true), &EngagementMode::Mention, SenderScope::All, IgnoredMessagePolicy::Accumulate, CTX),
            EngagementDecision::Engage
        );
        // Not a mention/DM, accumulate policy -> stored as context.
        assert_eq!(
            evaluate_engagement(&event("chatter", false, false), &EngagementMode::Mention, SenderScope::All, IgnoredMessagePolicy::Accumulate, CTX),
            EngagementDecision::Accumulate
        );
    }

    #[test]
    fn mention_sticky_keeps_followups_engaged() {
        let sticky = EngagementContext { sender_is_member: true, has_active_sticky: true };
        // No mention, but an active sticky session -> still engaged.
        assert_eq!(
            evaluate_engagement(&event("follow up", false, false), &EngagementMode::MentionSticky, SenderScope::All, IgnoredMessagePolicy::Drop, sticky),
            EngagementDecision::Engage
        );
        // Without sticky and without mention -> not engaged.
        assert_eq!(
            evaluate_engagement(&event("follow up", false, false), &EngagementMode::MentionSticky, SenderScope::All, IgnoredMessagePolicy::Drop, CTX),
            EngagementDecision::Drop { reason: DropReason::NotEngaged }
        );
    }

    #[test]
    fn known_scope_blocks_non_members_even_when_engaged() {
        let non_member = EngagementContext { sender_is_member: false, has_active_sticky: false };
        assert_eq!(
            evaluate_engagement(&event("hi", true, false), &EngagementMode::Mention, SenderScope::Known, IgnoredMessagePolicy::Drop, non_member),
            EngagementDecision::Drop { reason: DropReason::NotAuthorized }
        );
    }

    #[test]
    fn self_authored_events_are_ignored() {
        let mut e = event("echo", true, false);
        e.is_self_author = true;
        assert_eq!(
            evaluate_engagement(&e, &EngagementMode::Mention, SenderScope::All, IgnoredMessagePolicy::Drop, CTX),
            EngagementDecision::Ignore
        );
    }
}
