//! Periodic due-work sweep: the host side of claw-scheduler's exactly-once lease.
//!
//! [`sweep_once`] performs one pass over the installation's central DB. It expires
//! stale sticky-engagement windows, then claims every due scheduled occurrence and
//! drives it as a turn into the occurrence's target session. claw-scheduler never
//! reads the wall clock — the caller supplies `now` — so a sweep is deterministic
//! and covered offline with `FakeRuntime` and a fake shim.
//!
//! Wiring this onto a real cadence is the live tail: neither serve loop has a timer
//! (the terminal loop blocks on stdin, the Slack listener blocks on socket reads),
//! so a live sweep needs its own driver, must reconcile sweep-spawned containers
//! against the inbound loop's warm ones (a shared session id collides on the
//! `{agent}-{session}` container name), and must route a scheduled turn's reply
//! back to a channel. The message-driven creation that writes the authoritative
//! `ScheduledMessageMeta` into a session is likewise part of that tail.

use std::collections::HashMap;
use std::path::Path;

use claw_router::expire_sticky;
use claw_runtime_docker::ContainerRuntime;
use claw_scheduler::{claim_due, complete_occurrence, list_items, ProjectedItem};
use claw_session::{InboundMessage, SessionLayout};
use rusqlite::Connection;

use crate::error::HostError;
use crate::run::{Host, HostConfig};

/// Synthetic sender for a scheduled turn: the message is the agent's own standing
/// instruction firing, not a human's, so it is attributed to the scheduler.
const SCHEDULER_SENDER: &str = "scheduler";

/// What one sweep pass did, for logging and test assertions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Sticky-engagement windows expired this pass.
    pub expired_sticky: usize,
    /// Scheduled occurrences fired (a turn ran and the occurrence was completed).
    pub fired: usize,
}

/// Run one due-work sweep against the central DB at logical time `now` (epoch
/// seconds). Expires stale sticky windows, then claims and fires every due
/// occurrence for `agent_group_id`, driving each as a turn into its target session
/// under `group`. Exactly-once is enforced by claw-scheduler's lease: a claimed
/// occurrence is completed only after its turn runs, so a failed turn leaves the
/// lease to expire and be retried on a later sweep (with a bumped attempt count).
///
/// A claimed item with no bound session, or one missing from the projection, is
/// skipped (its lease simply expires). Sticky expiry is best-effort within the
/// pass — a claim/turn failure does not roll it back.
#[allow(clippy::too_many_arguments)]
pub fn sweep_once<R, F>(
    conn: &Connection,
    sessions_dir: &Path,
    group: &str,
    agent_group_id: i64,
    owner: &str,
    lease_ttl_secs: i64,
    host_config: &HostConfig,
    runtime_factory: &F,
    now: i64,
) -> Result<SweepReport, HostError>
where
    R: ContainerRuntime,
    R::Error: std::fmt::Display,
    F: Fn() -> R,
{
    let expired_sticky = expire_sticky(conn, now).map_err(|e| HostError::Db(e.to_string()))?;

    let leases = claim_due(conn, now, owner, lease_ttl_secs).map_err(|e| HostError::Db(e.to_string()))?;
    if leases.is_empty() {
        return Ok(SweepReport { expired_sticky, fired: 0 });
    }

    // Resolve each claimed occurrence's target session + summary. This composition
    // wires a single agent group, so listing its items is bounded and indexes by id.
    let items: HashMap<String, ProjectedItem> = list_items(conn, agent_group_id, None)
        .map_err(|e| HostError::Db(e.to_string()))?
        .into_iter()
        .map(|item| (item.id.clone(), item))
        .collect();

    let mut fired = 0;
    for lease in leases {
        let Some(item) = items.get(&lease.occurrence.scheduled_item_id) else {
            // The occurrence's item is not in this agent's projection (a stale or
            // cross-agent claim); leave the lease to expire rather than firing
            // something we cannot resolve.
            continue;
        };
        let Some(session_id) = item.session_id.as_deref() else {
            // A scheduled item with no bound session has no turn to drive.
            continue;
        };

        let layout = SessionLayout::derive(sessions_dir, group, session_id)?;
        let mut host = Host::new(layout, runtime_factory(), host_config.clone());
        let inbound = InboundMessage {
            sender: SCHEDULER_SENDER.to_string(),
            content: item.intent.clone(),
            metadata: None,
        };

        // Key the inbound enqueue on the occurrence so a retry (after a failed
        // attempt left the lease to expire) reuses the one inbound row instead
        // of duplicating it.
        match host.run_turn_keyed(&inbound, Some(&lease.occurrence.idempotency_key)) {
            Ok(_) => {
                complete_occurrence(conn, &lease.occurrence, now)
                    .map_err(|e| HostError::Db(e.to_string()))?;
                fired += 1;
            }
            // A failed scheduled turn is non-fatal: do not complete the occurrence,
            // so its lease expires and a later sweep retries it.
            Err(err) => {
                eprintln!(
                    "scheduler: turn failed for item {}: {err}",
                    lease.occurrence.scheduled_item_id
                );
            }
        }
        // A scheduled firing is a discrete spawn -> turn -> stop. Stop the container
        // rather than leaving it warm: the inbound serve loop owns its own warm
        // containers, and a shared session id would otherwise collide on name.
        let _ = host.shutdown();
    }

    Ok(SweepReport { expired_sticky, fired })
}
