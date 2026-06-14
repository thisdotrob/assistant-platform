//! The memory-health projection over the baseline `agent_memory_health` table.
//!
//! assistant-memory owns that table at baseline v1 (`agent_group_id INTEGER PRIMARY
//! KEY, status, last_index_at, detail`). The reindex loop projects each run's
//! [`MemoryHealth`] here so the host, CLI, and web surfaces can read current
//! memory health without touching qmd. `last_index_at` only advances on a
//! healthy reindex; a degraded run records the reason but leaves the last good
//! index time intact, so operators can see how stale the index has gone.

use rusqlite::{Connection, OptionalExtension};

use crate::catalog::MemoryDbError;
use crate::qmd::MemoryHealth;

/// A row of `agent_memory_health`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryHealthRecord {
    pub agent_group_id: i64,
    pub status: String,
    pub last_index_at: Option<String>,
    pub detail: Option<String>,
}

impl MemoryHealthRecord {
    pub fn is_healthy(&self) -> bool {
        self.status == "healthy"
    }
}

/// Project the outcome of a reindex into `agent_memory_health`. Idempotent per
/// agent group. A healthy run stamps `last_index_at`; a degraded run records the
/// reason and preserves the prior `last_index_at`.
pub fn project_health(
    conn: &Connection,
    agent_group_id: i64,
    health: &MemoryHealth,
) -> Result<(), MemoryDbError> {
    match health {
        MemoryHealth::Healthy { .. } => {
            conn.execute(
                "INSERT INTO agent_memory_health (agent_group_id, status, last_index_at, detail)
                 VALUES (?1, 'healthy', datetime('now'), NULL)
                 ON CONFLICT(agent_group_id) DO UPDATE SET
                     status = 'healthy',
                     last_index_at = datetime('now'),
                     detail = NULL",
                rusqlite::params![agent_group_id],
            )?;
        }
        MemoryHealth::Degraded { reason } => {
            conn.execute(
                "INSERT INTO agent_memory_health (agent_group_id, status, last_index_at, detail)
                 VALUES (?1, 'degraded', NULL, ?2)
                 ON CONFLICT(agent_group_id) DO UPDATE SET
                     status = 'degraded',
                     detail = excluded.detail",
                rusqlite::params![agent_group_id, reason.as_str()],
            )?;
        }
    }
    Ok(())
}

/// Read the current health row for an agent group, if any.
pub fn read_health(
    conn: &Connection,
    agent_group_id: i64,
) -> Result<Option<MemoryHealthRecord>, MemoryDbError> {
    let record = conn
        .query_row(
            "SELECT agent_group_id, status, last_index_at, detail
             FROM agent_memory_health WHERE agent_group_id = ?1",
            rusqlite::params![agent_group_id],
            |row| {
                Ok(MemoryHealthRecord {
                    agent_group_id: row.get(0)?,
                    status: row.get(1)?,
                    last_index_at: row.get(2)?,
                    detail: row.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmd::DegradedReason;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .iter()
            .map(|s| s.to_string())
            .collect();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        conn
    }

    #[test]
    fn healthy_then_degraded_preserves_last_index_at() {
        let conn = db();
        project_health(&conn, 1, &MemoryHealth::Healthy { indexed: 5 }).unwrap();
        let healthy = read_health(&conn, 1).unwrap().unwrap();
        assert_eq!(healthy.status, "healthy");
        assert!(healthy.last_index_at.is_some());
        assert!(healthy.detail.is_none());
        let stamped = healthy.last_index_at.clone();

        // A later degraded run records the reason but keeps the last good index.
        project_health(
            &conn,
            1,
            &MemoryHealth::Degraded {
                reason: DegradedReason::CorruptFts,
            },
        )
        .unwrap();
        let degraded = read_health(&conn, 1).unwrap().unwrap();
        assert_eq!(degraded.status, "degraded");
        assert_eq!(degraded.detail.as_deref(), Some("corrupt_fts"));
        assert_eq!(degraded.last_index_at, stamped, "stale time is preserved");
    }

    #[test]
    fn recovering_clears_detail() {
        let conn = db();
        project_health(
            &conn,
            2,
            &MemoryHealth::Degraded {
                reason: DegradedReason::EmptyIndex,
            },
        )
        .unwrap();
        assert_eq!(
            read_health(&conn, 2).unwrap().unwrap().detail.as_deref(),
            Some("empty_index")
        );
        project_health(&conn, 2, &MemoryHealth::Healthy { indexed: 1 }).unwrap();
        let rec = read_health(&conn, 2).unwrap().unwrap();
        assert!(rec.is_healthy());
        assert!(rec.detail.is_none());
    }

    #[test]
    fn missing_agent_reads_none() {
        let conn = db();
        assert!(read_health(&conn, 99).unwrap().is_none());
    }
}
