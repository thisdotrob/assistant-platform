//! Persisted sticky-engagement state for `mention-sticky` mode.
//!
//! When an agent first engages on a mention/DM in `MentionSticky` mode, the
//! router opens a sticky row keyed by (agent group, conversation). While that
//! row is active and unexpired, follow-up messages in the same conversation
//! keep the agent engaged without a fresh mention. State is persisted (not
//! in-memory) so it survives restarts and is visible to the web/CLI, and it
//! carries an expiry and a reset reason so the agent never stays engaged
//! indefinitely.
//!
//! The crate never reads the wall clock — callers pass `now`. The `expires_at`
//! column is a router-owned INTEGER holding the epoch second directly.

use claw_db::Migration;
use rusqlite::{Connection, OptionalExtension};

use crate::model::RouterError;

pub type EpochSecs = i64;

const STICKY_ENGAGEMENT_V2: &str = "
CREATE TABLE sticky_engagement (
    agent_group_id  INTEGER NOT NULL,
    engagement_key  TEXT NOT NULL,
    root_message_id TEXT,
    scope           TEXT,
    expires_at      INTEGER,
    active          INTEGER NOT NULL DEFAULT 1,
    reset_reason    TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT,
    PRIMARY KEY (agent_group_id, engagement_key)
);
";

/// claw-router's central-DB migrations beyond the baseline `dropped_messages`
/// (v1). The sticky-engagement table is v2.
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        crate::MODULE_ID,
        2,
        "sticky_engagement",
        STICKY_ENGAGEMENT_V2,
    )]
}

/// A sticky-engagement record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickyEngagement {
    pub agent_group_id: i64,
    pub engagement_key: String,
    pub root_message_id: Option<String>,
    pub scope: Option<String>,
    pub expires_at: Option<EpochSecs>,
    pub active: bool,
    pub reset_reason: Option<String>,
}

/// Open (or refresh) a sticky session for a conversation. Re-opening an existing
/// row reactivates it, refreshes the root/scope/expiry, and clears any prior
/// reset reason.
pub fn open_sticky(
    conn: &Connection,
    agent_group_id: i64,
    engagement_key: &str,
    root_message_id: Option<&str>,
    scope: Option<&str>,
    expires_at: Option<EpochSecs>,
) -> Result<(), RouterError> {
    conn.execute(
        "INSERT INTO sticky_engagement
             (agent_group_id, engagement_key, root_message_id, scope, expires_at, active, reset_reason, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL, datetime('now'))
         ON CONFLICT(agent_group_id, engagement_key) DO UPDATE SET
             root_message_id = excluded.root_message_id,
             scope           = excluded.scope,
             expires_at      = excluded.expires_at,
             active          = 1,
             reset_reason    = NULL,
             updated_at      = datetime('now')",
        rusqlite::params![agent_group_id, engagement_key, root_message_id, scope, expires_at],
    )?;
    Ok(())
}

fn read_row(row: &rusqlite::Row<'_>) -> Result<StickyEngagement, rusqlite::Error> {
    Ok(StickyEngagement {
        agent_group_id: row.get(0)?,
        engagement_key: row.get(1)?,
        root_message_id: row.get(2)?,
        scope: row.get(3)?,
        expires_at: row.get(4)?,
        active: row.get::<_, i64>(5)? != 0,
        reset_reason: row.get(6)?,
    })
}

/// The active, unexpired sticky session for a conversation, if one exists. A row
/// with `expires_at <= now` is treated as not active (it should be swept).
pub fn lookup_active_sticky(
    conn: &Connection,
    agent_group_id: i64,
    engagement_key: &str,
    now: EpochSecs,
) -> Result<Option<StickyEngagement>, RouterError> {
    Ok(conn
        .query_row(
            "SELECT agent_group_id, engagement_key, root_message_id, scope, expires_at, active, reset_reason
             FROM sticky_engagement
             WHERE agent_group_id = ?1 AND engagement_key = ?2
               AND active = 1
               AND (expires_at IS NULL OR expires_at > ?3)",
            rusqlite::params![agent_group_id, engagement_key, now],
            read_row,
        )
        .optional()?)
}

/// Whether an active, unexpired sticky session exists — the engagement-context
/// flag for [`crate::engagement::evaluate_engagement`].
pub fn has_active_sticky(
    conn: &Connection,
    agent_group_id: i64,
    engagement_key: &str,
    now: EpochSecs,
) -> Result<bool, RouterError> {
    Ok(lookup_active_sticky(conn, agent_group_id, engagement_key, now)?.is_some())
}

/// Deactivate every active sticky session whose expiry is at or before `now`,
/// recording the reset reason as expiry. Returns how many were swept.
pub fn expire_sticky(conn: &Connection, now: EpochSecs) -> Result<usize, RouterError> {
    let swept = conn.execute(
        "UPDATE sticky_engagement
             SET active = 0, reset_reason = 'expired', updated_at = datetime('now')
         WHERE active = 1 AND expires_at IS NOT NULL AND expires_at <= ?1",
        rusqlite::params![now],
    )?;
    Ok(swept)
}

/// Explicitly reset/stop a sticky session (e.g. a reset/stop command), recording
/// the reason. Returns whether an active session was cleared.
pub fn reset_sticky(
    conn: &Connection,
    agent_group_id: i64,
    engagement_key: &str,
    reason: &str,
) -> Result<bool, RouterError> {
    let changed = conn.execute(
        "UPDATE sticky_engagement
             SET active = 0, reset_reason = ?3, updated_at = datetime('now')
         WHERE agent_group_id = ?1 AND engagement_key = ?2 AND active = 1",
        rusqlite::params![agent_group_id, engagement_key, reason],
    )?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut set = baseline_migrations(order);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    #[test]
    fn first_mention_persists_root_and_followups_stay_engaged() {
        let conn = db();
        open_sticky(&conn, 1, "slack:C1:root", Some("root"), Some("thread"), Some(100)).unwrap();

        // Within expiry, a follow-up sees the active sticky session.
        assert!(has_active_sticky(&conn, 1, "slack:C1:root", 50).unwrap());
        let s = lookup_active_sticky(&conn, 1, "slack:C1:root", 50).unwrap().unwrap();
        assert_eq!(s.root_message_id.as_deref(), Some("root"));
        assert_eq!(s.scope.as_deref(), Some("thread"));
        assert!(s.active);
    }

    #[test]
    fn sticky_is_scoped_to_agent_and_conversation() {
        let conn = db();
        open_sticky(&conn, 1, "slack:C1", None, None, None).unwrap();
        // A different agent group or conversation has no sticky session.
        assert!(!has_active_sticky(&conn, 2, "slack:C1", 0).unwrap());
        assert!(!has_active_sticky(&conn, 1, "slack:C2", 0).unwrap());
        // A no-expiry session stays active indefinitely.
        assert!(has_active_sticky(&conn, 1, "slack:C1", 999_999).unwrap());
    }

    #[test]
    fn expiry_releases_engagement() {
        let conn = db();
        open_sticky(&conn, 1, "slack:C1", None, None, Some(100)).unwrap();
        // At exactly the expiry it is no longer active to a lookup.
        assert!(!has_active_sticky(&conn, 1, "slack:C1", 100).unwrap());
        // The sweep deactivates it and records the reason.
        assert_eq!(expire_sticky(&conn, 100).unwrap(), 1);
        // A second sweep finds nothing more to do.
        assert_eq!(expire_sticky(&conn, 100).unwrap(), 0);
    }

    #[test]
    fn explicit_reset_clears_with_reason_and_reopen_reactivates() {
        let conn = db();
        open_sticky(&conn, 1, "slack:C1", None, None, None).unwrap();
        assert!(reset_sticky(&conn, 1, "slack:C1", "stop command").unwrap());
        assert!(!has_active_sticky(&conn, 1, "slack:C1", 0).unwrap());
        // Resetting an already-inactive session reports no change.
        assert!(!reset_sticky(&conn, 1, "slack:C1", "again").unwrap());

        // Re-opening reactivates and clears the prior reset reason.
        open_sticky(&conn, 1, "slack:C1", Some("r2"), None, None).unwrap();
        let s = lookup_active_sticky(&conn, 1, "slack:C1", 0).unwrap().unwrap();
        assert!(s.active);
        assert_eq!(s.reset_reason, None);
        assert_eq!(s.root_message_id.as_deref(), Some("r2"));
    }
}
