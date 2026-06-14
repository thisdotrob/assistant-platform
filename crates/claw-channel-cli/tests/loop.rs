//! A CLI-channel test message runs end-to-end through the fake session DB loop:
//! the channel normalizes + enqueues inbound, a fake container processes it and
//! emits a reply, and the channel renders the outbound reply for the terminal.

use claw_channel_cli::CliChannel;
use claw_session::{LocalControl, SessionLayout};

#[test]
fn cli_message_runs_through_fake_session_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(tmp.path(), "orchestrator", "sess-1").unwrap();

    // Channel starts (initializing the session) and sends a normalized message.
    let mut channel = CliChannel::new(layout.clone());
    channel.start().unwrap();
    assert!(channel.is_started());
    let in_seq = channel.send_text("what's the weather?\n").unwrap();
    assert_eq!(in_seq % 2, 0);

    // Fake container picks up the inbound message and replies.
    let control = LocalControl::new(layout);
    let container = control.fake_container();
    container.start("run-1").unwrap();
    let seen = container.read_inbound().unwrap();
    assert_eq!(seen, vec![(in_seq, "what's the weather?".to_string())]);
    container.emit("text", "sunny").unwrap();
    container.emit("card", "Forecast | 21C").unwrap();
    container.stop().unwrap();

    // Channel renders the replies for a plain terminal (card falls back).
    let rendered = channel.render_outbound().unwrap();
    assert_eq!(rendered, vec!["sunny".to_string(), "[card] Forecast | 21C".to_string()]);
}

#[test]
fn attachment_path_rejects_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = SessionLayout::derive(tmp.path(), "orchestrator", "sess-1").unwrap();
    let channel = CliChannel::new(layout);
    assert!(channel.attachment_path("m1", "ok.png").is_ok());
    assert!(channel.attachment_path("m1", "../../escape").is_err());
}
