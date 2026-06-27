//! Periodic due-work sweep: the host side of assistant-scheduler's exactly-once lease.
//!
//! [`sweep_once`] performs one pass over the installation's central DB. It expires
//! stale sticky-engagement windows, then claims every due scheduled occurrence and
//! drives it as a turn into the occurrence's target session. assistant-scheduler never
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

use assistant_router::expire_sticky;
use assistant_runtime_docker::ContainerRuntime;
use assistant_scheduler::{
    claim_due, complete_occurrence, list_items, ProjectedItem, Recurrence, ScheduleStatus,
};
use assistant_session::{InboundMessage, SessionLayout};
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
/// under `group`. Exactly-once is enforced by assistant-scheduler's lease: a claimed
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

/// Render an agent group's active scheduled items as an `<active_schedules>`
/// context block for injection into a turn's inbound metadata, or `None` when it
/// has none (so an empty block is never injected). Each line carries the item's
/// id — which the agent passes to `cancel_schedule` to cancel it — its intent
/// summary, the next due time relative to `now`, and its recurrence. Scoped to a
/// single `agent_group_id` (the instance is the isolation boundary) and capped at
/// `limit` items, taking the soonest-due first (the projection lists in due
/// order). Read-only and fail-soft at the call site: a query error yields `None`.
pub fn render_active_schedules_block(
    conn: &Connection,
    agent_group_id: i64,
    now: i64,
    limit: usize,
) -> Option<String> {
    let items = list_items(conn, agent_group_id, Some(ScheduleStatus::Active)).ok()?;
    if items.is_empty() {
        return None;
    }
    let mut block = String::from(
        "<active_schedules>\nYour active scheduled items. To cancel one, call cancel_schedule with its id.\n",
    );
    for item in items.iter().take(limit) {
        block.push_str(&render_schedule_line(item, now));
        block.push('\n');
    }
    block.push_str("</active_schedules>");
    Some(block)
}

/// One `- id=… | "summary" | next: … | …` line for a projected item.
fn render_schedule_line(item: &ProjectedItem, now: i64) -> String {
    let summary = item.intent.replace('\n', " ");
    let due = match item.process_after {
        None => "unscheduled".to_string(),
        Some(t) if t <= now => "due now".to_string(),
        Some(t) => format!("in {}", human_duration(t - now)),
    };
    let recurrence = match &item.recurrence {
        None => "one-off".to_string(),
        Some(Recurrence::Every { seconds }) => format!("repeats every {}", human_duration(*seconds)),
    };
    format!("- id={} | \"{summary}\" | next: {due} | {recurrence}", item.id)
}

/// A compact human duration for a non-negative second count (largest whole unit:
/// seconds, minutes, hours, then days). Used only for display in the schedules
/// block, so coarse rounding is fine.
fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3_600)
    } else {
        format!("{}d", s / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};
    use assistant_scheduler::{upsert_item, ContextPolicy, Recurrence, ScheduleIntent, ScheduledMessageMeta};

    fn central() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules().into_iter().map(str::to_string).collect();
        let mut set = baseline_migrations(order);
        for m in assistant_scheduler::migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    fn seed(conn: &Connection, group: i64, summary: &str, due: i64, every: Option<i64>) -> String {
        let meta = ScheduledMessageMeta::create(
            group,
            ScheduleIntent { created_by: "u".into(), summary: summary.into(), created_at: 0 },
            due,
            every.map(|seconds| Recurrence::Every { seconds }),
            ContextPolicy::default(),
        )
        .unwrap();
        upsert_item(conn, &meta, Some("C1")).unwrap();
        meta.scheduled_item_id
    }

    #[test]
    fn no_active_items_renders_no_block() {
        let conn = central();
        assert!(render_active_schedules_block(&conn, 1, 1_000, 5).is_none());
    }

    #[test]
    fn active_items_render_id_summary_due_and_recurrence() {
        let conn = central();
        let one_off = seed(&conn, 1, "Stretch", 1_300, None);
        let recurring = seed(&conn, 1, "Standup nudge", 4_600, Some(86_400));
        // An item for another agent group must not leak into this block.
        seed(&conn, 2, "other agent", 1_100, None);

        let block = render_active_schedules_block(&conn, 1, 1_000, 5).unwrap();
        assert!(block.contains("<active_schedules>") && block.contains("</active_schedules>"));
        assert!(block.contains(&format!("id={one_off}")));
        assert!(block.contains("\"Stretch\" | next: in 5m | one-off"));
        assert!(block.contains(&format!("id={recurring}")));
        assert!(block.contains("\"Standup nudge\" | next: in 1h | repeats every 1d"));
        assert!(!block.contains("other agent"), "cross-agent item must not appear");
    }

    #[test]
    fn cap_limits_lines_to_the_soonest_due() {
        let conn = central();
        let soon = seed(&conn, 1, "soon", 1_010, None);
        seed(&conn, 1, "later", 9_000, None);
        let block = render_active_schedules_block(&conn, 1, 1_000, 1).unwrap();
        assert!(block.contains(&format!("id={soon}")));
        assert!(!block.contains("\"later\""), "cap must drop the later item");
    }
}
