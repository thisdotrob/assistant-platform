//! Graceful-shutdown signal handling for the live Slack daemon.
//!
//! `serve_slack` runs until its `stop` predicate returns true and only then
//! drains its per-channel containers (the cleanup loop in `serve_slack`). With
//! no handler installed, SIGTERM/SIGINT take their default disposition and kill
//! the process outright, so that drain never runs and the containers orphan.
//!
//! We install a handler that flips a process-global `AtomicBool`. The serve loop
//! observes it within one idle read window (~1s, via the `SocketError::Idle`
//! yield) and returns, draining containers on its way out. The handler itself
//! does nothing but a single lock-free atomic store, which is async-signal-safe.
//!
//! Shutdown latency note: specialist jobs now run on background worker threads
//! (see `slack.rs`). On its way out the serve loop joins every in-flight worker
//! and then does a final completion drain (delivering already-acknowledged
//! results) before tearing down containers. So a SIGTERM does not return
//! instantly when a browser job is mid-flight — it waits out the running
//! worker(s), each of which is itself bounded by the per-turn timeout wall
//! clock. This is deliberate: we'd rather deliver an acknowledged result than
//! drop it on the floor at shutdown.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install SIGTERM/SIGINT handlers and return the serve loop's `stop` predicate.
/// Call once at daemon start; re-installing is harmless (the disposition is just
/// re-set to the same handler).
pub(crate) fn install_shutdown_handler() -> impl Fn() -> bool {
    // SAFETY: `handle` only performs an async-signal-safe atomic store, and we
    // pass a fully-initialised `sigaction` with an empty mask and no flags.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    || SHUTDOWN.load(Ordering::SeqCst)
}
