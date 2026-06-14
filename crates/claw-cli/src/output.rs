//! Rendering a [`CommandOutcome`] for a human (an aligned table) or a machine
//! (JSON).
//!
//! The format and verbosity are passed in; the host sources their defaults from
//! claw-config. JSON output is always complete and ignores verbosity so tools
//! parsing it get the full result; verbosity only trims the human table's
//! trailing summary.

use crate::command::CommandOutcome;

/// How to render a command result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// A column-aligned text table for a terminal.
    #[default]
    Table,
    /// Pretty-printed JSON for machine consumption (and the in-container bridge).
    Json,
}

impl OutputFormat {
    /// Parse a format name (`"table"`/`"json"`), case-insensitively.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "table" => Some(OutputFormat::Table),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}

/// How much to render. Only affects the human table: `Quiet` drops the trailing
/// summary line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
}

/// Render an outcome in the requested format.
pub fn render(outcome: &CommandOutcome, format: OutputFormat, verbosity: Verbosity) -> String {
    match format {
        OutputFormat::Json => render_json(outcome),
        OutputFormat::Table => render_table(outcome, verbosity),
    }
}

fn render_json(outcome: &CommandOutcome) -> String {
    // Our model serializes infallibly (strings, vecs, an enum); fall back to a
    // valid JSON error object on the off chance serde reports a failure.
    serde_json::to_string_pretty(outcome)
        .unwrap_or_else(|e| format!("{{\"status\":\"error\",\"message\":\"render failed: {e}\"}}"))
}

fn render_table(outcome: &CommandOutcome, verbosity: Verbosity) -> String {
    let (table, message) = match outcome {
        CommandOutcome::Error { message } => return format!("error: {}", sanitize(message)),
        CommandOutcome::Ok { table, message } => (table, message),
    };

    let column_count = table.columns.len();

    // Sanitize every displayed string up front so width and emission agree and
    // no agent-controlled control/escape sequence reaches the operator's
    // terminal. Each row is normalized to exactly `column_count` cells.
    let header: Vec<String> = table.columns.iter().map(|c| sanitize(c)).collect();
    let rows: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|row| (0..column_count).map(|col| sanitize(cell(row, col))).collect())
        .collect();

    // Column width = widest of the header and any cell in that column.
    let mut widths: Vec<usize> = header.iter().map(|c| c.chars().count()).collect();
    for row in &rows {
        for (col, width) in widths.iter_mut().enumerate() {
            let w = row[col].chars().count();
            if w > *width {
                *width = w;
            }
        }
    }

    let mut out = String::new();
    if column_count > 0 {
        let header_refs: Vec<&str> = header.iter().map(String::as_str).collect();
        push_row(&mut out, &widths, &header_refs);
        out.push('\n');
        out.push_str(&separator(&widths));
        for row in &rows {
            out.push('\n');
            let cells: Vec<&str> = row.iter().map(String::as_str).collect();
            push_row(&mut out, &widths, &cells);
        }
    }

    if rows.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("(no rows)");
    }

    if let (Verbosity::Normal, Some(message)) = (verbosity, message) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&sanitize(message));
    }

    out
}

/// Replace control characters (C0/C1, DEL, and the ESC that starts ANSI
/// sequences) with the Unicode replacement char, so agent-controlled text
/// rendered for an operator cannot move the cursor, clear the screen, or forge
/// output. JSON output needs no equivalent: serde escapes control chars itself.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Write one space-padded row, joining columns with two spaces.
fn push_row(out: &mut String, widths: &[usize], cells: &[&str]) {
    for (col, width) in widths.iter().enumerate() {
        if col > 0 {
            out.push_str("  ");
        }
        let value = cells.get(col).copied().unwrap_or("");
        out.push_str(value);
        for _ in value.chars().count()..*width {
            out.push(' ');
        }
    }
    // Trailing padding on the last column is whitespace-only; trim it so lines
    // don't carry invisible spaces.
    while out.ends_with(' ') {
        out.pop();
    }
}

/// A row's cell at `col`, or `""` when the row is shorter than the header.
fn cell(row: &[String], col: usize) -> &str {
    row.get(col).map(String::as_str).unwrap_or("")
}

fn separator(widths: &[usize]) -> String {
    let mut s = String::new();
    for (col, width) in widths.iter().enumerate() {
        if col > 0 {
            s.push_str("  ");
        }
        for _ in 0..*width {
            s.push('-');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::ResultTable;

    fn sample() -> CommandOutcome {
        let mut table = ResultTable::new(["id", "name"]);
        table.push_row(["1", "alice"]);
        table.push_row(["20", "bob"]);
        CommandOutcome::message(table, "2 rows")
    }

    #[test]
    fn table_aligns_columns_to_their_widest_value() {
        let rendered = render(&sample(), OutputFormat::Table, Verbosity::Normal);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "id  name");
        assert_eq!(lines[1], "--  -----");
        assert_eq!(lines[2], "1   alice");
        assert_eq!(lines[3], "20  bob");
        assert_eq!(lines[4], "2 rows");
    }

    #[test]
    fn quiet_drops_the_summary_but_keeps_the_table() {
        let rendered = render(&sample(), OutputFormat::Table, Verbosity::Quiet);
        assert!(!rendered.contains("2 rows"));
        assert!(rendered.contains("alice"));
    }

    #[test]
    fn empty_result_reports_no_rows() {
        let outcome = CommandOutcome::table(ResultTable::new(["id"]));
        let rendered = render(&outcome, OutputFormat::Table, Verbosity::Normal);
        assert!(rendered.contains("(no rows)"));
    }

    #[test]
    fn error_renders_as_a_single_line() {
        let rendered = render(&CommandOutcome::error("nope"), OutputFormat::Table, Verbosity::Normal);
        assert_eq!(rendered, "error: nope");
    }

    #[test]
    fn json_is_parseable_and_ignores_verbosity() {
        let quiet = render(&sample(), OutputFormat::Json, Verbosity::Quiet);
        let normal = render(&sample(), OutputFormat::Json, Verbosity::Normal);
        assert_eq!(quiet, normal);
        let value: serde_json::Value = serde_json::from_str(&quiet).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["message"], "2 rows");
    }

    #[test]
    fn ragged_rows_are_padded_to_the_header() {
        let mut table = ResultTable::new(["a", "b", "c"]);
        table.push_row(["x"]); // short row
        let rendered = render(&CommandOutcome::table(table), OutputFormat::Table, Verbosity::Normal);
        // No panic, header still has three columns.
        assert!(rendered.starts_with("a  b  c"));
    }

    #[test]
    fn format_parse_is_case_insensitive() {
        assert_eq!(OutputFormat::parse("JSON"), Some(OutputFormat::Json));
        assert_eq!(OutputFormat::parse(" table "), Some(OutputFormat::Table));
        assert_eq!(OutputFormat::parse("yaml"), None);
    }

    #[test]
    fn control_and_escape_characters_are_neutralized_in_tables() {
        let mut table = ResultTable::new(["name"]);
        // An agent-controlled cell trying to clear the screen and forge output.
        table.push_row(["\u{1b}[2Jevil\r"]);
        let rendered = render(
            &CommandOutcome::message(table, "done\u{1b}[31m"),
            OutputFormat::Table,
            Verbosity::Normal,
        );
        assert!(!rendered.contains('\u{1b}'), "ESC must not reach the terminal");
        assert!(!rendered.contains('\r'), "CR must not reach the terminal");
        assert!(rendered.contains("evil"), "printable text is preserved");
        assert!(rendered.contains('\u{FFFD}'), "control chars become the replacement char");
    }

    #[test]
    fn error_message_is_also_sanitized() {
        let rendered = render(
            &CommandOutcome::error("bad\u{1b}[2J"),
            OutputFormat::Table,
            Verbosity::Normal,
        );
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.starts_with("error: bad"));
    }
}
