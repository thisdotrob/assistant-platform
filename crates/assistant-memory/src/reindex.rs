//! The reindex loop: rebuild the catalog and qmd index from the markdown source.
//!
//! Markdown is the source of truth. A reindex walks an agent's memory root,
//! normalizes every `.md` file (untrusted hand-authored files fall back to the
//! restrictive `manual`/`no_rag` defaults), upserts a catalog row per entry,
//! hands the bodies to the backend, and projects the resulting health. It is
//! idempotent: because catalog rows are keyed by the deterministic `memory_id`
//! and the backend rebuilds from the full corpus, running it twice over an
//! unchanged tree leaves the same catalog and the same health. Every file path
//! is confined to the agent's own root before it is read, so a stray symlink or
//! `..` cannot pull another agent's file into this agent's index.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::catalog::{upsert_entry, MemoryDbError};
use crate::entry::MemoryDocument;
use crate::health::project_health;
use crate::qmd::{IndexDoc, MemoryBackend, MemoryHealth};
use crate::root::{IsolationError, MemoryRoot};

#[derive(Debug)]
pub enum ReindexError {
    Io(std::io::Error),
    Db(MemoryDbError),
    Isolation(IsolationError),
}

impl std::fmt::Display for ReindexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReindexError::Io(e) => write!(f, "reindex io error: {e}"),
            ReindexError::Db(e) => write!(f, "reindex catalog error: {e}"),
            ReindexError::Isolation(e) => write!(f, "reindex isolation error: {e}"),
        }
    }
}

impl std::error::Error for ReindexError {}

impl From<std::io::Error> for ReindexError {
    fn from(value: std::io::Error) -> Self {
        ReindexError::Io(value)
    }
}
impl From<MemoryDbError> for ReindexError {
    fn from(value: MemoryDbError) -> Self {
        ReindexError::Db(value)
    }
}
impl From<IsolationError> for ReindexError {
    fn from(value: IsolationError) -> Self {
        ReindexError::Isolation(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReindexReport {
    pub indexed: usize,
    pub health: MemoryHealth,
}

/// Reindex one agent's memory root. The integer `agent_group_id` is the owning
/// agent's `agent_groups.id`; the root's logical `owner_agent_group_id` is
/// stamped into normalized files. Returns the count indexed and the projected
/// health. Health is also written to `agent_memory_health`.
pub fn reindex_root(
    conn: &Connection,
    root: &MemoryRoot,
    agent_group_id: i64,
    backend: &mut dyn MemoryBackend,
) -> Result<ReindexReport, ReindexError> {
    let mut files = Vec::new();
    if root.path().exists() {
        collect_markdown(root.path(), &mut files)?;
    }
    files.sort();

    let mut docs: Vec<IndexDoc> = Vec::with_capacity(files.len());
    for abs in &files {
        // Defense in depth: never read a path that is not lexically inside this
        // agent's own root.
        root.confine(abs)?;
        let rel = rel_path(root.path(), abs);
        let raw = fs::read_to_string(abs)?;
        let doc = MemoryDocument::normalize(&raw, root.owner_agent_group_id());
        upsert_entry(conn, agent_group_id, &rel, &doc.front_matter)?;
        docs.push(IndexDoc {
            memory_id: doc.front_matter.memory_id,
            rel_path: rel,
            body: doc.body,
        });
    }

    let health = backend.reindex(&docs);
    project_health(conn, agent_group_id, &health)?;
    Ok(ReindexReport {
        indexed: docs.len(),
        health,
    })
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // Skip the qmd collection/index directory, which is rebuildable
            // derived state, not a markdown source.
            if entry.file_name() == "qmd" || entry.file_name() == ".qmd" {
                continue;
            }
            collect_markdown(&path, out)?;
        } else if file_type.is_file()
            && path.extension().map(|e| e == "md").unwrap_or(false)
        {
            // The taxonomy's reserved index is a navigable map of the category
            // layout, not a memory note: it carries no front matter, so ingesting
            // it would mint a spurious manual entry. Skip it like `qmd/`.
            if entry.file_name() == crate::taxonomy::INDEX_FILE {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

fn rel_path(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{entries_for_agent, migrations};
    use crate::health::read_health;
    use crate::qmd::FakeQmd;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules, MigrationSet};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .iter()
            .map(|s| s.to_string())
            .collect();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        // Add assistant-memory's own v2 catalog migration.
        let mut set = MigrationSet::new(vec![crate::MODULE_ID.to_string()]);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn reindex_walks_normalizes_and_projects_health() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let root = MemoryRoot::orchestrator(&groups, "ag_orchestrator");
        // A file with proper front matter, and a hand-authored file without it.
        write(
            &root.path().join("people/alice.md"),
            "---\nmemory_id: mem_alice\nowner_agent_group_id: ag_orchestrator\nscope: all_chats\nsource_type: user_said\nconfidence: high\nreuse_policy: same_scope\nretention: normal\n---\nAlice prefers mornings.\n",
        );
        write(
            &root.path().join("journal/2026-06-01.md"),
            "loose human notes\n",
        );

        let conn = db();
        let mut backend = FakeQmd::new();
        let report = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        assert_eq!(report.indexed, 2);
        assert_eq!(report.health, MemoryHealth::Healthy { indexed: 2 });

        let rows = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(rows.len(), 2);
        // The hand-authored file became a restrictive manual/no_rag entry.
        let manual = rows
            .iter()
            .find(|r| r.rel_path == "journal/2026-06-01.md")
            .unwrap();
        assert_eq!(manual.scope, crate::entry::Scope::NoRag);
        assert_eq!(manual.source_type, crate::entry::SourceType::Manual);

        // Health was projected to the central DB.
        let h = read_health(&conn, 1).unwrap().unwrap();
        assert!(h.is_healthy());
    }

    #[test]
    fn reindex_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let root = MemoryRoot::orchestrator(&groups, "ag_o");
        write(&root.path().join("a.md"), "first note\n");
        write(&root.path().join("b.md"), "second note\n");

        let conn = db();
        let mut backend = FakeQmd::new();
        let first = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        let count_after_first = entries_for_agent(&conn, 1).unwrap().len();
        let second = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        let count_after_second = entries_for_agent(&conn, 1).unwrap().len();

        assert_eq!(first.indexed, 2);
        assert_eq!(second.indexed, 2);
        assert_eq!(count_after_first, 2);
        assert_eq!(
            count_after_second, count_after_first,
            "rerun does not duplicate rows"
        );
    }

    #[test]
    fn empty_root_reports_degraded_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let root = MemoryRoot::orchestrator(&groups, "ag_o");
        let conn = db();
        let mut backend = FakeQmd::new();
        let report = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        assert_eq!(report.indexed, 0);
        assert!(matches!(report.health, MemoryHealth::Degraded { .. }));
        let h = read_health(&conn, 1).unwrap().unwrap();
        assert_eq!(h.status, "degraded");
    }

    #[test]
    fn taxonomy_index_is_not_indexed_as_an_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let root = MemoryRoot::orchestrator(&groups, "ag_o");
        write(&root.path().join("real.md"), "real note\n");
        // The reserved taxonomy index (front-matter-less map) must not become a
        // spurious manual entry.
        write(
            &root.path().join(crate::taxonomy::INDEX_FILE),
            "# Memory taxonomy\n\n- people\n- journal\n",
        );
        let conn = db();
        let mut backend = FakeQmd::new();
        let report = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        assert_eq!(report.indexed, 1);
        let rows = entries_for_agent(&conn, 1).unwrap();
        assert!(rows.iter().all(|r| r.rel_path != crate::taxonomy::INDEX_FILE));
    }

    #[test]
    fn qmd_collection_dir_is_not_indexed_as_source() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let root = MemoryRoot::orchestrator(&groups, "ag_o");
        write(&root.path().join("real.md"), "real note\n");
        // A stray .md under the qmd collection dir must be ignored.
        write(&root.path().join("qmd/index.md"), "derived junk\n");
        let conn = db();
        let mut backend = FakeQmd::new();
        let report = reindex_root(&conn, &root, 1, &mut backend).unwrap();
        assert_eq!(report.indexed, 1);
    }
}
