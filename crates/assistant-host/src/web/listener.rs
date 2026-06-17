//! The host-side HTTP listener for the operator web UI.
//!
//! `assistant-web` is transport-free by design (no async, no socket code): it
//! supplies the router, the auth choke point, the page handlers, and the memory
//! editor as pure synchronous logic. This module is the one piece the platform
//! deferred — a real listener. It is deliberately built on `std::net` only (no
//! tokio, no axum, no extra deps) to match the rest of the platform's
//! synchronous, dependency-light style, so it compiles in the default offline
//! build and is exercised over a real loopback socket in tests.
//!
//! The loop is single-threaded and handles one request per connection
//! (`Connection: close`): the operator UI is a low-traffic, loopback-only admin
//! surface, so sequential handling is sufficient and keeps the surface small.
//! Every request passes [`assistant_web::authenticate`] before dispatch, so a
//! route is private by default.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use assistant_web::{authenticate, AuthDecision, Method, Request, Response, Router, ServerSettings, TokenStore};

/// The largest request head (request line + headers) we will buffer before
/// giving up with a 400. Generous for a handful of headers; bounds memory.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// The largest request body we will read. The memory editor caps far tighter
/// (see `memory_api::reject_if_oversized`); this is the outer backstop so a
/// single connection can't make us buffer unbounded bytes.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Per-connection read timeout, so a slow or stalled client can't wedge the
/// single-threaded accept loop.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Bind the listener described by `settings`. Split from [`serve`] so a test can
/// bind to an ephemeral port (`port: 0`) and learn the assigned address via
/// `local_addr()` before serving.
pub fn bind(settings: &ServerSettings) -> io::Result<TcpListener> {
    TcpListener::bind((settings.bind.as_str(), settings.port))
}

/// Serve requests until `stop()` returns true. Authenticates every request, then
/// dispatches the allowed ones through `router` against `state`. Runs on the
/// calling thread (the serve-web process does nothing else).
pub fn serve<S>(
    listener: TcpListener,
    settings: &ServerSettings,
    store: &TokenStore,
    router: &Router<S>,
    state: &S,
    stop: &dyn Fn() -> bool,
) -> io::Result<()> {
    // Non-blocking accept so we can observe `stop` between connections instead
    // of parking forever in `accept()`.
    listener.set_nonblocking(true)?;
    while !stop() {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // Handle synchronously. A per-connection error (malformed
                // request, client hangup) is logged and never takes down the
                // loop.
                if let Err(e) = handle_connection(stream, settings, store, router, state) {
                    eprintln!("web: connection error: {e}");
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn handle_connection<S>(
    mut stream: TcpStream,
    settings: &ServerSettings,
    store: &TokenStore,
    router: &Router<S>,
    state: &S,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;

    let response = match read_request(&mut stream) {
        Ok(req) => match authenticate(store, settings, &req) {
            AuthDecision::Allow => router.dispatch(state, &req),
            AuthDecision::Reject(resp) | AuthDecision::Exchange(resp) => resp,
        },
        Err(ReadError::TooLarge) => Response::text(413, "request too large"),
        Err(ReadError::Malformed) => Response::text(400, "malformed request"),
        Err(ReadError::Io(e)) => return Err(e),
    };

    write_response(&mut stream, &response)?;
    stream.flush()
}

enum ReadError {
    Malformed,
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Read and parse one HTTP/1.1 request: the head up to the blank line, then the
/// `Content-Length` body if present. Bounded by `MAX_HEAD_BYTES`/`MAX_BODY_BYTES`.
fn read_request<R: Read>(stream: &mut R) -> Result<Request, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Read until we see the end-of-head marker, capping total head bytes.
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(ReadError::TooLarge);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            // Connection closed before a full head arrived.
            return Err(ReadError::Malformed);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| ReadError::Malformed)?;
    let parsed = parse_head(head).ok_or(ReadError::Malformed)?;

    // Any bytes already read past the head are the start of the body.
    let body_start = head_end + 4;
    let mut body = buf[body_start..].to_vec();

    if let Some(len) = content_length(&parsed.headers)? {
        if len > MAX_BODY_BYTES {
            return Err(ReadError::TooLarge);
        }
        while body.len() < len {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(ReadError::Malformed);
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    }

    let mut req = Request::new(parsed.method, parsed.path).with_query(parsed.query);
    for (k, v) in parsed.headers {
        req = req.with_header(k, v);
    }
    Ok(req.with_body(body))
}

/// The request line + headers, parsed off the wire.
struct ParsedHead {
    method: Method,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

/// Parse the request line + header lines (the head, without the trailing blank
/// line). Returns `None` for anything malformed or an unsupported method.
fn parse_head(head: &str) -> Option<ParsedHead> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = Method::parse(parts.next()?)?;
    let target = parts.next()?;
    // Require an HTTP version token so a junk line isn't accepted as a request.
    let _version = parts.next()?;

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Some(ParsedHead {
        method,
        path,
        query,
        headers,
    })
}

fn content_length(headers: &[(String, String)]) -> Result<Option<usize>, ReadError> {
    match headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        None => Ok(None),
        Some((_, v)) => v
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| ReadError::Malformed),
    }
}

fn write_response<W: Write>(stream: &mut W, resp: &Response) -> io::Result<()> {
    let reason = reason_phrase(resp.status);
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason);
    for (name, value) in &resp.headers {
        // Header values are produced by assistant-web (status text, JSON content
        // type, redirect locations) — not raw request echo — so they carry no
        // attacker-controlled CRLF.
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_web::{Params, WebToken};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct TestState;

    fn ping(_s: &TestState, _req: &Request, _p: &Params) -> Response {
        Response::text(200, "pong")
    }

    fn router() -> Router<TestState> {
        let mut r = Router::new();
        r.route(Method::Get, "/ping", ping);
        r
    }

    #[test]
    fn parses_a_get_with_query_and_headers() {
        let raw = b"GET /ping?a=1 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\r\n";
        let req = read_request(&mut &raw[..]).ok().unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/ping");
        assert_eq!(req.query, "a=1");
        assert_eq!(req.header("authorization"), Some("Bearer t"));
    }

    #[test]
    fn parses_a_post_body_by_content_length() {
        let raw = b"POST /echo HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let req = read_request(&mut &raw[..]).ok().unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn malformed_request_line_is_rejected() {
        let raw = b"GARBAGE\r\n\r\n";
        assert!(matches!(
            read_request(&mut &raw[..]),
            Err(ReadError::Malformed)
        ));
    }

    fn store() -> TokenStore {
        TokenStore::for_token(&WebToken::from_secret("good-token"), 1)
    }

    /// Bind to an ephemeral port, serve on a thread, drive real TCP requests.
    #[test]
    fn loopback_round_trip_enforces_auth_then_dispatches() {
        let settings = ServerSettings {
            bind: "127.0.0.1".to_string(),
            port: 0,
            allowed_origins: Vec::new(),
        };
        let listener = bind(&settings).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let settings = ServerSettings {
                bind: "127.0.0.1".to_string(),
                port: addr.port(),
                allowed_origins: Vec::new(),
            };
            let router = router();
            serve(
                listener,
                &settings,
                &store(),
                &router,
                &TestState,
                &|| stop_thread.load(Ordering::SeqCst),
            )
            .unwrap();
        });

        // Missing credential → 401.
        assert!(request(addr, "GET /ping HTTP/1.1\r\nHost: x\r\n\r\n").starts_with("HTTP/1.1 401"));
        // Valid bearer → 200 pong.
        let ok = request(
            addr,
            "GET /ping HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer good-token\r\n\r\n",
        );
        assert!(ok.starts_with("HTTP/1.1 200"));
        assert!(ok.ends_with("pong"));
        // Unknown path with a valid token → 404.
        assert!(request(
            addr,
            "GET /nope HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer good-token\r\n\r\n"
        )
        .starts_with("HTTP/1.1 404"));

        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
    }

    /// Send one raw request, read the whole response (the server closes the
    /// connection, so read-to-end terminates).
    fn request(addr: std::net::SocketAddr, raw: &str) -> String {
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        conn.read_to_string(&mut out).unwrap();
        out
    }
}
