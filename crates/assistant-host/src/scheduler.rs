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
    claim_due, complete_occurrence, generate_scheduled_item_id, item_status, list_items, upsert_item,
    ContextPolicy, ProjectedItem, Recurrence, ScheduleIntent, ScheduledMessageMeta, ScheduleStatus,
    Weekday,
};
use assistant_session::{InboundMessage, SessionLayout};
use assistant_specialist_spec::StandingTask;
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

        // Pre-task gate: if configured, run the command before deciding to fire.
        // Empty stdout or non-zero exit → skip (advance the schedule without inference).
        // Non-empty stdout → fire, injecting that stdout as the turn's metadata context.
        // A command that fails to spawn (IO error) is non-fatal but retried: leave
        // the lease to expire rather than advancing the schedule.
        let gate_metadata: Option<String>;
        if let Some(gate_cmd) = &item.gate_command {
            let outcome = match &item.gate_onecli_agent {
                Some(agent) => run_gate_in_container(
                    gate_cmd,
                    agent,
                    &host_config.image.reference(),
                    &host_config.onecli_ca_dir,
                ),
                None => run_gate(gate_cmd),
            };
            match outcome {
                Ok(GateOutcome::Skip) => {
                    complete_occurrence(conn, &lease.occurrence, now)
                        .map_err(|e| HostError::Db(e.to_string()))?;
                    continue;
                }
                Ok(GateOutcome::Fire(metadata)) => {
                    gate_metadata = metadata;
                }
                Err(e) => {
                    eprintln!(
                        "scheduler: gate command error for item {}, leaving for retry: {e}",
                        lease.occurrence.scheduled_item_id
                    );
                    continue;
                }
            }
        } else {
            gate_metadata = None;
        }

        let layout = SessionLayout::derive(sessions_dir, group, session_id)?;
        let mut host = Host::new(layout, runtime_factory(), host_config.clone());
        let inbound = InboundMessage {
            sender: SCHEDULER_SENDER.to_string(),
            content: item.intent.clone(),
            metadata: gate_metadata,
            thread_id: None,
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

/// The outcome of running a pre-task gate command.
pub(crate) enum GateOutcome {
    /// Gate passed; optional stdout to inject as the turn's metadata context.
    Fire(Option<String>),
    /// Gate says no work right now — advance the schedule without inference.
    Skip,
}

/// Run a pre-task gate command via `sh -c` on the host process. Returns
/// `Ok(Skip)` on non-zero exit or empty stdout, `Ok(Fire(Some(stdout)))` when
/// there is work, and `Err` when the command could not be spawned at all.
pub(crate) fn run_gate(cmd: &str) -> Result<GateOutcome, String> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("could not spawn gate command: {e}"))?;
    if !output.status.success() {
        return Ok(GateOutcome::Skip);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Ok(GateOutcome::Skip)
    } else {
        Ok(GateOutcome::Fire(Some(stdout)))
    }
}

/// Run a pre-task gate command inside a `docker run --rm` container, with the
/// OneCLI proxy env and CA trust anchor injected for `gate_agent`. This mirrors
/// how normal turns get credentials — the container routes all HTTPS through the
/// proxy, which injects the agent's credentials per request. Falls back to host
/// execution when no OneCLI gateway is configured.
pub(crate) fn run_gate_in_container(
    cmd: &str,
    gate_agent: &str,
    image: &str,
    ca_dir: &std::path::Path,
) -> Result<GateOutcome, String> {
    let Some(gateway_url) = crate::onecli::gateway_url() else {
        // No gateway configured (stub / offline mode): fall back to host.
        return run_gate(cmd);
    };

    let cfg = crate::onecli::fetch_container_config(&gateway_url, gate_agent)
        .map_err(|e| format!("gate: container-config fetch failed for {gate_agent}: {e}"))?;

    let mut docker_args: Vec<String> = vec!["run".into(), "--rm".into()];

    // Inject proxy env vars (sorted for deterministic docker args).
    let mut sorted_env: Vec<(&String, &String)> = cfg.env.iter().collect();
    sorted_env.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in &sorted_env {
        docker_args.push("--env".into());
        docker_args.push(format!("{k}={v}"));
    }

    // Write and mount the CA trust anchor when the config provides one.
    if let (Some(pem), Some(container_path)) =
        (&cfg.ca_certificate, &cfg.ca_certificate_container_path)
    {
        let ca_cert = ca_dir.join("ca.pem");
        if std::fs::create_dir_all(ca_dir).is_ok() && std::fs::write(&ca_cert, pem).is_ok() {
            docker_args.push("--volume".into());
            docker_args.push(format!("{}:{container_path}:ro", ca_cert.display()));
        }
    }

    // Override the image's default entrypoint (the Node.js agent runner) so the
    // gate command runs under plain sh rather than as arguments to the agent.
    docker_args.push("--entrypoint".into());
    docker_args.push("sh".into());
    docker_args.push(image.into());
    docker_args.push("-c".into());
    docker_args.push(cmd.into());

    let output = std::process::Command::new("docker")
        .args(&docker_args)
        .output()
        .map_err(|e| format!("docker run gate: {e}"))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();
    if !output.status.success() {
        eprintln!(
            "scheduler: gate container for agent {gate_agent} exited {:?}; stderr: {}",
            output.status.code(),
            stderr_trimmed,
        );
        return Ok(GateOutcome::Skip);
    }
    if !stderr_trimmed.is_empty() {
        eprintln!("scheduler: gate container stderr (agent={gate_agent}): {stderr_trimmed}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Ok(GateOutcome::Skip)
    } else {
        Ok(GateOutcome::Fire(Some(stdout)))
    }
}

/// The session id used for all standing-task turns. A single dedicated session
/// keeps standing-task messages separate from user conversations.
const STANDING_SESSION_ID: &str = "standing";

/// Idempotently create any standing tasks not yet present in the central DB
/// projection. Called once at serve-slack startup after the specialist list is
/// resolved. Each task fires into the orchestrator's `standing` session; the
/// session directory is created lazily on first sweep by `ensure_spawned`.
///
/// The second element of each tuple is the OneCLI agent that should run the
/// gate command inside a container (`Some("agent-name")`), or `None` to run it
/// as a raw host subprocess. For specialist standing tasks this should be the
/// specialist's `onecli_agent`; for product-level tasks `None` is fine.
///
/// Already-existing tasks (any status, including operator-cancelled) are skipped.
/// Changing a task's `id` field orphans the old item and creates a new one.
pub fn ensure_standing_tasks(
    conn: &Connection,
    agent_group_id: i64,
    now: i64,
    tasks: &[(StandingTask, Option<String>)],
) -> Result<(), HostError> {
    for (task, gate_onecli_agent) in tasks {
        let intent = ScheduleIntent {
            created_by: task.id.clone(),
            summary: task.summary.clone(),
            created_at: 0,
        };
        let id = generate_scheduled_item_id(agent_group_id, &intent);
        if item_status(conn, &id).map_err(|e| HostError::Db(e.to_string()))?.is_some() {
            continue;
        }
        let mut meta = ScheduledMessageMeta::create(
            agent_group_id,
            intent,
            now,
            Some(Recurrence::Every { seconds: task.interval_secs as i64 }),
            ContextPolicy::CurrentMemory,
        )
        .map_err(|e| HostError::Db(e.to_string()))?;
        meta.gate_command = task.gate_command.clone();
        meta.gate_onecli_agent = gate_onecli_agent.clone();
        upsert_item(conn, &meta, Some(STANDING_SESSION_ID))
            .map_err(|e| HostError::Db(e.to_string()))?;
        eprintln!("scheduler: registered standing task {:?} ({})", task.id, meta.scheduled_item_id);
    }
    Ok(())
}

/// Render an agent group's live scheduled items (active and paused) as a
/// `<schedules>` context block for injection into a turn's inbound metadata, or
/// `None` when it has none (so an empty block is never injected). Each line
/// carries the item's id — which the agent passes to `cancel_schedule` /
/// `pause_schedule` / `resume_schedule` — its intent summary, the next due time
/// relative to `now`, its recurrence, and a `paused` marker when suspended. Paused
/// items are included so the agent has their ids to resume; cancelled/completed
/// items are omitted (they are terminal). Scoped to a single `agent_group_id` (the
/// instance is the isolation boundary) and capped at `limit` items, taking the
/// soonest-due first (the projection lists in due order). Read-only and fail-soft
/// at the call site: a query error yields `None`.
pub fn render_schedules_block(
    conn: &Connection,
    agent_group_id: i64,
    now: i64,
    limit: usize,
) -> Option<String> {
    let items: Vec<ProjectedItem> = list_items(conn, agent_group_id, None)
        .ok()?
        .into_iter()
        .filter(|i| matches!(i.status, ScheduleStatus::Active | ScheduleStatus::Paused))
        .collect();
    if items.is_empty() {
        return None;
    }
    let mut block = String::from(
        "<schedules>\nYour scheduled items (active, and any you have paused). \
         To cancel one, call cancel_schedule with its id; to pause an active one, \
         call pause_schedule; to resume a paused one, call resume_schedule.\n",
    );
    for item in items.iter().take(limit) {
        block.push_str(&render_schedule_line(item, now));
        block.push('\n');
    }
    block.push_str("</schedules>");
    Some(block)
}

/// One `- id=… | "summary" | next: … | …` line for a projected item. A paused
/// item carries a trailing `| paused` marker (its `next:` is when it would fire
/// once resumed).
fn render_schedule_line(item: &ProjectedItem, now: i64) -> String {
    let summary = item.intent.replace('\n', " ");
    let due = match item.process_after {
        None => "unscheduled".to_string(),
        Some(t) if t <= now => "due now".to_string(),
        Some(t) => format!("in {}", human_duration(t - now)),
    };
    let recurrence = match &item.recurrence {
        None => "one-off".to_string(),
        Some(rec) => describe_recurrence(rec),
    };
    let paused = if item.status == ScheduleStatus::Paused { " | paused" } else { "" };
    format!("- id={} | \"{summary}\" | next: {due} | {recurrence}{paused}", item.id)
}

/// A human-readable phrase for a recurrence, shown in the `<schedules>` block.
fn describe_recurrence(rec: &Recurrence) -> String {
    match rec {
        Recurrence::Every { seconds } => format!("repeats every {}", human_duration(*seconds)),
        Recurrence::Daily { minute_of_day, tz } => {
            format!("daily at {} {tz}", hhmm(*minute_of_day))
        }
        Recurrence::Weekly {
            weekdays,
            minute_of_day,
            tz,
        } => {
            let days = weekdays
                .iter()
                .map(|w| weekday_label(*w))
                .collect::<Vec<_>>()
                .join(", ");
            format!("weekly on {days} at {} {tz}", hhmm(*minute_of_day))
        }
        Recurrence::Monthly {
            day,
            minute_of_day,
            tz,
        } => format!("monthly on day {day} at {} {tz}", hhmm(*minute_of_day)),
    }
}

fn hhmm(minute_of_day: u32) -> String {
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

fn weekday_label(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
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
    use assistant_scheduler::{
        pause_item, upsert_item, ContextPolicy, Recurrence, ScheduleIntent, ScheduledMessageMeta,
        Weekday,
    };

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
    fn no_live_items_renders_no_block() {
        let conn = central();
        assert!(render_schedules_block(&conn, 1, 1_000, 5).is_none());
    }

    #[test]
    fn active_items_render_id_summary_due_and_recurrence() {
        let conn = central();
        let one_off = seed(&conn, 1, "Stretch", 1_300, None);
        let recurring = seed(&conn, 1, "Standup nudge", 4_600, Some(86_400));
        // An item for another agent group must not leak into this block.
        seed(&conn, 2, "other agent", 1_100, None);

        let block = render_schedules_block(&conn, 1, 1_000, 5).unwrap();
        assert!(block.contains("<schedules>") && block.contains("</schedules>"));
        assert!(block.contains(&format!("id={one_off}")));
        assert!(block.contains("\"Stretch\" | next: in 5m | one-off"));
        assert!(block.contains(&format!("id={recurring}")));
        assert!(block.contains("\"Standup nudge\" | next: in 1h | repeats every 1d"));
        assert!(!block.contains("other agent"), "cross-agent item must not appear");
    }

    #[test]
    fn calendar_recurrences_render_their_human_phrase() {
        let conn = central();
        let seed_cal = |summary: &str, due: i64, rec: Recurrence| {
            let meta = ScheduledMessageMeta::create(
                1,
                ScheduleIntent { created_by: "u".into(), summary: summary.into(), created_at: 0 },
                due,
                Some(rec),
                ContextPolicy::default(),
            )
            .unwrap();
            upsert_item(&conn, &meta, Some("C1")).unwrap();
        };
        seed_cal(
            "Morning check-in",
            2_000,
            Recurrence::Daily { minute_of_day: 9 * 60, tz: "Europe/London".into() },
        );
        seed_cal(
            "Standup",
            2_100,
            Recurrence::Weekly {
                weekdays: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                minute_of_day: 10 * 60 + 30,
                tz: "America/New_York".into(),
            },
        );
        seed_cal(
            "Rent reminder",
            2_200,
            Recurrence::Monthly { day: 1, minute_of_day: 8 * 60, tz: "Europe/London".into() },
        );

        let block = render_schedules_block(&conn, 1, 1_000, 5).unwrap();
        assert!(block.contains("\"Morning check-in\" | next: in 16m | daily at 09:00 Europe/London"));
        assert!(block.contains(
            "\"Standup\" | next: in 18m | weekly on Mon, Wed, Fri at 10:30 America/New_York"
        ));
        assert!(block.contains("\"Rent reminder\" | next: in 20m | monthly on day 1 at 08:00 Europe/London"));
    }

    #[test]
    fn paused_items_are_included_with_a_marker() {
        let conn = central();
        let active = seed(&conn, 1, "Active one", 1_300, None);
        let paused = seed(&conn, 1, "Paused one", 1_500, None);
        pause_item(&conn, &paused).unwrap();

        let block = render_schedules_block(&conn, 1, 1_000, 5).unwrap();
        // Both the active and the paused item appear; only the paused one is marked.
        assert!(block.contains(&format!("id={active}")));
        assert!(block.contains(&format!("id={paused}")));
        assert!(block.contains("\"Paused one\" | next: in 8m | one-off | paused"));
        assert!(
            block.contains("\"Active one\" | next: in 5m | one-off\n"),
            "an active line must not carry the paused marker"
        );
        // The header advertises pause/resume alongside cancel.
        assert!(block.contains("pause_schedule") && block.contains("resume_schedule"));
    }

    #[test]
    fn terminal_items_are_excluded() {
        let conn = central();
        let active = seed(&conn, 1, "live", 1_300, None);
        let gone = seed(&conn, 1, "cancelled", 1_400, None);
        assistant_scheduler::cancel_item(&conn, &gone).unwrap();

        let block = render_schedules_block(&conn, 1, 1_000, 5).unwrap();
        assert!(block.contains(&format!("id={active}")));
        assert!(!block.contains("\"cancelled\""), "a cancelled item must not appear");
    }

    #[test]
    fn cap_limits_lines_to_the_soonest_due() {
        let conn = central();
        let soon = seed(&conn, 1, "soon", 1_010, None);
        seed(&conn, 1, "later", 9_000, None);
        let block = render_schedules_block(&conn, 1, 1_000, 1).unwrap();
        assert!(block.contains(&format!("id={soon}")));
        assert!(!block.contains("\"later\""), "cap must drop the later item");
    }

    #[test]
    fn ensure_standing_tasks_creates_new_and_skips_existing() {
        let conn = central();
        let task = StandingTask {
            id: "jira-sync".to_string(),
            summary: "Sync the Jira board".to_string(),
            interval_secs: 600,
            gate_command: None,
        };

        // First call: task does not exist yet → created.
        ensure_standing_tasks(&conn, 1, 1_000, &[(task.clone(), None)]).unwrap();
        let items = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].intent, "Sync the Jira board");
        assert_eq!(items[0].session_id.as_deref(), Some(STANDING_SESSION_ID));

        // Second call: idempotent — same task already in DB, not duplicated.
        ensure_standing_tasks(&conn, 1, 2_000, &[(task.clone(), None)]).unwrap();
        let items = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assert_eq!(items.len(), 1, "must not duplicate an existing standing task");
    }

    #[test]
    fn ensure_standing_tasks_sets_gate_command_and_recurrence() {
        let conn = central();
        let task = StandingTask {
            id: "board-sync".to_string(),
            summary: "Board sync".to_string(),
            interval_secs: 300,
            gate_command: Some("check-gate.sh".to_string()),
        };

        ensure_standing_tasks(&conn, 1, 1_000, &[(task, None)]).unwrap();
        let items = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].gate_command.as_deref(), Some("check-gate.sh"));
        assert!(
            matches!(items[0].recurrence, Some(Recurrence::Every { seconds: 300 })),
            "recurrence should be Every 300s"
        );
    }

    #[test]
    fn ensure_standing_tasks_stores_gate_onecli_agent() {
        let conn = central();
        let task = StandingTask {
            id: "board-sync-agent".to_string(),
            summary: "Board sync with agent gate".to_string(),
            interval_secs: 300,
            gate_command: Some("python3 -c 'print(1)'".to_string()),
        };

        ensure_standing_tasks(
            &conn,
            1,
            1_000,
            &[(task, Some("my-specialist-agent".to_string()))],
        )
        .unwrap();
        let items = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].gate_onecli_agent.as_deref(),
            Some("my-specialist-agent")
        );
    }

    #[test]
    fn ensure_standing_tasks_skips_cancelled_task() {
        let conn = central();
        let task = StandingTask {
            id: "jira-sync".to_string(),
            summary: "Sync the Jira board".to_string(),
            interval_secs: 600,
            gate_command: None,
        };

        // Create and then cancel it (simulates operator cancellation).
        ensure_standing_tasks(&conn, 1, 1_000, &[(task.clone(), None)]).unwrap();
        let items = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assistant_scheduler::cancel_item(&conn, &items[0].id).unwrap();

        // Re-registering does not resurrect it.
        ensure_standing_tasks(&conn, 1, 2_000, &[(task, None)]).unwrap();
        let all = assistant_scheduler::list_items(&conn, 1, None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, assistant_scheduler::ScheduleStatus::Cancelled);
    }
}
