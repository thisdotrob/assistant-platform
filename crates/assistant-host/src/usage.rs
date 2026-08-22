//! Per-agent inference usage & cost accounting.
//!
//! Each credentialed turn's shim emits a `usage` outbound row built from the
//! Agent SDK's terminal `result` message (`modelUsage` — the SDK's documented
//! field for token/cost accounting). The serve loop [`record`]s it into the
//! central DB `agent_usage` table, attributed to the agent that ran the turn
//! (its route name, e.g. `orchestrator`/`se`/`browser`, plus its OneCLI
//! credential identity). [`report`] rolls the table up by agent and a time
//! bucket for the `report-usage` CLI. Costs are the SDK's estimates, not a
//! billing statement.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use assistant_db::Migration;
use rusqlite::Connection;
use serde::Deserialize;

pub const MODULE_ID: &str = "assistant-host-usage";

const AGENT_USAGE_V2: &str = "\
CREATE TABLE IF NOT EXISTS agent_usage (
    id                     INTEGER PRIMARY KEY,
    turn_id                TEXT NOT NULL,
    recorded_at            INTEGER NOT NULL,
    agent                  TEXT NOT NULL,
    onecli_agent           TEXT NOT NULL,
    session_id             TEXT NOT NULL,
    model                  TEXT NOT NULL,
    provider               TEXT,
    input_tokens           INTEGER NOT NULL,
    output_tokens          INTEGER NOT NULL,
    cache_read_tokens      INTEGER NOT NULL,
    cache_creation_tokens  INTEGER NOT NULL,
    cost_usd               REAL NOT NULL,
    num_turns              INTEGER,
    subtype                TEXT
);
CREATE INDEX IF NOT EXISTS agent_usage_recorded_at ON agent_usage(recorded_at);
CREATE INDEX IF NOT EXISTS agent_usage_agent ON agent_usage(agent);";

/// The central-DB migrations this module contributes (chained into the domain
/// migration set in `setup_steps`).
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(MODULE_ID, 2, "agent_usage", AGENT_USAGE_V2)]
}

// A turn emits its per-model usage in one `record` call; every model row from
// that call shares a turn id so `report` can COUNT(DISTINCT turn_id) for turns.
// `{recorded_at}-{seq}` stays unique across restarts (the counter resets but the
// timestamp advances).
static TURN_SEQ: AtomicU64 = AtomicU64::new(0);

/// The shim's `usage` row payload: per-turn totals plus a per-model breakdown.
// The shim's `usage` payload mirrors the Agent SDK's `modelUsage`, whose fields
// are camelCase (e.g. `inputTokens`, `costUSD`); accept those names via serde
// aliases so the wire format can stay faithful to the SDK.
#[derive(Debug, Deserialize)]
pub struct TurnUsage {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default, alias = "numTurns")]
    pub num_turns: Option<i64>,
    #[serde(default)]
    pub models: Vec<ModelUsage>,
}

#[derive(Debug, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, alias = "inputTokens")]
    pub input_tokens: i64,
    #[serde(default, alias = "outputTokens")]
    pub output_tokens: i64,
    #[serde(default, alias = "cacheReadInputTokens")]
    pub cache_read_input_tokens: i64,
    #[serde(default, alias = "cacheCreationInputTokens")]
    pub cache_creation_input_tokens: i64,
    #[serde(default, alias = "costUSD")]
    pub cost_usd: f64,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record a turn's per-model usage (the shim's `usage` row JSON), attributed to
/// `agent` (route name) / `onecli_agent` (credential identity) / `session_id`.
/// One DB row per model. Returns the number of model rows written. A malformed
/// payload is a no-op error — usage accounting must never fail a turn.
pub fn record(
    conn: &Connection,
    agent: &str,
    onecli_agent: &str,
    session_id: &str,
    usage_json: &str,
) -> Result<usize, String> {
    let usage: TurnUsage = serde_json::from_str(usage_json)
        .map_err(|e| format!("parsing usage payload: {e}"))?;
    if usage.models.is_empty() {
        return Ok(0);
    }
    let recorded_at = now_epoch();
    let turn_id = format!("{recorded_at}-{}", TURN_SEQ.fetch_add(1, Ordering::Relaxed));
    let mut written = 0;
    for m in &usage.models {
        conn.execute(
            "INSERT INTO agent_usage (
                turn_id, recorded_at, agent, onecli_agent, session_id, model, provider,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                cost_usd, num_turns, subtype
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                turn_id,
                recorded_at,
                agent,
                onecli_agent,
                session_id,
                m.model,
                m.provider,
                m.input_tokens,
                m.output_tokens,
                m.cache_read_input_tokens,
                m.cache_creation_input_tokens,
                m.cost_usd,
                usage.num_turns,
                usage.subtype,
            ],
        )
        .map_err(|e| format!("inserting agent_usage row: {e}"))?;
        written += 1;
    }
    Ok(written)
}

/// Time bucket for a usage report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Hour,
    Day,
    Week,
    Month,
}

impl Bucket {
    /// Parse a `--bucket` value; defaults handled by the caller.
    pub fn parse(s: &str) -> Option<Bucket> {
        match s.to_ascii_lowercase().as_str() {
            "hour" | "hourly" => Some(Bucket::Hour),
            "day" | "daily" => Some(Bucket::Day),
            "week" | "weekly" => Some(Bucket::Week),
            "month" | "monthly" => Some(Bucket::Month),
            _ => None,
        }
    }

    /// The SQLite `strftime` format that labels a row's local-time bucket.
    fn strftime(self) -> &'static str {
        match self {
            Bucket::Hour => "%Y-%m-%d %H:00",
            Bucket::Day => "%Y-%m-%d",
            Bucket::Week => "%Y-W%W",
            Bucket::Month => "%Y-%m",
        }
    }
}

/// One rolled-up row: a (time bucket, agent) pair with its totals.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageReportRow {
    pub bucket: String,
    pub agent: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost_usd: f64,
}

/// Roll up `agent_usage` by (time bucket, agent), oldest bucket first. When
/// `since_epoch` is set, only rows recorded at/after it are included.
pub fn report(
    conn: &Connection,
    since_epoch: Option<i64>,
    bucket: Bucket,
) -> Result<Vec<UsageReportRow>, String> {
    let label = format!("strftime('{}', recorded_at, 'unixepoch', 'localtime')", bucket.strftime());
    let sql = format!(
        "SELECT {label} AS bucket, agent,
                COUNT(DISTINCT turn_id) AS turns,
                COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0),
                COALESCE(SUM(cost_usd),0.0)
           FROM agent_usage
          WHERE recorded_at >= ?1
          GROUP BY bucket, agent
          ORDER BY bucket ASC, cost_usd DESC"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("preparing usage report: {e}"))?;
    let rows = stmt
        .query_map([since_epoch.unwrap_or(0)], |r| {
            Ok(UsageReportRow {
                bucket: r.get(0)?,
                agent: r.get(1)?,
                turns: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                cache_read_tokens: r.get(5)?,
                cache_creation_tokens: r.get(6)?,
                cost_usd: r.get(7)?,
            })
        })
        .map_err(|e| format!("querying usage report: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reading usage report: {e}"))?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(AGENT_USAGE_V2).unwrap();
        c
    }

    #[test]
    fn record_writes_one_row_per_model_and_report_rolls_up_by_agent() {
        let c = conn();
        let se = r#"{"subtype":"success","num_turns":12,"models":[
            {"model":"claude-opus-4-8","provider":"firstParty","input_tokens":1000,"output_tokens":200,
             "cache_read_input_tokens":50,"cache_creation_input_tokens":10,"cost_usd":0.42},
            {"model":"claude-haiku-4-5","input_tokens":300,"output_tokens":40,
             "cache_read_input_tokens":0,"cache_creation_input_tokens":0,"cost_usd":0.01}]}"#;
        assert_eq!(record(&c, "se", "inst-se", "sess-1", se).unwrap(), 2);
        let orch = r#"{"subtype":"success","num_turns":1,"models":[
            {"model":"claude-opus-4-8","input_tokens":100,"output_tokens":20,
             "cache_read_input_tokens":0,"cache_creation_input_tokens":0,"cost_usd":0.03}]}"#;
        assert_eq!(record(&c, "orchestrator", "inst", "sess-2", orch).unwrap(), 1);

        let rows = report(&c, None, Bucket::Day).unwrap();
        let se_row = rows.iter().find(|r| r.agent == "se").unwrap();
        assert_eq!(se_row.turns, 1); // two model rows, one turn
        assert_eq!(se_row.input_tokens, 1300);
        assert_eq!(se_row.output_tokens, 240);
        assert!((se_row.cost_usd - 0.43).abs() < 1e-9);
        let orch_row = rows.iter().find(|r| r.agent == "orchestrator").unwrap();
        assert_eq!(orch_row.turns, 1);
        assert!((orch_row.cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn parses_the_shims_camelcase_sdk_payload() {
        // The shim emits the Agent SDK's camelCase field names verbatim; the host
        // must read the values, not default them to zero.
        let c = conn();
        let payload = r#"{"subtype":"success","numTurns":3,"models":[
            {"model":"claude-opus-4-8","provider":"firstParty","inputTokens":1500,"outputTokens":210,
             "cacheReadInputTokens":400,"cacheCreationInputTokens":30,"costUSD":0.71}]}"#;
        assert_eq!(record(&c, "orchestrator", "inst", "sess", payload).unwrap(), 1);
        let rows = report(&c, None, Bucket::Day).unwrap();
        let row = &rows[0];
        assert_eq!(row.input_tokens, 1500);
        assert_eq!(row.output_tokens, 210);
        assert_eq!(row.cache_read_tokens, 400);
        assert!((row.cost_usd - 0.71).abs() < 1e-9);
    }

    #[test]
    fn empty_models_is_a_noop_and_bad_json_errs() {
        let c = conn();
        assert_eq!(record(&c, "se", "x", "s", r#"{"models":[]}"#).unwrap(), 0);
        assert!(record(&c, "se", "x", "s", "not json").is_err());
    }

    #[test]
    fn bucket_parses_common_spellings() {
        assert_eq!(Bucket::parse("hour"), Some(Bucket::Hour));
        assert_eq!(Bucket::parse("Daily"), Some(Bucket::Day));
        assert_eq!(Bucket::parse("MONTH"), Some(Bucket::Month));
        assert_eq!(Bucket::parse("nope"), None);
    }
}
