//! Agent-scoped memory: the on-disk markdown entry format and front-matter
//! metadata that back per-agent qmd memory roots and host-side RAG.
//!
//! Markdown is the source of truth; everything else (the qmd index, the derived
//! metadata catalog, RAG candidates) is rebuildable from it. The owning agent
//! group is the hard isolation boundary — metadata only narrows reuse within an
//! agent, it never permits cross-agent reads.

pub mod archive;
pub mod catalog;
pub mod entry;
pub mod health;
pub mod journal;
pub mod nudge;
pub mod observe;
pub mod qmd;
pub mod rag;
pub mod reindex;
pub mod root;
pub mod taxonomy;

pub use archive::{archive_pointer, read_archive_pointer, ArchiveTarget};
pub use catalog::{
    candidates, entries_for_agent, migrations, upsert_entry, CatalogEntry, MemoryDbError,
};
pub use entry::{
    generate_memory_id, Confidence, EntryError, MemoryDocument, MemoryFrontMatter, Retention,
    ReusePolicy, Scope, SourceChannel, SourceRef, SourceType,
};
pub use health::{project_health, read_health, MemoryHealthRecord};
pub use journal::{validate_journal, validate_journal_bullet, JournalError, JournalReport};
pub use nudge::{pre_compaction_nudge, PRE_COMPACTION_NUDGE};
pub use observe::{
    dedupe_key, prepare_observed_write, AcceptedWrite, IngestOutcome, ObserveRejection, Provenance,
    Watermark,
};
pub use qmd::{
    corpus_stem_for_memory_id, parse_qmd_response, DegradedReason, FakeQmd, Hit, IndexDoc,
    MemoryBackend, MemoryHealth, SearchOutcome,
};
pub use rag::{
    inject_from_search, render_memory_block, retrieve, select_for_injection, InjectionEnvelope,
    ProvenanceLabel, RetrievalContext, DEGRADED_MARKER,
};
pub use reindex::{reindex_root, ReindexError, ReindexReport};
pub use root::{IsolationError, MemoryRoot};
pub use taxonomy::{ScaffoldError, Taxonomy, TaxonomyError};

pub const MODULE_ID: &str = "claw-memory";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
