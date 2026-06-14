//! The derived metadata catalog: a central-DB projection of the front matter
//! that backs each markdown entry, so host-side RAG can filter candidates by
//! agent group and scope without re-reading every file's body.
//!
//! Markdown is the source of truth; this table is rebuildable from it (the
//! reindex loop upserts a row per entry). claw-memory already owns the baseline
//! `agent_memory_health` table at version 1, so the catalog's own migration
//! starts at version 2. Rows are keyed by the INTEGER `agent_group_id`
//! (`agent_groups.id`), matching every other central-DB table; the string
//! `owner_agent_group_id` stays the on-disk logical ID in front matter. The
//! owning agent group is the hard isolation boundary: every query is scoped to
//! a single `agent_group_id`, so a candidate row for one agent can never be
//! returned to another.

use claw_db::Migration;
use rusqlite::Connection;

use crate::entry::{
    Confidence, MemoryFrontMatter, Retention, ReusePolicy, Scope, SourceChannel, SourceRef,
    SourceType,
};

const MEMORY_ENTRIES_V2: &str = "
CREATE TABLE memory_entries (
    memory_id         TEXT PRIMARY KEY,
    agent_group_id    INTEGER NOT NULL,
    rel_path          TEXT NOT NULL,
    scope             TEXT NOT NULL,
    source_type       TEXT NOT NULL,
    source_channel    TEXT,
    source_chat_id    TEXT,
    source_thread_id  TEXT,
    source_message_id TEXT,
    source_permalink  TEXT,
    source_user_id    TEXT,
    captured_at       TEXT,
    confidence        TEXT NOT NULL,
    reuse_policy      TEXT NOT NULL,
    retention         TEXT NOT NULL,
    indexed_at        TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_memory_entries_agent_scope ON memory_entries (agent_group_id, scope);
";

const SELECT_COLUMNS: &str = "memory_id, agent_group_id, rel_path, scope, source_type, \
     source_channel, source_chat_id, source_thread_id, source_message_id, source_permalink, \
     source_user_id, captured_at, confidence, reuse_policy, retention, indexed_at";

/// claw-memory's central-DB migrations beyond the baseline `memory_health` (v1).
/// The catalog projection is v2.
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        crate::MODULE_ID,
        2,
        "memory_entries_catalog",
        MEMORY_ENTRIES_V2,
    )]
}

#[derive(Debug)]
pub enum MemoryDbError {
    Sqlite(rusqlite::Error),
    /// A stored TEXT value did not parse back into its enum — a corrupt row that
    /// must not be silently coerced into a default that could widen reuse.
    UnknownEnum { column: &'static str, value: String },
}

impl std::fmt::Display for MemoryDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryDbError::Sqlite(e) => write!(f, "memory catalog sqlite error: {e}"),
            MemoryDbError::UnknownEnum { column, value } => {
                write!(f, "memory catalog column {column} has unparseable value {value:?}")
            }
        }
    }
}

impl std::error::Error for MemoryDbError {}

impl From<rusqlite::Error> for MemoryDbError {
    fn from(value: rusqlite::Error) -> Self {
        MemoryDbError::Sqlite(value)
    }
}

/// One catalog row: the projected metadata for a single markdown entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub memory_id: String,
    pub agent_group_id: i64,
    pub rel_path: String,
    pub scope: Scope,
    pub source_type: SourceType,
    pub source_ref: Option<SourceRef>,
    pub source_user_id: Option<String>,
    pub captured_at: Option<String>,
    pub confidence: Confidence,
    pub reuse_policy: ReusePolicy,
    pub retention: Retention,
    pub indexed_at: String,
}

/// Project one entry's front matter into the catalog. The integer
/// `agent_group_id` is the owning agent's `agent_groups.id`; the caller maps the
/// front matter's string `owner_agent_group_id` to it. Idempotent on
/// `memory_id`, so re-running the reindex over the same file updates in place.
pub fn upsert_entry(
    conn: &Connection,
    agent_group_id: i64,
    rel_path: &str,
    front_matter: &MemoryFrontMatter,
) -> Result<(), MemoryDbError> {
    let (channel, chat_id, thread_id, message_id, permalink) = match &front_matter.source_ref {
        Some(s) => (
            s.channel.map(SourceChannel::as_str),
            s.chat_id.as_deref(),
            s.thread_id.as_deref(),
            s.message_id.as_deref(),
            s.permalink.as_deref(),
        ),
        None => (None, None, None, None, None),
    };
    conn.execute(
        "INSERT INTO memory_entries
             (memory_id, agent_group_id, rel_path, scope, source_type,
              source_channel, source_chat_id, source_thread_id, source_message_id,
              source_permalink, source_user_id, captured_at, confidence,
              reuse_policy, retention, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, datetime('now'))
         ON CONFLICT(memory_id) DO UPDATE SET
             agent_group_id    = excluded.agent_group_id,
             rel_path          = excluded.rel_path,
             scope             = excluded.scope,
             source_type       = excluded.source_type,
             source_channel    = excluded.source_channel,
             source_chat_id    = excluded.source_chat_id,
             source_thread_id  = excluded.source_thread_id,
             source_message_id = excluded.source_message_id,
             source_permalink  = excluded.source_permalink,
             source_user_id    = excluded.source_user_id,
             captured_at       = excluded.captured_at,
             confidence        = excluded.confidence,
             reuse_policy      = excluded.reuse_policy,
             retention         = excluded.retention,
             indexed_at        = excluded.indexed_at",
        rusqlite::params![
            front_matter.memory_id,
            agent_group_id,
            rel_path,
            front_matter.scope.as_str(),
            front_matter.source_type.as_str(),
            channel,
            chat_id,
            thread_id,
            message_id,
            permalink,
            front_matter.source_user_id,
            front_matter.captured_at,
            front_matter.confidence.as_str(),
            front_matter.reuse_policy.as_str(),
            front_matter.retention.as_str(),
        ],
    )?;
    Ok(())
}

/// Every catalog row for one agent group, newest first. This is the hard
/// isolation boundary: rows for other agents are never returned.
pub fn entries_for_agent(
    conn: &Connection,
    agent_group_id: i64,
) -> Result<Vec<CatalogEntry>, MemoryDbError> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM memory_entries \
         WHERE agent_group_id = ?1 \
         ORDER BY indexed_at DESC, memory_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params![agent_group_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_entry(row)?);
    }
    Ok(out)
}

/// Catalog rows for one agent group narrowed to the given scopes, newest first.
/// Scope filtering only narrows reuse *within* the agent boundary; it never
/// widens it. An empty scope list selects nothing.
pub fn candidates(
    conn: &Connection,
    agent_group_id: i64,
    scopes: &[Scope],
) -> Result<Vec<CatalogEntry>, MemoryDbError> {
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; scopes.len()].join(", ");
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM memory_entries \
         WHERE agent_group_id = ? AND scope IN ({placeholders}) \
         ORDER BY indexed_at DESC, memory_id ASC"
    );
    let scope_strs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + scope_strs.len());
    params.push(&agent_group_id);
    for s in &scope_strs {
        params.push(s);
    }
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_entry(row)?);
    }
    Ok(out)
}

fn parse_enum<T>(parsed: Option<T>, column: &'static str, raw: String) -> Result<T, MemoryDbError> {
    parsed.ok_or(()).map_err(|()| MemoryDbError::UnknownEnum { column, value: raw })
}

fn row_to_entry(row: &rusqlite::Row) -> Result<CatalogEntry, MemoryDbError> {
    let scope_s: String = row.get(3)?;
    let source_type_s: String = row.get(4)?;
    let channel_s: Option<String> = row.get(5)?;
    let chat_id: Option<String> = row.get(6)?;
    let thread_id: Option<String> = row.get(7)?;
    let message_id: Option<String> = row.get(8)?;
    let permalink: Option<String> = row.get(9)?;
    let confidence_s: String = row.get(12)?;
    let reuse_s: String = row.get(13)?;
    let retention_s: String = row.get(14)?;

    let channel = match channel_s {
        Some(c) => Some(parse_enum(SourceChannel::parse(&c), "source_channel", c)?),
        None => None,
    };
    let source_ref = if channel.is_some()
        || chat_id.is_some()
        || thread_id.is_some()
        || message_id.is_some()
        || permalink.is_some()
    {
        Some(SourceRef {
            channel,
            chat_id,
            thread_id,
            message_id,
            permalink,
        })
    } else {
        None
    };

    Ok(CatalogEntry {
        memory_id: row.get(0)?,
        agent_group_id: row.get(1)?,
        rel_path: row.get(2)?,
        scope: parse_enum(Scope::parse(&scope_s), "scope", scope_s)?,
        source_type: parse_enum(SourceType::parse(&source_type_s), "source_type", source_type_s)?,
        source_ref,
        source_user_id: row.get(10)?,
        captured_at: row.get(11)?,
        confidence: parse_enum(Confidence::parse(&confidence_s), "confidence", confidence_s)?,
        reuse_policy: parse_enum(ReusePolicy::parse(&reuse_s), "reuse_policy", reuse_s)?,
        retention: parse_enum(Retention::parse(&retention_s), "retention", retention_s)?,
        indexed_at: row.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_db::{apply, MigrationSet};

    fn db_with_catalog() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let mut set = MigrationSet::new(vec![crate::MODULE_ID.to_string()]);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn fm(memory_id: &str, owner: &str, scope: Scope) -> MemoryFrontMatter {
        MemoryFrontMatter {
            memory_id: memory_id.to_string(),
            owner_agent_group_id: owner.to_string(),
            scope,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: None,
            captured_at: Some("2026-06-01T10:00:00Z".to_string()),
            confidence: Confidence::High,
            reuse_policy: ReusePolicy::SameScope,
            retention: Retention::Normal,
        }
    }

    #[test]
    fn migration_creates_catalog_table_and_index() {
        let conn = db_with_catalog();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_entries'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        let idx: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_memory_entries_agent_scope'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn catalog_migration_starts_at_version_two() {
        // Baseline owns v1 `memory_health`; the catalog must not collide with it.
        let only = migrations();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].module_id, crate::MODULE_ID);
        assert_eq!(only[0].version, 2);
    }

    #[test]
    fn upsert_then_query_round_trips_source_ref_and_enums() {
        let conn = db_with_catalog();
        let mut entry = fm("mem_1", "ag_orchestrator", Scope::Channel);
        entry.source_ref = Some(SourceRef {
            channel: Some(SourceChannel::Slack),
            chat_id: Some("C1".to_string()),
            thread_id: Some("T2".to_string()),
            message_id: Some("M3".to_string()),
            permalink: Some("https://slack/x".to_string()),
        });
        entry.source_user_id = Some("U9".to_string());
        upsert_entry(&conn, 1, "people/alice.md", &entry).unwrap();

        let rows = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(rows.len(), 1);
        let got = &rows[0];
        assert_eq!(got.memory_id, "mem_1");
        assert_eq!(got.agent_group_id, 1);
        assert_eq!(got.rel_path, "people/alice.md");
        assert_eq!(got.scope, Scope::Channel);
        assert_eq!(got.source_type, SourceType::UserSaid);
        assert_eq!(got.source_user_id.as_deref(), Some("U9"));
        assert_eq!(got.confidence, Confidence::High);
        assert_eq!(got.reuse_policy, ReusePolicy::SameScope);
        assert_eq!(got.retention, Retention::Normal);
        let sr = got.source_ref.as_ref().expect("source_ref preserved");
        assert_eq!(sr.channel, Some(SourceChannel::Slack));
        assert_eq!(sr.chat_id.as_deref(), Some("C1"));
        assert_eq!(sr.permalink.as_deref(), Some("https://slack/x"));
    }

    #[test]
    fn entry_without_source_ref_reads_back_as_none() {
        let conn = db_with_catalog();
        upsert_entry(&conn, 7, "notes.md", &fm("mem_n", "ag_x", Scope::AllChats)).unwrap();
        let rows = entries_for_agent(&conn, 7).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].source_ref.is_none());
    }

    #[test]
    fn upsert_is_idempotent_and_updates_in_place() {
        let conn = db_with_catalog();
        upsert_entry(&conn, 1, "a.md", &fm("mem_1", "ag_o", Scope::Thread)).unwrap();
        // Re-upsert the same memory_id with a changed scope and path.
        let mut changed = fm("mem_1", "ag_o", Scope::AllChats);
        changed.reuse_policy = ReusePolicy::BroaderOk;
        upsert_entry(&conn, 1, "moved.md", &changed).unwrap();

        let rows = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(rows.len(), 1, "same memory_id stays one row");
        assert_eq!(rows[0].scope, Scope::AllChats);
        assert_eq!(rows[0].rel_path, "moved.md");
        assert_eq!(rows[0].reuse_policy, ReusePolicy::BroaderOk);
    }

    #[test]
    fn queries_are_isolated_per_agent_group() {
        let conn = db_with_catalog();
        upsert_entry(&conn, 1, "a.md", &fm("mem_a", "ag_one", Scope::AllChats)).unwrap();
        upsert_entry(&conn, 2, "b.md", &fm("mem_b", "ag_two", Scope::AllChats)).unwrap();

        let one = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].memory_id, "mem_a");

        let two = entries_for_agent(&conn, 2).unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].memory_id, "mem_b");

        // A scope query for agent 1 never reaches agent 2's row.
        let c = candidates(&conn, 1, &[Scope::AllChats]).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].memory_id, "mem_a");
    }

    #[test]
    fn candidates_filter_by_scope_set() {
        let conn = db_with_catalog();
        upsert_entry(&conn, 1, "chan.md", &fm("mem_chan", "ag_o", Scope::Channel)).unwrap();
        upsert_entry(&conn, 1, "thr.md", &fm("mem_thr", "ag_o", Scope::Thread)).unwrap();
        upsert_entry(&conn, 1, "all.md", &fm("mem_all", "ag_o", Scope::AllChats)).unwrap();

        let mut got: Vec<String> = candidates(&conn, 1, &[Scope::Channel, Scope::AllChats])
            .unwrap()
            .into_iter()
            .map(|e| e.memory_id)
            .collect();
        got.sort();
        assert_eq!(got, vec!["mem_all".to_string(), "mem_chan".to_string()]);

        // Empty scope list selects nothing.
        assert!(candidates(&conn, 1, &[]).unwrap().is_empty());
    }
}
