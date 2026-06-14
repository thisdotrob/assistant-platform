//! Telegram inbound normalization: turn a Telegram Bot API `Update` into the
//! platform's neutral [`RoutingEvent`].
//!
//! No network here — this maps the JSON `getUpdates`/webhook already delivered.
//! A private chat is a direct message; forum topics scope engagement via
//! `message_thread_id`; replies carry their reply target; and the bot's own
//! messages are flagged self-authored so the router drops them.

use claw_router::{dedupe_key, engagement_key, MessageState, RoutingEvent};
use serde::Deserialize;

/// Our own bot identity, for mention detection and self-author filtering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelegramIdentity {
    pub bot_id: i64,
    /// The bot's `@username` without the leading `@`.
    pub bot_username: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub edited_message: Option<Message>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
    #[serde(default)]
    pub entities: Vec<Entity>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Chat {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "type", default)]
    pub chat_type: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Entity {
    #[serde(rename = "type", default)]
    pub entity_type: String,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub length: i64,
    #[serde(default)]
    pub user: Option<User>,
}

/// Parse a Telegram `Update` JSON payload.
pub fn parse_update(json: &str) -> serde_json::Result<Update> {
    serde_json::from_str(json)
}

/// Normalize an `Update` into a [`RoutingEvent`], or `None` when there is no
/// message we route (callbacks, joins, polls, etc.).
pub fn normalize(update: &Update, id: &TelegramIdentity) -> Option<RoutingEvent> {
    let (state, msg) = if let Some(m) = &update.message {
        (MessageState::New, m)
    } else if let Some(m) = &update.edited_message {
        (MessageState::Edited, m)
    } else {
        return None;
    };

    let chat_id = msg.chat.id.to_string();
    let is_direct = msg.chat.chat_type == "private";
    let text = msg.text.clone().unwrap_or_default();
    let sender_id = msg.from.as_ref().map(|u| u.id.to_string()).unwrap_or_default();
    let sender_label = msg
        .from
        .as_ref()
        .and_then(|u| u.username.clone().or_else(|| u.first_name.clone()));
    let is_self = msg
        .from
        .as_ref()
        .is_some_and(|u| u.is_bot && u.id == id.bot_id);

    let is_mention = mentions_bot(&text, &msg.entities, id);
    let thread_root_id = msg.message_thread_id.map(|t| t.to_string());
    let reply_target_id = msg
        .reply_to_message
        .as_ref()
        .map(|r| r.message_id.to_string());
    let platform_message_id = msg.message_id.to_string();

    Some(RoutingEvent {
        channel_kind: "telegram".to_string(),
        chat_id: chat_id.clone(),
        sender_id,
        sender_label,
        platform_message_id: platform_message_id.clone(),
        thread_root_id: thread_root_id.clone(),
        reply_target_id,
        engagement_key: engagement_key("telegram", &chat_id, thread_root_id.as_deref()),
        permalink: None,
        state,
        is_mention,
        is_direct,
        is_self_author: is_self,
        dedupe_key: dedupe_key("telegram", &chat_id, &platform_message_id),
        text,
        raw_ref: None,
    })
}

/// Whether the message mentions our bot: an `@username` mention entity whose
/// span actually spells our `@bot_username`, or a `text_mention` entity whose
/// user id is the bot's.
///
/// Telegram entity offsets/lengths are UTF-16 code-unit counts, so we slice the
/// text in UTF-16 space and compare the decoded span exactly. A global
/// `text.contains` would let an unrelated mention entity plus a stray
/// `@claw_bot` in the body — or a longer `@claw_bot_evil` handle — spoof a
/// mention.
fn mentions_bot(text: &str, entities: &[Entity], id: &TelegramIdentity) -> bool {
    let target = format!("@{}", id.bot_username);
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let has_username_mention = entities.iter().any(|e| {
        e.entity_type == "mention"
            && entity_span(&utf16, e.offset, e.length).as_deref() == Some(target.as_str())
    });
    let has_text_mention = entities
        .iter()
        .any(|e| e.entity_type == "text_mention" && e.user.as_ref().map(|u| u.id) == Some(id.bot_id));
    has_username_mention || has_text_mention
}

/// Decode the substring a Telegram entity covers, given the text's UTF-16 code
/// units and the entity's `offset`/`length`. Returns `None` for a negative or
/// out-of-range span.
fn entity_span(utf16: &[u16], offset: i64, length: i64) -> Option<String> {
    let start = usize::try_from(offset).ok()?;
    let len = usize::try_from(length).ok()?;
    let end = start.checked_add(len)?;
    if end > utf16.len() {
        return None;
    }
    String::from_utf16(&utf16[start..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> TelegramIdentity {
        TelegramIdentity { bot_id: 999, bot_username: "claw_bot".to_string() }
    }

    #[test]
    fn private_chat_is_a_direct_message() {
        let u = parse_update(
            r#"{"update_id":1,"message":{"message_id":10,"from":{"id":1,"is_bot":false,"username":"alice"},"chat":{"id":1,"type":"private"},"text":"hi"}}"#,
        )
        .unwrap();
        let ev = normalize(&u, &id()).unwrap();
        assert_eq!(ev.channel_kind, "telegram");
        assert_eq!(ev.chat_id, "1");
        assert_eq!(ev.sender_id, "1");
        assert_eq!(ev.sender_label.as_deref(), Some("alice"));
        assert!(ev.is_direct);
        assert_eq!(ev.state, MessageState::New);
        assert_eq!(ev.platform_message_id, "10");
        assert_eq!(ev.engagement_key, "telegram:1");
        assert_eq!(ev.dedupe_key, "telegram:1:10");
    }

    #[test]
    fn group_username_mention_is_detected() {
        let u = parse_update(
            r#"{"update_id":2,"message":{"message_id":11,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"supergroup"},"text":"hey @claw_bot deploy","entities":[{"type":"mention","offset":4,"length":9}]}}"#,
        )
        .unwrap();
        let ev = normalize(&u, &id()).unwrap();
        assert!(!ev.is_direct);
        assert!(ev.is_mention);
    }

    #[test]
    fn text_mention_by_user_id_is_detected() {
        let u = parse_update(
            r#"{"update_id":3,"message":{"message_id":12,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"group"},"text":"ping bot","entities":[{"type":"text_mention","offset":5,"length":3,"user":{"id":999,"is_bot":true}}]}}"#,
        )
        .unwrap();
        assert!(normalize(&u, &id()).unwrap().is_mention);
    }

    #[test]
    fn mention_entity_must_actually_spell_the_bot_username() {
        // The mention entity points at a different @handle while the bot's
        // handle appears only as plain body text. A global substring match
        // would be spoofed into treating this as a mention.
        let u = parse_update(
            r#"{"update_id":10,"message":{"message_id":18,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"supergroup"},"text":"@someone_else look at @claw_bot","entities":[{"type":"mention","offset":0,"length":13}]}}"#,
        )
        .unwrap();
        assert!(!normalize(&u, &id()).unwrap().is_mention);
    }

    #[test]
    fn longer_lookalike_handle_is_not_our_mention() {
        // `@claw_bot_evil` contains `@claw_bot` as a prefix; an exact span
        // comparison must reject it.
        let u = parse_update(
            r#"{"update_id":11,"message":{"message_id":19,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"supergroup"},"text":"hey @claw_bot_evil ping","entities":[{"type":"mention","offset":4,"length":14}]}}"#,
        )
        .unwrap();
        assert!(!normalize(&u, &id()).unwrap().is_mention);
    }

    #[test]
    fn unrelated_group_chatter_is_not_a_mention() {
        let u = parse_update(
            r#"{"update_id":4,"message":{"message_id":13,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"group"},"text":"just chatting"}}"#,
        )
        .unwrap();
        let ev = normalize(&u, &id()).unwrap();
        assert!(!ev.is_mention);
        assert!(!ev.is_direct);
    }

    #[test]
    fn forum_topic_scopes_engagement_and_reply_is_captured() {
        let u = parse_update(
            r#"{"update_id":5,"message":{"message_id":14,"from":{"id":2,"is_bot":false},"chat":{"id":-100,"type":"supergroup"},"message_thread_id":7,"text":"in topic","reply_to_message":{"message_id":7}}}"#,
        )
        .unwrap();
        let ev = normalize(&u, &id()).unwrap();
        assert_eq!(ev.thread_root_id.as_deref(), Some("7"));
        assert_eq!(ev.reply_target_id.as_deref(), Some("7"));
        assert_eq!(ev.engagement_key, "telegram:-100:7");
    }

    #[test]
    fn edited_message_maps_to_edited_state() {
        let u = parse_update(
            r#"{"update_id":6,"edited_message":{"message_id":15,"from":{"id":2,"is_bot":false},"chat":{"id":1,"type":"private"},"text":"fixed"}}"#,
        )
        .unwrap();
        let ev = normalize(&u, &id()).unwrap();
        assert_eq!(ev.state, MessageState::Edited);
        assert_eq!(ev.text, "fixed");
    }

    #[test]
    fn the_bots_own_message_is_self_authored() {
        let u = parse_update(
            r#"{"update_id":7,"message":{"message_id":16,"from":{"id":999,"is_bot":true,"username":"claw_bot"},"chat":{"id":1,"type":"private"},"text":"echo"}}"#,
        )
        .unwrap();
        assert!(normalize(&u, &id()).unwrap().is_self_author);
    }

    #[test]
    fn another_bots_message_is_not_self_authored() {
        let u = parse_update(
            r#"{"update_id":8,"message":{"message_id":17,"from":{"id":555,"is_bot":true,"username":"other_bot"},"chat":{"id":1,"type":"private"},"text":"hi"}}"#,
        )
        .unwrap();
        assert!(!normalize(&u, &id()).unwrap().is_self_author);
    }

    #[test]
    fn updates_without_a_message_are_not_routed() {
        let u = parse_update(r#"{"update_id":9}"#).unwrap();
        assert!(normalize(&u, &id()).is_none());
    }
}
