//! Method + path routing over the neutral [`crate::http`] model.
//!
//! Routes are registered against a path pattern whose segments are either
//! literals or `:name` captures (e.g. `/api/runs/:run_id`). The router is
//! generic over an application `State` the host owns (its bundle of provider
//! trait objects), so handlers are plain function pointers that take `&State` —
//! no captured closures, no lifetime gymnastics, and the crate links no domain
//! code. A path that matches but with the wrong method yields 405, not 404, so
//! a caller can tell "no such resource" from "wrong verb".

use crate::http::{Method, Request, Response};

/// Captured path parameters from a matched route.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    values: Vec<(String, String)>,
}

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A handler is a function from the app state, the request, and the matched
/// params to a response.
pub type Handler<S> = fn(&S, &Request, &Params) -> Response;

#[derive(Clone)]
enum Seg {
    Literal(String),
    Param(String),
}

struct Route<S> {
    method: Method,
    pattern: Vec<Seg>,
    handler: Handler<S>,
}

/// The route table. Build it once at startup; dispatch is a linear match in
/// registration order (the table is small and fixed).
pub struct Router<S> {
    routes: Vec<Route<S>>,
}

impl<S> Default for Router<S> {
    fn default() -> Self {
        Self { routes: Vec::new() }
    }
}

impl<S> Router<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler for a method + path pattern.
    pub fn route(&mut self, method: Method, pattern: &str, handler: Handler<S>) -> &mut Self {
        self.routes.push(Route {
            method,
            pattern: parse_pattern(pattern),
            handler,
        });
        self
    }

    /// Resolve a request to a handler and run it. Returns 404 when no path
    /// matches and 405 when a path matches but no registered method does.
    pub fn dispatch(&self, state: &S, req: &Request) -> Response {
        let segments = split_path(&req.path);
        let mut path_matched = false;
        for route in &self.routes {
            if let Some(params) = match_pattern(&route.pattern, &segments) {
                path_matched = true;
                if route.method == req.method {
                    return (route.handler)(state, req, &params);
                }
            }
        }
        if path_matched {
            Response::text(405, "method not allowed")
        } else {
            Response::text(404, "not found")
        }
    }
}

fn parse_pattern(pattern: &str) -> Vec<Seg> {
    split_path(pattern)
        .into_iter()
        .map(|s| {
            if let Some(name) = s.strip_prefix(':') {
                Seg::Param(name.to_string())
            } else {
                Seg::Literal(s)
            }
        })
        .collect()
}

/// Split a path into non-empty segments, so `/`, `//a`, and `/a/` normalize the
/// same way a pattern does.
fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn match_pattern(pattern: &[Seg], segments: &[String]) -> Option<Params> {
    if pattern.len() != segments.len() {
        return None;
    }
    let mut values = Vec::new();
    for (seg, actual) in pattern.iter().zip(segments.iter()) {
        match seg {
            Seg::Literal(lit) if lit == actual => {}
            Seg::Literal(_) => return None,
            Seg::Param(name) => values.push((name.clone(), actual.clone())),
        }
    }
    Some(Params { values })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct State {
        label: &'static str,
    }

    fn root(state: &State, _req: &Request, _p: &Params) -> Response {
        Response::text(200, state.label)
    }

    fn run_detail(_state: &State, _req: &Request, p: &Params) -> Response {
        Response::text(200, format!("run={}", p.get("run_id").unwrap()))
    }

    fn create_run(_state: &State, _req: &Request, _p: &Params) -> Response {
        Response::text(201, "created")
    }

    fn router() -> Router<State> {
        let mut r = Router::new();
        r.route(Method::Get, "/", root)
            .route(Method::Get, "/api/runs/:run_id", run_detail)
            .route(Method::Post, "/api/runs", create_run);
        r
    }

    #[test]
    fn dispatches_to_the_matching_route_and_passes_state() {
        let state = State { label: "overview" };
        let resp = router().dispatch(&state, &Request::new(Method::Get, "/"));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"overview");
    }

    #[test]
    fn captures_path_parameters() {
        let state = State { label: "x" };
        let resp = router().dispatch(&state, &Request::new(Method::Get, "/api/runs/abc123"));
        assert_eq!(resp.body, b"run=abc123");
    }

    #[test]
    fn unknown_path_is_404_wrong_method_is_405() {
        let state = State { label: "x" };
        let r = router();
        assert_eq!(r.dispatch(&state, &Request::new(Method::Get, "/nope")).status, 404);
        // /api/runs exists for POST; a GET on it is a method mismatch.
        assert_eq!(r.dispatch(&state, &Request::new(Method::Get, "/api/runs")).status, 405);
    }

    #[test]
    fn trailing_and_double_slashes_normalize() {
        let state = State { label: "x" };
        let r = router();
        assert_eq!(r.dispatch(&state, &Request::new(Method::Get, "/api/runs/abc/")).status, 200);
        assert_eq!(r.dispatch(&state, &Request::new(Method::Get, "//api//runs//abc")).status, 200);
    }
}
