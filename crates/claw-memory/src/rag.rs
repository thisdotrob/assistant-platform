//! Pre-reply RAG candidate filtering and the injection envelope.
//!
//! The owning agent group is the hard isolation boundary and is enforced by the
//! catalog query that produces `candidates` (see [`crate::catalog`]). This module
//! narrows *within* that boundary by per-entry scope and reuse policy, then caps
//! the result — the limit is applied AFTER filtering so restricted entries can
//! never crowd out eligible ones. The most restrictive interpretation always
//! wins: anything not clearly eligible is dropped, `no_rag` is never injected,
//! and `owner_admin` memory stays gated on owner/admin context even when a write
//! marked it `broader_ok`.
//!
//! The output is a metadata envelope (memory ID, path, source type, confidence,
//! scope, captured-at, source reference, provenance label). The snippet text is
//! attached later from qmd hits keyed by memory ID; raw qmd output is never
//! injected without this host-side filtering in front of it.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::catalog::{entries_for_agent, CatalogEntry, MemoryDbError};
use crate::entry::{Confidence, ReusePolicy, Scope, SourceChannel, SourceRef, SourceType};
use crate::qmd::SearchOutcome;

/// The retrieval-time context a turn is running in. Scope filtering compares
/// each candidate's recorded source against this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetrievalContext {
    pub channel: Option<SourceChannel>,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub user_id: Option<String>,
    pub is_owner_admin: bool,
}

/// A clear caution label for memories that were not stated directly by a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvenanceLabel {
    Direct,
    Observed,
    Backfilled,
    Inferred,
}

impl ProvenanceLabel {
    fn for_source(source_type: SourceType) -> Self {
        match source_type {
            SourceType::Observed => ProvenanceLabel::Observed,
            SourceType::Backfilled => ProvenanceLabel::Backfilled,
            SourceType::Inferred => ProvenanceLabel::Inferred,
            _ => ProvenanceLabel::Direct,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProvenanceLabel::Direct => "direct",
            ProvenanceLabel::Observed => "observed",
            ProvenanceLabel::Backfilled => "backfilled",
            ProvenanceLabel::Inferred => "inferred",
        }
    }
}

/// The metadata wrapper injected around a retrieved memory. Carries everything a
/// turn needs to weigh and cite the memory; the optional snippet is filled in
/// from the qmd hit for the same `memory_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectionEnvelope {
    pub memory_id: String,
    pub path: String,
    pub source_type: SourceType,
    pub label: ProvenanceLabel,
    pub confidence: Confidence,
    pub scope: Scope,
    pub captured_at: Option<String>,
    pub source_ref: Option<SourceRef>,
    pub cite_only: bool,
    pub snippet: Option<String>,
}

impl InjectionEnvelope {
    fn from_entry(entry: &CatalogEntry) -> Self {
        Self {
            memory_id: entry.memory_id.clone(),
            path: entry.rel_path.clone(),
            source_type: entry.source_type,
            label: ProvenanceLabel::for_source(entry.source_type),
            confidence: entry.confidence,
            scope: entry.scope,
            captured_at: entry.captured_at.clone(),
            source_ref: entry.source_ref.clone(),
            cite_only: matches!(entry.reuse_policy, ReusePolicy::CiteOnly),
            snippet: None,
        }
    }

    /// Attach the qmd-retrieved snippet for this memory.
    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = Some(snippet.into());
        self
    }

    fn render_line(&self) -> String {
        let mut attrs = vec![
            self.source_type.as_str().to_string(),
            format!("confidence={}", self.confidence.as_str()),
            format!("scope={}", self.scope.as_str()),
        ];
        if let Some(ts) = &self.captured_at {
            attrs.push(format!("captured_at={ts}"));
        }
        if let Some(source) = self.source_ref.as_ref().and_then(render_source_ref) {
            attrs.push(format!("source={source}"));
        }
        if self.cite_only {
            attrs.push("cite_only".to_string());
        }
        let label = match self.label {
            ProvenanceLabel::Direct => String::new(),
            other => format!("({}) ", other.as_str()),
        };
        let mut line = format!(
            "- {label}[{}] {} ({})",
            self.memory_id,
            self.path,
            attrs.join(", ")
        );
        if let Some(snippet) = &self.snippet {
            for snippet_line in snippet.lines() {
                line.push_str("\n  ");
                line.push_str(snippet_line);
            }
        }
        line
    }
}

/// Short marker injected when memory retrieval is unavailable for a turn. The
/// turn still runs (fail-open); this tells the agent not to assert remembered
/// facts it could not load.
pub const DEGRADED_MARKER: &str = "<memory_status>degraded: memory retrieval unavailable for this turn; \
     answer without stored context and do not assert remembered facts.</memory_status>";

/// Filter pre-queried candidates (already scoped to one agent group) by scope
/// and reuse policy, then cap at `limit`. The limit is applied after filtering.
pub fn select_for_injection(
    candidates: &[CatalogEntry],
    ctx: &RetrievalContext,
    limit: usize,
) -> Vec<InjectionEnvelope> {
    candidates
        .iter()
        .filter(|entry| is_eligible(entry, ctx))
        .take(limit)
        .map(InjectionEnvelope::from_entry)
        .collect()
}

/// Convenience host entry point: load one agent group's catalog rows (the hard
/// isolation boundary) and filter them for this retrieval context. qmd relevance
/// ranking composes on top by intersecting on `memory_id`.
pub fn retrieve(
    conn: &Connection,
    agent_group_id: i64,
    ctx: &RetrievalContext,
    limit: usize,
) -> Result<Vec<InjectionEnvelope>, MemoryDbError> {
    let candidates = entries_for_agent(conn, agent_group_id)?;
    Ok(select_for_injection(&candidates, ctx, limit))
}

/// Merge a qmd search outcome with the agent's catalog metadata. qmd supplies
/// ranking and snippets; the catalog supplies eligibility and provenance. A hit
/// is injected only if its `memory_id` is present and eligible in `candidates`,
/// so raw qmd output never reaches the prompt unfiltered. Hit order is
/// preserved (qmd relevance) and the limit is applied after eligibility. Returns
/// the envelopes and whether memory is degraded (fail-open: degraded yields no
/// envelopes and a `true` flag so the caller injects the marker).
pub fn inject_from_search(
    candidates: &[CatalogEntry],
    outcome: &SearchOutcome,
    ctx: &RetrievalContext,
    limit: usize,
) -> (Vec<InjectionEnvelope>, bool) {
    let hits = match outcome {
        SearchOutcome::Degraded(_) => return (Vec::new(), true),
        SearchOutcome::Hits(hits) => hits,
    };
    let eligible: HashMap<&str, &CatalogEntry> = candidates
        .iter()
        .filter(|entry| is_eligible(entry, ctx))
        .map(|entry| (entry.memory_id.as_str(), entry))
        .collect();
    let mut out = Vec::new();
    for hit in hits {
        if out.len() >= limit {
            break;
        }
        if let Some(entry) = eligible.get(hit.memory_id.as_str()) {
            out.push(InjectionEnvelope::from_entry(entry).with_snippet(hit.snippet.clone()));
        }
    }
    (out, false)
}

/// Render the prompt block. Returns `None` when there is nothing to inject and
/// retrieval was healthy. When `degraded`, a marker is included even if no
/// memories survived filtering, so the agent is told context may be missing.
pub fn render_memory_block(envelopes: &[InjectionEnvelope], degraded: bool) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if degraded {
        sections.push(DEGRADED_MARKER.to_string());
    }
    if !envelopes.is_empty() {
        let mut block = String::from("<retrieved_memories>");
        for envelope in envelopes {
            block.push('\n');
            block.push_str(&envelope.render_line());
        }
        block.push_str("\n</retrieved_memories>");
        sections.push(block);
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n"))
    }
}

fn is_eligible(entry: &CatalogEntry, ctx: &RetrievalContext) -> bool {
    // Never injected, regardless of context.
    if matches!(entry.scope, Scope::NoRag) || matches!(entry.reuse_policy, ReusePolicy::NoRag) {
        return false;
    }
    // owner/admin memory is an access gate that `broader_ok` must not bypass.
    if matches!(entry.scope, Scope::OwnerAdmin) {
        return ctx.is_owner_admin;
    }
    match entry.reuse_policy {
        // The write path explicitly widened this beyond its scope.
        ReusePolicy::BroaderOk => true,
        // Reusable only within its own scope; cite_only adds a citation flag but
        // the same scope gate.
        ReusePolicy::SameScope | ReusePolicy::CiteOnly => scope_matches(entry, ctx),
        ReusePolicy::NoRag => false,
    }
}

fn scope_matches(entry: &CatalogEntry, ctx: &RetrievalContext) -> bool {
    match entry.scope {
        Scope::AllChats => true,
        Scope::Channel => same_channel(entry, ctx),
        Scope::Thread => same_thread(entry, ctx),
        Scope::User => same_user(entry, ctx),
        Scope::OwnerAdmin => ctx.is_owner_admin,
        Scope::NoRag => false,
    }
}

fn same_channel(entry: &CatalogEntry, ctx: &RetrievalContext) -> bool {
    let Some(source_ref) = &entry.source_ref else {
        return false;
    };
    source_ref.channel.is_some()
        && source_ref.channel == ctx.channel
        && source_ref.chat_id.is_some()
        && source_ref.chat_id == ctx.chat_id
}

fn same_thread(entry: &CatalogEntry, ctx: &RetrievalContext) -> bool {
    if !same_channel(entry, ctx) {
        return false;
    }
    let Some(source_ref) = &entry.source_ref else {
        return false;
    };
    source_ref.thread_id.is_some() && source_ref.thread_id == ctx.thread_id
}

fn same_user(entry: &CatalogEntry, ctx: &RetrievalContext) -> bool {
    entry.source_user_id.is_some() && entry.source_user_id == ctx.user_id
}

fn render_source_ref(source_ref: &SourceRef) -> Option<String> {
    let channel = source_ref.channel?;
    let mut rendered = channel.as_str().to_string();
    if let Some(chat) = &source_ref.chat_id {
        rendered.push(':');
        rendered.push_str(chat);
    }
    if let Some(thread) = &source_ref.thread_id {
        rendered.push('/');
        rendered.push_str(thread);
    }
    if let Some(message) = &source_ref.message_id {
        rendered.push('#');
        rendered.push_str(message);
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(memory_id: &str, scope: Scope, reuse: ReusePolicy) -> CatalogEntry {
        CatalogEntry {
            memory_id: memory_id.to_string(),
            agent_group_id: 1,
            rel_path: format!("notes/{memory_id}.md"),
            scope,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: None,
            captured_at: Some("2026-06-01T10:00:00Z".to_string()),
            confidence: Confidence::High,
            reuse_policy: reuse,
            retention: crate::entry::Retention::Normal,
            indexed_at: "2026-06-01T10:00:00Z".to_string(),
        }
    }

    fn slack_ref(chat: &str, thread: Option<&str>) -> SourceRef {
        SourceRef {
            channel: Some(SourceChannel::Slack),
            chat_id: Some(chat.to_string()),
            thread_id: thread.map(|t| t.to_string()),
            message_id: None,
            permalink: None,
        }
    }

    fn ctx_in_channel(chat: &str) -> RetrievalContext {
        RetrievalContext {
            channel: Some(SourceChannel::Slack),
            chat_id: Some(chat.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn no_rag_scope_or_reuse_is_never_eligible() {
        let owner = RetrievalContext {
            is_owner_admin: true,
            ..Default::default()
        };
        assert!(select_for_injection(
            &[entry("m", Scope::NoRag, ReusePolicy::BroaderOk)],
            &owner,
            10
        )
        .is_empty());
        assert!(select_for_injection(
            &[entry("m", Scope::AllChats, ReusePolicy::NoRag)],
            &owner,
            10
        )
        .is_empty());
    }

    #[test]
    fn all_chats_is_eligible_regardless_of_context() {
        let got = select_for_injection(
            &[entry("m", Scope::AllChats, ReusePolicy::SameScope)],
            &RetrievalContext::default(),
            10,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].memory_id, "m");
    }

    #[test]
    fn channel_scope_same_scope_requires_matching_channel() {
        let mut e = entry("m", Scope::Channel, ReusePolicy::SameScope);
        e.source_ref = Some(slack_ref("C1", None));
        // Matching channel: eligible.
        assert_eq!(
            select_for_injection(&[e.clone()], &ctx_in_channel("C1"), 10).len(),
            1
        );
        // Different channel: dropped.
        assert!(select_for_injection(&[e.clone()], &ctx_in_channel("C2"), 10).is_empty());
        // No channel context at all: dropped.
        assert!(select_for_injection(&[e], &RetrievalContext::default(), 10).is_empty());
    }

    #[test]
    fn channel_scope_broader_ok_surfaces_across_chats() {
        let mut e = entry("m", Scope::Channel, ReusePolicy::BroaderOk);
        e.source_ref = Some(slack_ref("C1", None));
        // Different channel still surfaces because the write widened it.
        assert_eq!(
            select_for_injection(&[e], &ctx_in_channel("C999"), 10).len(),
            1
        );
    }

    #[test]
    fn thread_scope_requires_matching_thread() {
        let mut e = entry("m", Scope::Thread, ReusePolicy::SameScope);
        e.source_ref = Some(slack_ref("C1", Some("T7")));
        let mut ctx = ctx_in_channel("C1");
        ctx.thread_id = Some("T7".to_string());
        assert_eq!(select_for_injection(&[e.clone()], &ctx, 10).len(), 1);
        // Right channel, wrong thread: dropped.
        ctx.thread_id = Some("T8".to_string());
        assert!(select_for_injection(&[e], &ctx, 10).is_empty());
    }

    #[test]
    fn user_scope_requires_matching_user() {
        let mut e = entry("m", Scope::User, ReusePolicy::SameScope);
        e.source_user_id = Some("U42".to_string());
        let mut ctx = RetrievalContext {
            user_id: Some("U42".to_string()),
            ..Default::default()
        };
        assert_eq!(select_for_injection(&[e.clone()], &ctx, 10).len(), 1);
        ctx.user_id = Some("U99".to_string());
        assert!(select_for_injection(&[e], &ctx, 10).is_empty());
    }

    #[test]
    fn owner_admin_scope_stays_gated_even_with_broader_ok() {
        let e = entry("m", Scope::OwnerAdmin, ReusePolicy::BroaderOk);
        let non_admin = RetrievalContext::default();
        assert!(select_for_injection(std::slice::from_ref(&e), &non_admin, 10).is_empty());
        let admin = RetrievalContext {
            is_owner_admin: true,
            ..Default::default()
        };
        assert_eq!(select_for_injection(&[e], &admin, 10).len(), 1);
    }

    #[test]
    fn cite_only_is_eligible_within_scope_and_flagged() {
        let got = select_for_injection(
            &[entry("m", Scope::AllChats, ReusePolicy::CiteOnly)],
            &RetrievalContext::default(),
            10,
        );
        assert_eq!(got.len(), 1);
        assert!(got[0].cite_only);
    }

    #[test]
    fn limit_is_applied_after_filtering() {
        // Two eligible (all_chats) interleaved with two ineligible (no_rag).
        let candidates = vec![
            entry("ok1", Scope::AllChats, ReusePolicy::SameScope),
            entry("skip1", Scope::NoRag, ReusePolicy::SameScope),
            entry("ok2", Scope::AllChats, ReusePolicy::SameScope),
            entry("skip2", Scope::NoRag, ReusePolicy::SameScope),
            entry("ok3", Scope::AllChats, ReusePolicy::SameScope),
        ];
        let got = select_for_injection(&candidates, &RetrievalContext::default(), 2);
        // Exactly two eligible entries, not crowded out by the skipped ones.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].memory_id, "ok1");
        assert_eq!(got[1].memory_id, "ok2");
    }

    #[test]
    fn envelope_labels_observed_memory_and_carries_fields() {
        let mut e = entry("m", Scope::AllChats, ReusePolicy::SameScope);
        e.source_type = SourceType::Observed;
        e.source_ref = Some(slack_ref("C1", Some("T2")));
        let got = select_for_injection(&[e], &RetrievalContext::default(), 10);
        let env = &got[0];
        assert_eq!(env.label, ProvenanceLabel::Observed);
        assert_eq!(env.source_type, SourceType::Observed);
        assert_eq!(env.captured_at.as_deref(), Some("2026-06-01T10:00:00Z"));
        assert!(env.source_ref.is_some());
    }

    #[test]
    fn render_block_includes_memories_and_provenance() {
        let mut e = entry("mem_x", Scope::Channel, ReusePolicy::CiteOnly);
        e.source_type = SourceType::Inferred;
        e.source_ref = Some(slack_ref("C1", Some("T2")));
        let env = select_for_injection(&[e], &{
            let mut c = ctx_in_channel("C1");
            c.thread_id = Some("T2".to_string());
            c
        }, 10);
        let block = render_memory_block(&env, false).expect("a block");
        assert!(block.starts_with("<retrieved_memories>"));
        assert!(block.contains("[mem_x]"));
        assert!(block.contains("(inferred)"));
        assert!(block.contains("cite_only"));
        assert!(block.contains("source=slack:C1/T2"));
        assert!(block.ends_with("</retrieved_memories>"));
    }

    #[test]
    fn render_returns_none_when_empty_and_healthy() {
        assert!(render_memory_block(&[], false).is_none());
    }

    #[test]
    fn render_emits_degraded_marker_even_with_no_memories() {
        let block = render_memory_block(&[], true).expect("degraded marker");
        assert!(block.contains("degraded"));
        assert!(!block.contains("<retrieved_memories>"));
    }

    #[test]
    fn snippet_is_indented_under_header() {
        let env = InjectionEnvelope::from_entry(&entry(
            "m",
            Scope::AllChats,
            ReusePolicy::SameScope,
        ))
        .with_snippet("first line\nsecond line");
        let line = env.render_line();
        assert!(line.contains("\n  first line"));
        assert!(line.contains("\n  second line"));
    }

    #[test]
    fn inject_from_search_filters_qmd_hits_through_catalog() {
        use crate::qmd::{DegradedReason, Hit, SearchOutcome};

        let eligible = entry("mem_ok", Scope::AllChats, ReusePolicy::SameScope);
        let blocked = entry("mem_no", Scope::NoRag, ReusePolicy::NoRag);
        let candidates = vec![eligible, blocked];

        // qmd ranks the blocked memory first, then the eligible one, plus a hit
        // with no catalog row at all.
        let outcome = SearchOutcome::Hits(vec![
            Hit {
                memory_id: "mem_no".into(),
                score: 9,
                snippet: "secret".into(),
            },
            Hit {
                memory_id: "mem_ghost".into(),
                score: 8,
                snippet: "orphaned hit".into(),
            },
            Hit {
                memory_id: "mem_ok".into(),
                score: 7,
                snippet: "the eligible snippet".into(),
            },
        ]);
        let (envelopes, degraded) =
            inject_from_search(&candidates, &outcome, &RetrievalContext::default(), 10);
        assert!(!degraded);
        assert_eq!(envelopes.len(), 1, "only the eligible, catalogued hit survives");
        assert_eq!(envelopes[0].memory_id, "mem_ok");
        assert_eq!(envelopes[0].snippet.as_deref(), Some("the eligible snippet"));

        // A degraded search injects nothing and flags degraded (fail-open).
        let (envelopes, degraded) = inject_from_search(
            &candidates,
            &SearchOutcome::Degraded(DegradedReason::CorruptFts),
            &RetrievalContext::default(),
            10,
        );
        assert!(degraded);
        assert!(envelopes.is_empty());
    }

    #[test]
    fn retrieve_enforces_agent_boundary() {
        use crate::catalog::{migrations, upsert_entry};
        use claw_db::{apply, MigrationSet};

        let mut conn = Connection::open_in_memory().unwrap();
        let mut set = MigrationSet::new(vec![crate::MODULE_ID.to_string()]);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();

        let mk = |id: &str, owner: &str| crate::entry::MemoryFrontMatter {
            memory_id: id.to_string(),
            owner_agent_group_id: owner.to_string(),
            scope: Scope::AllChats,
            source_type: SourceType::UserSaid,
            source_ref: None,
            source_user_id: None,
            captured_at: None,
            confidence: Confidence::High,
            reuse_policy: ReusePolicy::SameScope,
            retention: crate::entry::Retention::Normal,
        };
        upsert_entry(&conn, 1, "a.md", &mk("mem_a", "ag_one")).unwrap();
        upsert_entry(&conn, 2, "b.md", &mk("mem_b", "ag_two")).unwrap();

        let got = retrieve(&conn, 1, &RetrievalContext::default(), 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].memory_id, "mem_a");
    }
}
