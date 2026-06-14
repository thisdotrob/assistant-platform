//! Archive pointers.
//!
//! When an entry is archived, its body moves under `archive/` and a small
//! pointer is left in place so citations and the memory editor can still resolve
//! it. A pointer is an ordinary memory document — same YAML front matter, same
//! parser — with `retention=archive_only`, `scope=no_rag`, and
//! `reuse_policy=no_rag`, so it is browsable but never injected. Keeping the
//! pointer in the standard entry format is what makes it "stay readable after
//! migration": no separate format to keep in sync, and [`MemoryDocument::parse`]
//! round-trips it unchanged. The archived location and time live as machine-
//! readable `key: value` lines in the body.

use crate::entry::{MemoryDocument, MemoryFrontMatter, ReusePolicy, Retention, Scope};

const ARCHIVED_PATH_KEY: &str = "archived_path";
const ARCHIVED_AT_KEY: &str = "archived_at";

/// Where an archived entry went, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveTarget {
    pub archived_path: String,
    pub archived_at: String,
}

/// Build a pointer document for `original`, recording where its body was moved.
/// Identity (`memory_id`, owner, source) is carried so citations still resolve;
/// retention/scope/reuse are forced to the non-injectable archive defaults.
pub fn archive_pointer(
    original: &MemoryFrontMatter,
    target: &ArchiveTarget,
) -> MemoryDocument {
    let front_matter = MemoryFrontMatter {
        memory_id: original.memory_id.clone(),
        owner_agent_group_id: original.owner_agent_group_id.clone(),
        scope: Scope::NoRag,
        source_type: original.source_type,
        source_ref: original.source_ref.clone(),
        source_user_id: original.source_user_id.clone(),
        captured_at: original.captured_at.clone(),
        confidence: original.confidence,
        reuse_policy: ReusePolicy::NoRag,
        retention: Retention::ArchiveOnly,
    };
    let body = format!(
        "Archived entry.\n\n{ARCHIVED_PATH_KEY}: {}\n{ARCHIVED_AT_KEY}: {}\n",
        target.archived_path, target.archived_at
    );
    MemoryDocument { front_matter, body }
}

/// Read the archive target back out of a pointer document. Returns `None` if the
/// document is not an archive pointer (`retention != archive_only`) or is
/// missing either machine-readable line.
pub fn read_archive_pointer(doc: &MemoryDocument) -> Option<ArchiveTarget> {
    if doc.front_matter.retention != Retention::ArchiveOnly {
        return None;
    }
    Some(ArchiveTarget {
        archived_path: body_field(&doc.body, ARCHIVED_PATH_KEY)?,
        archived_at: body_field(&doc.body, ARCHIVED_AT_KEY)?,
    })
}

fn body_field(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    body.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Confidence, SourceType};

    fn original() -> MemoryFrontMatter {
        MemoryFrontMatter {
            memory_id: "mem_keepme".into(),
            owner_agent_group_id: "ag_orchestrator".into(),
            scope: Scope::AllChats,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: Some("U1".into()),
            captured_at: Some("2026-01-01T00:00:00Z".into()),
            confidence: Confidence::High,
            reuse_policy: ReusePolicy::SameScope,
            retention: Retention::Normal,
        }
    }

    fn target() -> ArchiveTarget {
        ArchiveTarget {
            archived_path: "archive/2026/mem_keepme.md".into(),
            archived_at: "2026-06-01T12:00:00Z".into(),
        }
    }

    #[test]
    fn pointer_is_non_injectable_but_keeps_identity() {
        let doc = archive_pointer(&original(), &target());
        assert_eq!(doc.front_matter.memory_id, "mem_keepme");
        assert_eq!(doc.front_matter.retention, Retention::ArchiveOnly);
        assert_eq!(doc.front_matter.scope, Scope::NoRag);
        assert_eq!(doc.front_matter.reuse_policy, ReusePolicy::NoRag);
        // Source identity is preserved for citation.
        assert_eq!(doc.front_matter.source_type, SourceType::UserSaid);
        assert_eq!(doc.front_matter.source_user_id.as_deref(), Some("U1"));
    }

    #[test]
    fn pointer_round_trips_through_the_entry_parser() {
        let doc = archive_pointer(&original(), &target());
        let rendered = doc.to_markdown();
        // Readable by the same parser the catalog uses — no special-case format.
        let parsed = MemoryDocument::parse(&rendered).unwrap();
        assert_eq!(parsed, doc);
        let recovered = read_archive_pointer(&parsed).expect("is an archive pointer");
        assert_eq!(recovered, target());
    }

    #[test]
    fn non_pointer_document_reads_as_none() {
        let mut doc = archive_pointer(&original(), &target());
        doc.front_matter.retention = Retention::Normal;
        assert!(read_archive_pointer(&doc).is_none());
    }
}
