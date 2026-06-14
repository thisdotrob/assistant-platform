//! The neutral command model the host CLI and the in-container bridge share.
//!
//! A [`CommandRequest`] names a resource, an action on it, and its arguments.
//! Resources answer with a [`CommandOutcome`] carrying a [`ResultTable`] so the
//! same structured result can be rendered as JSON or an aligned table. None of
//! this knows about any concrete domain — resources are wired in by the host.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Who is invoking a command. Access control keys off this: an operator on the
/// host is trusted, while an agent inside a container is gated by policy and
/// scoped to its group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Caller {
    /// A human operator at the host control surface.
    Operator,
    /// An agent acting on behalf of a group, via the in-container bridge.
    Agent { agent_group_id: String },
}

impl Caller {
    /// The group an agent caller is scoped to, if any. Operators are unscoped.
    pub fn agent_group_id(&self) -> Option<&str> {
        match self {
            Caller::Operator => None,
            Caller::Agent { agent_group_id } => Some(agent_group_id),
        }
    }
}

/// Whether an action reads or mutates state. Writes are policy-gated; reads are
/// allowed by default within scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
}

impl ActionKind {
    pub fn is_write(self) -> bool {
        matches!(self, ActionKind::Write)
    }
}

/// A parsed command: the resource to act on, the action, positional arguments,
/// and named options. Options are an ordered map so rendering and round-trips
/// stay deterministic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub resource: String,
    pub action: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl CommandRequest {
    /// A resource/action command with no arguments.
    pub fn new(resource: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            action: action.into(),
            args: Vec::new(),
            options: BTreeMap::new(),
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }
}

/// Named columns and the rows beneath them. Every cell is already a string so
/// the result is render-agnostic; numeric/typed values are the resource's job
/// to format.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl ResultTable {
    pub fn new(columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            columns: columns.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    /// Append a row. The caller is responsible for matching the column count;
    /// rendering tolerates a mismatch by padding/truncating to the header.
    pub fn push_row(&mut self, row: impl IntoIterator<Item = impl Into<String>>) {
        self.rows.push(row.into_iter().map(Into::into).collect());
    }
}

/// The result of running a command: a structured table on success, or an error
/// message. Resources never panic on bad input — they return `Error`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandOutcome {
    Ok {
        table: ResultTable,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Error {
        message: String,
    },
}

impl CommandOutcome {
    pub fn table(table: ResultTable) -> Self {
        CommandOutcome::Ok { table, message: None }
    }

    pub fn message(table: ResultTable, message: impl Into<String>) -> Self {
        CommandOutcome::Ok { table, message: Some(message.into()) }
    }

    pub fn error(message: impl Into<String>) -> Self {
        CommandOutcome::Error { message: message.into() }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, CommandOutcome::Ok { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_scope_is_only_set_for_agents() {
        assert_eq!(Caller::Operator.agent_group_id(), None);
        assert_eq!(
            Caller::Agent { agent_group_id: "g1".to_string() }.agent_group_id(),
            Some("g1")
        );
    }

    #[test]
    fn request_builder_collects_args_and_options() {
        let req = CommandRequest::new("users", "list")
            .with_args(["--role", "admin"])
            .with_option("format", "json");
        assert_eq!(req.resource, "users");
        assert_eq!(req.action, "list");
        assert_eq!(req.args, vec!["--role", "admin"]);
        assert_eq!(req.options.get("format").map(String::as_str), Some("json"));
    }

    #[test]
    fn outcome_constructors_classify_success() {
        let ok = CommandOutcome::message(ResultTable::new(["id"]), "1 row");
        assert!(ok.is_ok());
        assert!(!CommandOutcome::error("boom").is_ok());
    }

    #[test]
    fn outcome_round_trips_through_json() {
        let mut table = ResultTable::new(["id", "name"]);
        table.push_row(["1", "alice"]);
        let outcome = CommandOutcome::table(table);
        let json = serde_json::to_string(&outcome).unwrap();
        let back: CommandOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    #[test]
    fn ok_outcome_omits_absent_message_in_json() {
        let json = serde_json::to_string(&CommandOutcome::table(ResultTable::new(["x"]))).unwrap();
        assert!(!json.contains("message"));
    }
}
