//! The qmd search backend contract, fake-backed for the offline gate.
//!
//! The host talks to qmd through [`MemoryBackend`]; the real implementation
//! (`assistant_host::qmd::NodeQmd`) shells the pinned host-side qmd sidecar over the
//! agent's own collection and parses its JSON, while [`FakeQmd`] gives an
//! in-process backend for tests and for products that have not wired the sidecar
//! yet. Two contract rules are baked in here:
//!
//! 1. Failures are fail-open. An empty index, corrupt FTS state, or a missing
//!    binary degrade *memory health*; they never raise an error that fails the
//!    agent turn. Both [`MemoryBackend::health`] and [`MemoryBackend::search`]
//!    report this via [`MemoryHealth`] / [`SearchOutcome::Degraded`].
//! 2. The sidecar speaks JSON: `{ "hits": [{ displayPath, score, bestChunk }] }`
//!    on success, or `{ "error": "<reason>" }` to degrade explicitly.
//!    [`parse_qmd_response`] maps a hit's `displayPath` back to its `memory_id`
//!    (the corpus is written one file per memory id, the filename a hex encoding
//!    of the id via [`corpus_stem_for_memory_id`] so qmd's path normalization
//!    round-trips), coerces the float `score` into the opaque `i64` rank, and
//!    treats any malformed or error response as degraded rather than letting it
//!    corrupt the result set.
//!    Hit order from qmd (relevance) is preserved; the host applies eligibility
//!    and the limit downstream in [`crate::rag::inject_from_search`].

/// A markdown entry handed to the backend for indexing. The `memory_id` is the
/// stable citation key; rebuilding the index from markdown must preserve it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDoc {
    pub memory_id: String,
    pub rel_path: String,
    pub body: String,
}

/// One search hit. `score` is an opaque rank (higher is better); the host orders
/// by it but does not interpret the magnitude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub memory_id: String,
    pub score: i64,
    pub snippet: String,
}

/// Why memory is degraded. Every variant is a non-fatal condition: the turn
/// proceeds without stored context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DegradedReason {
    EmptyIndex,
    CorruptFts,
    MissingBinary,
    StartupTimeout,
    QueryTimeout,
    Other(String),
}

impl DegradedReason {
    pub fn as_str(&self) -> &str {
        match self {
            DegradedReason::EmptyIndex => "empty_index",
            DegradedReason::CorruptFts => "corrupt_fts",
            DegradedReason::MissingBinary => "missing_binary",
            DegradedReason::StartupTimeout => "startup_timeout",
            DegradedReason::QueryTimeout => "query_timeout",
            DegradedReason::Other(reason) => reason,
        }
    }

    /// Map a wire reason token (the sidecar's `{ "error": "<reason>" }` value, or
    /// a `status` response's error) to a known [`DegradedReason`], falling back to
    /// [`DegradedReason::Other`] for anything unrecognized. Shared by the search
    /// parser here and the host's status parser so both speak one wire vocabulary.
    pub fn from_wire(reason: &str) -> Self {
        match reason {
            "empty_index" => DegradedReason::EmptyIndex,
            "corrupt_fts" => DegradedReason::CorruptFts,
            "missing_binary" => DegradedReason::MissingBinary,
            "startup_timeout" => DegradedReason::StartupTimeout,
            "query_timeout" => DegradedReason::QueryTimeout,
            other => DegradedReason::Other(other.to_string()),
        }
    }
}

/// Current memory health for an agent's index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryHealth {
    Healthy { indexed: usize },
    Degraded { reason: DegradedReason },
}

impl MemoryHealth {
    pub fn is_healthy(&self) -> bool {
        matches!(self, MemoryHealth::Healthy { .. })
    }
}

/// The result of a search: either ranked hits or a degraded signal. There is no
/// error variant — a degraded search is the fail-open path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchOutcome {
    Hits(Vec<Hit>),
    Degraded(DegradedReason),
}

impl SearchOutcome {
    pub fn is_degraded(&self) -> bool {
        matches!(self, SearchOutcome::Degraded(_))
    }
}

/// What the host codes against. The real qmd process wrapper and [`FakeQmd`] both
/// implement this; the host never depends on which is behind it.
pub trait MemoryBackend {
    /// (Re)build the index from the full markdown corpus. Returns resulting
    /// health. Must preserve memory IDs.
    fn reindex(&mut self, docs: &[IndexDoc]) -> MemoryHealth;
    /// Search the current index. Fail-open: degraded state is reported, not
    /// raised.
    fn search(&self, query: &str, limit: usize) -> SearchOutcome;
    /// Current health without running a query.
    fn health(&self) -> MemoryHealth;
}

/// Multiplier turning qmd's small float relevance score into the opaque `i64`
/// rank [`Hit`] carries. The host never interprets the magnitude (and
/// [`crate::rag::inject_from_search`] preserves qmd's order rather than
/// re-sorting), so this only needs to keep relative ordering for any consumer
/// that does sort by score.
const SCORE_SCALE: f64 = 1_000_000.0;

/// Encode a `memory_id` into a corpus filename stem that survives qmd's path
/// normalization (`handelize`) unchanged. handelize lowercases and replaces every
/// run outside `[\p{L}\p{N}$]` with `-`, so a literal `mem_<hex>` stem would be
/// mangled to `mem-<hex>` and stop matching its id on the way back. Lowercase hex
/// is a fixed point of that normalization, so we hex-encode the id's bytes; the
/// round trip is exact and reversed by [`memory_id_from_stem`]. This is the
/// shared contract between the corpus writer (`assistant_host::qmd`, the only producer
/// of these filenames) and the search parser here.
pub fn corpus_stem_for_memory_id(memory_id: &str) -> String {
    let mut stem = String::with_capacity(memory_id.len() * 2);
    for byte in memory_id.as_bytes() {
        stem.push_str(&format!("{byte:02x}"));
    }
    stem
}

/// Inverse of [`corpus_stem_for_memory_id`]: decode a hex filename stem back to
/// the `memory_id`. Returns `None` for a stem that is not even-length lowercase
/// hex over valid UTF-8 (e.g. a file a human dropped into the corpus by hand), so
/// the caller skips that hit rather than fabricating an id.
fn memory_id_from_stem(stem: &str) -> Option<String> {
    if stem.is_empty() || !stem.len().is_multiple_of(2) {
        return None;
    }
    let raw = stem.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    let mut i = 0;
    while i < raw.len() {
        let hi = (raw[i] as char).to_digit(16)?;
        let lo = (raw[i + 1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
        i += 2;
    }
    String::from_utf8(bytes).ok()
}

/// Map a qmd `displayPath` back to its `memory_id`. The corpus is written one
/// markdown file per memory id with the filename a hex encoding of the id (see
/// [`corpus_stem_for_memory_id`]), so the file stem — regardless of any leading
/// collection directory qmd echoes back — decodes to the id. A stem that is not
/// our hex encoding (a hand-dropped file) yields `None` and the hit is skipped.
fn memory_id_from_path(display_path: &str) -> Option<String> {
    let stem = std::path::Path::new(display_path)
        .file_stem()?
        .to_str()?
        .trim();
    memory_id_from_stem(stem)
}

/// Parse the host-side qmd sidecar's JSON response into a [`SearchOutcome`],
/// fail-open throughout:
/// - `{ "error": "<reason>" }` (or any non-JSON / shape we don't recognize)
///   degrades rather than raising;
/// - `{ "hits": [{ displayPath, score, bestChunk }, ...] }` yields ranked
///   [`Hit`]s in the order qmd returned them, with `memory_id` recovered from
///   `displayPath` and the float `score` scaled into the opaque `i64` rank;
/// - a hit with no usable `displayPath` is skipped, never fatal.
///
/// No limit is applied here: the sidecar's `topN` is the upstream cap and the
/// host applies eligibility + the final limit in `inject_from_search`.
pub fn parse_qmd_response(stdout: &str) -> SearchOutcome {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return SearchOutcome::Degraded(DegradedReason::Other("empty qmd response".to_string()));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return SearchOutcome::Degraded(DegradedReason::Other("malformed qmd response".to_string()));
    };
    if let Some(reason) = value.get("error").and_then(|e| e.as_str()) {
        return SearchOutcome::Degraded(DegradedReason::from_wire(reason.trim()));
    }
    let Some(raw_hits) = value.get("hits").and_then(|h| h.as_array()) else {
        return SearchOutcome::Degraded(DegradedReason::Other(
            "qmd response missing hits".to_string(),
        ));
    };
    let mut hits = Vec::with_capacity(raw_hits.len());
    for raw in raw_hits {
        let Some(memory_id) = raw
            .get("displayPath")
            .and_then(|p| p.as_str())
            .and_then(memory_id_from_path)
        else {
            continue;
        };
        let score = raw.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let snippet = raw
            .get("bestChunk")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        hits.push(Hit {
            memory_id,
            score: (score * SCORE_SCALE).round() as i64,
            snippet,
        });
    }
    SearchOutcome::Hits(hits)
}

/// An in-process backend: naive case-insensitive term-AND search with `-term`
/// negation, plus injectable faults so tests can exercise the degraded paths.
#[derive(Clone, Debug, Default)]
pub struct FakeQmd {
    corpus: Vec<IndexDoc>,
    fault: Option<DegradedReason>,
}

impl FakeQmd {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_corpus(docs: Vec<IndexDoc>) -> Self {
        Self {
            corpus: docs,
            fault: None,
        }
    }

    /// Force a degraded condition (corrupt FTS, missing binary, timeouts).
    pub fn with_fault(reason: DegradedReason) -> Self {
        Self {
            corpus: Vec::new(),
            fault: Some(reason),
        }
    }

    fn current_health(&self) -> MemoryHealth {
        if let Some(reason) = &self.fault {
            return MemoryHealth::Degraded {
                reason: reason.clone(),
            };
        }
        if self.corpus.is_empty() {
            return MemoryHealth::Degraded {
                reason: DegradedReason::EmptyIndex,
            };
        }
        MemoryHealth::Healthy {
            indexed: self.corpus.len(),
        }
    }
}

fn snippet_for(body: &str, required: &[String]) -> String {
    body.lines()
        .find(|line| {
            let lower = line.to_lowercase();
            required.iter().any(|term| lower.contains(term))
        })
        .unwrap_or_else(|| body.lines().next().unwrap_or(""))
        .trim()
        .to_string()
}

impl MemoryBackend for FakeQmd {
    fn reindex(&mut self, docs: &[IndexDoc]) -> MemoryHealth {
        // A real rebuild reopens the collection; the fake just replaces the
        // corpus. Memory IDs are carried through unchanged.
        if self.fault.is_none() {
            self.corpus = docs.to_vec();
        }
        self.current_health()
    }

    fn search(&self, query: &str, limit: usize) -> SearchOutcome {
        if let MemoryHealth::Degraded { reason } = self.current_health() {
            return SearchOutcome::Degraded(reason);
        }
        let mut required: Vec<String> = Vec::new();
        let mut negated: Vec<String> = Vec::new();
        for token in query.split_whitespace() {
            if let Some(neg) = token.strip_prefix('-') {
                if !neg.is_empty() {
                    negated.push(neg.to_lowercase());
                }
            } else {
                required.push(token.to_lowercase());
            }
        }

        let mut hits: Vec<Hit> = self
            .corpus
            .iter()
            .filter_map(|doc| {
                let lower = doc.body.to_lowercase();
                if negated.iter().any(|term| lower.contains(term)) {
                    return None;
                }
                if !required.iter().all(|term| lower.contains(term)) {
                    return None;
                }
                let score = required
                    .iter()
                    .map(|term| lower.matches(term.as_str()).count() as i64)
                    .sum();
                Some(Hit {
                    memory_id: doc.memory_id.clone(),
                    score,
                    snippet: snippet_for(&doc.body, &required),
                })
            })
            .collect();

        // Highest score first, ties broken by memory_id for determinism.
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.memory_id.cmp(&b.memory_id)));
        hits.truncate(limit);
        SearchOutcome::Hits(hits)
    }

    fn health(&self) -> MemoryHealth {
        self.current_health()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Vec<IndexDoc> {
        vec![
            IndexDoc {
                memory_id: "mem_slack".into(),
                rel_path: "people/alice.md".into(),
                body: "<@U123> asked to review the launch plan".into(),
            },
            IndexDoc {
                memory_id: "mem_jira".into(),
                rel_path: "activities/eng.md".into(),
                body: "Fix ENG-1234 before the crash report ships".into(),
            },
            IndexDoc {
                memory_id: "mem_hyphen".into(),
                rel_path: "decisions/copilot.md".into(),
                body: "Decision: enable co-pilot mode for the team".into(),
            },
        ]
    }

    fn fake() -> FakeQmd {
        FakeQmd::with_corpus(corpus())
    }

    #[test]
    fn finds_slack_markup_term() {
        let SearchOutcome::Hits(hits) = fake().search("review", 10) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory_id, "mem_slack");
        assert!(hits[0].snippet.contains("<@U123>"));
    }

    #[test]
    fn finds_jira_key() {
        let SearchOutcome::Hits(hits) = fake().search("ENG-1234", 10) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory_id, "mem_jira");
    }

    #[test]
    fn finds_hyphenated_term() {
        let SearchOutcome::Hits(hits) = fake().search("co-pilot", 10) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory_id, "mem_hyphen");
    }

    #[test]
    fn negation_excludes_matching_docs() {
        // "report" matches the Jira doc; "-crash" removes it.
        let SearchOutcome::Hits(hits) = fake().search("report -crash", 10) else {
            panic!("expected hits");
        };
        assert!(hits.iter().all(|h| h.memory_id != "mem_jira"));
    }

    #[test]
    fn empty_index_is_degraded_not_fatal() {
        let backend = FakeQmd::new();
        assert_eq!(
            backend.health(),
            MemoryHealth::Degraded {
                reason: DegradedReason::EmptyIndex
            }
        );
        assert_eq!(
            backend.search("anything", 10),
            SearchOutcome::Degraded(DegradedReason::EmptyIndex)
        );
    }

    #[test]
    fn corrupt_fts_and_missing_binary_are_degraded() {
        for reason in [DegradedReason::CorruptFts, DegradedReason::MissingBinary] {
            let backend = FakeQmd::with_fault(reason.clone());
            assert_eq!(backend.health(), MemoryHealth::Degraded { reason: reason.clone() });
            assert_eq!(backend.search("q", 5), SearchOutcome::Degraded(reason));
        }
    }

    #[test]
    fn reindex_preserves_memory_ids_and_restores_health() {
        let mut backend = FakeQmd::new();
        assert!(!backend.health().is_healthy());
        let health = backend.reindex(&corpus());
        assert_eq!(health, MemoryHealth::Healthy { indexed: 3 });
        let SearchOutcome::Hits(hits) = backend.search("co-pilot", 10) else {
            panic!("expected hits");
        };
        assert_eq!(hits[0].memory_id, "mem_hyphen");
    }

    #[test]
    fn faulted_backend_does_not_accept_reindex() {
        let mut backend = FakeQmd::with_fault(DegradedReason::CorruptFts);
        let health = backend.reindex(&corpus());
        assert_eq!(
            health,
            MemoryHealth::Degraded {
                reason: DegradedReason::CorruptFts
            }
        );
    }

    #[test]
    fn parses_hits_recovering_memory_id_and_preserving_order() {
        // displayPath carries a leading collection dir + .md; memory_id is the
        // hex-decoded stem. Order is preserved (qmd relevance), score scaled to i64.
        let a = corpus_stem_for_memory_id("mem_a");
        let b = corpus_stem_for_memory_id("mem_b");
        let json = format!(
            r#"{{"hits":[
            {{"displayPath":"notes/{a}.md","title":"A","bestChunk":"the alice snippet","score":0.87,"docid":1}},
            {{"displayPath":"{b}.md","title":"B","bestChunk":"the bob snippet","score":0.4,"docid":2}}
        ]}}"#
        );
        let SearchOutcome::Hits(hits) = parse_qmd_response(&json) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].memory_id, "mem_a");
        assert_eq!(hits[0].snippet, "the alice snippet");
        assert_eq!(hits[0].score, 870_000);
        assert_eq!(hits[1].memory_id, "mem_b");
        assert_eq!(hits[1].score, 400_000);
        // Order is qmd's, highest-relevance first.
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn corpus_stem_round_trips_and_is_handelize_stable() {
        for id in ["mem_0011aabb", "mem_a", "good", "ag_orchestrator__user"] {
            let stem = corpus_stem_for_memory_id(id);
            // Pure lowercase hex — a fixed point of qmd's path handelize.
            assert!(stem.chars().all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')));
            assert_eq!(memory_id_from_stem(&stem).as_deref(), Some(id));
        }
    }

    #[test]
    fn non_hex_stem_is_skipped_not_fatal() {
        // A file a human dropped into the corpus (stem isn't our hex encoding) is
        // skipped rather than yielding a bogus memory_id.
        let json =
            r#"{"hits":[{"displayPath":"notes/hand-written.md","bestChunk":"x","score":0.5}]}"#;
        let SearchOutcome::Hits(hits) = parse_qmd_response(json) else {
            panic!("expected hits");
        };
        assert!(hits.is_empty());
    }

    #[test]
    fn parses_empty_hits_as_healthy_empty_result() {
        // An indexed-but-no-match search is healthy with zero hits, NOT degraded.
        let SearchOutcome::Hits(hits) = parse_qmd_response(r#"{"hits":[]}"#) else {
            panic!("expected (empty) hits");
        };
        assert!(hits.is_empty());
    }

    #[test]
    fn skips_hits_without_a_usable_path() {
        let good = corpus_stem_for_memory_id("good");
        let json = format!(
            r#"{{"hits":[
            {{"bestChunk":"no path","score":0.9}},
            {{"displayPath":"","score":0.8}},
            {{"displayPath":"{good}.md","bestChunk":"kept","score":0.7}}
        ]}}"#
        );
        let SearchOutcome::Hits(hits) = parse_qmd_response(&json) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory_id, "good");
    }

    #[test]
    fn maps_explicit_error_field_to_degraded() {
        assert_eq!(
            parse_qmd_response(r#"{"error":"corrupt_fts"}"#),
            SearchOutcome::Degraded(DegradedReason::CorruptFts)
        );
        // An unknown reason round-trips through Other.
        assert_eq!(
            parse_qmd_response(r#"{"error":"model_load_failed"}"#),
            SearchOutcome::Degraded(DegradedReason::Other("model_load_failed".to_string()))
        );
    }

    #[test]
    fn malformed_or_empty_output_is_degraded_not_fatal() {
        assert!(parse_qmd_response("not json at all").is_degraded());
        assert!(parse_qmd_response("").is_degraded());
        // Valid JSON of the wrong shape (no hits, no error) still degrades.
        assert!(parse_qmd_response(r#"{"unexpected":true}"#).is_degraded());
    }
}
