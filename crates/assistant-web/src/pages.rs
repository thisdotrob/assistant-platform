//! Read-only page/API handlers and their route table.
//!
//! Handlers are generic over the host's `A: WebApp`, so each one monomorphizes
//! into a concrete function pointer the [`Router`] can hold — the crate stays
//! free of domain code while the host plugs in its real data. [`register`]
//! wires every read route in one call; mutating routes (approvals, memory
//! edits) are registered by their own surfaces so this read map carries no
//! state-changing verbs.

use crate::http::{Method, Request, Response};
use crate::router::{Params, Router};
use crate::view::WebApp;

/// Register the read pages onto a router. All routes are `GET`.
pub fn register<A: WebApp>(router: &mut Router<A>) {
    router
        .route(Method::Get, "/api/overview", overview)
        .route(Method::Get, "/api/groups", groups)
        .route(Method::Get, "/api/sessions", sessions)
        .route(Method::Get, "/api/runs/:run_id", run_detail)
        .route(Method::Get, "/api/queue", queue)
        .route(Method::Get, "/api/scheduled", scheduled)
        .route(Method::Get, "/api/users", users)
        .route(Method::Get, "/api/approvals", approvals)
        .route(Method::Get, "/api/capabilities", capabilities)
        .route(Method::Get, "/api/readiness", readiness)
        .route(Method::Get, "/api/specialists", specialists);
}

fn overview<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.overview())
}

fn groups<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.groups())
}

fn sessions<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.sessions())
}

fn run_detail<A: WebApp>(app: &A, _req: &Request, p: &Params) -> Response {
    let run_id = p.get("run_id").unwrap_or_default();
    match app.run_detail(run_id) {
        Some(detail) => Response::json(200, &detail),
        None => Response::json(
            404,
            &serde_json::json!({ "error": "run not found", "run_id": run_id }),
        ),
    }
}

fn queue<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.queue())
}

fn scheduled<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.scheduled())
}

fn users<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.users())
}

fn approvals<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.approvals())
}

fn capabilities<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.capabilities())
}

fn readiness<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.readiness())
}

fn specialists<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    Response::json(200, &app.specialists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::*;

    /// A fixture app with a couple of rows in each table and one known run.
    struct FakeApp;

    impl WebApp for FakeApp {
        fn overview(&self) -> Overview {
            Overview {
                product_id: "assistant".to_string(),
                product_version: "0.1.0".to_string(),
                platform_version: "0.1.0".to_string(),
                instance: Some("default".to_string()),
                ready: true,
                counts: OverviewCounts {
                    groups: 2,
                    active_sessions: 1,
                    pending_approvals: 1,
                    scheduled_items: 3,
                },
            }
        }
        fn groups(&self) -> Vec<GroupView> {
            vec![GroupView {
                id: "g1".to_string(),
                name: "orchestrator".to_string(),
                kind: "orchestrator".to_string(),
                channels: vec![ChannelView {
                    kind: "telegram".to_string(),
                    identifier: "@bot".to_string(),
                    connected: true,
                }],
            }]
        }
        fn sessions(&self) -> Vec<SessionView> {
            vec![SessionView {
                session_id: "s1".to_string(),
                group_id: "g1".to_string(),
                state: "active".to_string(),
                last_activity: Some(1000),
            }]
        }
        fn run_detail(&self, run_id: &str) -> Option<RunDetail> {
            if run_id != "r1" {
                return None;
            }
            Some(RunDetail {
                run: RunView {
                    run_id: "r1".to_string(),
                    session_id: "s1".to_string(),
                    state: "finished".to_string(),
                    started_at: Some(10),
                    finished_at: Some(20),
                },
                timeline: vec![TimelineEntry {
                    at: 11,
                    kind: "log".to_string(),
                    text: "started".to_string(),
                }],
            })
        }
        fn queue(&self) -> Vec<QueueItem> {
            vec![]
        }
        fn scheduled(&self) -> Vec<ScheduledItem> {
            vec![ScheduledItem {
                id: "sch1".to_string(),
                description: "daily digest".to_string(),
                next_run_at: Some(5000),
                recurrence: Some("daily".to_string()),
            }]
        }
        fn users(&self) -> Vec<UserView> {
            vec![UserView {
                id: "u1".to_string(),
                handle: "owner".to_string(),
                role: "owner".to_string(),
            }]
        }
        fn approvals(&self) -> Vec<ApprovalView> {
            vec![ApprovalView {
                id: "a1".to_string(),
                kind: "credential".to_string(),
                requested_by: "g1".to_string(),
                state: "pending".to_string(),
                expires_at: Some(9999),
            }]
        }
        fn capabilities(&self) -> Vec<CapabilityView> {
            vec![CapabilityView {
                id: "browser".to_string(),
                enabled: true,
                ready: false,
                setup_gaps: vec!["chromium image not pulled".to_string()],
            }]
        }
        fn readiness(&self) -> ReadinessReportView {
            ReadinessReportView {
                ready: false,
                blocking_failures: 1,
                checks: vec![
                    ReadinessCheckView {
                        module: "assistant-db".to_string(),
                        name: "migrations".to_string(),
                        status: "pass".to_string(),
                        detail: "schema at head".to_string(),
                    },
                    ReadinessCheckView {
                        module: "assistant-browser".to_string(),
                        name: "chromium".to_string(),
                        status: "fail".to_string(),
                        detail: "image not pulled".to_string(),
                    },
                    ReadinessCheckView {
                        module: "claw-email".to_string(),
                        name: "smtp".to_string(),
                        status: "skipped".to_string(),
                        detail: "capability disabled".to_string(),
                    },
                ],
            }
        }
        fn specialists(&self) -> Vec<SpecialistStatusView> {
            vec![SpecialistStatusView {
                id: "spec1".to_string(),
                kind: "browser".to_string(),
                ready: true,
                state: "idle".to_string(),
                artifacts: vec![ArtifactRefView {
                    name: "screenshot.png".to_string(),
                    path: "artifacts/spec1/screenshot.png".to_string(),
                    captured_at: Some(1234),
                    size: 4096,
                }],
            }]
        }
    }

    fn router() -> Router<FakeApp> {
        let mut r = Router::new();
        register(&mut r);
        r
    }

    fn body_json(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    #[test]
    fn overview_route_renders_counts() {
        let resp = router().dispatch(&FakeApp, &Request::new(Method::Get, "/api/overview"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["product_id"], "assistant");
        assert_eq!(v["counts"]["groups"], 2);
        assert_eq!(v["ready"], true);
    }

    #[test]
    fn run_detail_renders_timeline_and_404s_when_missing() {
        let r = router();
        let ok = r.dispatch(&FakeApp, &Request::new(Method::Get, "/api/runs/r1"));
        assert_eq!(ok.status, 200);
        let v = body_json(&ok);
        assert_eq!(v["run"]["state"], "finished");
        assert_eq!(v["timeline"][0]["text"], "started");

        let missing = r.dispatch(&FakeApp, &Request::new(Method::Get, "/api/runs/nope"));
        assert_eq!(missing.status, 404);
        assert_eq!(body_json(&missing)["run_id"], "nope");
    }

    #[test]
    fn list_routes_render_their_rows() {
        let r = router();
        for (path, pointer, expected) in [
            ("/api/groups", "/0/name", "orchestrator"),
            ("/api/sessions", "/0/session_id", "s1"),
            ("/api/scheduled", "/0/description", "daily digest"),
            ("/api/users", "/0/role", "owner"),
            ("/api/approvals", "/0/state", "pending"),
            ("/api/capabilities", "/0/id", "browser"),
        ] {
            let resp = r.dispatch(&FakeApp, &Request::new(Method::Get, path));
            assert_eq!(resp.status, 200, "{path}");
            let v = body_json(&resp);
            assert_eq!(v.pointer(pointer).unwrap(), expected, "{path}");
        }
    }

    #[test]
    fn capabilities_surface_setup_gaps() {
        let resp = router().dispatch(&FakeApp, &Request::new(Method::Get, "/api/capabilities"));
        let v = body_json(&resp);
        assert_eq!(v[0]["ready"], false);
        assert_eq!(v[0]["setup_gaps"][0], "chromium image not pulled");
    }

    #[test]
    fn readiness_surfaces_blocking_failures_and_check_statuses() {
        let resp = router().dispatch(&FakeApp, &Request::new(Method::Get, "/api/readiness"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v["ready"], false);
        assert_eq!(v["blocking_failures"], 1);
        assert_eq!(v["checks"][1]["module"], "assistant-browser");
        assert_eq!(v["checks"][1]["status"], "fail");
        assert_eq!(v["checks"][2]["status"], "skipped");
    }

    #[test]
    fn specialists_surface_state_and_artifacts() {
        let resp = router().dispatch(&FakeApp, &Request::new(Method::Get, "/api/specialists"));
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        assert_eq!(v[0]["kind"], "browser");
        assert_eq!(v[0]["state"], "idle");
        assert_eq!(v[0]["artifacts"][0]["name"], "screenshot.png");
        assert_eq!(v[0]["artifacts"][0]["size"], 4096);
    }
}
