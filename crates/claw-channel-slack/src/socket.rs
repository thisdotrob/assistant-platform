//! Slack Socket Mode inbound listener.
//!
//! Socket Mode is Slack's inbound transport: `apps.connections.open` (with the
//! app-level token) yields a WSS URL, the app dials it, and Slack pushes event
//! envelopes down the socket. Each envelope must be acknowledged within ~3s by
//! echoing its `envelope_id`; the connection rotates periodically, so the
//! listener must reconnect.
//!
//! The protocol logic — envelope classification, the mandatory ACK, reconnect,
//! and the hand-off of the inner event to [`crate::event::normalize`] — lives
//! here in the default build and is exercised entirely offline against an
//! injected [`SocketOpener`]/[`SocketConn`]. The real websocket transport is a
//! thin [`tungstenite`] shell behind the non-default `socket-mode` feature, so
//! the offline gate never compiles a websocket stack or touches the network.
//!
//! Security: the host never holds a real app token. `apps.connections.open`
//! goes through the OneCLI proxy carrying only a placeholder Bearer (swapped for
//! the real `xapp-` on the wire), fed via curl's stdin config (`-K -`) like the
//! outbound adapter — nothing sensitive lands in argv, a log, or on disk.

use serde::{Deserialize, Serialize};

use claw_router::RoutingEvent;

use crate::event::{normalize, SlackEnvelope, SlackIdentity};

/// Why a Socket Mode connection could not be opened, read, or written.
#[derive(Debug)]
pub enum SocketError {
    /// `apps.connections.open` or the websocket dial failed. Retryable: the
    /// listener tries to open a fresh connection.
    Connect(String),
    /// The peer hung up or sent a Close frame mid-stream — reconnect.
    Closed,
    /// Transport I/O broke mid-stream — reconnect.
    Io(String),
    /// No frame arrived within the read window. Not an error: a yield point so
    /// the listener can run periodic work (the scheduler tick) between frames.
    Idle,
    /// Unrecoverable (e.g. the app-level token was rejected) — stop the listener.
    Fatal(String),
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(d) => write!(f, "could not open a Socket Mode connection: {d}"),
            Self::Closed => write!(f, "the Socket Mode connection closed"),
            Self::Io(d) => write!(f, "Socket Mode transport IO failed: {d}"),
            Self::Idle => write!(f, "no Socket Mode frame within the read window"),
            Self::Fatal(d) => write!(f, "Socket Mode cannot continue: {d}"),
        }
    }
}

impl std::error::Error for SocketError {}

/// One live Socket Mode websocket connection. The real impl wraps a tungstenite
/// websocket; tests script the frames.
pub trait SocketConn {
    /// Block for the next text frame from Slack, up to the connection's read
    /// window. `Err(Closed)`/`Err(Io)` mean the connection is gone and the
    /// listener should reconnect; `Err(Idle)` means the window elapsed with no
    /// frame — the listener ticks and reads again.
    fn read(&mut self) -> Result<String, SocketError>;
    /// Send a frame back to Slack (the envelope ACK).
    fn ack(&mut self, frame: &str) -> Result<(), SocketError>;
}

/// Opens Socket Mode connections (`apps.connections.open` + WSS dial). Kept
/// separate from [`SocketConn`] so reconnect is just "open another one".
pub trait SocketOpener {
    fn open(&mut self) -> Result<Box<dyn SocketConn>, SocketError>;
}

/// A Socket Mode envelope, classified for the listener loop.
#[derive(Debug)]
pub enum Incoming {
    /// The server's greeting after a successful connect. No ACK, nothing to do.
    Hello,
    /// Slack is about to drop this connection — reconnect.
    Disconnect { reason: String },
    /// An Events API envelope carrying an inner event to route. Must be ACKed.
    /// The event is boxed because [`SlackEnvelope`] dwarfs the other variants.
    Event {
        envelope_id: String,
        event: Box<SlackEnvelope>,
    },
    /// An envelope that must be ACKed but carries nothing we route (an
    /// Events API envelope with no inner event, a slash command, an
    /// interactive payload, etc.).
    AckOnly { envelope_id: String },
    /// A frame we don't recognize or can't parse — ignored.
    Ignored,
}

/// The wire shape of a Socket Mode envelope — only the fields the listener
/// inspects. Unknown fields are ignored.
#[derive(Deserialize)]
struct RawEnvelope {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    payload: Option<EventPayload>,
}

/// The `payload` of an `events_api` envelope is the Events API event wrapper;
/// the routable event nests under its `event` key.
#[derive(Deserialize)]
struct EventPayload {
    #[serde(default)]
    event: Option<SlackEnvelope>,
}

/// Classify a raw Socket Mode frame. Pure and offline-testable.
pub fn parse_incoming(frame: &str) -> Incoming {
    let raw: RawEnvelope = match serde_json::from_str(frame) {
        Ok(r) => r,
        Err(_) => return Incoming::Ignored,
    };
    match raw.kind.as_str() {
        "hello" => Incoming::Hello,
        "disconnect" => Incoming::Disconnect {
            reason: raw.reason.unwrap_or_default(),
        },
        "events_api" => match (raw.envelope_id, raw.payload.and_then(|p| p.event)) {
            (Some(envelope_id), Some(event)) => Incoming::Event {
                envelope_id,
                event: Box::new(event),
            },
            // An Events API envelope still wants its ACK even when there is no
            // inner event for us to route.
            (Some(envelope_id), None) => Incoming::AckOnly { envelope_id },
            (None, _) => Incoming::Ignored,
        },
        // slash_commands / interactive / future envelope types: ACK to keep
        // Slack from redelivering, but we don't route them yet.
        _ => match raw.envelope_id {
            Some(envelope_id) => Incoming::AckOnly { envelope_id },
            None => Incoming::Ignored,
        },
    }
}

/// The ACK frame Slack expects in reply to an envelope: just its `envelope_id`.
fn ack_frame(envelope_id: &str) -> String {
    #[derive(Serialize)]
    struct Ack<'a> {
        envelope_id: &'a str,
    }
    serde_json::to_string(&Ack { envelope_id }).expect("ack serialization is infallible")
}

/// Run the Socket Mode listener until `stop` returns true.
///
/// Opens a connection, pumps its frames, and reconnects whenever the connection
/// closes or Slack sends a `disconnect`. Each routed event is handed to `sink`
/// **after** its envelope has been ACKed, so the ~3s ACK window is honored
/// regardless of how long downstream processing takes. `tick` is invoked
/// whenever a read window elapses with no frame, so the caller can run periodic
/// work (the scheduler) on the same thread without a side thread racing the
/// per-channel containers. Returns `Err` only on an unrecoverable
/// [`SocketError::Fatal`] (e.g. a rejected app token).
pub fn run_listener(
    opener: &mut dyn SocketOpener,
    identity: &SlackIdentity,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(RoutingEvent),
    tick: &mut dyn FnMut(),
) -> Result<(), SocketError> {
    while !stop() {
        let mut conn = match opener.open() {
            Ok(conn) => conn,
            Err(SocketError::Fatal(detail)) => return Err(SocketError::Fatal(detail)),
            // Retryable open failure: loop and try again (the opener paces
            // reconnects so this is not a busy spin in production).
            Err(_) => continue,
        };
        pump(conn.as_mut(), identity, stop, sink, tick)?;
    }
    Ok(())
}

/// Drain one connection until it closes, Slack asks us to reconnect, or `stop`
/// trips. Returns `Ok` to signal "reconnect"; `Err(Fatal)` to stop the listener.
fn pump(
    conn: &mut dyn SocketConn,
    identity: &SlackIdentity,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(RoutingEvent),
    tick: &mut dyn FnMut(),
) -> Result<(), SocketError> {
    loop {
        if stop() {
            return Ok(());
        }
        let frame = match conn.read() {
            Ok(frame) => frame,
            Err(SocketError::Fatal(detail)) => return Err(SocketError::Fatal(detail)),
            // No frame this window: run periodic work, then read again on the
            // same connection.
            Err(SocketError::Idle) => {
                tick();
                continue;
            }
            // Closed / Io / Connect → this connection is done; reconnect.
            Err(_) => return Ok(()),
        };
        match parse_incoming(&frame) {
            Incoming::Hello | Incoming::Ignored => {}
            Incoming::Disconnect { .. } => return Ok(()),
            Incoming::Event { envelope_id, event } => {
                // ACK before any routing so Slack's ~3s window is met even if
                // the sink is slow.
                if let Some(err) = ack_or_reconnect(conn, &envelope_id)? {
                    return err;
                }
                if let Some(routed) = normalize(&event, identity) {
                    sink(routed);
                }
            }
            Incoming::AckOnly { envelope_id } => {
                if let Some(err) = ack_or_reconnect(conn, &envelope_id)? {
                    return err;
                }
            }
        }
    }
}

/// ACK an envelope. A failed ACK on a broken connection is a reconnect signal,
/// not a fatal error: `Ok(Some(Ok(())))` means "stop pumping, reconnect";
/// `Ok(None)` means the ACK went through; `Err(Fatal)` propagates.
#[allow(clippy::type_complexity)]
fn ack_or_reconnect(
    conn: &mut dyn SocketConn,
    envelope_id: &str,
) -> Result<Option<Result<(), SocketError>>, SocketError> {
    match conn.ack(&ack_frame(envelope_id)) {
        Ok(()) => Ok(None),
        Err(SocketError::Fatal(detail)) => Err(SocketError::Fatal(detail)),
        Err(_) => Ok(Some(Ok(()))),
    }
}

#[cfg(feature = "socket-mode")]
pub use real::TungsteniteOpener;

/// The live websocket transport. Compiled only under `socket-mode`; everything
/// above stays websocket-free so the default offline gate is unchanged.
#[cfg(feature = "socket-mode")]
mod real {
    use std::io::Write;
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::sync::Once;
    use std::time::Duration;

    use serde_json::Value;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{connect, Message, WebSocket};

    use super::{SocketConn, SocketError, SocketOpener};
    use crate::adapter::{curl_config, ProxyInjection};

    /// How long the `apps.connections.open` curl call may take.
    const OPEN_TIMEOUT_SECS: u32 = 10;

    /// Per-read window on the live socket. When it elapses with no frame, the
    /// read returns [`SocketError::Idle`] so the listener can run the scheduler
    /// tick. Short enough that scheduled work is checked promptly; long enough
    /// that idle reads don't busy-spin.
    const READ_WINDOW: Duration = Duration::from_secs(1);

    /// tungstenite pulls rustls 0.23 without a crypto provider, so rustls panics
    /// at the first TLS handshake unless one is installed process-wide. Install
    /// `ring` once before any WSS dial. The error is ignored: a provider may
    /// already be installed (idempotent across reconnects and a host that set one).
    fn ensure_crypto_provider() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    /// Opens real Socket Mode connections: `apps.connections.open` returns a WSS
    /// URL that tungstenite dials. The app token is injected on the wire by the
    /// OneCLI proxy (this opener carries only a placeholder); the WSS dial then
    /// goes directly to Slack (its URL carries a Slack-issued ticket, no token).
    pub struct TungsteniteOpener {
        injection: ProxyInjection,
        reconnect_delay: Duration,
        opened: bool,
    }

    impl TungsteniteOpener {
        pub fn via_proxy(injection: ProxyInjection) -> Self {
            Self {
                injection,
                reconnect_delay: Duration::from_secs(1),
                opened: false,
            }
        }

        /// `apps.connections.open` → the WSS URL. The call goes through the
        /// OneCLI proxy with a placeholder Bearer (swapped for the real app token
        /// on the wire); the proxy URL, CA, and placeholder are fed via curl's
        /// stdin config so nothing sensitive lands in argv/log/disk.
        fn open_url(&self) -> Result<String, SocketError> {
            let mut cmd = Command::new("curl");
            cmd.args([
                "-sS",
                "--max-time",
                &OPEN_TIMEOUT_SECS.to_string(),
                "-X",
                "POST",
                "-K",
                "-",
                "https://slack.com/api/apps.connections.open",
            ]);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| SocketError::Connect(format!("failed to launch curl: {e}")))?;
            {
                let mut config = child
                    .stdin
                    .take()
                    .ok_or_else(|| SocketError::Connect("curl stdin unavailable".to_string()))?;
                config
                    .write_all(curl_config(&self.injection).as_bytes())
                    .map_err(|e| SocketError::Connect(format!("failed to write curl config: {e}")))?;
            }

            let output = child
                .wait_with_output()
                .map_err(|e| SocketError::Connect(format!("curl io failed: {e}")))?;
            if !output.status.success() {
                return Err(SocketError::Connect(format!(
                    "curl exited with {}",
                    output.status
                )));
            }

            let value: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
                SocketError::Connect(format!("malformed apps.connections.open response: {e}"))
            })?;
            if value.get("ok").and_then(Value::as_bool) != Some(true) {
                let code = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                // A rejected app token won't fix itself on retry.
                return Err(SocketError::Fatal(format!(
                    "apps.connections.open failed: {code}"
                )));
            }
            value
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| SocketError::Connect("apps.connections.open returned no url".to_string()))
        }
    }

    impl SocketOpener for TungsteniteOpener {
        fn open(&mut self) -> Result<Box<dyn SocketConn>, SocketError> {
            // Pace reconnects (the first open is immediate); keeps a persistently
            // failing open from becoming a busy loop in `run_listener`.
            if self.opened {
                std::thread::sleep(self.reconnect_delay);
            }
            self.opened = true;

            ensure_crypto_provider();
            let url = self.open_url()?;
            let (ws, _response) = connect(url.as_str())
                .map_err(|e| SocketError::Connect(format!("websocket dial failed: {e}")))?;
            // Bound each read so the listener can tick between frames. Best-effort:
            // if the underlying stream isn't reachable for a timeout, reads stay
            // blocking and the scheduler simply isn't driven on this connection.
            if let Some(tcp) = tcp_stream(ws.get_ref()) {
                let _ = tcp.set_read_timeout(Some(READ_WINDOW));
            }
            Ok(Box::new(TungsteniteConn { ws }))
        }
    }

    /// Reach the underlying `TcpStream` behind tungstenite's `MaybeTlsStream` so a
    /// read timeout can be set. `MaybeTlsStream` is `#[non_exhaustive]`; any
    /// variant we can't unwrap yields `None` (timed reads disabled, blocking reads
    /// retained).
    fn tcp_stream(stream: &MaybeTlsStream<TcpStream>) -> Option<&TcpStream> {
        match stream {
            MaybeTlsStream::Plain(tcp) => Some(tcp),
            MaybeTlsStream::Rustls(tls) => Some(tls.get_ref()),
            _ => None,
        }
    }

    struct TungsteniteConn {
        ws: WebSocket<MaybeTlsStream<TcpStream>>,
    }

    impl SocketConn for TungsteniteConn {
        fn read(&mut self) -> Result<String, SocketError> {
            loop {
                match self.ws.read() {
                    Ok(Message::Text(text)) => return Ok(text.to_string()),
                    Ok(Message::Close(_)) => return Err(SocketError::Closed),
                    // Ping/Pong/Binary/raw frames: tungstenite auto-queues pong
                    // replies, and none of these carry a routable envelope, so
                    // wait for the next frame.
                    Ok(_) => continue,
                    Err(tungstenite::Error::ConnectionClosed)
                    | Err(tungstenite::Error::AlreadyClosed) => return Err(SocketError::Closed),
                    // The read window elapsed with no frame. tungstenite retains
                    // any partially-read frame in its buffer, so the next read
                    // resumes cleanly; surface it as a tick opportunity.
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Err(SocketError::Idle);
                    }
                    Err(e) => return Err(SocketError::Io(e.to_string())),
                }
            }
        }

        fn ack(&mut self, frame: &str) -> Result<(), SocketError> {
            self.ws
                .send(Message::Text(frame.to_string().into()))
                .map_err(|e| match e {
                    tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                        SocketError::Closed
                    }
                    other => SocketError::Io(other.to_string()),
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use claw_router::MessageState;

    #[derive(Default)]
    struct Log {
        opens: usize,
        /// Ordered trace of side effects: `ack:<envelope_id>` and `event:<chat_id>`.
        actions: Vec<String>,
        events: Vec<RoutingEvent>,
    }
    type Shared = Rc<RefCell<Log>>;

    /// Sentinel a scripted connection pops to yield [`SocketError::Idle`] (an
    /// elapsed read window) instead of a frame.
    const IDLE: &str = "__IDLE__";

    fn idle() -> String {
        IDLE.to_string()
    }

    struct FakeConn {
        frames: VecDeque<String>,
        log: Shared,
    }

    impl SocketConn for FakeConn {
        fn read(&mut self) -> Result<String, SocketError> {
            match self.frames.pop_front() {
                Some(ref frame) if frame == IDLE => Err(SocketError::Idle),
                Some(frame) => Ok(frame),
                // Script drained → behave like a closed connection (reconnect).
                None => Err(SocketError::Closed),
            }
        }

        fn ack(&mut self, frame: &str) -> Result<(), SocketError> {
            let value: serde_json::Value = serde_json::from_str(frame).unwrap();
            let id = value.get("envelope_id").and_then(|v| v.as_str()).unwrap();
            self.log.borrow_mut().actions.push(format!("ack:{id}"));
            Ok(())
        }
    }

    struct FakeOpener {
        scripts: VecDeque<Vec<String>>,
        log: Shared,
    }

    impl SocketOpener for FakeOpener {
        fn open(&mut self) -> Result<Box<dyn SocketConn>, SocketError> {
            match self.scripts.pop_front() {
                Some(frames) => {
                    self.log.borrow_mut().opens += 1;
                    Ok(Box::new(FakeConn {
                        frames: frames.into(),
                        log: self.log.clone(),
                    }))
                }
                // Scripts exhausted: signal the listener to stop. Fatal is the
                // only terminal SocketError, so the test opener reuses it as an
                // end-of-script marker that `drive` treats as normal completion.
                None => Err(SocketError::Fatal("test: scripts exhausted".to_string())),
            }
        }
    }

    fn identity() -> SlackIdentity {
        SlackIdentity {
            bot_user_id: "U_BOT".to_string(),
            self_bot_id: Some("B_SELF".to_string()),
        }
    }

    fn hello() -> String {
        r#"{"type":"hello","num_connections":1}"#.to_string()
    }

    fn disconnect() -> String {
        r#"{"type":"disconnect","reason":"warning"}"#.to_string()
    }

    fn msg(channel: &str, user: &str, ts: &str, text: &str) -> String {
        format!(r#"{{"type":"message","channel":"{channel}","user":"{user}","ts":"{ts}","text":"{text}"}}"#)
    }

    /// Wrap an inner Events API event in an `events_api` Socket Mode envelope.
    fn events_api(envelope_id: &str, inner: &str) -> String {
        format!(
            r#"{{"envelope_id":"{envelope_id}","type":"events_api","payload":{{"type":"event_callback","event":{inner}}}}}"#
        )
    }

    /// Drive the listener over the given per-connection frame scripts. The fake
    /// opener ends the run (via its end-of-script `Fatal`) once every script is
    /// opened and drained. Returns (opens, ordered actions, routed events).
    fn drive(scripts: Vec<Vec<String>>) -> (usize, Vec<String>, Vec<RoutingEvent>) {
        let log: Shared = Rc::new(RefCell::new(Log::default()));
        let mut opener = FakeOpener {
            scripts: scripts.into(),
            log: log.clone(),
        };
        // No external shutdown in these tests; termination comes from the opener.
        let stop = || false;
        let sink_log = log.clone();
        let mut sink = move |routed: RoutingEvent| {
            let mut l = sink_log.borrow_mut();
            l.actions.push(format!("event:{}", routed.chat_id));
            l.events.push(routed);
        };
        let mut tick = || {};

        match run_listener(&mut opener, &identity(), &stop, &mut sink, &mut tick) {
            Ok(()) => {}
            // The fake opener's end-of-script marker — expected, not a failure.
            Err(SocketError::Fatal(_)) => {}
            Err(other) => panic!("unexpected listener error: {other}"),
        }

        let mut l = log.borrow_mut();
        (l.opens, std::mem::take(&mut l.actions), std::mem::take(&mut l.events))
    }

    #[test]
    fn hello_is_neither_acked_nor_routed() {
        let (opens, actions, events) = drive(vec![vec![hello()]]);
        assert_eq!(opens, 1);
        assert!(actions.is_empty(), "hello should not be acked: {actions:?}");
        assert!(events.is_empty());
    }

    #[test]
    fn events_api_envelope_is_acked_then_routed_in_order() {
        let frame = events_api("env-1", &msg("C1", "U1", "100.1", "hi"));
        let (opens, actions, events) = drive(vec![vec![frame]]);
        assert_eq!(opens, 1);
        // ACK must precede the routed hand-off.
        assert_eq!(actions, vec!["ack:env-1".to_string(), "event:C1".to_string()]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].chat_id, "C1");
        assert_eq!(events[0].text, "hi");
        assert_eq!(events[0].state, MessageState::New);
    }

    #[test]
    fn disconnect_envelope_triggers_reconnect() {
        let conn1 = vec![disconnect()];
        let conn2 = vec![events_api("env-2", &msg("C2", "U2", "9.0", "after reconnect"))];
        let (opens, actions, events) = drive(vec![conn1, conn2]);
        assert_eq!(opens, 2, "should have reconnected after disconnect");
        assert_eq!(actions, vec!["ack:env-2".to_string(), "event:C2".to_string()]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].chat_id, "C2");
    }

    #[test]
    fn unparseable_frame_is_ignored_and_stream_continues() {
        let frames = vec![
            "this is not json".to_string(),
            events_api("env-3", &msg("C3", "U3", "1.0", "ok")),
        ];
        let (opens, actions, events) = drive(vec![frames]);
        assert_eq!(opens, 1);
        assert_eq!(actions, vec!["ack:env-3".to_string(), "event:C3".to_string()]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn events_api_without_inner_event_is_acked_only() {
        let frame =
            r#"{"envelope_id":"env-4","type":"events_api","payload":{"type":"event_callback"}}"#
                .to_string();
        let (_, actions, events) = drive(vec![vec![frame]]);
        assert_eq!(actions, vec!["ack:env-4".to_string()]);
        assert!(events.is_empty());
    }

    #[test]
    fn unrouted_event_is_still_acked_but_not_routed() {
        // A channel_join is a real Events API envelope (so it must be ACKed) that
        // normalize() declines to route.
        let frame = events_api(
            "env-5",
            r#"{"type":"message","subtype":"channel_join","channel":"C1","user":"U1","ts":"1.0"}"#,
        );
        let (_, actions, events) = drive(vec![vec![frame]]);
        assert_eq!(actions, vec!["ack:env-5".to_string()]);
        assert!(events.is_empty());
    }

    #[test]
    fn slash_command_envelope_is_acked_only() {
        let frame =
            r#"{"envelope_id":"env-6","type":"slash_commands","payload":{"command":"/foo"}}"#
                .to_string();
        let (_, actions, events) = drive(vec![vec![frame]]);
        assert_eq!(actions, vec!["ack:env-6".to_string()]);
        assert!(events.is_empty());
    }

    #[test]
    fn bot_own_message_is_acked_and_emitted_flagged_self() {
        // The socket layer ACKs and emits the event with is_self_author set; the
        // router is what drops self-authored events, not this listener.
        let frame = events_api("env-7", &msg("C1", "U_BOT", "9.0", "echo"));
        let (_, actions, events) = drive(vec![vec![frame]]);
        assert_eq!(actions, vec!["ack:env-7".to_string(), "event:C1".to_string()]);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_self_author);
    }

    #[test]
    fn stop_signal_halts_the_listener_before_the_next_frame() {
        let log: Shared = Rc::new(RefCell::new(Log::default()));
        let two_events = vec![
            events_api("a", &msg("C1", "U1", "1.0", "first")),
            events_api("b", &msg("C1", "U1", "2.0", "second")),
        ];
        let mut opener = FakeOpener {
            scripts: vec![two_events].into(),
            log: log.clone(),
        };
        let flag = Rc::new(std::cell::Cell::new(false));
        let stop_flag = flag.clone();
        let stop = move || stop_flag.get();
        let sink_log = log.clone();
        let mut sink = move |routed: RoutingEvent| {
            sink_log.borrow_mut().events.push(routed);
            // Request shutdown after the first routed event.
            flag.set(true);
        };
        let mut tick = || {};

        let result = run_listener(&mut opener, &identity(), &stop, &mut sink, &mut tick);
        assert!(result.is_ok(), "clean stop, not the end-of-script Fatal");
        assert_eq!(
            log.borrow().events.len(),
            1,
            "the second frame must not be processed after stop"
        );
    }

    #[test]
    fn idle_read_runs_the_tick_then_keeps_reading() {
        // A connection that yields two idle windows before delivering one event.
        // The tick must fire once per idle window, and the event after them must
        // still route normally on the same connection.
        let log: Shared = Rc::new(RefCell::new(Log::default()));
        let frames = vec![
            idle(),
            idle(),
            events_api("env-i", &msg("C9", "U9", "5.0", "after idle")),
        ];
        let mut opener = FakeOpener {
            scripts: vec![frames].into(),
            log: log.clone(),
        };
        let stop = || false;
        let sink_log = log.clone();
        let mut sink = move |routed: RoutingEvent| {
            let mut l = sink_log.borrow_mut();
            l.actions.push(format!("event:{}", routed.chat_id));
            l.events.push(routed);
        };
        let ticks = Rc::new(std::cell::Cell::new(0usize));
        let tick_count = ticks.clone();
        let mut tick = move || tick_count.set(tick_count.get() + 1);

        match run_listener(&mut opener, &identity(), &stop, &mut sink, &mut tick) {
            Ok(()) | Err(SocketError::Fatal(_)) => {}
            Err(other) => panic!("unexpected listener error: {other}"),
        }

        assert_eq!(ticks.get(), 2, "tick should fire once per idle window");
        let l = log.borrow();
        assert_eq!(l.events.len(), 1, "the event after the idle windows routes");
        assert_eq!(l.events[0].chat_id, "C9");
        assert_eq!(l.actions, vec!["ack:env-i".to_string(), "event:C9".to_string()]);
    }

    #[test]
    fn parse_incoming_classifies_envelope_types() {
        assert!(matches!(parse_incoming(&hello()), Incoming::Hello));
        assert!(matches!(
            parse_incoming(&disconnect()),
            Incoming::Disconnect { .. }
        ));
        assert!(matches!(parse_incoming("garbage"), Incoming::Ignored));
        match parse_incoming(&events_api("env-x", &msg("C", "U", "1.0", "x"))) {
            Incoming::Event { envelope_id, .. } => assert_eq!(envelope_id, "env-x"),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn ack_frame_carries_only_the_envelope_id() {
        let frame = ack_frame("env-z");
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["envelope_id"], "env-z");
        assert_eq!(value.as_object().unwrap().len(), 1);
    }
}
