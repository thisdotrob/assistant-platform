//! Server-rendered HTML for the operator dashboard.
//!
//! These handlers render the same data the JSON `/api/*` routes expose, but as
//! clickable pages a browser can navigate. They are generic over `A: WebApp`, so
//! the crate stays domain-free; the host plugs in its real data. Every value
//! interpolated into the markup is run through [`esc`], so a group name, memory
//! path, or any other operator/agent-controlled string cannot inject HTML.
//!
//! The pages are read-only. Mutations stay on the JSON surface
//! ([`crate::memory_api`]) behind the cookie + origin CSRF gate in
//! [`crate::server`].

use crate::http::{Method, Request, Response};
use crate::router::{Params, Router};
use crate::view::*;

/// Register the human-facing HTML pages. All routes are `GET`.
pub fn register<A: WebApp>(router: &mut Router<A>) {
    router
        .route(Method::Get, "/", overview_page)
        .route(Method::Get, "/groups", groups_page)
        .route(Method::Get, "/sessions", sessions_page)
        .route(Method::Get, "/scheduled", scheduled_page)
        .route(Method::Get, "/users", users_page)
        .route(Method::Get, "/approvals", approvals_page)
        .route(Method::Get, "/capabilities", capabilities_page)
        .route(Method::Get, "/readiness", readiness_page)
        .route(Method::Get, "/specialists", specialists_page)
        .route(Method::Get, "/runs/:run_id", run_page);
}

const NAV: &[(&str, &str)] = &[
    ("/", "Overview"),
    ("/groups", "Groups"),
    ("/sessions", "Sessions"),
    ("/scheduled", "Scheduled"),
    ("/users", "Users"),
    ("/approvals", "Approvals"),
    ("/capabilities", "Capabilities"),
    ("/readiness", "Readiness"),
    ("/specialists", "Specialists"),
    ("/memory", "Memory"),
];

const STYLE: &str = "\
:root{--fg:#1d2129;--muted:#6b7280;--line:#e5e7eb;--bg:#f7f8fa;--accent:#2563eb}\
*{box-sizing:border-box}\
body{margin:0;font:15px/1.5 -apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;color:var(--fg);background:var(--bg)}\
nav{display:flex;flex-wrap:wrap;gap:2px;background:#fff;border-bottom:1px solid var(--line);padding:8px 16px}\
nav a{padding:6px 12px;border-radius:6px;text-decoration:none;color:var(--muted);font-weight:500}\
nav a:hover{background:var(--bg);color:var(--fg)}\
nav a.active{background:var(--accent);color:#fff}\
main{max-width:1000px;margin:0 auto;padding:24px 16px}\
h1{font-size:22px;margin:0 0 16px}\
table{width:100%;border-collapse:collapse;background:#fff;border:1px solid var(--line);border-radius:8px;overflow:hidden}\
th,td{text-align:left;padding:8px 12px;border-bottom:1px solid var(--line);vertical-align:top}\
th{background:#fafafa;font-size:12px;text-transform:uppercase;letter-spacing:.03em;color:var(--muted)}\
tr:last-child td{border-bottom:none}\
.cards{display:flex;flex-wrap:wrap;gap:12px;margin-bottom:16px}\
.card{flex:1;min-width:140px;background:#fff;border:1px solid var(--line);border-radius:8px;padding:12px 14px}\
.card .k{font-size:12px;color:var(--muted)}\
.card .v{font-size:20px;font-weight:600;margin-top:2px}\
.badge{display:inline-block;padding:1px 8px;border-radius:999px;font-size:12px;font-weight:600}\
.badge.ok{background:#dcfce7;color:#166534}\
.badge.bad{background:#fee2e2;color:#991b1b}\
.badge.neutral{background:#e5e7eb;color:#374151}\
.empty{color:var(--muted);font-style:italic}\
code{font:13px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace}\
a{color:var(--accent)}\
.note{padding:10px 12px;border-radius:8px;margin-bottom:16px;font-weight:500}\
.note.ok{background:#dcfce7;color:#166534}\
.note.bad{background:#fee2e2;color:#991b1b}\
.note.warn{background:#fef9c3;color:#854d0e}\
.crumb{margin-bottom:12px;color:var(--muted)}\
.crumb a{text-decoration:none}\
.row{margin:12px 0}\
.row label{display:block;font-size:12px;color:var(--muted);margin-bottom:4px}\
textarea,input[type=text]{width:100%;padding:10px;border:1px solid var(--line);border-radius:8px;font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;background:#fff}\
textarea{min-height:440px;resize:vertical}\
button,.btn{background:var(--accent);color:#fff;border:none;padding:8px 16px;border-radius:6px;font-weight:600;cursor:pointer;text-decoration:none;display:inline-block}\
.btn.ghost{background:#fff;color:var(--fg);border:1px solid var(--line)}\
";

/// Escape the five HTML-significant characters so interpolated data is inert.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap a page body in the shared shell (nav + styles). `active` is the href of
/// the current page, used to highlight its nav link.
pub(crate) fn layout(active: &str, title: &str, body: &str) -> String {
    let nav: String = NAV
        .iter()
        .map(|(href, label)| {
            let cls = if *href == active { " class=\"active\"" } else { "" };
            format!("<a href=\"{href}\"{cls}>{}</a>", esc(label))
        })
        .collect();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{t} · assistant</title><style>{STYLE}</style></head>\
<body><nav>{nav}</nav><main><h1>{t}</h1>{body}</main></body></html>",
        t = esc(title),
    )
}

/// Render a table from header labels and pre-built (trusted-HTML) row cells. An
/// empty body renders a friendly placeholder instead.
pub(crate) fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    if rows.is_empty() {
        return "<p class=\"empty\">Nothing here yet.</p>".to_string();
    }
    let head: String = headers.iter().map(|h| format!("<th>{}</th>", esc(h))).collect();
    let body: String = rows
        .iter()
        .map(|r| {
            let cells: String = r.iter().map(|c| format!("<td>{c}</td>")).collect();
            format!("<tr>{cells}</tr>")
        })
        .collect();
    format!("<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>")
}

fn badge(ok: bool, text: &str) -> String {
    let cls = if ok { "ok" } else { "bad" };
    format!("<span class=\"badge {cls}\">{}</span>", esc(text))
}

fn status_badge(status: &str) -> String {
    let cls = match status {
        "pass" | "succeeded" | "connected" | "ready" | "active" | "granted" => "ok",
        "fail" | "failed" | "error" | "revoked" => "bad",
        _ => "neutral",
    };
    format!("<span class=\"badge {cls}\">{}</span>", esc(status))
}

fn opt_time(t: Option<i64>) -> String {
    match t {
        Some(s) => esc(&fmt_epoch(s)),
        None => "—".to_string(),
    }
}

fn opt_str(s: &Option<String>) -> String {
    match s {
        Some(v) => esc(v),
        None => "—".to_string(),
    }
}

/// Format an epoch-second timestamp as `YYYY-MM-DD HH:MM:SSZ` (UTC). Self
/// contained so the crate needs no time dependency.
fn fmt_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, min, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}Z")
}

/// Howard Hinnant's days-from-civil inverse: epoch-day → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn overview_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let o = app.overview();
    let body = format!(
        "<div class=\"cards\">\
<div class=\"card\"><div class=\"k\">Product</div><div class=\"v\">{pid} {pver}</div></div>\
<div class=\"card\"><div class=\"k\">Platform</div><div class=\"v\">{plat}</div></div>\
<div class=\"card\"><div class=\"k\">Instance</div><div class=\"v\">{inst}</div></div>\
<div class=\"card\"><div class=\"k\">Ready</div><div class=\"v\">{ready}</div></div>\
</div>\
<div class=\"cards\">\
<div class=\"card\"><div class=\"k\">Groups</div><div class=\"v\">{g}</div></div>\
<div class=\"card\"><div class=\"k\">Active sessions</div><div class=\"v\">{s}</div></div>\
<div class=\"card\"><div class=\"k\">Pending approvals</div><div class=\"v\">{a}</div></div>\
<div class=\"card\"><div class=\"k\">Scheduled items</div><div class=\"v\">{sc}</div></div>\
</div>",
        pid = esc(&o.product_id),
        pver = esc(&o.product_version),
        plat = esc(&o.platform_version),
        inst = esc(o.instance.as_deref().unwrap_or("default")),
        ready = badge(o.ready, if o.ready { "ready" } else { "not ready" }),
        g = o.counts.groups,
        s = o.counts.active_sessions,
        a = o.counts.pending_approvals,
        sc = o.counts.scheduled_items,
    );
    Response::html(200, layout("/", "Overview", &body))
}

fn groups_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .groups()
        .into_iter()
        .map(|g| {
            let channels = if g.channels.is_empty() {
                "—".to_string()
            } else {
                g.channels
                    .iter()
                    .map(|c| {
                        format!(
                            "{} <code>{}</code> {}",
                            esc(&c.kind),
                            esc(&c.identifier),
                            badge(c.connected, if c.connected { "connected" } else { "off" }),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("<br>")
            };
            vec![esc(&g.id), esc(&g.name), esc(&g.kind), channels]
        })
        .collect();
    let body = table(&["ID", "Name", "Kind", "Channels"], rows);
    Response::html(200, layout("/groups", "Groups", &body))
}

fn sessions_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .sessions()
        .into_iter()
        .map(|s| {
            vec![
                esc(&s.session_id),
                esc(&s.group_id),
                status_badge(&s.state),
                opt_time(s.last_activity),
            ]
        })
        .collect();
    let body = table(&["Session", "Group", "State", "Last activity"], rows);
    Response::html(200, layout("/sessions", "Sessions", &body))
}

fn scheduled_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .scheduled()
        .into_iter()
        .map(|s| {
            vec![
                esc(&s.id),
                esc(&s.description),
                opt_time(s.next_run_at),
                opt_str(&s.recurrence),
            ]
        })
        .collect();
    let body = table(&["ID", "Description", "Next run", "Recurrence"], rows);
    Response::html(200, layout("/scheduled", "Scheduled", &body))
}

fn users_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .users()
        .into_iter()
        .map(|u| vec![esc(&u.id), esc(&u.handle), status_badge(&u.role)])
        .collect();
    let body = table(&["ID", "Handle", "Role"], rows);
    Response::html(200, layout("/users", "Users", &body))
}

fn approvals_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .approvals()
        .into_iter()
        .map(|a| {
            vec![
                esc(&a.id),
                esc(&a.kind),
                esc(&a.requested_by),
                status_badge(&a.state),
                opt_time(a.expires_at),
            ]
        })
        .collect();
    let body = table(&["ID", "Kind", "Requested by", "State", "Expires"], rows);
    Response::html(200, layout("/approvals", "Approvals", &body))
}

fn capabilities_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .capabilities()
        .into_iter()
        .map(|c| {
            let gaps = if c.setup_gaps.is_empty() {
                "—".to_string()
            } else {
                c.setup_gaps.iter().map(|g| esc(g)).collect::<Vec<_>>().join("<br>")
            };
            vec![
                esc(&c.id),
                badge(c.enabled, if c.enabled { "enabled" } else { "disabled" }),
                badge(c.ready, if c.ready { "ready" } else { "not ready" }),
                gaps,
            ]
        })
        .collect();
    let body = table(&["Capability", "Enabled", "Ready", "Setup gaps"], rows);
    Response::html(200, layout("/capabilities", "Capabilities", &body))
}

fn readiness_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let r = app.readiness();
    let summary = format!(
        "<div class=\"cards\">\
<div class=\"card\"><div class=\"k\">Overall</div><div class=\"v\">{ready}</div></div>\
<div class=\"card\"><div class=\"k\">Blocking failures</div><div class=\"v\">{bf}</div></div>\
</div>",
        ready = badge(r.ready, if r.ready { "ready" } else { "not ready" }),
        bf = r.blocking_failures,
    );
    let rows = r
        .checks
        .into_iter()
        .map(|c| {
            vec![
                esc(&c.module),
                esc(&c.name),
                status_badge(&c.status),
                esc(&c.detail),
            ]
        })
        .collect();
    let body = format!("{summary}{}", table(&["Module", "Check", "Status", "Detail"], rows));
    Response::html(200, layout("/readiness", "Readiness", &body))
}

fn specialists_page<A: WebApp>(app: &A, _req: &Request, _p: &Params) -> Response {
    let rows = app
        .specialists()
        .into_iter()
        .map(|s| {
            let artifacts = if s.artifacts.is_empty() {
                "—".to_string()
            } else {
                s.artifacts
                    .iter()
                    .map(|a| format!("<code>{}</code> ({} B)", esc(&a.name), a.size))
                    .collect::<Vec<_>>()
                    .join("<br>")
            };
            vec![
                esc(&s.id),
                esc(&s.kind),
                badge(s.ready, if s.ready { "ready" } else { "not ready" }),
                status_badge(&s.state),
                artifacts,
            ]
        })
        .collect();
    let body = table(&["ID", "Kind", "Ready", "State", "Artifacts"], rows);
    Response::html(200, layout("/specialists", "Specialists", &body))
}

fn run_page<A: WebApp>(app: &A, _req: &Request, p: &Params) -> Response {
    let run_id = p.get("run_id").unwrap_or_default();
    let Some(detail) = app.run_detail(run_id) else {
        let body = format!(
            "<p class=\"empty\">No run <code>{}</code>.</p>",
            esc(run_id)
        );
        return Response::html(404, layout("", "Run not found", &body));
    };
    let run = &detail.run;
    let summary = format!(
        "<div class=\"cards\">\
<div class=\"card\"><div class=\"k\">Run</div><div class=\"v\">{rid}</div></div>\
<div class=\"card\"><div class=\"k\">Session</div><div class=\"v\">{sid}</div></div>\
<div class=\"card\"><div class=\"k\">State</div><div class=\"v\">{state}</div></div>\
<div class=\"card\"><div class=\"k\">Started</div><div class=\"v\">{started}</div></div>\
<div class=\"card\"><div class=\"k\">Finished</div><div class=\"v\">{finished}</div></div>\
</div>",
        rid = esc(&run.run_id),
        sid = esc(&run.session_id),
        state = status_badge(&run.state),
        started = opt_time(run.started_at),
        finished = opt_time(run.finished_at),
    );
    let rows = detail
        .timeline
        .into_iter()
        .map(|e| vec![opt_time(Some(e.at)), esc(&e.kind), esc(&e.text)])
        .collect();
    let body = format!("{summary}{}", table(&["At", "Kind", "Text"], rows));
    Response::html(200, layout("", &format!("Run {}", run.run_id), &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeApp {
        group_name: String,
    }

    impl WebApp for FakeApp {
        fn overview(&self) -> Overview {
            Overview {
                product_id: "assistant".into(),
                product_version: "0.1.0".into(),
                platform_version: "0.1.0".into(),
                instance: None,
                ready: true,
                counts: OverviewCounts {
                    groups: 2,
                    active_sessions: 1,
                    pending_approvals: 0,
                    scheduled_items: 3,
                },
            }
        }
        fn groups(&self) -> Vec<GroupView> {
            vec![GroupView {
                id: "g1".into(),
                name: self.group_name.clone(),
                kind: "orchestrator".into(),
                channels: vec![ChannelView {
                    kind: "slack".into(),
                    identifier: "#ops".into(),
                    connected: true,
                }],
            }]
        }
        fn sessions(&self) -> Vec<SessionView> {
            vec![]
        }
        fn run_detail(&self, run_id: &str) -> Option<RunDetail> {
            if run_id != "r1" {
                return None;
            }
            Some(RunDetail {
                run: RunView {
                    run_id: "r1".into(),
                    session_id: "s1".into(),
                    state: "finished".into(),
                    started_at: Some(0),
                    finished_at: Some(20),
                },
                timeline: vec![TimelineEntry {
                    at: 11,
                    kind: "log".into(),
                    text: "started".into(),
                }],
            })
        }
        fn queue(&self) -> Vec<QueueItem> {
            vec![]
        }
        fn scheduled(&self) -> Vec<ScheduledItem> {
            vec![]
        }
        fn users(&self) -> Vec<UserView> {
            vec![]
        }
        fn approvals(&self) -> Vec<ApprovalView> {
            vec![]
        }
        fn capabilities(&self) -> Vec<CapabilityView> {
            vec![]
        }
        fn readiness(&self) -> ReadinessReportView {
            ReadinessReportView {
                ready: false,
                blocking_failures: 1,
                checks: vec![ReadinessCheckView {
                    module: "assistant-db".into(),
                    name: "migrations".into(),
                    status: "pass".into(),
                    detail: "head".into(),
                }],
            }
        }
        fn specialists(&self) -> Vec<SpecialistStatusView> {
            vec![]
        }
    }

    fn app() -> FakeApp {
        FakeApp {
            group_name: "orchestrator".into(),
        }
    }

    fn router() -> Router<FakeApp> {
        let mut r = Router::new();
        register(&mut r);
        r
    }

    fn html(resp: &Response) -> String {
        String::from_utf8(resp.body.clone()).unwrap()
    }

    #[test]
    fn overview_renders_counts_and_is_html() {
        let resp = router().dispatch(&app(), &Request::new(Method::Get, "/"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("content-type"), Some("text/html; charset=utf-8"));
        let body = html(&resp);
        assert!(body.contains("assistant"));
        assert!(body.contains(">2<"), "groups count card");
        assert!(body.contains("class=\"active\""), "nav highlights overview");
    }

    #[test]
    fn group_name_is_html_escaped_against_stored_xss() {
        let mut a = app();
        a.group_name = "<script>alert(1)</script>".into();
        let resp = router().dispatch(&a, &Request::new(Method::Get, "/groups"));
        let body = html(&resp);
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<script>alert"));
    }

    #[test]
    fn run_page_renders_timeline_and_404s_when_missing() {
        let r = router();
        let ok = r.dispatch(&app(), &Request::new(Method::Get, "/runs/r1"));
        assert_eq!(ok.status, 200);
        assert!(html(&ok).contains("started"));

        let missing = r.dispatch(&app(), &Request::new(Method::Get, "/runs/nope"));
        assert_eq!(missing.status, 404);
        assert_eq!(missing.header("content-type"), Some("text/html; charset=utf-8"));
    }

    #[test]
    fn fmt_epoch_is_utc_iso() {
        assert_eq!(fmt_epoch(0), "1970-01-01 00:00:00Z");
        assert_eq!(fmt_epoch(1_700_000_000), "2023-11-14 22:13:20Z");
    }
}
