//! Daily journal validation.
//!
//! A daily journal file is the one free-form surface in the memory tree, so it
//! still has a small contract the host validates before accepting a write:
//!
//! - the first non-blank line is an ISO date heading `# YYYY-MM-DD`;
//! - every other non-blank line is a bullet `- <content>` with non-empty content;
//! - blank lines are allowed anywhere after the heading.
//!
//! Anything else (a stray paragraph, a heading in the wrong place, an empty
//! bullet) is rejected with the offending 1-based line number so the writer can
//! be corrected rather than silently storing malformed journal state.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    MissingDateHeading,
    InvalidDateHeading { line: usize, found: String },
    EmptyBullet { line: usize },
    UnexpectedLine { line: usize, found: String },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::MissingDateHeading => {
                write!(f, "journal has no `# YYYY-MM-DD` date heading")
            }
            JournalError::InvalidDateHeading { line, found } => {
                write!(f, "line {line}: invalid journal date heading {found:?}")
            }
            JournalError::EmptyBullet { line } => write!(f, "line {line}: empty journal bullet"),
            JournalError::UnexpectedLine { line, found } => {
                write!(f, "line {line}: unexpected journal line {found:?}")
            }
        }
    }
}

impl std::error::Error for JournalError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalReport {
    pub date: String,
    pub bullets: usize,
}

/// Validate a single bullet line, returning its trimmed content. The line must
/// start with `- ` and carry non-whitespace content.
pub fn validate_journal_bullet(line: &str) -> Result<&str, JournalError> {
    if let Some(rest) = line.strip_prefix("- ") {
        let content = rest.trim();
        if content.is_empty() {
            Err(JournalError::EmptyBullet { line: 0 })
        } else {
            Ok(content)
        }
    } else {
        Err(JournalError::UnexpectedLine {
            line: 0,
            found: line.to_string(),
        })
    }
}

/// Validate a whole journal file.
pub fn validate_journal(content: &str) -> Result<JournalReport, JournalError> {
    let mut lines = content.lines().enumerate();
    let date = loop {
        match lines.next() {
            None => return Err(JournalError::MissingDateHeading),
            Some((_, line)) if line.trim().is_empty() => continue,
            Some((idx, line)) => {
                let heading = line.strip_prefix("# ").ok_or_else(|| {
                    JournalError::InvalidDateHeading {
                        line: idx + 1,
                        found: line.to_string(),
                    }
                })?;
                if !is_iso_date(heading.trim()) {
                    return Err(JournalError::InvalidDateHeading {
                        line: idx + 1,
                        found: line.to_string(),
                    });
                }
                break heading.trim().to_string();
            }
        }
    };

    let mut bullets = 0;
    for (idx, line) in lines {
        if line.trim().is_empty() {
            continue;
        }
        match validate_journal_bullet(line) {
            Ok(_) => bullets += 1,
            Err(JournalError::EmptyBullet { .. }) => {
                return Err(JournalError::EmptyBullet { line: idx + 1 })
            }
            Err(_) => {
                return Err(JournalError::UnexpectedLine {
                    line: idx + 1,
                    found: line.to_string(),
                })
            }
        }
    }
    Ok(JournalReport { date, bullets })
}

/// A loose `YYYY-MM-DD` check: digit shape plus sane month/day ranges.
fn is_iso_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| s[range].bytes().all(|b| b.is_ascii_digit());
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u8 = s[5..7].parse().unwrap_or(0);
    let day: u8 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_journal_counts_bullets() {
        let content = "# 2026-06-01\n\n- talked to Alice about mornings\n- ENG-1234 shipped\n";
        let report = validate_journal(content).unwrap();
        assert_eq!(report.date, "2026-06-01");
        assert_eq!(report.bullets, 2);
    }

    #[test]
    fn missing_date_heading_is_rejected() {
        assert_eq!(
            validate_journal("- a bullet with no heading\n"),
            Err(JournalError::InvalidDateHeading {
                line: 1,
                found: "- a bullet with no heading".to_string()
            })
        );
        assert_eq!(validate_journal("\n\n"), Err(JournalError::MissingDateHeading));
    }

    #[test]
    fn invalid_date_is_rejected() {
        assert!(matches!(
            validate_journal("# 2026-13-40\n- x\n"),
            Err(JournalError::InvalidDateHeading { line: 1, .. })
        ));
        assert!(matches!(
            validate_journal("# not a date\n"),
            Err(JournalError::InvalidDateHeading { line: 1, .. })
        ));
    }

    #[test]
    fn empty_bullet_is_rejected_with_line_number() {
        assert_eq!(
            validate_journal("# 2026-06-01\n- ok\n- \n"),
            Err(JournalError::EmptyBullet { line: 3 })
        );
    }

    #[test]
    fn unexpected_line_is_rejected_with_line_number() {
        assert_eq!(
            validate_journal("# 2026-06-01\n- ok\na stray paragraph\n"),
            Err(JournalError::UnexpectedLine {
                line: 3,
                found: "a stray paragraph".to_string()
            })
        );
    }

    #[test]
    fn standalone_bullet_validator() {
        assert_eq!(validate_journal_bullet("- hello").unwrap(), "hello");
        assert!(matches!(
            validate_journal_bullet("- "),
            Err(JournalError::EmptyBullet { .. })
        ));
        assert!(matches!(
            validate_journal_bullet("not a bullet"),
            Err(JournalError::UnexpectedLine { .. })
        ));
    }
}
