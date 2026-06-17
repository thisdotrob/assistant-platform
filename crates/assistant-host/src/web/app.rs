//! [`HostWebApp`]: the [`WebApp`] the operator UI reads through, backed by the
//! live central database.
//!
//! `assistant-web` links no domain code — it renders whatever this provider
//! returns. Every method is infallible by contract (the page handlers can't
//! surface a `Result`), so a query failure is logged and degrades to an empty /
//! default view rather than taking the page down. Reads run against the same
//! `main.db` the daemon writes; `open_central` sets a busy timeout + WAL so a
//! concurrent daemon write makes a read wait briefly instead of erroring.

use std::path::{Path, PathBuf};

use assistant_db::{open_central, DbError};
use assistant_web::{
    ApprovalView, CapabilityView, ChannelView, EditorPolicy, GroupView, MemoryApp, MemoryEditor,
    Overview, OverviewCounts, QueueItem, ReadinessCheckView, ReadinessReportView, RunDetail,
    RunView, ScheduledItem, SessionView, SpecialistStatusView, UserView, WebApp,
};
use rusqlite::{Connection, OptionalExtension};

/// A read-only view of the instance's central DB for the web UI (plus the
/// read/write memory editor bound to the orchestrator memory root), and the
/// static identity (product/platform versions) the overview surfaces. Backs both
/// the [`WebApp`] page routes and the [`MemoryApp`] memory routes so a single
/// router can serve them.
pub struct HostWebApp {
    conn: Connection,
    editor: MemoryEditor,
    product_id: String,
    product_version: String,
    platform_version: String,
    instance: Option<String>,
}

impl HostWebApp {
    /// Open the instance's central DB for reading and bind the memory editor to
    /// `memory_root` (the orchestrator's `groups/orchestrator/memory` tree). Uses
    /// the standard DB pragmas (WAL + busy timeout) so reads coexist with a live
    /// daemon.
    pub fn open(
        central_db_path: &Path,
        memory_root: impl Into<PathBuf>,
        product_id: impl Into<String>,
        product_version: impl Into<String>,
        platform_version: impl Into<String>,
        instance: Option<String>,
    ) -> Result<Self, DbError> {
        let conn = open_central(central_db_path)?;
        Ok(Self {
            conn,
            editor: MemoryEditor::new(memory_root, EditorPolicy::default()),
            product_id: product_id.into(),
            product_version: product_version.into(),
            platform_version: platform_version.into(),
            instance,
        })
    }

    fn count(&self, sql: &str) -> rusqlite::Result<u64> {
        self.conn.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
    }

    /// DB-derived readiness: checks we can answer purely from central state.
    /// Runtime-only checks (containers, gateway reachability) aren't persisted,
    /// so they're out of scope here.
    fn compute_readiness(&self) -> ReadinessReportView {
        let mut checks = Vec::new();

        // The central DB is reachable — we hold an open connection and a trivial
        // query succeeds. A failure here means the file is gone or corrupt.
        let db_ok = self.conn.query_row("SELECT 1", [], |_| Ok(())).is_ok();
        checks.push(ReadinessCheckView {
            module: "assistant-db".to_string(),
            name: "central-db".to_string(),
            status: if db_ok { "pass" } else { "fail" }.to_string(),
            detail: if db_ok {
                "central database reachable".to_string()
            } else {
                "central database query failed".to_string()
            },
        });

        // An owner must exist for the instance to be administrable.
        let owner_exists = self
            .conn
            .query_row(
                "SELECT 1 FROM user_roles WHERE role = 'owner' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()
            .map(|o| o.is_some())
            .unwrap_or(false);
        checks.push(ReadinessCheckView {
            module: "assistant-permissions".to_string(),
            name: "owner-registered".to_string(),
            status: if owner_exists { "pass" } else { "fail" }.to_string(),
            detail: if owner_exists {
                "an owner is registered".to_string()
            } else {
                "no owner registered (run register-user --owner)".to_string()
            },
        });

        let blocking_failures = checks.iter().filter(|c| c.status == "fail").count() as u64;
        ReadinessReportView {
            ready: blocking_failures == 0,
            blocking_failures,
            checks,
        }
    }

    fn try_groups(&self) -> rusqlite::Result<Vec<GroupView>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, kind FROM agent_groups ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut groups = Vec::with_capacity(rows.len());
        for (id, slug, kind) in rows {
            let mut chan_stmt = self.conn.prepare(
                "SELECT channel, address FROM agent_destinations WHERE agent_group_id = ?1 ORDER BY id",
            )?;
            let channels = chan_stmt
                .query_map([id], |r| {
                    Ok(ChannelView {
                        kind: r.get::<_, String>(0)?,
                        identifier: r.get::<_, String>(1)?,
                        // A destination row means the channel is wired in config.
                        // Live socket state isn't tracked centrally.
                        connected: true,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            groups.push(GroupView {
                id: id.to_string(),
                name: slug.clone(),
                kind,
                channels,
            });
        }
        Ok(groups)
    }

    fn try_sessions(&self) -> rusqlite::Result<Vec<SessionView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent_group_id, status, CAST(strftime('%s', updated_at) AS INTEGER)
             FROM sessions ORDER BY updated_at DESC",
        )?;
        stmt.query_map([], |r| {
            Ok(SessionView {
                session_id: r.get::<_, String>(0)?,
                group_id: r.get::<_, i64>(1)?.to_string(),
                state: r.get::<_, String>(2)?,
                last_activity: r.get::<_, Option<i64>>(3)?,
            })
        })?
        .collect()
    }

    fn try_run_detail(&self, run_id: &str) -> rusqlite::Result<Option<RunDetail>> {
        let run = self
            .conn
            .query_row(
                "SELECT id, session_id, status,
                        CAST(strftime('%s', started_at) AS INTEGER),
                        CAST(strftime('%s', stopped_at) AS INTEGER)
                 FROM container_runs WHERE id = ?1",
                [run_id],
                |r| {
                    Ok(RunView {
                        run_id: r.get::<_, String>(0)?,
                        session_id: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        state: r.get::<_, String>(2)?,
                        started_at: r.get::<_, Option<i64>>(3)?,
                        finished_at: r.get::<_, Option<i64>>(4)?,
                    })
                },
            )
            .optional()?;
        // The central DB tracks the run lifecycle but not a per-run event
        // timeline (logs/messages live in the per-channel session DBs), so the
        // timeline is empty here.
        Ok(run.map(|run| RunDetail {
            run,
            timeline: Vec::new(),
        }))
    }

    fn try_scheduled(&self) -> rusqlite::Result<Vec<ScheduledItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, intent, CAST(strftime('%s', process_after) AS INTEGER), recurrence
             FROM scheduled_items WHERE status = 'active' ORDER BY process_after",
        )?;
        stmt.query_map([], |r| {
            Ok(ScheduledItem {
                id: r.get::<_, String>(0)?,
                description: r.get::<_, String>(1)?,
                next_run_at: r.get::<_, Option<i64>>(2)?,
                recurrence: r.get::<_, Option<String>>(3)?,
            })
        })?
        .collect()
    }

    fn try_users(&self) -> rusqlite::Result<Vec<UserView>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.id, u.handle,
                    (SELECT role FROM user_roles WHERE user_id = u.id ORDER BY role LIMIT 1)
             FROM users u ORDER BY u.id",
        )?;
        stmt.query_map([], |r| {
            Ok(UserView {
                id: r.get::<_, i64>(0)?.to_string(),
                handle: r.get::<_, String>(1)?,
                // A registered user with no explicit role row is a plain member.
                role: r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "member".to_string()),
            })
        })?
        .collect()
    }

    fn try_approvals(&self) -> rusqlite::Result<Vec<ApprovalView>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.kind, a.status,
                    CAST(strftime('%s', a.expires_at) AS INTEGER),
                    u.handle
             FROM pending_approvals a
             LEFT JOIN users u ON u.id = a.requested_by
             ORDER BY a.created_at DESC",
        )?;
        stmt.query_map([], |r| {
            let id = r.get::<_, i64>(0)?;
            let handle = r.get::<_, Option<String>>(4)?;
            Ok(ApprovalView {
                id: id.to_string(),
                kind: r.get::<_, String>(1)?,
                state: r.get::<_, String>(2)?,
                expires_at: r.get::<_, Option<i64>>(3)?,
                requested_by: handle.unwrap_or_else(|| "unknown".to_string()),
            })
        })?
        .collect()
    }

    fn try_capabilities(&self) -> rusqlite::Result<Vec<CapabilityView>> {
        let mut stmt = self.conn.prepare(
            "SELECT capability_id, enabled FROM capability_metadata ORDER BY capability_id",
        )?;
        stmt.query_map([], |r| {
            let enabled = r.get::<_, i64>(1)? != 0;
            Ok(CapabilityView {
                id: r.get::<_, String>(0)?,
                enabled,
                // Central metadata records enablement, not a separate live
                // readiness signal, so ready mirrors enabled and there are no
                // recorded setup gaps to surface here.
                ready: enabled,
                setup_gaps: Vec::new(),
            })
        })?
        .collect()
    }

    fn try_specialists(&self) -> rusqlite::Result<Vec<SpecialistStatusView>> {
        let mut stmt = self.conn.prepare(
            "SELECT job_id, profile_id, status FROM specialist_jobs ORDER BY created_at DESC",
        )?;
        stmt.query_map([], |r| {
            let status = r.get::<_, String>(2)?;
            Ok(SpecialistStatusView {
                id: r.get::<_, String>(0)?,
                kind: r.get::<_, String>(1)?,
                ready: status == "succeeded",
                state: status,
                // Specialist artifacts aren't projected into the central DB.
                artifacts: Vec::new(),
            })
        })?
        .collect()
    }
}

/// Log a query failure and fall back to a default view, keeping the page
/// infallible per the `WebApp` contract.
fn or_default<T: Default>(context: &str, result: rusqlite::Result<T>) -> T {
    result.unwrap_or_else(|e| {
        eprintln!("web: {context} query failed: {e}");
        T::default()
    })
}

impl WebApp for HostWebApp {
    fn overview(&self) -> Overview {
        let readiness = self.compute_readiness();
        let counts = OverviewCounts {
            groups: or_default("overview.groups", self.count("SELECT COUNT(*) FROM agent_groups")),
            active_sessions: or_default(
                "overview.sessions",
                self.count("SELECT COUNT(*) FROM sessions WHERE status NOT IN ('idle', 'closed')"),
            ),
            pending_approvals: or_default(
                "overview.approvals",
                self.count("SELECT COUNT(*) FROM pending_approvals WHERE status = 'pending'"),
            ),
            scheduled_items: or_default(
                "overview.scheduled",
                self.count("SELECT COUNT(*) FROM scheduled_items WHERE status = 'active'"),
            ),
        };
        Overview {
            product_id: self.product_id.clone(),
            product_version: self.product_version.clone(),
            platform_version: self.platform_version.clone(),
            instance: self.instance.clone(),
            ready: readiness.ready,
            counts,
        }
    }

    fn groups(&self) -> Vec<GroupView> {
        or_default("groups", self.try_groups())
    }

    fn sessions(&self) -> Vec<SessionView> {
        or_default("sessions", self.try_sessions())
    }

    fn run_detail(&self, run_id: &str) -> Option<RunDetail> {
        or_default("run_detail", self.try_run_detail(run_id))
    }

    fn queue(&self) -> Vec<QueueItem> {
        // The inbound/outbound work queues live in the per-channel session DBs,
        // not the central projection, so there's nothing to surface centrally.
        Vec::new()
    }

    fn scheduled(&self) -> Vec<ScheduledItem> {
        or_default("scheduled", self.try_scheduled())
    }

    fn users(&self) -> Vec<UserView> {
        or_default("users", self.try_users())
    }

    fn approvals(&self) -> Vec<ApprovalView> {
        or_default("approvals", self.try_approvals())
    }

    fn capabilities(&self) -> Vec<CapabilityView> {
        or_default("capabilities", self.try_capabilities())
    }

    fn readiness(&self) -> ReadinessReportView {
        self.compute_readiness()
    }

    fn specialists(&self) -> Vec<SpecialistStatusView> {
        or_default("specialists", self.try_specialists())
    }
}

impl MemoryApp for HostWebApp {
    fn memory(&self) -> &MemoryEditor {
        &self.editor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules, open_in_memory};
    use assistant_web::{Method, Params, Request, Response, Router};

    /// A memory editor pointed at a path that is never touched by the WebApp
    /// tests (the editor is FS-lazy: it only reads/writes on an actual call).
    fn unused_editor() -> MemoryEditor {
        MemoryEditor::new(std::env::temp_dir().join("assistant-web-unused"), EditorPolicy::default())
    }

    /// A baseline-migrated in-memory central DB, plus the `specialist_jobs` table
    /// (owned by a later agent-graph migration, not the baseline set) so every
    /// `WebApp` query has its table.
    fn seeded() -> HostWebApp {
        let mut conn = open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules().into_iter().map(String::from).collect();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        conn.execute_batch(
            "CREATE TABLE specialist_jobs (
                 job_id TEXT PRIMARY KEY, orchestrator_group TEXT NOT NULL,
                 specialist_group TEXT NOT NULL, profile_id TEXT NOT NULL,
                 status TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now')));

             INSERT INTO users (id, handle, display_name) VALUES (1, 'rob', 'Rob'), (2, 'sam', NULL);
             INSERT INTO user_roles (user_id, role) VALUES (1, 'owner');

             INSERT INTO agent_groups (id, slug, kind, profile_id, profile_version)
                 VALUES (1, 'orchestrator', 'orchestrator', 'assistant.orchestrator', '0.1.0');
             INSERT INTO agent_destinations (id, agent_group_id, channel, address)
                 VALUES (1, 1, 'slack', 'C123');

             INSERT INTO sessions (id, agent_group_id, status) VALUES ('s1', 1, 'running');

             INSERT INTO scheduled_items (id, agent_group_id, intent, process_after, status)
                 VALUES ('sched1', 1, 'daily standup', '2099-01-01 09:00:00', 'active');

             INSERT INTO pending_approvals (id, kind, subject, status, requested_by)
                 VALUES (1, 'tool', 'rm -rf', 'pending', 1);

             INSERT INTO specialist_jobs (job_id, orchestrator_group, specialist_group, profile_id, status)
                 VALUES ('job1', 'orchestrator', 'browser-1', 'browser-specialist', 'succeeded');",
        )
        .unwrap();
        HostWebApp {
            conn,
            editor: unused_editor(),
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            instance: None,
        }
    }

    #[test]
    fn overview_counts_reflect_seeded_rows() {
        let app = seeded();
        let ov = app.overview();
        assert_eq!(ov.product_id, "assistant");
        assert_eq!(ov.counts.groups, 1);
        assert_eq!(ov.counts.active_sessions, 1);
        assert_eq!(ov.counts.pending_approvals, 1);
        assert_eq!(ov.counts.scheduled_items, 1);
        assert!(ov.ready, "owner is registered and DB reachable");
    }

    #[test]
    fn groups_carry_their_channels() {
        let app = seeded();
        let groups = app.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "orchestrator");
        assert_eq!(groups[0].channels.len(), 1);
        assert_eq!(groups[0].channels[0].kind, "slack");
        assert_eq!(groups[0].channels[0].identifier, "C123");
    }

    #[test]
    fn sessions_scheduled_users_approvals_specialists_map_through() {
        let app = seeded();

        let sessions = app.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
        assert_eq!(sessions[0].state, "running");

        let scheduled = app.scheduled();
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].description, "daily standup");
        assert!(scheduled[0].next_run_at.is_some());

        let users = app.users();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].role, "owner");
        assert_eq!(users[1].role, "member", "no role row defaults to member");

        let approvals = app.approvals();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].kind, "tool");
        assert_eq!(approvals[0].requested_by, "rob");

        let specialists = app.specialists();
        assert_eq!(specialists.len(), 1);
        assert_eq!(specialists[0].kind, "browser-specialist");
        assert!(specialists[0].ready);

        // No central queue projection.
        assert!(app.queue().is_empty());
    }

    #[test]
    fn readiness_fails_when_no_owner() {
        let mut conn = open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules().into_iter().map(String::from).collect();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        let app = HostWebApp {
            conn,
            editor: unused_editor(),
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            instance: None,
        };
        let report = app.readiness();
        assert!(!report.ready);
        assert_eq!(report.blocking_failures, 1);
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "owner-registered" && c.status == "fail"));
    }

    #[test]
    fn missing_run_is_none() {
        let app = seeded();
        assert!(app.run_detail("does-not-exist").is_none());
    }

    const GOOD_FM: &str = "---\nmemory_id: mem_1\nowner_agent_group_id: ag_x\nscope: all_chats\nsource_type: user_said\nconfidence: high\nreuse_policy: same_scope\nretention: normal\n---\nbody\n";

    fn page_overview(app: &HostWebApp, _req: &Request, _p: &Params) -> Response {
        Response::json(200, &app.overview())
    }

    /// One `Router<HostWebApp>` carrying both a page route (`WebApp`) and the
    /// memory routes (`MemoryApp`) dispatches each — proving the host type
    /// satisfies both provider traits on a single router, as the serve path wires
    /// it.
    #[test]
    fn one_router_serves_pages_and_memory_through_host_app() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();

        let mut conn = open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules().into_iter().map(String::from).collect();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        let app = HostWebApp {
            conn,
            editor: MemoryEditor::new(root, EditorPolicy::default()),
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            instance: None,
        };

        let mut router: Router<HostWebApp> = Router::new();
        router.route(Method::Get, "/api/overview", page_overview);
        assistant_web::memory_api::register(&mut router);

        // The WebApp page renders.
        let ov = router.dispatch(&app, &Request::new(Method::Get, "/api/overview"));
        assert_eq!(ov.status, 200);

        // The MemoryApp create→read round-trips on the same router/app.
        let create = Request::new(Method::Post, "/api/memory/file").with_body(
            serde_json::to_vec(&serde_json::json!({"path": "note.md", "content": GOOD_FM})).unwrap(),
        );
        assert_eq!(router.dispatch(&app, &create).status, 201);
        let read = Request::new(Method::Get, "/api/memory/file").with_query("path=note.md");
        assert_eq!(router.dispatch(&app, &read).status, 200);
    }
}
