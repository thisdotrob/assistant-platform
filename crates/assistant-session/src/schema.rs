//! Baseline session DB schemas.
//!
//! `inbound.db` is host-owned (host enqueues, container reads); `outbound.db` is
//! runner-protocol-owned (container writes, host reads, host migrates only
//! before a container starts or under the recovery exception). Both keep
//! `journal_mode=DELETE` so host/container mount visibility is predictable.
//!
//! A v2 migration adds a benign nullable column to each message table. It exists
//! so older v1 fixtures can be exercised through the lazy migration path; adding
//! a column never breaks the container's read-only access.

use crate::layout::DbKind;
use crate::migrate::SessionMigration;

/// Highest inbound schema version the current build ships.
pub const CURRENT_INBOUND_VERSION: u32 = 3;
/// Highest outbound schema version the current build ships.
pub const CURRENT_OUTBOUND_VERSION: u32 = 2;

const INBOUND_V1: &str = "\
CREATE TABLE messages_in (
    seq        INTEGER PRIMARY KEY,
    sender     TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    metadata   TEXT,
    created_at TEXT    NOT NULL
);
CREATE TABLE delivered (
    seq          INTEGER PRIMARY KEY,
    delivered_at TEXT    NOT NULL
);
CREATE TABLE destinations (
    destination_id TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    display_name   TEXT,
    updated_at     TEXT NOT NULL
);
CREATE TABLE session_routing (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";

const INBOUND_V2: &str = "ALTER TABLE messages_in ADD COLUMN edited_at TEXT;";

// An optional caller-supplied idempotency key for an enqueue. A scheduler retry
// re-runs the same occurrence's turn; keying the inbound row on the occurrence's
// stable idempotency key lets the enqueue reuse the existing row instead of
// accumulating duplicates that a later container would all reply to. SQLite
// treats NULLs as distinct, so ordinary (keyless) inbound rows are unconstrained.
const INBOUND_V3: &str = "\
ALTER TABLE messages_in ADD COLUMN idempotency_key TEXT;
CREATE UNIQUE INDEX idx_messages_in_idempotency_key ON messages_in (idempotency_key);";

const OUTBOUND_V1: &str = "\
CREATE TABLE messages_out (
    seq        INTEGER PRIMARY KEY,
    kind       TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    metadata   TEXT,
    created_at TEXT    NOT NULL
);
CREATE TABLE processing_ack (
    in_seq     INTEGER PRIMARY KEY,
    claimed_by TEXT    NOT NULL,
    claimed_at TEXT    NOT NULL
);
CREATE TABLE session_state (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE container_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    status     TEXT    NOT NULL,
    run_id     TEXT,
    updated_at TEXT    NOT NULL
);";

const OUTBOUND_V2: &str = "ALTER TABLE messages_out ADD COLUMN edited_at TEXT;";

pub fn inbound_migrations() -> Vec<SessionMigration> {
    vec![
        SessionMigration {
            db_kind: DbKind::Inbound,
            version: 1,
            name: "inbound_base",
            sql: INBOUND_V1,
        },
        SessionMigration {
            db_kind: DbKind::Inbound,
            version: 2,
            name: "inbound_message_edited_at",
            sql: INBOUND_V2,
        },
        SessionMigration {
            db_kind: DbKind::Inbound,
            version: 3,
            name: "inbound_message_idempotency_key",
            sql: INBOUND_V3,
        },
    ]
}

pub fn outbound_migrations() -> Vec<SessionMigration> {
    vec![
        SessionMigration {
            db_kind: DbKind::Outbound,
            version: 1,
            name: "outbound_base",
            sql: OUTBOUND_V1,
        },
        SessionMigration {
            db_kind: DbKind::Outbound,
            version: 2,
            name: "outbound_message_edited_at",
            sql: OUTBOUND_V2,
        },
    ]
}

pub fn migrations_for(kind: DbKind) -> Vec<SessionMigration> {
    match kind {
        DbKind::Inbound => inbound_migrations(),
        DbKind::Outbound => outbound_migrations(),
    }
}

/// Only the v1 prefix — used by tests to fabricate an old, un-migrated fixture.
pub fn migrations_v1_only(kind: DbKind) -> Vec<SessionMigration> {
    let mut migs = migrations_for(kind);
    migs.truncate(1);
    migs
}
