//! A neutral request/response model.
//!
//! The crate is transport-free: a real server (or a test) parses the wire into a
//! [`Request`] and serializes a [`Response`] back. Keeping the model independent
//! of any HTTP library lets the routing, auth, and page logic be exercised
//! directly in unit tests, and lets the host pick whatever listener it wants.

use serde::Serialize;

/// The HTTP methods the UI uses. Anything else is treated as unroutable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }

    /// A method that changes server state; used to gate origin checks.
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Method::Get)
    }
}

/// One inbound request, already split into method, path, raw query, headers, and
/// body. `path` excludes the query string; `query` is the raw `a=1&b=2` form.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Case-insensitive header lookup (HTTP header names are case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The first value of a query parameter, if present.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

/// One response: a status code, headers, and a body. Construct via the helpers
/// so the content type stays in sync with the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// A JSON body. Serialization failure becomes a 500 with a JSON error so a
    /// handler can stay infallible.
    pub fn json<T: Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_vec_pretty(value) {
            Ok(body) => Response::new(status)
                .with_header("Content-Type", "application/json")
                .with_body_bytes(body),
            Err(e) => Response::new(500)
                .with_header("Content-Type", "application/json")
                .with_body_bytes(
                    format!("{{\"error\":\"serialize failed: {e}\"}}").into_bytes(),
                ),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response::new(status)
            .with_header("Content-Type", "text/plain; charset=utf-8")
            .with_body_bytes(body.into().into_bytes())
    }

    /// A 303 redirect (used after a one-time token exchange so the browser
    /// re-requests the clean URL).
    pub fn redirect(location: impl Into<String>) -> Self {
        Response::new(303).with_header("Location", location)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn with_body_bytes(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_parse_is_case_insensitive_and_bounded() {
        assert_eq!(Method::parse("get"), Some(Method::Get));
        assert_eq!(Method::parse("POST"), Some(Method::Post));
        assert_eq!(Method::parse("TRACE"), None);
        assert!(Method::Post.is_mutating());
        assert!(!Method::Get.is_mutating());
    }

    #[test]
    fn header_lookup_ignores_case() {
        let req = Request::new(Method::Get, "/x").with_header("Authorization", "Bearer t");
        assert_eq!(req.header("authorization"), Some("Bearer t"));
        assert_eq!(req.header("missing"), None);
    }

    #[test]
    fn query_param_reads_named_values() {
        let req = Request::new(Method::Get, "/x").with_query("a=1&token=z&b=2");
        assert_eq!(req.query_param("token"), Some("z"));
        assert_eq!(req.query_param("b"), Some("2"));
        assert_eq!(req.query_param("c"), None);
    }

    #[test]
    fn json_response_sets_content_type() {
        let resp = Response::json(200, &serde_json::json!({"ok": true}));
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("content-type"), Some("application/json"));
        assert!(String::from_utf8(resp.body).unwrap().contains("\"ok\""));
    }

    #[test]
    fn redirect_carries_a_location() {
        let resp = Response::redirect("/dashboard");
        assert_eq!(resp.status, 303);
        assert_eq!(resp.header("location"), Some("/dashboard"));
    }
}
