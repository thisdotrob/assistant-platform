//! Render neutral [`OutboundContent`] into Telegram-safe MarkdownV2 text.
//!
//! MarkdownV2 reserves a large punctuation set; any literal occurrence in
//! user-facing text must be backslash-escaped or Telegram rejects the whole
//! message with a 400. Our own formatting markers (`*` for bold) are applied
//! around already-escaped text so they are never themselves escaped.

use claw_router::OutboundContent;

/// The characters MarkdownV2 reserves. Each literal occurrence is backslash
/// escaped. Source: Telegram Bot API "MarkdownV2 style".
const RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Escape reserved MarkdownV2 characters in arbitrary text.
pub fn escape_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if RESERVED.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Render outbound content as Telegram-safe MarkdownV2.
pub fn render(content: &OutboundContent) -> String {
    match content {
        OutboundContent::Text { body } => escape_markdown_v2(body),
        OutboundContent::Card { title, body, .. } => {
            format!("*{}*\n{}", escape_markdown_v2(title), escape_markdown_v2(body))
        }
        OutboundContent::Question { prompt, options, .. } => {
            let mut out = escape_markdown_v2(prompt);
            for (i, opt) in options.iter().enumerate() {
                // The list-number dot is itself reserved, so escape it.
                out.push_str(&format!("\n{}\\. {}", i + 1, escape_markdown_v2(opt)));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_punctuation_is_escaped() {
        assert_eq!(escape_markdown_v2("a.b-c!"), "a\\.b\\-c\\!");
        assert_eq!(escape_markdown_v2("(x)[y]"), "\\(x\\)\\[y\\]");
        // A literal backslash is escaped too.
        assert_eq!(escape_markdown_v2("a\\b"), "a\\\\b");
    }

    #[test]
    fn plain_letters_and_spaces_are_untouched() {
        assert_eq!(escape_markdown_v2("hello world"), "hello world");
    }

    #[test]
    fn text_is_escaped() {
        let c = OutboundContent::Text { body: "deploy v1.2 done!".to_string() };
        assert_eq!(render(&c), "deploy v1\\.2 done\\!");
    }

    #[test]
    fn card_bolds_an_escaped_title() {
        let c = OutboundContent::Card {
            title: "Build #42".to_string(),
            body: "ok.".to_string(),
            fallback: "Build 42: ok".to_string(),
        };
        assert_eq!(render(&c), "*Build \\#42*\nok\\.");
    }

    #[test]
    fn question_numbers_with_escaped_dots() {
        let c = OutboundContent::Question {
            prompt: "Pick".to_string(),
            options: vec!["a-1".to_string(), "b".to_string()],
            fallback: "Pick: a-1 / b".to_string(),
        };
        assert_eq!(render(&c), "Pick\n1\\. a\\-1\n2\\. b");
    }
}
