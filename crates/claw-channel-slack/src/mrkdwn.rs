//! Render neutral [`OutboundContent`] into Slack mrkdwn text.
//!
//! Two concerns live here. First, escaping: Slack reserves `&`, `<`, and `>` for
//! control sequences (links, mentions), so any literal occurrence in user-facing
//! text must be entity-escaped or it will be mis-parsed or dropped. Second,
//! flavor translation: the model emits GitHub-flavored Markdown (`**bold**`,
//! `## headings`, `[text](url)`), but Slack's mrkdwn is a different dialect
//! (`*bold*`, no headings, `<url|text>`). Without translation that syntax reaches
//! Slack verbatim and shows up literally. [`to_mrkdwn`] bridges the dialects; rich
//! kinds (card, question) degrade to a readable mrkdwn layout rather than relying
//! on a separate Block Kit payload.

use claw_router::OutboundContent;

/// Escape the three characters Slack treats specially in mrkdwn text. This is
/// the minimal, Slack-documented set — escaping more would corrupt the text.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// Translate a GitHub-flavored Markdown body (what the model emits) into Slack
/// mrkdwn, entity-escaping `&<>` as it goes.
///
/// Translated: `**bold**`/`__bold__` → `*bold*`, `~~strike~~` → `~strike~`,
/// `# heading` → `*heading*` (Slack has no headings), `- `/`* `/`+ ` bullets →
/// `• `, and `[text](url)` → `<url|text>`. A fenced code block keeps its content
/// verbatim and its opening language tag is dropped (Slack would show it as a
/// literal first code line).
///
/// Deliberately left alone: the contents of inline (`` `code` ``) and fenced
/// code (so identifiers, snippets, and `<`-bearing code survive), and bare single
/// `*`/`_` emphasis — Slack already renders `_x_` as italic, and translating a
/// lone `*` would mangle `a * b` arithmetic, while word-internal `_` would mangle
/// `snake_case`.
pub fn to_mrkdwn(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in body.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            // Normalize to a bare fence: Slack renders a language tag as a
            // literal first line inside the code block.
            out.push("```".to_string());
            continue;
        }
        if in_fence {
            // Inside a code block Slack does no mrkdwn parsing; only entity-escape.
            out.push(escape(line));
        } else {
            out.push(convert_line(line));
        }
    }
    out.join("\n")
}

/// Render outbound content as a Slack mrkdwn string.
pub fn render(content: &OutboundContent) -> String {
    match content {
        OutboundContent::Text { body } => to_mrkdwn(body),
        OutboundContent::Card { title, body, .. } => {
            format!("*{}*\n{}", escape(title), to_mrkdwn(body))
        }
        OutboundContent::Question { prompt, options, .. } => {
            let mut out = to_mrkdwn(prompt);
            for (i, opt) in options.iter().enumerate() {
                out.push_str(&format!("\n{}. {}", i + 1, to_mrkdwn(opt)));
            }
            out
        }
    }
}

/// Translate one non-fence line: a heading or bullet is a line-level rewrite;
/// anything else is inline-translated as prose.
fn convert_line(line: &str) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);

    if is_thematic_break(rest) {
        // Slack mrkdwn has no horizontal rule, so a `---`/`***`/`___` separator
        // would render as literal characters. Drop it; the blank lines the model
        // puts around a rule already separate the sections.
        return String::new();
    }
    if let Some(text) = heading_text(rest) {
        return format!("{indent}*{}*", convert_inline(text));
    }
    if let Some(item) = bullet_text(rest) {
        return format!("{indent}\u{2022} {}", convert_inline(item));
    }
    convert_inline(line)
}

/// The text of an ATX heading (`#`..`######` then a space), or `None`. Requires
/// the space so a bare `#tag` is not treated as a heading.
fn heading_text(s: &str) -> Option<&str> {
    let hashes = s.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        return s[hashes..].strip_prefix(' ').map(str::trim_end);
    }
    None
}

/// True for a Markdown thematic break: a line of three or more matching `-`,
/// `*`, or `_` markers, with any spacing between them (`---`, `***`, `- - -`).
fn is_thematic_break(s: &str) -> bool {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 3 {
        return false;
    }
    let first = compact.chars().next().unwrap();
    matches!(first, '-' | '*' | '_') && compact.chars().all(|c| c == first)
}

/// The text of a bullet item (`-`, `*`, or `+` then a space), or `None`.
fn bullet_text(s: &str) -> Option<&str> {
    for marker in ['-', '*', '+'] {
        if let Some(rest) = s.strip_prefix(marker)
            && let Some(item) = rest.strip_prefix(' ')
        {
            return Some(item);
        }
    }
    None
}

/// Inline-translate a fragment, leaving inline code spans verbatim. Prose
/// segments are escaped and emphasis/link-translated; a `` `code` `` span keeps
/// its content (entity-escaped only).
fn convert_inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        if let Some(rel_close) = rest[open + 1..].find('`') {
            let close = open + 1 + rel_close;
            out.push_str(&transform_prose(&rest[..open]));
            out.push('`');
            out.push_str(&escape(&rest[open + 1..close]));
            out.push('`');
            rest = &rest[close + 1..];
        } else {
            break; // unmatched backtick: the remainder is prose
        }
    }
    out.push_str(&transform_prose(rest));
    out
}

/// Escape, then translate emphasis and links on a code-free prose fragment.
/// Escaping first means the only `<`/`>` left in the result are the link control
/// sequences this function emits.
fn transform_prose(s: &str) -> String {
    let s = escape(s);
    let s = convert_links(&s);
    let s = convert_pairs(&s, "**", '*');
    let s = convert_pairs(&s, "__", '*');
    convert_pairs(&s, "~~", '~')
}

/// Rewrite `[label](url)` as `<url|label>`. Skips a candidate whose label nests a
/// `[` or whose url is empty, copying it through unchanged.
fn convert_links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](") else {
            break;
        };
        let label = &rest[open + 1..open + mid];
        let after = &rest[open + mid + 2..];
        let Some(close) = after.find(')') else {
            break;
        };
        let url = &after[..close];
        if label.contains('[') || url.is_empty() {
            // Not a clean link; emit up to and including the `[` and continue.
            out.push_str(&rest[..open + 1]);
            rest = &rest[open + 1..];
            continue;
        }
        out.push_str(&rest[..open]);
        out.push('<');
        out.push_str(url);
        out.push('|');
        out.push_str(label);
        out.push('>');
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Replace each balanced pair of `delim` with `wrap` … `wrap`. An unbalanced
/// trailing `delim` is left literal. Used to turn `**x**`/`__x__` into `*x*` and
/// `~~x~~` into `~x~`.
fn convert_pairs(s: &str, delim: &str, wrap: char) -> String {
    let parts: Vec<&str> = s.split(delim).collect();
    let delim_count = parts.len() - 1;
    if delim_count < 2 {
        return s.to_string();
    }
    let paired = (delim_count / 2) * 2;
    let mut out = String::new();
    out.push_str(parts[0]);
    for (di, part) in parts[1..].iter().enumerate() {
        if di < paired {
            out.push(wrap);
        } else {
            out.push_str(delim);
        }
        out.push_str(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_characters_are_entity_escaped() {
        assert_eq!(escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        // Nothing else is touched.
        assert_eq!(escape("*bold* _i_ `code`"), "*bold* _i_ `code`");
    }

    #[test]
    fn text_is_escaped() {
        let c = OutboundContent::Text { body: "1 < 2 && true".to_string() };
        assert_eq!(render(&c), "1 &lt; 2 &amp;&amp; true");
    }

    #[test]
    fn card_renders_bold_title_then_body() {
        let c = OutboundContent::Card {
            title: "Deploy <prod>".to_string(),
            body: "done".to_string(),
            fallback: "Deploy prod: done".to_string(),
        };
        assert_eq!(render(&c), "*Deploy &lt;prod&gt;*\ndone");
    }

    #[test]
    fn question_renders_a_numbered_list() {
        let c = OutboundContent::Question {
            prompt: "Pick one".to_string(),
            options: vec!["Yes".to_string(), "No & maybe".to_string()],
            fallback: "Pick one: Yes / No & maybe".to_string(),
        };
        assert_eq!(render(&c), "Pick one\n1. Yes\n2. No &amp; maybe");
    }

    #[test]
    fn double_star_and_underscore_bold_become_single_star() {
        assert_eq!(to_mrkdwn("**3.14**"), "*3.14*");
        assert_eq!(to_mrkdwn("a **bold** b"), "a *bold* b");
        assert_eq!(to_mrkdwn("an __bold__ word"), "an *bold* word");
    }

    #[test]
    fn strikethrough_collapses_to_single_tilde() {
        assert_eq!(to_mrkdwn("~~gone~~"), "~gone~");
    }

    #[test]
    fn headings_become_bold_lines() {
        assert_eq!(to_mrkdwn("# Title"), "*Title*");
        assert_eq!(to_mrkdwn("## 🧠 General"), "*🧠 General*");
        // A bare #tag (no space) is not a heading.
        assert_eq!(to_mrkdwn("#nottag"), "#nottag");
        // Seven hashes is past the heading range.
        assert_eq!(to_mrkdwn("####### x"), "####### x");
    }

    #[test]
    fn bullets_become_slack_bullets_preserving_indent() {
        assert_eq!(to_mrkdwn("- one"), "\u{2022} one");
        assert_eq!(to_mrkdwn("* two"), "\u{2022} two");
        assert_eq!(to_mrkdwn("  - nested"), "  \u{2022} nested");
    }

    #[test]
    fn links_become_angle_bracket_form() {
        assert_eq!(
            to_mrkdwn("see [the docs](https://example.com)"),
            "see <https://example.com|the docs>"
        );
        // A query `&` is entity-escaped, as Slack expects in a link URL.
        assert_eq!(
            to_mrkdwn("[x](https://e.com?a=1&b=2)"),
            "<https://e.com?a=1&amp;b=2|x>"
        );
    }

    #[test]
    fn code_spans_and_blocks_are_left_verbatim() {
        // Bold markers inside inline code are not translated.
        assert_eq!(to_mrkdwn("call `a**b**c` now"), "call `a**b**c` now");
        // Arithmetic asterisks outside code are untouched (no lone-* italic).
        assert_eq!(to_mrkdwn("2 * 3 * 4"), "2 * 3 * 4");
        // A fenced block keeps content and drops the language tag.
        let fenced = "```python\nx = a < b\n```";
        assert_eq!(to_mrkdwn(fenced), "```\nx = a &lt; b\n```");
    }

    #[test]
    fn thematic_breaks_are_dropped() {
        // A horizontal rule between two paragraphs becomes a blank line.
        assert_eq!(to_mrkdwn("a\n\n---\n\nb"), "a\n\n\n\nb");
        assert_eq!(to_mrkdwn("***"), "");
        assert_eq!(to_mrkdwn("___"), "");
        assert_eq!(to_mrkdwn("- - -"), "");
        // A bullet is not a rule, and bold is not a rule.
        assert_eq!(to_mrkdwn("- item"), "\u{2022} item");
        assert_eq!(to_mrkdwn("**x**"), "*x*");
    }

    #[test]
    fn snake_case_is_not_italicized() {
        assert_eq!(to_mrkdwn("call do_a_thing here"), "call do_a_thing here");
    }

    #[test]
    fn a_realistic_multiline_reply_translates() {
        let body = "## Code\n- Review PRs (`/code-review`)\n- **Important**: ship it";
        assert_eq!(
            to_mrkdwn(body),
            "*Code*\n\u{2022} Review PRs (`/code-review`)\n\u{2022} *Important*: ship it"
        );
    }
}
