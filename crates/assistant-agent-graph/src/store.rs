//! Central-DB persistence for specialist jobs and specialist agent groups.
//!
//! The specialist-job tables are added on top of the Milestone 1 baseline via a
//! `assistant-agent-graph` v2 migration (the baseline owns v1 — `agent_groups` and
//! friends). Jobs persist their status machine, budget, timeout, cancellation
//! flag, and the container runs they spawned, so the host can recover and audit
//! delegation across restarts.

use rusqlite::{params, Connection, OptionalExtension};

use assistant_db::Migration;

use crate::job::{JobBudget, JobStatus, SpecialistJob};

/// Module ID this crate owns migrations under (matches the baseline owner).
pub const MODULE_ID: &str = "assistant-agent-graph";

/// The specialist-jobs schema version (v1 is the baseline `agent_groups` set).
pub const AGENT_GRAPH_JOBS_VERSION: u32 = 2;

const SPECIALIST_JOBS_SQL: &str = "
CREATE TABLE specialist_jobs (
    job_id             TEXT PRIMARY KEY,
    orchestrator_group TEXT NOT NULL,
    specialist_group   TEXT NOT NULL,
    profile_id         TEXT NOT NULL,
    status             TEXT NOT NULL,
    max_tokens         INTEGER,
    max_wall_secs      INTEGER,
    timeout_secs       INTEGER NOT NULL,
    cancel_requested   INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    started_at         TEXT,
    ended_at           TEXT
);
CREATE INDEX idx_specialist_jobs_profile_status
    ON specialist_jobs (profile_id, status);
CREATE TABLE specialist_job_runs (
    job_id     TEXT NOT NULL,
    run_link   TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (job_id, run_link)
);
";

/// The migrations this crate contributes on top of the baseline.
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        MODULE_ID,
        AGENT_GRAPH_JOBS_VERSION,
        "specialist_jobs",
        SPECIALIST_JOBS_SQL,
    )]
}

#[derive(Debug)]
pub enum StoreError {
    Db(rusqlite::Error),
    NotFound { job_id: String },
    UnknownStatus { value: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "store sqlite error: {e}"),
            StoreError::NotFound { job_id } => write!(f, "specialist job {job_id:?} not found"),
            StoreError::UnknownStatus { value } => {
                write!(f, "unknown specialist job status {value:?}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        StoreError::Db(value)
    }
}

/// Insert a freshly created job and any run links it already carries.
pub fn insert_job(conn: &Connection, job: &SpecialistJob) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO specialist_jobs
            (job_id, orchestrator_group, specialist_group, profile_id, status,
             max_tokens, max_wall_secs, timeout_secs, cancel_requested, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
        params![
            job.job_id,
            job.orchestrator_group,
            job.specialist_group,
            job.profile_id,
            job.status.as_str(),
            job.budget.max_tokens.map(|v| v as i64),
            job.budget.max_wall_secs.map(|v| v as i64),
            job.timeout_secs as i64,
            job.cancel_requested as i64,
        ],
    )?;
    for link in &job.run_links {
        add_run_link(conn, &job.job_id, link)?;
    }
    Ok(())
}

/// Load a job, reconstructing its budget and run links.
pub fn load_job(conn: &Connection, job_id: &str) -> Result<SpecialistJob, StoreError> {
    let row = conn
        .query_row(
            "SELECT orchestrator_group, specialist_group, profile_id, status,
                    max_tokens, max_wall_secs, timeout_secs, cancel_requested
             FROM specialist_jobs WHERE job_id = ?1",
            params![job_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            job_id: job_id.to_string(),
        })?;

    let status = JobStatus::parse(&row.3).ok_or_else(|| StoreError::UnknownStatus {
        value: row.3.clone(),
    })?;

    let mut stmt =
        conn.prepare("SELECT run_link FROM specialist_job_runs WHERE job_id = ?1 ORDER BY run_link")?;
    let links = stmt
        .query_map(params![job_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(SpecialistJob {
        job_id: job_id.to_string(),
        orchestrator_group: row.0,
        specialist_group: row.1,
        profile_id: row.2,
        status,
        budget: JobBudget {
            max_tokens: row.4.map(|v| v as u64),
            max_wall_secs: row.5.map(|v| v as u64),
        },
        timeout_secs: row.6 as u64,
        cancel_requested: row.7 != 0,
        run_links: links,
    })
}

/// Persist a status transition. Stamps `started_at` the first time a job enters
/// `Running`, and `ended_at` whenever it reaches a terminal state.
pub fn update_status(conn: &Connection, job_id: &str, status: JobStatus) -> Result<(), StoreError> {
    let is_running = (status == JobStatus::Running) as i64;
    let is_terminal = status.is_terminal() as i64;
    let affected = conn.execute(
        "UPDATE specialist_jobs SET
            status = ?1,
            started_at = CASE WHEN ?2 = 1 AND started_at IS NULL
                              THEN datetime('now') ELSE started_at END,
            ended_at = CASE WHEN ?3 = 1 THEN datetime('now') ELSE ended_at END
         WHERE job_id = ?4",
        params![status.as_str(), is_running, is_terminal, job_id],
    )?;
    if affected == 0 {
        return Err(StoreError::NotFound {
            job_id: job_id.to_string(),
        });
    }
    Ok(())
}

/// Record that cancellation was requested for a job.
pub fn set_cancel_requested(conn: &Connection, job_id: &str) -> Result<(), StoreError> {
    let affected = conn.execute(
        "UPDATE specialist_jobs SET cancel_requested = 1 WHERE job_id = ?1",
        params![job_id],
    )?;
    if affected == 0 {
        return Err(StoreError::NotFound {
            job_id: job_id.to_string(),
        });
    }
    Ok(())
}

/// Link a container run to a job. Idempotent on `(job_id, run_link)`.
pub fn add_run_link(conn: &Connection, job_id: &str, run_link: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO specialist_job_runs (job_id, run_link, created_at)
         VALUES (?1, ?2, datetime('now'))",
        params![job_id, run_link],
    )?;
    Ok(())
}

/// How many of a profile's jobs are still in flight (queued or running).
pub fn running_or_queued_job_count(conn: &Connection, profile_id: &str) -> Result<u32, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM specialist_jobs
         WHERE profile_id = ?1 AND status IN ('queued', 'running')",
        params![profile_id],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// How many specialist agent groups exist for a profile.
pub fn specialist_group_count(conn: &Connection, profile_id: &str) -> Result<u32, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM agent_groups WHERE kind = 'specialist' AND profile_id = ?1",
        params![profile_id],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// Create a specialist agent group row, returning its rowid.
pub fn create_specialist_group(
    conn: &Connection,
    slug: &str,
    profile_id: &str,
    profile_version: &str,
) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO agent_groups (slug, kind, profile_id, profile_version)
         VALUES (?1, 'specialist', ?2, ?3)",
        params![slug, profile_id, profile_version],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};

    fn migrated_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut set = baseline_migrations(order);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn job(id: &str, profile: &str) -> SpecialistJob {
        SpecialistJob::new(
            id,
            "orchestrator",
            "browser-1",
            profile,
            JobBudget {
                max_tokens: Some(1000),
                max_wall_secs: Some(120),
            },
            60,
        )
    }

    #[test]
    fn insert_and_load_round_trips() {
        let conn = migrated_db();
        let mut j = job("j1", "browser-specialist");
        j.run_links.push("run-1".into());
        insert_job(&conn, &j).unwrap();
        let back = load_job(&conn, "j1").unwrap();
        assert_eq!(back, j);
    }

    #[test]
    fn load_missing_job_is_not_found() {
        let conn = migrated_db();
        assert!(matches!(
            load_job(&conn, "nope"),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn update_status_stamps_started_and_ended() {
        let conn = migrated_db();
        insert_job(&conn, &job("j1", "browser-specialist")).unwrap();

        update_status(&conn, "j1", JobStatus::Running).unwrap();
        let (started, ended): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT started_at, ended_at FROM specialist_jobs WHERE job_id = 'j1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(started.is_some());
        assert!(ended.is_none());

        update_status(&conn, "j1", JobStatus::Succeeded).unwrap();
        let (started2, ended2): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT started_at, ended_at FROM specialist_jobs WHERE job_id = 'j1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // started_at is preserved, ended_at now set.
        assert_eq!(started, started2);
        assert!(ended2.is_some());
    }

    #[test]
    fn update_status_missing_job_is_not_found() {
        let conn = migrated_db();
        assert!(matches!(
            update_status(&conn, "ghost", JobStatus::Running),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn in_flight_count_tracks_status() {
        let conn = migrated_db();
        insert_job(&conn, &job("j1", "browser-specialist")).unwrap();
        insert_job(&conn, &job("j2", "browser-specialist")).unwrap();
        assert_eq!(running_or_queued_job_count(&conn, "browser-specialist").unwrap(), 2);
        update_status(&conn, "j1", JobStatus::Running).unwrap();
        update_status(&conn, "j1", JobStatus::Succeeded).unwrap();
        assert_eq!(running_or_queued_job_count(&conn, "browser-specialist").unwrap(), 1);
    }

    #[test]
    fn specialist_groups_are_counted_per_profile() {
        let conn = migrated_db();
        assert_eq!(specialist_group_count(&conn, "browser-specialist").unwrap(), 0);
        create_specialist_group(&conn, "browser-1", "browser-specialist", "0.1.0").unwrap();
        assert_eq!(specialist_group_count(&conn, "browser-specialist").unwrap(), 1);
        // A different profile's groups are not counted.
        assert_eq!(specialist_group_count(&conn, "other").unwrap(), 0);
    }

    #[test]
    fn cancel_requested_is_persisted() {
        let conn = migrated_db();
        insert_job(&conn, &job("j1", "browser-specialist")).unwrap();
        set_cancel_requested(&conn, "j1").unwrap();
        assert!(load_job(&conn, "j1").unwrap().cancel_requested);
    }
}
