//! Slack inbound normalization: turn a Slack Events API / Socket Mode event
//! payload into the platform's neutral [`RoutingEvent`].
//!
//! The crate never talks to the network here — it parses the JSON Slack already
//! delivered and maps it. Thread replies are scoped to their root so engagement
//! stays distinct from the channel top level; edits, deletes, and reactions are
//! normalized to their [`MessageState`]; and the bot's own messages are flagged
//! self-authored so the router drops them instead of looping.

use claw_router::{dedupe_key, engagement_key, MessageState, RoutingEvent};
use serde::Deserialize;

/// The identity of our own Slack app, used to detect mentions of the bot and to
/// filter the bot's own messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackIdentity {
    /// The bot's user id, e.g. `U123BOT` — used for `<@U123BOT>` mention
    /// detection and self-author filtering.
    pub bot_user_id: String,
    /// The app's `bot_id`, e.g. `B456` — Slack stamps it on the bot's own
    /// messages; used as a second self-author signal.
    pub self_bot_id: Option<String>,
}

/// A focused view of a Slack event envelope — only the fields normalization
/// consumes. Unknown fields are ignored; absent ones default to `None`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SlackEnvelope {
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub channel_type: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub deleted_ts: Option<String>,
    #[serde(default)]
    pub reaction: Option<String>,
    #[serde(default)]
    pub item: Option<SlackReactionItem>,
    /// For `message_changed`, the edited message nests here.
    #[serde(default)]
    pub message: Option<Box<SlackEnvelope>>,
}

/// The `item` of a `reaction_added` event — the message the reaction targets.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SlackReactionItem {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
}

/// Parse a Slack event JSON payload into a [`SlackEnvelope`].
pub fn parse_event(json: &str) -> serde_json::Result<SlackEnvelope> {
    serde_json::from_str(json)
}

/// Normalize a Slack event into a [`RoutingEvent`], or `None` when the event is
/// not one we route (channel joins, reaction removals, unknown types).
pub fn normalize(env: &SlackEnvelope, id: &SlackIdentity) -> Option<RoutingEvent> {
    match env.event_type.as_str() {
        "reaction_added" => normalize_reaction(env, id),
        "message" | "app_mention" => normalize_message(env, id),
        _ => None,
    }
}

fn normalize_message(env: &SlackEnvelope, id: &SlackIdentity) -> Option<RoutingEvent> {
    let force_mention = env.event_type == "app_mention";

    match env.subtype.as_deref() {
        Some("message_changed") => {
            let inner = env.message.as_deref()?;
            let channel = inner.channel.as_deref().or(env.channel.as_deref())?;
            let ts = inner.ts.as_deref()?;
            // The edit shares the original message's ts, so fold the state into
            // the dedupe seed — otherwise it collides with the original New
            // event's key and the edit gets dropped as a duplicate.
            let seed = format!("{ts}:edited");
            Some(build(
                channel,
                ts,
                &seed,
                inner.thread_ts.as_deref(),
                inner.user.as_deref(),
                inner.bot_id.as_deref().or(env.bot_id.as_deref()),
                inner.text.as_deref().unwrap_or(""),
                env.channel_type.as_deref(),
                force_mention,
                MessageState::Edited,
                id,
            ))
        }
        Some("message_deleted") => {
            let channel = env.channel.as_deref()?;
            let ts = env.deleted_ts.as_deref()?;
            // A tombstone: no author/text survive, but it stays auditable. Its
            // dedupe seed folds in the state so it never collides with the
            // original message's New event.
            let seed = format!("{ts}:deleted");
            Some(build(
                channel,
                ts,
                &seed,
                env.thread_ts.as_deref(),
                None,
                None,
                "",
                env.channel_type.as_deref(),
                false,
                MessageState::Deleted,
                id,
            ))
        }
        // channel_join / channel_leave / bot_add etc. are not routed.
        Some(_) => None,
        None => {
            let channel = env.channel.as_deref()?;
            let ts = env.ts.as_deref()?;
            Some(build(
                channel,
                ts,
                ts,
                env.thread_ts.as_deref(),
                env.user.as_deref(),
                env.bot_id.as_deref(),
                env.text.as_deref().unwrap_or(""),
                env.channel_type.as_deref(),
                force_mention,
                MessageState::New,
                id,
            ))
        }
    }
}

fn normalize_reaction(env: &SlackEnvelope, id: &SlackIdentity) -> Option<RoutingEvent> {
    let item = env.item.as_ref()?;
    let channel = item.channel.as_deref()?;
    let target_ts = item.ts.as_deref()?;
    let emoji = env.reaction.clone().unwrap_or_default();
    let reactor = env.user.as_deref();

    // The reaction targets an existing message (its ts is the platform id), but
    // its dedupe key must stay distinct from that message and from other
    // reactions, so it folds in the emoji and the reacting user.
    let is_self = is_self_author(reactor, env.bot_id.as_deref(), id);
    let dedupe_seed = format!("{target_ts}:reaction:{emoji}:{}", reactor.unwrap_or(""));
    Some(RoutingEvent {
        channel_kind: "slack".to_string(),
        chat_id: channel.to_string(),
        sender_id: reactor.unwrap_or("").to_string(),
        sender_label: None,
        platform_message_id: target_ts.to_string(),
        thread_root_id: None,
        reply_target_id: Some(target_ts.to_string()),
        engagement_key: engagement_key("slack", channel, None),
        permalink: None,
        state: MessageState::Reaction { emoji },
        is_mention: false,
        is_direct: false,
        is_self_author: is_self,
        dedupe_key: dedupe_key("slack", channel, &dedupe_seed),
        text: String::new(),
        raw_ref: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn build(
    channel: &str,
    ts: &str,
    dedupe_seed: &str,
    thread_ts: Option<&str>,
    user: Option<&str>,
    bot_id: Option<&str>,
    text: &str,
    channel_type: Option<&str>,
    force_mention: bool,
    state: MessageState,
    id: &SlackIdentity,
) -> RoutingEvent {
    let is_direct = channel_type == Some("im");
    let is_mention = force_mention || text.contains(&format!("<@{}>", id.bot_user_id));
    let is_self = is_self_author(user, bot_id, id);

    // A thread reply carries thread_ts != ts; the thread root carries thread_ts
    // == ts (or none), so it stays scoped to the channel top level.
    let thread_root_id = thread_ts.filter(|t| *t != ts).map(str::to_string);
    let reply_target_id = thread_ts.map(str::to_string);
    let sender_id = user.or(bot_id).unwrap_or("").to_string();

    RoutingEvent {
        channel_kind: "slack".to_string(),
        chat_id: channel.to_string(),
        sender_id,
        sender_label: None,
        platform_message_id: ts.to_string(),
        thread_root_id: thread_root_id.clone(),
        reply_target_id,
        engagement_key: engagement_key("slack", channel, thread_root_id.as_deref()),
        permalink: None,
        state,
        is_mention,
        is_direct,
        is_self_author: is_self,
        dedupe_key: dedupe_key("slack", channel, dedupe_seed),
        text: text.to_string(),
        raw_ref: None,
    }
}

fn is_self_author(user: Option<&str>, bot_id: Option<&str>, id: &SlackIdentity) -> bool {
    if user == Some(id.bot_user_id.as_str()) {
        return true;
    }
    matches!((bot_id, id.self_bot_id.as_deref()), (Some(b), Some(s)) if b == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SlackIdentity {
        SlackIdentity {
            bot_user_id: "U_BOT".to_string(),
            self_bot_id: Some("B_SELF".to_string()),
        }
    }

    #[test]
    fn plain_channel_message_is_new_and_top_level() {
        let env = parse_event(
            r#"{"type":"message","channel":"C1","user":"U1","ts":"100.1","text":"hello there"}"#,
        )
        .unwrap();
        let ev = normalize(&env, &id()).unwrap();
        assert_eq!(ev.channel_kind, "slack");
        assert_eq!(ev.chat_id, "C1");
        assert_eq!(ev.sender_id, "U1");
        assert_eq!(ev.platform_message_id, "100.1");
        assert_eq!(ev.state, MessageState::New);
        assert!(!ev.is_mention);
        assert!(!ev.is_direct);
        assert!(!ev.is_self_author);
        assert_eq!(ev.thread_root_id, None);
        assert_eq!(ev.engagement_key, "slack:C1");
        assert_eq!(ev.dedupe_key, "slack:C1:100.1");
    }

    #[test]
    fn dm_and_inline_mention_are_detected() {
        let dm = parse_event(
            r#"{"type":"message","channel":"D1","channel_type":"im","user":"U1","ts":"1.0","text":"hi"}"#,
        )
        .unwrap();
        assert!(normalize(&dm, &id()).unwrap().is_direct);

        let mention = parse_event(
            r#"{"type":"message","channel":"C1","user":"U1","ts":"2.0","text":"hey <@U_BOT> deploy"}"#,
        )
        .unwrap();
        assert!(normalize(&mention, &id()).unwrap().is_mention);

        let app_mention = parse_event(
            r#"{"type":"app_mention","channel":"C1","user":"U1","ts":"3.0","text":"go"}"#,
        )
        .unwrap();
        assert!(normalize(&app_mention, &id()).unwrap().is_mention);
    }

    #[test]
    fn thread_reply_is_scoped_to_its_root() {
        let reply = parse_event(
            r#"{"type":"message","channel":"C1","user":"U1","ts":"200.2","thread_ts":"100.1","text":"reply"}"#,
        )
        .unwrap();
        let ev = normalize(&reply, &id()).unwrap();
        assert_eq!(ev.thread_root_id.as_deref(), Some("100.1"));
        assert_eq!(ev.reply_target_id.as_deref(), Some("100.1"));
        assert_eq!(ev.engagement_key, "slack:C1:100.1");

        // The root message itself (thread_ts == ts) stays top-level.
        let root = parse_event(
            r#"{"type":"message","channel":"C1","user":"U1","ts":"100.1","thread_ts":"100.1","text":"root"}"#,
        )
        .unwrap();
        let rev = normalize(&root, &id()).unwrap();
        assert_eq!(rev.thread_root_id, None);
        assert_eq!(rev.engagement_key, "slack:C1");
    }

    #[test]
    fn edit_and_delete_map_to_their_states() {
        let edited = parse_event(
            r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"ts":"100.1","user":"U1","text":"edited"}}"#,
        )
        .unwrap();
        let e = normalize(&edited, &id()).unwrap();
        assert_eq!(e.state, MessageState::Edited);
        assert_eq!(e.text, "edited");
        assert_eq!(e.platform_message_id, "100.1");

        let deleted = parse_event(
            r#"{"type":"message","subtype":"message_deleted","channel":"C1","deleted_ts":"100.1"}"#,
        )
        .unwrap();
        let d = normalize(&deleted, &id()).unwrap();
        assert_eq!(d.state, MessageState::Deleted);
        assert_eq!(d.platform_message_id, "100.1");
        assert_eq!(d.text, "");
    }

    #[test]
    fn edit_and_delete_dedupe_distinctly_from_the_original_message() {
        let original = dedupe_key("slack", "C1", "100.1");

        let edited = parse_event(
            r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"ts":"100.1","user":"U1","text":"edited"}}"#,
        )
        .unwrap();
        let e = normalize(&edited, &id()).unwrap();
        assert_ne!(e.dedupe_key, original);

        let deleted = parse_event(
            r#"{"type":"message","subtype":"message_deleted","channel":"C1","deleted_ts":"100.1"}"#,
        )
        .unwrap();
        let d = normalize(&deleted, &id()).unwrap();
        assert_ne!(d.dedupe_key, original);
        assert_ne!(d.dedupe_key, e.dedupe_key);
    }

    #[test]
    fn reaction_is_distinct_from_its_target_message() {
        let reaction = parse_event(
            r#"{"type":"reaction_added","user":"U1","reaction":"thumbsup","item":{"channel":"C1","ts":"100.1"}}"#,
        )
        .unwrap();
        let r = normalize(&reaction, &id()).unwrap();
        assert_eq!(r.state, MessageState::Reaction { emoji: "thumbsup".to_string() });
        assert_eq!(r.platform_message_id, "100.1");
        // The reaction's dedupe key never collides with the reacted message.
        assert_ne!(r.dedupe_key, dedupe_key("slack", "C1", "100.1"));
    }

    #[test]
    fn the_bots_own_messages_are_flagged_self_authored() {
        let by_user = parse_event(
            r#"{"type":"message","channel":"C1","user":"U_BOT","ts":"9.0","text":"echo"}"#,
        )
        .unwrap();
        assert!(normalize(&by_user, &id()).unwrap().is_self_author);

        let by_bot_id = parse_event(
            r#"{"type":"message","channel":"C1","bot_id":"B_SELF","ts":"9.1","text":"echo"}"#,
        )
        .unwrap();
        assert!(normalize(&by_bot_id, &id()).unwrap().is_self_author);
    }

    #[test]
    fn retries_of_the_same_message_collapse() {
        let json = r#"{"type":"message","channel":"C1","user":"U1","ts":"100.1","text":"hi"}"#;
        let a = normalize(&parse_event(json).unwrap(), &id()).unwrap();
        let b = normalize(&parse_event(json).unwrap(), &id()).unwrap();
        assert_eq!(a.dedupe_key, b.dedupe_key);
    }

    #[test]
    fn unrouted_events_return_none() {
        let join = parse_event(
            r#"{"type":"message","subtype":"channel_join","channel":"C1","user":"U1","ts":"1.0"}"#,
        )
        .unwrap();
        assert!(normalize(&join, &id()).is_none());

        let removed = parse_event(
            r#"{"type":"reaction_removed","user":"U1","reaction":"x","item":{"channel":"C1","ts":"1.0"}}"#,
        )
        .unwrap();
        assert!(normalize(&removed, &id()).is_none());
    }
}
