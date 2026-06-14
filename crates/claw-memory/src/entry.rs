//! The on-disk memory entry format: YAML front matter over a markdown body.
//!
//! Markdown is the source of truth; the qmd index and the derived metadata
//! catalog are rebuildable from it. Every entry the system writes carries the
//! required front matter so host-side RAG can filter candidates by scope before
//! injection without re-reading the body. Files a human dropped in by hand
//! without valid front matter are not trusted: they are normalized to
//! `source_type=manual`, `confidence=unknown`, and the most restrictive
//! `no_rag` scope/reuse until an explicit editor save re-classifies them.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Retrieval scope inside the owning agent group. This never widens the hard
/// agent-group isolation boundary; it only narrows reuse within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    AllChats,
    Channel,
    Thread,
    User,
    OwnerAdmin,
    NoRag,
}

/// How an entry came to exist. Drives default trust and reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    UserSaid,
    Observed,
    Backfilled,
    Inferred,
    Imported,
    SpecialistDerived,
    Manual,
}

/// The channel family a source reference points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChannel {
    Slack,
    Telegram,
    Cli,
    Agent,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReusePolicy {
    SameScope,
    BroaderOk,
    CiteOnly,
    NoRag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Normal,
    Ephemeral,
    ArchiveOnly,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::AllChats => "all_chats",
            Scope::Channel => "channel",
            Scope::Thread => "thread",
            Scope::User => "user",
            Scope::OwnerAdmin => "owner_admin",
            Scope::NoRag => "no_rag",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "all_chats" => Scope::AllChats,
            "channel" => Scope::Channel,
            "thread" => Scope::Thread,
            "user" => Scope::User,
            "owner_admin" => Scope::OwnerAdmin,
            "no_rag" => Scope::NoRag,
            _ => return None,
        })
    }
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::UserSaid => "user_said",
            SourceType::Observed => "observed",
            SourceType::Backfilled => "backfilled",
            SourceType::Inferred => "inferred",
            SourceType::Imported => "imported",
            SourceType::SpecialistDerived => "specialist_derived",
            SourceType::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user_said" => SourceType::UserSaid,
            "observed" => SourceType::Observed,
            "backfilled" => SourceType::Backfilled,
            "inferred" => SourceType::Inferred,
            "imported" => SourceType::Imported,
            "specialist_derived" => SourceType::SpecialistDerived,
            "manual" => SourceType::Manual,
            _ => return None,
        })
    }
}

impl SourceChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceChannel::Slack => "slack",
            SourceChannel::Telegram => "telegram",
            SourceChannel::Cli => "cli",
            SourceChannel::Agent => "agent",
            SourceChannel::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "slack" => SourceChannel::Slack,
            "telegram" => SourceChannel::Telegram,
            "cli" => SourceChannel::Cli,
            "agent" => SourceChannel::Agent,
            "system" => SourceChannel::System,
            _ => return None,
        })
    }
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
            Confidence::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            "low" => Confidence::Low,
            "unknown" => Confidence::Unknown,
            _ => return None,
        })
    }
}

impl ReusePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ReusePolicy::SameScope => "same_scope",
            ReusePolicy::BroaderOk => "broader_ok",
            ReusePolicy::CiteOnly => "cite_only",
            ReusePolicy::NoRag => "no_rag",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "same_scope" => ReusePolicy::SameScope,
            "broader_ok" => ReusePolicy::BroaderOk,
            "cite_only" => ReusePolicy::CiteOnly,
            "no_rag" => ReusePolicy::NoRag,
            _ => return None,
        })
    }
}

impl Retention {
    pub fn as_str(self) -> &'static str {
        match self {
            Retention::Normal => "normal",
            Retention::Ephemeral => "ephemeral",
            Retention::ArchiveOnly => "archive_only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "normal" => Retention::Normal,
            "ephemeral" => Retention::Ephemeral,
            "archive_only" => Retention::ArchiveOnly,
            _ => return None,
        })
    }
}

/// Where an entry was captured from, when applicable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub channel: Option<SourceChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
}

/// The required (and optional-when-applicable) metadata that precedes every
/// memory body. Field order here is the order serialized back to disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFrontMatter {
    pub memory_id: String,
    pub owner_agent_group_id: String,
    pub scope: Scope,
    pub source_type: SourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<SourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    pub confidence: Confidence,
    pub reuse_policy: ReusePolicy,
    pub retention: Retention,
}

/// A parsed memory entry: validated front matter plus the markdown body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDocument {
    pub front_matter: MemoryFrontMatter,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryError {
    /// Front matter was present but could not be parsed as valid metadata.
    MalformedFrontMatter(String),
}

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntryError::MalformedFrontMatter(e) => write!(f, "malformed memory front matter: {e}"),
        }
    }
}

impl std::error::Error for EntryError {}

const FENCE: &str = "---";

/// Split a raw file into (optional front matter YAML, body). Returns `None` for
/// the front matter when the file does not open with a `---` fence.
fn split_front_matter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let after_open = match trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => return (None, raw),
    };
    // Find a closing fence that sits on its own line.
    let mut search_from = 0;
    while let Some(idx) = after_open[search_from..].find(FENCE) {
        let abs = search_from + idx;
        let at_line_start = abs == 0 || after_open.as_bytes()[abs - 1] == b'\n';
        let after = &after_open[abs + FENCE.len()..];
        let ends_line = after.is_empty() || after.starts_with('\n') || after.starts_with('\r');
        if at_line_start && ends_line {
            let yaml = &after_open[..abs];
            let body = after.strip_prefix('\n').or_else(|| {
                after.strip_prefix("\r\n").or_else(|| after.strip_prefix('\r'))
            });
            return (Some(yaml), body.unwrap_or(""));
        }
        search_from = abs + FENCE.len();
    }
    // Opened a fence that never closed: treat the whole thing as body.
    (None, raw)
}

impl MemoryDocument {
    /// Parse a file written by the system. Front matter must be present and
    /// valid; otherwise this is a malformed entry (use [`MemoryDocument::normalize`]
    /// for untrusted, hand-authored files).
    pub fn parse(raw: &str) -> Result<Self, EntryError> {
        let (fm, body) = split_front_matter(raw);
        let yaml = fm.ok_or_else(|| {
            EntryError::MalformedFrontMatter("file has no front matter block".to_string())
        })?;
        let front_matter: MemoryFrontMatter =
            serde_yaml::from_str(yaml).map_err(|e| EntryError::MalformedFrontMatter(e.to_string()))?;
        Ok(Self {
            front_matter,
            body: body.to_string(),
        })
    }

    /// Parse a file, falling back to manual-entry defaults when the front matter
    /// is absent or malformed. The owning agent group is supplied by the caller
    /// (it is the agent whose memory root the file lives in), and a canonical
    /// `memory_id` is derived from the body so the same hand-authored note keeps
    /// a stable ID across normalizations.
    pub fn normalize(raw: &str, owner_agent_group_id: &str) -> Self {
        match Self::parse(raw) {
            Ok(doc) => doc,
            Err(_) => {
                let (_, body) = split_front_matter(raw);
                Self {
                    front_matter: MemoryFrontMatter::manual(owner_agent_group_id, body),
                    body: body.to_string(),
                }
            }
        }
    }

    /// Serialize back to disk: a `---`-fenced YAML front matter block followed by
    /// the body. Round-trips with [`MemoryDocument::parse`].
    pub fn to_markdown(&self) -> String {
        let yaml = serde_yaml::to_string(&self.front_matter)
            .expect("front matter is plain data and always serializes");
        let mut out = String::with_capacity(yaml.len() + self.body.len() + 8);
        out.push_str("---\n");
        out.push_str(&yaml);
        if !yaml.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

/// Derive a canonical, content-stable memory ID for an entry. Scoped by the
/// owning agent group so the same text in two different agents gets distinct
/// IDs (memory never crosses the agent-group boundary).
pub fn generate_memory_id(owner_agent_group_id: &str, basis: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(owner_agent_group_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(basis.as_bytes());
    let digest = hasher.finalize();
    let mut id = String::with_capacity(4 + 24);
    id.push_str("mem_");
    for byte in &digest[..12] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

impl MemoryFrontMatter {
    /// The untrusted-default front matter for a hand-authored file: manual
    /// source, unknown confidence, and never eligible for RAG until an explicit
    /// editor save re-classifies it.
    pub fn manual(owner_agent_group_id: &str, body: &str) -> Self {
        Self {
            memory_id: generate_memory_id(owner_agent_group_id, body),
            owner_agent_group_id: owner_agent_group_id.to_string(),
            scope: Scope::NoRag,
            source_type: SourceType::Manual,
            source_ref: None,
            source_user_id: None,
            captured_at: None,
            confidence: Confidence::Unknown,
            reuse_policy: ReusePolicy::NoRag,
            retention: Retention::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemoryDocument {
        MemoryDocument {
            front_matter: MemoryFrontMatter {
                memory_id: "mem_abc123".into(),
                owner_agent_group_id: "ag_orchestrator".into(),
                scope: Scope::Channel,
                source_type: SourceType::UserSaid,
                source_ref: Some(SourceRef {
                    channel: Some(SourceChannel::Slack),
                    chat_id: Some("C123".into()),
                    thread_id: Some("T9".into()),
                    message_id: Some("M1".into()),
                    permalink: Some("https://slack/x".into()),
                }),
                source_user_id: Some("U42".into()),
                captured_at: Some("2026-06-01T10:00:00Z".into()),
                confidence: Confidence::High,
                reuse_policy: ReusePolicy::SameScope,
                retention: Retention::Normal,
            },
            body: "The user prefers morning meetings.\n".into(),
        }
    }

    #[test]
    fn round_trips_through_markdown() {
        let doc = sample();
        let rendered = doc.to_markdown();
        assert!(rendered.starts_with("---\n"));
        let parsed = MemoryDocument::parse(&rendered).unwrap();
        assert_eq!(parsed, doc);
    }

    #[test]
    fn parse_preserves_body_exactly() {
        let raw = "---\nmemory_id: mem_1\nowner_agent_group_id: ag_x\nscope: all_chats\nsource_type: observed\nconfidence: low\nreuse_policy: cite_only\nretention: normal\n---\nline one\nline two\n";
        let doc = MemoryDocument::parse(raw).unwrap();
        assert_eq!(doc.body, "line one\nline two\n");
        assert_eq!(doc.front_matter.scope, Scope::AllChats);
        assert_eq!(doc.front_matter.source_type, SourceType::Observed);
        assert!(doc.front_matter.source_ref.is_none());
    }

    #[test]
    fn file_without_front_matter_normalizes_to_manual_no_rag() {
        let raw = "just some notes a human dropped in\n";
        let doc = MemoryDocument::normalize(raw, "ag_orchestrator");
        assert_eq!(doc.front_matter.source_type, SourceType::Manual);
        assert_eq!(doc.front_matter.confidence, Confidence::Unknown);
        assert_eq!(doc.front_matter.reuse_policy, ReusePolicy::NoRag);
        assert_eq!(doc.front_matter.scope, Scope::NoRag);
        assert_eq!(doc.front_matter.owner_agent_group_id, "ag_orchestrator");
        assert_eq!(doc.body, raw);
        // The derived ID is stable for the same body + owner.
        assert_eq!(
            doc.front_matter.memory_id,
            generate_memory_id("ag_orchestrator", raw)
        );
    }

    #[test]
    fn malformed_front_matter_normalizes_to_manual_but_parse_errors() {
        // Front matter present but missing required fields.
        let raw = "---\nscope: all_chats\n---\nbody text\n";
        assert!(matches!(
            MemoryDocument::parse(raw),
            Err(EntryError::MalformedFrontMatter(_))
        ));
        let doc = MemoryDocument::normalize(raw, "ag_x");
        assert_eq!(doc.front_matter.source_type, SourceType::Manual);
        assert_eq!(doc.body, "body text\n");
    }

    #[test]
    fn memory_id_is_owner_scoped_and_deterministic() {
        let a = generate_memory_id("ag_a", "same body");
        let b = generate_memory_id("ag_b", "same body");
        let a_again = generate_memory_id("ag_a", "same body");
        assert_ne!(a, b, "same text in different agents must differ");
        assert_eq!(a, a_again, "deterministic for the same inputs");
        assert!(a.starts_with("mem_"));
    }

    #[test]
    fn unclosed_fence_is_treated_as_body() {
        let raw = "---\nnot really front matter\nstill going\n";
        let doc = MemoryDocument::normalize(raw, "ag_x");
        assert_eq!(doc.front_matter.source_type, SourceType::Manual);
        assert_eq!(doc.body, raw);
    }
}
