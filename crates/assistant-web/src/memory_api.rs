//! The memory browser/editor's HTTP surface.
//!
//! Reads (`GET`) browse a directory or fetch a file; mutations (`POST` to
//! create, `PUT` to save) carry a JSON body. Every error from the filesystem
//! layer maps to a precise status — a path that escapes or hits a protected
//! surface is a `400`, a missing file `404`, an exclusive-create clash or a
//! stale optimistic-concurrency version `409`, an oversized body `413` — so a
//! client can react without scraping prose. The handlers are generic over a
//! host `A: MemoryApp`, keeping the crate domain-free.

use serde::Deserialize;

use crate::http::{Method, Request, Response};
use crate::memoryfs::{MemoryEditor, MemoryFsError};
use crate::router::{Params, Router};

/// The host exposes its bound [`MemoryEditor`] through this trait so the memory
/// routes can be registered onto the shared router.
pub trait MemoryApp {
    fn memory(&self) -> &MemoryEditor;
}

#[derive(Deserialize)]
struct CreateBody {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct SaveBody {
    path: String,
    content: String,
    version: String,
}

/// Register the memory browser/editor routes.
pub fn register<A: MemoryApp>(router: &mut Router<A>) {
    router
        .route(Method::Get, "/api/memory", list)
        .route(Method::Get, "/api/memory/file", read)
        .route(Method::Post, "/api/memory/file", create)
        .route(Method::Put, "/api/memory/file", save);
}

fn list<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let dir = req.query_param("dir").unwrap_or("");
    match app.memory().list(dir) {
        Ok(entries) => Response::json(200, &entries),
        Err(e) => error_response(&e),
    }
}

fn read<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let path = match req.query_param("path") {
        Some(p) => p,
        None => return bad_request("missing path query parameter"),
    };
    match app.memory().read(path) {
        Ok(file) => Response::json(200, &file),
        Err(e) => error_response(&e),
    }
}

fn create<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    if let Some(resp) = reject_if_oversized(app, req) {
        return resp;
    }
    let body: CreateBody = match parse_body(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match app.memory().create(&body.path, &body.content) {
        Ok(res) => Response::json(201, &res),
        Err(e) => error_response(&e),
    }
}

fn save<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    if let Some(resp) = reject_if_oversized(app, req) {
        return resp;
    }
    let body: SaveBody = match parse_body(req) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    match app.memory().save(&body.path, &body.content, &body.version) {
        Ok(res) => Response::json(200, &res),
        Err(e) => error_response(&e),
    }
}

/// Cap the raw request body before it is parsed, so a giant payload is rejected
/// without being buffered into a `String` first. The ceiling is the editor's
/// per-file byte limit times the worst-case JSON string-escape blow-up (6×, for
/// all-control-char content) plus headroom for the framing fields — generous
/// enough never to reject a legitimate at-the-limit save, but still finite.
fn reject_if_oversized<A: MemoryApp>(app: &A, req: &Request) -> Option<Response> {
    let cap = app
        .memory()
        .max_bytes()
        .saturating_mul(6)
        .saturating_add(64 * 1024);
    if req.body.len() as u64 > cap {
        return Some(Response::json(
            413,
            &serde_json::json!({
                "error": format!("request body of {} bytes exceeds the limit", req.body.len())
            }),
        ));
    }
    None
}

fn parse_body<T: for<'de> Deserialize<'de>>(req: &Request) -> Result<T, Response> {
    serde_json::from_slice(&req.body).map_err(|e| bad_request(&format!("invalid JSON body: {e}")))
}

fn bad_request(detail: &str) -> Response {
    Response::json(400, &serde_json::json!({ "error": detail }))
}

/// Map a filesystem-layer error to a status + JSON error body. Every variant
/// but `Io` echoes only the operator-supplied relative path, so its `Display`
/// is safe to return; `Io` wraps a raw `std::io::Error` whose text can leak
/// absolute paths or host detail, so the 500 carries a fixed generic message.
fn error_response(e: &MemoryFsError) -> Response {
    match e {
        MemoryFsError::PathEscape { .. }
        | MemoryFsError::OutsideRoot { .. }
        | MemoryFsError::Symlink { .. }
        | MemoryFsError::SpecialFile { .. }
        | MemoryFsError::ExtensionNotAllowed { .. }
        | MemoryFsError::Protected { .. } => {
            Response::json(400, &serde_json::json!({ "error": e.to_string() }))
        }
        MemoryFsError::NotFound { .. } => {
            Response::json(404, &serde_json::json!({ "error": e.to_string() }))
        }
        MemoryFsError::AlreadyExists { .. } | MemoryFsError::VersionConflict { .. } => {
            Response::json(409, &serde_json::json!({ "error": e.to_string() }))
        }
        MemoryFsError::TooLarge { .. } => {
            Response::json(413, &serde_json::json!({ "error": e.to_string() }))
        }
        MemoryFsError::Io { .. } => {
            Response::json(500, &serde_json::json!({ "error": "internal error" }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memoryfs::EditorPolicy;

    const GOOD_FM: &str = "---\nmemory_id: mem_1\nowner_agent_group_id: ag_x\nscope: all_chats\nsource_type: user_said\nconfidence: high\nreuse_policy: same_scope\nretention: normal\n---\nbody\n";

    struct App {
        editor: MemoryEditor,
    }

    impl MemoryApp for App {
        fn memory(&self) -> &MemoryEditor {
            &self.editor
        }
    }

    fn app() -> (tempfile::TempDir, Router<App>, App) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let app = App {
            editor: MemoryEditor::new(root, EditorPolicy::default()),
        };
        let mut r = Router::new();
        register(&mut r);
        (dir, r, app)
    }

    fn json(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    #[test]
    fn create_read_and_save_round_trip_over_http() {
        let (_d, r, app) = app();

        let create_req = Request::new(Method::Post, "/api/memory/file")
            .with_body(serde_json::to_vec(&serde_json::json!({"path":"n.md","content":GOOD_FM})).unwrap());
        let created = r.dispatch(&app, &create_req);
        assert_eq!(created.status, 201);
        assert_eq!(json(&created)["rag_eligible"], true);

        let read_req = Request::new(Method::Get, "/api/memory/file").with_query("path=n.md");
        let read = r.dispatch(&app, &read_req);
        assert_eq!(read.status, 200);
        let version = json(&read)["version"].as_str().unwrap().to_string();

        let updated = format!("{GOOD_FM}extra\n");
        let save_req = Request::new(Method::Put, "/api/memory/file").with_body(
            serde_json::to_vec(&serde_json::json!({"path":"n.md","content":updated,"version":version})).unwrap(),
        );
        let saved = r.dispatch(&app, &save_req);
        assert_eq!(saved.status, 200);
    }

    #[test]
    fn traversal_is_a_400_and_missing_file_is_a_404() {
        let (_d, r, app) = app();
        let bad = Request::new(Method::Post, "/api/memory/file")
            .with_body(serde_json::to_vec(&serde_json::json!({"path":"../evil.md","content":GOOD_FM})).unwrap());
        assert_eq!(r.dispatch(&app, &bad).status, 400);

        let missing = Request::new(Method::Get, "/api/memory/file").with_query("path=ghost.md");
        assert_eq!(r.dispatch(&app, &missing).status, 404);
    }

    #[test]
    fn duplicate_create_is_409_and_stale_save_is_409() {
        let (_d, r, app) = app();
        let body = serde_json::to_vec(&serde_json::json!({"path":"n.md","content":GOOD_FM})).unwrap();
        assert_eq!(
            r.dispatch(&app, &Request::new(Method::Post, "/api/memory/file").with_body(body.clone())).status,
            201
        );
        assert_eq!(
            r.dispatch(&app, &Request::new(Method::Post, "/api/memory/file").with_body(body)).status,
            409
        );

        let stale = Request::new(Method::Put, "/api/memory/file").with_body(
            serde_json::to_vec(&serde_json::json!({"path":"n.md","content":GOOD_FM,"version":"stale"})).unwrap(),
        );
        assert_eq!(r.dispatch(&app, &stale).status, 409);
    }

    #[test]
    fn protected_surface_create_is_a_400() {
        let (_d, r, app) = app();
        let req = Request::new(Method::Post, "/api/memory/file").with_body(
            serde_json::to_vec(&serde_json::json!({"path":"standing_instructions.md","content":GOOD_FM})).unwrap(),
        );
        let resp = r.dispatch(&app, &req);
        assert_eq!(resp.status, 400);
        assert!(json(&resp)["error"].as_str().unwrap().contains("protected"));
    }

    #[test]
    fn malformed_json_body_is_a_400() {
        let (_d, r, app) = app();
        let req = Request::new(Method::Post, "/api/memory/file").with_body(b"not json".to_vec());
        assert_eq!(r.dispatch(&app, &req).status, 400);
    }

    #[test]
    fn an_oversized_request_body_is_rejected_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let app = App {
            editor: MemoryEditor::new(
                root,
                EditorPolicy {
                    max_bytes: 16,
                    ..EditorPolicy::default()
                },
            ),
        };
        let mut r = Router::new();
        register(&mut r);
        // Cap is 16*6 + 64KiB; exceed it with a body that is not even valid JSON,
        // proving the size gate fires ahead of the parse.
        let huge = vec![b'x'; 16 * 6 + 64 * 1024 + 1];
        let req = Request::new(Method::Post, "/api/memory/file").with_body(huge);
        assert_eq!(r.dispatch(&app, &req).status, 413);
    }
}
