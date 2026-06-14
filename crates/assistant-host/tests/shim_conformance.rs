//! Cross-language parity: the real Node shim (`shim/src/index.js`) drives the
//! session DB protocol identically to the in-process `FakeContainer`.
//!
//! This is the proof that the Rust host and the Node container agree on the
//! contract byte-for-byte — odd-seq replies, `messages_out` shape, heartbeat,
//! `container_state` — without which `verify_sequence_parity` would break in a
//! real turn. It runs the actual shim in `stub` mode (which imports no npm
//! dependencies — the Claude SDK is lazy-loaded only on the `claude_oauth`
//! path), over the real session DBs, through the real host run-loop.
//!
//! `#[ignore]`d so the offline `cargo test --workspace` gate never depends on a
//! Node toolchain. Run on demand:
//!   cargo test -p assistant-host --test shim_conformance -- --ignored --nocapture

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use assistant_host::{Host, HostConfig};
use assistant_runtime_docker::{FakeRuntime, ImageRef, OneCliReadiness, RunnerAuthMode};
use assistant_session::{init_session, verify_sequence_parity, InboundMessage, SessionLayout};

fn ready() -> OneCliReadiness {
    OneCliReadiness {
        proxy_configured: true,
        anthropic_secret_present: true,
        placeholder_injection_ok: true,
    }
}

fn shim_entry() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../shim/src/index.js")
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn the real shim as an external process standing in for the container.
/// `ASSISTANT_SESSION_DIR` points it at the host's session folder; stub mode echoes.
fn spawn_shim(layout: &SessionLayout) -> Child {
    Command::new("node")
        .arg("--experimental-sqlite")
        .arg(shim_entry())
        .env("ASSISTANT_SESSION_DIR", layout.dir())
        .env("ASSISTANT_RUNNER_MODE", "stub")
        .env("ASSISTANT_POLL_INTERVAL_MS", "20")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn node shim")
}

#[test]
#[ignore = "requires a Node toolchain; run with --ignored"]
fn real_node_shim_round_trips_a_turn() {
    if !node_available() {
        eprintln!("skipping: `node` not found on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "shim-sess").unwrap();

    // Initialize + migrate both session DBs before the shim opens them.
    init_session(&layout).unwrap();

    let mut shim = spawn_shim(&layout);

    let mut config =
        HostConfig::new(ImageRef::new("assistant-base", "0.1.0"), vec![sessions], RunnerAuthMode::Stub, ready());
    config.poll_interval = Duration::from_millis(10);
    config.turn_timeout = Duration::from_secs(30);
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out: Vec<u8> = Vec::new();
    let delivered = host.process_turn("hello", &mut out).unwrap();

    assert_eq!(delivered, 1);
    assert_eq!(String::from_utf8(out).unwrap(), "echo: hello\n");
    // The cross-process host-even / container-odd contract holds end-to-end.
    verify_sequence_parity(&layout).unwrap();

    host.shutdown().unwrap();
    shim.kill().ok();
    shim.wait().ok();
}

#[test]
#[ignore = "requires a Node toolchain; run with --ignored"]
fn real_node_shim_round_trips_a_turn_carrying_injected_metadata() {
    if !node_available() {
        eprintln!("skipping: `node` not found on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "shim-meta").unwrap();

    init_session(&layout).unwrap();

    let mut shim = spawn_shim(&layout);

    let mut config =
        HostConfig::new(ImageRef::new("assistant-base", "0.1.0"), vec![sessions], RunnerAuthMode::Stub, ready());
    config.poll_interval = Duration::from_millis(10);
    config.turn_timeout = Duration::from_secs(30);
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    // A host-injected memory block rides in `metadata`. In stub mode the shim
    // reads the row (its `readInbound` now selects the column) but echoes only
    // `content`; the point is that the real shim's metadata-aware SELECT round-
    // trips against the real schema. The Claude path's prompt-prepend is live-only.
    let inbound = InboundMessage {
        sender: "local".to_string(),
        content: "hello".to_string(),
        metadata: Some("<retrieved_memories>\nmem-x\n</retrieved_memories>".to_string()),
    };
    let replies = host.run_turn(&inbound).unwrap();

    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].content, "echo: hello");
    verify_sequence_parity(&layout).unwrap();

    host.shutdown().unwrap();
    shim.kill().ok();
    shim.wait().ok();
}

#[test]
#[ignore = "requires a Node toolchain; run with --ignored"]
fn real_node_shim_reuses_container_across_turns() {
    if !node_available() {
        eprintln!("skipping: `node` not found on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let layout = SessionLayout::derive(&sessions, "orchestrator", "shim-sess2").unwrap();

    init_session(&layout).unwrap();

    let mut shim = spawn_shim(&layout);

    let mut config =
        HostConfig::new(ImageRef::new("assistant-base", "0.1.0"), vec![sessions], RunnerAuthMode::Stub, ready());
    config.poll_interval = Duration::from_millis(10);
    config.turn_timeout = Duration::from_secs(30);
    let mut host = Host::new(layout.clone(), FakeRuntime::new(), config);

    let mut out1: Vec<u8> = Vec::new();
    host.process_turn("first", &mut out1).unwrap();
    let mut out2: Vec<u8> = Vec::new();
    host.process_turn("second", &mut out2).unwrap();

    assert_eq!(String::from_utf8(out1).unwrap(), "echo: first\n");
    assert_eq!(String::from_utf8(out2).unwrap(), "echo: second\n");
    verify_sequence_parity(&layout).unwrap();

    host.shutdown().unwrap();
    shim.kill().ok();
    shim.wait().ok();
}
