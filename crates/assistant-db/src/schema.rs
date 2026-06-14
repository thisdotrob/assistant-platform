//! Baseline (Milestone 1) central-DB schema.
//!
//! Each table is attributed to the module that owns it via a per-module `v1`
//! migration, so the migration registry keeps ownership namespaced. As modules
//! grow their own migration sets these definitions move into the owning crates;
//! for the skeleton they are assembled here.

use crate::migrate::{Migration, MigrationSet};

const PERMISSIONS_V1: &str = "
CREATE TABLE users (
    id          INTEGER PRIMARY KEY,
    handle      TEXT NOT NULL UNIQUE,
    display_name TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE user_roles (
    user_id INTEGER NOT NULL,
    role    TEXT NOT NULL,
    PRIMARY KEY (user_id, role)
);
CREATE TABLE user_dms (
    user_id INTEGER NOT NULL,
    channel TEXT NOT NULL,
    address TEXT NOT NULL,
    PRIMARY KEY (user_id, channel)
);
";

const AGENT_GRAPH_V1: &str = "
CREATE TABLE agent_groups (
    id              INTEGER PRIMARY KEY,
    slug            TEXT NOT NULL UNIQUE,
    kind            TEXT NOT NULL,
    profile_id      TEXT NOT NULL,
    profile_version TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE agent_group_config (
    agent_group_id INTEGER NOT NULL,
    key            TEXT NOT NULL,
    value          TEXT NOT NULL,
    PRIMARY KEY (agent_group_id, key)
);
CREATE TABLE messaging_groups (
    id         INTEGER PRIMARY KEY,
    slug       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE messaging_group_agents (
    messaging_group_id INTEGER NOT NULL,
    agent_group_id     INTEGER NOT NULL,
    PRIMARY KEY (messaging_group_id, agent_group_id)
);
CREATE TABLE agent_group_members (
    agent_group_id INTEGER NOT NULL,
    user_id        INTEGER NOT NULL,
    role           TEXT NOT NULL,
    PRIMARY KEY (agent_group_id, user_id)
);
CREATE TABLE agent_destinations (
    id             INTEGER PRIMARY KEY,
    agent_group_id INTEGER NOT NULL,
    channel        TEXT NOT NULL,
    address        TEXT NOT NULL,
    acl            TEXT NOT NULL DEFAULT 'strict'
);
";

const SESSION_V1: &str = "
CREATE TABLE sessions (
    id             TEXT PRIMARY KEY,
    agent_group_id INTEGER NOT NULL,
    status         TEXT NOT NULL DEFAULT 'idle',
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
);
";

const APPROVALS_V1: &str = "
CREATE TABLE pending_questions (
    id         INTEGER PRIMARY KEY,
    session_id TEXT,
    prompt     TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE pending_approvals (
    id           INTEGER PRIMARY KEY,
    kind         TEXT NOT NULL,
    subject      TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'pending',
    requested_by INTEGER,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at   TEXT
);
";

const ROUTER_V1: &str = "
CREATE TABLE dropped_messages (
    id         INTEGER PRIMARY KEY,
    channel    TEXT NOT NULL,
    sender     TEXT,
    reason     TEXT NOT NULL,
    payload    TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
";

const SCHEDULER_V1: &str = "
CREATE TABLE scheduled_items (
    id             TEXT PRIMARY KEY,
    agent_group_id INTEGER NOT NULL,
    session_id     TEXT,
    intent         TEXT NOT NULL,
    process_after  TEXT,
    recurrence     TEXT,
    status         TEXT NOT NULL DEFAULT 'active',
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE scheduled_occurrences (
    scheduled_item_id TEXT NOT NULL,
    sequence          INTEGER NOT NULL,
    idempotency_key   TEXT NOT NULL,
    fired_at          TEXT,
    status            TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY (scheduled_item_id, sequence)
);
";

const RUNTIME_DOCKER_V1: &str = "
CREATE TABLE container_runs (
    id                   TEXT PRIMARY KEY,
    session_id           TEXT,
    agent_group_id       INTEGER,
    status               TEXT NOT NULL DEFAULT 'created',
    container_id         TEXT,
    started_at           TEXT,
    stopped_at           TEXT,
    scheduled_message_id TEXT
);
";

const MEMORY_V1: &str = "
CREATE TABLE agent_memory_health (
    agent_group_id INTEGER PRIMARY KEY,
    status         TEXT NOT NULL DEFAULT 'unknown',
    last_index_at  TEXT,
    detail         TEXT
);
";

const CAPABILITIES_V1: &str = "
CREATE TABLE capability_metadata (
    capability_id TEXT PRIMARY KEY,
    module_id     TEXT NOT NULL,
    version       TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 0,
    detail        TEXT
);
";

/// Owner module ID -> baseline SQL. Each becomes that module's `v1` migration.
const BASELINE: &[(&str, &str, &str)] = &[
    ("assistant-permissions", "users_roles_dms", PERMISSIONS_V1),
    ("assistant-agent-graph", "agent_graph_core", AGENT_GRAPH_V1),
    ("assistant-session", "sessions", SESSION_V1),
    ("assistant-approvals", "questions_approvals", APPROVALS_V1),
    ("assistant-router", "dropped_messages", ROUTER_V1),
    ("assistant-scheduler", "scheduled_projection", SCHEDULER_V1),
    ("assistant-runtime-docker", "container_runs", RUNTIME_DOCKER_V1),
    ("assistant-memory", "memory_health", MEMORY_V1),
    ("assistant-capabilities", "capability_metadata", CAPABILITIES_V1),
];

/// Build the baseline migration set against the given module dependency order.
/// The order must contain every owner module listed in [`BASELINE`].
pub fn baseline_migrations(module_order: Vec<String>) -> MigrationSet {
    let mut set = MigrationSet::new(module_order);
    for (module_id, name, sql) in BASELINE {
        set.add(Migration::new(*module_id, 1, *name, *sql));
    }
    set
}

/// The owner modules referenced by the baseline, for callers assembling a
/// module order.
pub fn baseline_owner_modules() -> Vec<&'static str> {
    BASELINE.iter().map(|(m, _, _)| *m).collect()
}

/// Every table the baseline creates, for verification.
pub const BASELINE_TABLES: &[&str] = &[
    "users",
    "user_roles",
    "user_dms",
    "agent_groups",
    "agent_group_config",
    "messaging_groups",
    "messaging_group_agents",
    "agent_group_members",
    "agent_destinations",
    "sessions",
    "pending_questions",
    "pending_approvals",
    "dropped_messages",
    "scheduled_items",
    "scheduled_occurrences",
    "container_runs",
    "agent_memory_health",
    "capability_metadata",
];
