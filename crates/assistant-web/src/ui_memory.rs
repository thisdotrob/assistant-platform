//! Server-rendered, clickable memory browser/editor.
//!
//! These pages are the human-facing twin of the JSON memory API
//! ([`crate::memory_api`]): they browse a directory, open a file in a textarea,
//! and save it back through the same safety-checked [`MemoryEditor`]. They are
//! plain HTML forms — no client-side script — so a save is an ordinary
//! `application/x-www-form-urlencoded` POST. Those POSTs are state-changing and
//! cookie-authenticated, so they ride the same `Origin` CSRF gate as the JSON
//! mutations (a same-origin form submission carries an allowlisted `Origin`; a
//! foreign site cannot forge one).
//!
//! Every interpolated value — path, content, version, error text — is escaped
//! via [`crate::ui::esc`], including inside HTML attributes, so operator/agent
//! data cannot break out of the markup.

use std::collections::HashMap;

use crate::http::{Method, Request, Response};
use crate::memory_api::MemoryApp;
use crate::memoryfs::MemoryFsError;
use crate::router::{Params, Router};
use crate::ui::{esc, layout, table};

const ACTIVE: &str = "/memory";

/// Register the clickable memory pages. Reads are `GET`; the two mutations
/// (`save`, `create`) are `POST` so they pass through the cookie + `Origin`
/// CSRF gate in [`crate::server`].
pub fn register<A: MemoryApp>(router: &mut Router<A>) {
    router
        .route(Method::Get, "/memory", browse)
        .route(Method::Get, "/memory/edit", edit_page)
        .route(Method::Get, "/memory/new", new_page)
        .route(Method::Post, "/memory/save", save_action)
        .route(Method::Post, "/memory/create", create_action);
}

fn browse<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let dir = form_decode(req.query_param("dir").unwrap_or(""));
    let body = match app.memory().list(&dir) {
        Ok(entries) => {
            let rows = entries
                .into_iter()
                .map(|e| {
                    let href = if e.is_dir {
                        format!("/memory?dir={}", enc_path(&e.path))
                    } else {
                        format!("/memory/edit?path={}", enc_path(&e.path))
                    };
                    let name = leaf(&e.path);
                    let kind = if e.is_dir { "dir" } else { "file" };
                    let size = if e.is_dir {
                        "—".to_string()
                    } else {
                        format!("{} B", e.size)
                    };
                    vec![
                        format!("<a href=\"{}\">{}</a>", esc(&href), esc(name)),
                        kind.to_string(),
                        size,
                    ]
                })
                .collect();
            format!(
                "{crumb}<p><a class=\"btn\" href=\"/memory/new?dir={d}\">New file</a></p>{tbl}",
                crumb = breadcrumb(&dir),
                d = enc_path(&dir),
                tbl = table(&["Name", "Type", "Size"], rows),
            )
        }
        Err(e) => format!("{}{}", breadcrumb(&dir), note("bad", &describe(&e))),
    };
    let title = if dir.is_empty() {
        "Memory".to_string()
    } else {
        format!("Memory · {dir}")
    };
    Response::html(200, layout(ACTIVE, &title, &body))
}

fn edit_page<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let path = form_decode(req.query_param("path").unwrap_or(""));
    if path.is_empty() {
        return Response::html(400, layout(ACTIVE, "Edit", &note("bad", "missing path")));
    }
    match app.memory().read(&path) {
        Ok(file) => {
            let saved = req.query_param("saved").is_some();
            let banner = if saved {
                note("ok", "Saved.")
            } else {
                String::new()
            };
            let body = editor_form(&file.path, &file.content, &file.version, Some(file.rag_eligible), &banner);
            Response::html(200, layout(ACTIVE, &format!("Edit · {path}"), &body))
        }
        Err(e) => {
            let status = if matches!(e, MemoryFsError::NotFound { .. }) {
                404
            } else {
                400
            };
            let body = format!("{}{}", breadcrumb(parent_of(&path)), note("bad", &describe(&e)));
            Response::html(status, layout(ACTIVE, "Edit", &body))
        }
    }
}

fn new_page<A: MemoryApp>(_app: &A, req: &Request, _p: &Params) -> Response {
    let dir = form_decode(req.query_param("dir").unwrap_or(""));
    let prefill = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir.trim_end_matches('/'))
    };
    let body = format!(
        "{crumb}\
<form method=\"post\" action=\"/memory/create\">\
<div class=\"row\"><label>Path (relative to the memory root, must end in <code>.md</code>)</label>\
<input type=\"text\" name=\"path\" value=\"{p}\" placeholder=\"people/alice.md\"></div>\
<div class=\"row\"><label>Content</label><textarea name=\"content\" placeholder=\"---\nscope: ...\n---\n\"></textarea></div>\
<div class=\"row\"><button type=\"submit\">Create</button> \
<a class=\"btn ghost\" href=\"/memory?dir={d}\">Cancel</a></div>\
</form>",
        crumb = breadcrumb(&dir),
        p = esc(&prefill),
        d = enc_path(&dir),
    );
    Response::html(200, layout(ACTIVE, "New file", &body))
}

fn save_action<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let form = parse_form(&req.body);
    let path = form.get("path").cloned().unwrap_or_default();
    let version = form.get("version").cloned().unwrap_or_default();
    let content = form.get("content").cloned().unwrap_or_default();
    if path.is_empty() {
        return Response::html(400, layout(ACTIVE, "Save", &note("bad", "missing path")));
    }
    match app.memory().save(&path, &content, &version) {
        Ok(_) => Response::redirect(format!("/memory/edit?path={}&saved=1", enc_path(&path))),
        Err(MemoryFsError::VersionConflict { .. }) => {
            // Re-read for the current version so the operator can resubmit, but
            // keep their text in the box so no edit is lost.
            let (fresh_version, rag) = match app.memory().read(&path) {
                Ok(f) => (f.version, Some(f.rag_eligible)),
                Err(_) => (version, None),
            };
            let banner = note(
                "warn",
                "This file changed on disk since you opened it. Your text is preserved below \
                 and the version has been refreshed — saving again will overwrite the on-disk copy.",
            );
            let body = editor_form(&path, &content, &fresh_version, rag, &banner);
            Response::html(409, layout(ACTIVE, &format!("Edit · {path}"), &body))
        }
        Err(e) => {
            let body = editor_form(&path, &content, &version, None, &note("bad", &describe(&e)));
            Response::html(400, layout(ACTIVE, &format!("Edit · {path}"), &body))
        }
    }
}

fn create_action<A: MemoryApp>(app: &A, req: &Request, _p: &Params) -> Response {
    let form = parse_form(&req.body);
    let path = form.get("path").cloned().unwrap_or_default();
    let content = form.get("content").cloned().unwrap_or_default();
    match app.memory().create(&path, &content) {
        Ok(_) => Response::redirect(format!("/memory/edit?path={}&saved=1", enc_path(&path))),
        Err(e) => {
            // Re-render the new-file form with the attempted input and the reason.
            let body = format!(
                "{banner}\
<form method=\"post\" action=\"/memory/create\">\
<div class=\"row\"><label>Path</label><input type=\"text\" name=\"path\" value=\"{p}\"></div>\
<div class=\"row\"><label>Content</label><textarea name=\"content\">{c}</textarea></div>\
<div class=\"row\"><button type=\"submit\">Create</button> \
<a class=\"btn ghost\" href=\"/memory?dir={d}\">Cancel</a></div>\
</form>",
                banner = note("bad", &describe(&e)),
                p = esc(&path),
                c = esc(&content),
                d = enc_path(parent_of(&path)),
            );
            Response::html(400, layout(ACTIVE, "New file", &body))
        }
    }
}

/// The shared editor form, reused by the GET edit page and the POST re-renders
/// (conflict / error). `content` and `version` are echoed back so a failed save
/// never loses the operator's text.
fn editor_form(path: &str, content: &str, version: &str, rag: Option<bool>, banner: &str) -> String {
    let status = match rag {
        Some(true) => "<span class=\"badge ok\">RAG eligible</span>".to_string(),
        Some(false) => {
            "<span class=\"badge neutral\">not RAG-eligible (missing front-matter)</span>".to_string()
        }
        None => String::new(),
    };
    format!(
        "{banner}\
<div class=\"crumb\"><a href=\"/memory?dir={parent}\">← back to folder</a> · <code>{pathdisp}</code> {status}</div>\
<form method=\"post\" action=\"/memory/save\">\
<input type=\"hidden\" name=\"path\" value=\"{pathattr}\">\
<input type=\"hidden\" name=\"version\" value=\"{ver}\">\
<div class=\"row\"><textarea name=\"content\">{content}</textarea></div>\
<div class=\"row\"><button type=\"submit\">Save</button> \
<a class=\"btn ghost\" href=\"/memory?dir={parent}\">Cancel</a></div>\
</form>",
        parent = enc_path(parent_of(path)),
        pathdisp = esc(path),
        pathattr = esc(path),
        ver = esc(version),
        content = esc(content),
    )
}

/// A clickable breadcrumb from the root to `dir` (each segment links to its own
/// browse page).
fn breadcrumb(dir: &str) -> String {
    let mut out = String::from("<div class=\"crumb\"><a href=\"/memory\">root</a>");
    let mut acc = String::new();
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        out.push_str(&format!(
            " / <a href=\"/memory?dir={}\">{}</a>",
            enc_path(&acc),
            esc(seg)
        ));
    }
    out.push_str("</div>");
    out
}

/// A success/warning/error banner. `kind` is `ok`, `warn`, or `bad`.
fn note(kind: &str, msg: &str) -> String {
    format!("<div class=\"note {kind}\">{}</div>", esc(msg))
}

/// The on-screen leaf name of a relative path (the part after the last `/`).
fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The parent directory of a relative path (`""` for a top-level entry).
fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Map a filesystem-layer error to operator-facing text. As in the JSON surface,
/// every variant but `Io` echoes only the relative path; `Io` is collapsed to a
/// generic message so a raw `std::io::Error` cannot leak host paths.
fn describe(e: &MemoryFsError) -> String {
    match e {
        MemoryFsError::Io { .. } => "internal error".to_string(),
        other => other.to_string(),
    }
}

/// Parse an `application/x-www-form-urlencoded` body into a field map.
fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let s = String::from_utf8_lossy(body);
    let mut map = HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(form_decode(k), form_decode(v));
    }
    map
}

/// Decode one urlencoded token: `+` → space and `%XX` → byte, then interpret the
/// result as UTF-8 (lossily, so a malformed escape can never panic).
fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a relative path for use in a query string, leaving the
/// unreserved set and `/` (the path separator) intact.
fn enc_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memoryfs::{EditorPolicy, MemoryEditor};

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

    fn html(resp: &Response) -> String {
        String::from_utf8(resp.body.clone()).unwrap()
    }

    #[test]
    fn browse_lists_files_as_edit_links() {
        let (_d, r, app) = app();
        app.editor.create("a.md", GOOD_FM).unwrap();
        let resp = r.dispatch(&app, &Request::new(Method::Get, "/memory"));
        assert_eq!(resp.status, 200);
        let body = html(&resp);
        assert!(body.contains("/memory/edit?path=a.md"));
        assert!(body.contains("New file"));
    }

    #[test]
    fn edit_page_shows_content_and_version_then_404s_when_missing() {
        let (_d, r, app) = app();
        app.editor.create("n.md", GOOD_FM).unwrap();
        let ok = r.dispatch(
            &app,
            &Request::new(Method::Get, "/memory/edit").with_query("path=n.md"),
        );
        assert_eq!(ok.status, 200);
        let body = html(&ok);
        assert!(body.contains("<textarea"));
        assert!(body.contains("name=\"version\""));
        assert!(body.contains("body")); // file content rendered

        let missing = r.dispatch(
            &app,
            &Request::new(Method::Get, "/memory/edit").with_query("path=ghost.md"),
        );
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn create_then_save_round_trips_via_forms() {
        let (_d, r, app) = app();

        // Create via the HTML form (urlencoded body).
        let create = r.dispatch(
            &app,
            &Request::new(Method::Post, "/memory/create")
                .with_body(urlencode(&[("path", "note.md"), ("content", GOOD_FM)])),
        );
        assert_eq!(create.status, 303);
        assert_eq!(
            create.header("location"),
            Some("/memory/edit?path=note.md&saved=1")
        );

        // The editor now reads it back with a version.
        let version = app.editor.read("note.md").unwrap().version;
        let updated = format!("{GOOD_FM}more\n");
        let save = r.dispatch(
            &app,
            &Request::new(Method::Post, "/memory/save").with_body(urlencode(&[
                ("path", "note.md"),
                ("version", &version),
                ("content", &updated),
            ])),
        );
        assert_eq!(save.status, 303);
        assert_eq!(app.editor.read("note.md").unwrap().content, updated);
    }

    #[test]
    fn a_stale_save_re_renders_with_a_conflict_and_keeps_the_text() {
        let (_d, r, app) = app();
        app.editor.create("n.md", GOOD_FM).unwrap();
        let mine = format!("{GOOD_FM}my unsaved edit\n");
        let resp = r.dispatch(
            &app,
            &Request::new(Method::Post, "/memory/save").with_body(urlencode(&[
                ("path", "n.md"),
                ("version", "stale-version"),
                ("content", &mine),
            ])),
        );
        assert_eq!(resp.status, 409);
        let body = html(&resp);
        assert!(body.contains("changed on disk"));
        assert!(body.contains("my unsaved edit"), "operator text preserved");
        // The refreshed (real) version is embedded so a resubmit can succeed.
        let real = app.editor.read("n.md").unwrap().version;
        assert!(body.contains(&real));
    }

    #[test]
    fn content_is_escaped_against_xss_in_the_editor() {
        let (_d, r, app) = app();
        let evil = "<script>alert(1)</script>\n";
        app.editor.create("x.md", evil).unwrap();
        let resp = r.dispatch(
            &app,
            &Request::new(Method::Get, "/memory/edit").with_query("path=x.md"),
        );
        let body = html(&resp);
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<script>alert"));
    }

    #[test]
    fn form_decode_handles_escapes_and_plus() {
        assert_eq!(form_decode("a+b"), "a b");
        assert_eq!(form_decode("line%0Abreak"), "line\nbreak");
        assert_eq!(form_decode("%2Fslash"), "/slash");
        // A malformed escape is left literal rather than panicking.
        assert_eq!(form_decode("100%"), "100%");
    }

    #[test]
    fn enc_path_keeps_slashes_and_escapes_the_rest() {
        assert_eq!(enc_path("people/alice.md"), "people/alice.md");
        assert_eq!(enc_path("a b&c"), "a%20b%26c");
    }

    fn urlencode(pairs: &[(&str, &str)]) -> Vec<u8> {
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc_form(k), enc_form(v)))
            .collect::<Vec<_>>()
            .join("&")
            .into_bytes()
    }

    // Test-only encoder: percent-encode everything outside the unreserved set
    // (so `/`, newlines, etc. round-trip through `form_decode`).
    fn enc_form(s: &str) -> String {
        let mut out = String::new();
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
}
