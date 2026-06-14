//! claw-router: inbound-message routing and the audit of messages it declines.
//!
//! This slice implements only the dropped-message audit: recording why an
//! inbound message was turned away and reporting those drops. Routing decisions
//! themselves are a later milestone.

pub mod channel;
pub mod engagement;
pub mod model;
pub mod registry;
pub mod sticky;
pub mod store;

pub use channel::{
    dedupe_key, engagement_key, ChannelAdapter, ChannelError, ChannelHealth, DeliveryTarget,
    FileRef, MessageState, OutboundContent, RoutingEvent, SenderIdentity, SetupStep,
};
pub use engagement::{
    evaluate_engagement, EngagementContext, EngagementDecision, EngagementMode,
    IgnoredMessagePolicy, SenderScope,
};
pub use model::{DropReason, DroppedMessage, RouterError};
pub use registry::{ChannelRegistry, ChannelStatus, RegistryError};
pub use sticky::{
    expire_sticky, has_active_sticky, lookup_active_sticky, migrations, open_sticky, reset_sticky,
    EpochSecs, StickyEngagement,
};
pub use store::{count_drops, count_drops_by_reason, list_drops, record_drop};

pub const MODULE_ID: &str = "claw-router";
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
    fn record_and_read_back_a_drop() {
        let conn = db();
        let id = record_drop(
            &conn,
            "slack",
            Some("U123"),
            DropReason::UnknownSender,
            Some("hi"),
        )
        .unwrap();

        let drops = list_drops(&conn, 10).unwrap();
        assert_eq!(drops.len(), 1);
        let d = &drops[0];
        assert_eq!(d.id, id);
        assert_eq!(d.channel, "slack");
        assert_eq!(d.sender.as_deref(), Some("U123"));
        assert_eq!(d.reason, DropReason::UnknownSender);
        assert_eq!(d.payload.as_deref(), Some("hi"));
        assert!(!d.created_at.is_empty());
    }

    #[test]
    fn missing_sender_and_payload_are_null() {
        let conn = db();
        record_drop(&conn, "cli", None, DropReason::Malformed, None).unwrap();
        let d = &list_drops(&conn, 10).unwrap()[0];
        assert_eq!(d.sender, None);
        assert_eq!(d.payload, None);
        assert_eq!(d.reason, DropReason::Malformed);
    }

    #[test]
    fn list_is_newest_first_and_respects_limit() {
        let conn = db();
        record_drop(&conn, "cli", None, DropReason::NoRoute, None).unwrap();
        let second = record_drop(&conn, "cli", None, DropReason::Duplicate, None).unwrap();

        let drops = list_drops(&conn, 1).unwrap();
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].id, second);
    }

    #[test]
    fn counts_total_and_by_reason() {
        let conn = db();
        record_drop(&conn, "slack", None, DropReason::UnknownSender, None).unwrap();
        record_drop(&conn, "slack", None, DropReason::UnknownSender, None).unwrap();
        record_drop(&conn, "cli", None, DropReason::NotAuthorized, None).unwrap();

        assert_eq!(count_drops(&conn).unwrap(), 3);
        assert_eq!(
            count_drops_by_reason(&conn, DropReason::UnknownSender).unwrap(),
            2
        );
        assert_eq!(
            count_drops_by_reason(&conn, DropReason::NotAuthorized).unwrap(),
            1
        );
        assert_eq!(count_drops_by_reason(&conn, DropReason::NoRoute).unwrap(), 0);
    }
}
