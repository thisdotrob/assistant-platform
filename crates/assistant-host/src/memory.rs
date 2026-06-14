//! Host-mediated memory writes and pre-reply snippet hydration.
//!
//! An agent never touches its memory root directly: it emits a `save_memory`
//! outbound row (the serde body of
//! [`assistant_agent_protocol::OutboundAction::SaveMemory`]) and the host writes the
//! markdown note plus its catalog projection here, exactly as `schedule_message`
//! rows are intercepted in [`crate::slack`]. The same module hydrates retrieval
//! envelopes with each entry's body so the injected `<retrieved_memories>` block
//! actually carries remembered text without a qmd index in front of it.

use std::path::Path;

use assistant_memory::{
    generate_memory_id, upsert_entry, Confidence, InjectionEnvelope, MemoryDocument,
    MemoryFrontMatter, MemoryRoot, Retention, ReusePolicy, Scope, SourceRef, SourceType,
};
use rusqlite::Connection;
use serde::Deserialize;

/// Cap on the body text attached to one injection envelope. Bodies are short by
/// construction, but a runaway note must not blow up the prompt block.
const SNIPPET_CAP: usize = 1000;

/// The wire payload an agent's `save_memory` action carries in its outbound
/// `content` (the serde body of [`assistant_agent_protocol::OutboundAction::SaveMemory`]
/// without the action tag, which travels as the row `kind`).
#[derive(Deserialize)]
struct MemoryWritePayload {
    content: String,
    #[serde(default)]
    title: Option<String>,
}

/// Persist one memory from an agent's `save_memory` action: write the markdown
/// note under the agent's orchestrator memory root and project its front matter
/// into the central catalog. Returns the derived `memory_id`. The host owns the
/// entry's identity, scope, and trust (a directly-stated, `all_chats`,
/// same-scope, high-confidence note); the agent supplies only the text and an
/// optional title. `source_ref`/`source_user_id` record where the turn ran (the
/// channel and sender) for provenance and citation — they are stamped but not
/// filtered on: retrieval stays unscoped (the instance is the isolation
/// boundary), so the recorded scope is always `all_chats`. Central + on-disk
/// only, matching `schedule_message`'s host-mediated projection.
pub(crate) fn write_memory(
    conn: &Connection,
    groups_dir: &Path,
    owner: &str,
    agent_group_id: i64,
    payload: &str,
    source_ref: Option<SourceRef>,
    source_user_id: Option<String>,
) -> Result<String, String> {
    let payload: MemoryWritePayload =
        serde_json::from_str(payload).map_err(|e| format!("bad save_memory payload: {e}"))?;

    let body = build_body(&payload);
    let memory_id = generate_memory_id(owner, &body);
    let front_matter = MemoryFrontMatter {
        memory_id: memory_id.clone(),
        owner_agent_group_id: owner.to_string(),
        scope: Scope::AllChats,
        source_type: SourceType::UserSaid,
        source_ref,
        source_user_id,
        captured_at: None,
        confidence: Confidence::High,
        reuse_policy: ReusePolicy::SameScope,
        retention: Retention::Normal,
    };

    let rel_path = format!("notes/{memory_id}.md");
    let root = MemoryRoot::orchestrator(groups_dir, owner);
    root.authorize_owner(owner).map_err(|e| e.to_string())?;
    let abs = root.resolve(&rel_path).map_err(|e| e.to_string())?;
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create memory dir: {e}"))?;
    }
    let document = MemoryDocument {
        front_matter: front_matter.clone(),
        body,
    };
    std::fs::write(&abs, document.to_markdown()).map_err(|e| format!("write memory note: {e}"))?;

    upsert_entry(conn, agent_group_id, &rel_path, &front_matter).map_err(|e| e.to_string())?;
    Ok(memory_id)
}

/// Compose the markdown body: an optional `# title` heading followed by the
/// content, always newline-terminated so the file round-trips cleanly.
fn build_body(payload: &MemoryWritePayload) -> String {
    let mut body = String::new();
    if let Some(title) = payload.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str("# ");
        body.push_str(title);
        body.push_str("\n\n");
    }
    body.push_str(payload.content.trim_end_matches('\n'));
    body.push('\n');
    body
}

/// Attach each entry's on-disk body to its injection envelope as the snippet, so
/// the rendered block carries the remembered text. Catalog-only retrieval leaves
/// `snippet` empty (snippets are a qmd concern); reading the body here is what
/// makes injection useful before qmd exists. Fail-open per entry: a missing or
/// unparseable file just leaves that envelope snippet-less rather than dropping
/// it.
pub(crate) fn hydrate_snippets(
    envelopes: Vec<InjectionEnvelope>,
    groups_dir: &Path,
    owner: &str,
) -> Vec<InjectionEnvelope> {
    let root = MemoryRoot::orchestrator(groups_dir, owner);
    envelopes
        .into_iter()
        .map(|envelope| match read_body(&root, &envelope.path) {
            Some(body) => envelope.with_snippet(body),
            None => envelope,
        })
        .collect()
}

/// Read and cap one entry's markdown body, confined to the agent's own root.
fn read_body(root: &MemoryRoot, rel_path: &str) -> Option<String> {
    let abs = root.resolve(rel_path).ok()?;
    let raw = std::fs::read_to_string(abs).ok()?;
    let document = MemoryDocument::parse(&raw).ok()?;
    Some(cap_chars(document.body.trim(), SNIPPET_CAP))
}

/// Truncate to at most `max` characters on a char boundary, marking elision.
fn cap_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, MigrationSet};
    use assistant_memory::{entries_for_agent, retrieve, RetrievalContext, SourceChannel};

    fn catalog_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let mut set = MigrationSet::new(vec![assistant_memory::MODULE_ID.to_string()]);
        for m in assistant_memory::migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    #[test]
    fn write_memory_persists_markdown_and_catalog_row() {
        let conn = catalog_db();
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let payload = r#"{"content":"user prefers terse replies","title":"reply style"}"#;

        let memory_id =
            write_memory(&conn, &groups, "ag_orchestrator", 1, payload, None, None).unwrap();

        let rows = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].memory_id, memory_id);
        assert_eq!(rows[0].scope, Scope::AllChats);
        assert_eq!(rows[0].source_type, SourceType::UserSaid);
        assert_eq!(rows[0].reuse_policy, ReusePolicy::SameScope);

        let abs = groups
            .join("orchestrator")
            .join("memory")
            .join(&rows[0].rel_path);
        let doc = MemoryDocument::parse(&std::fs::read_to_string(&abs).unwrap()).unwrap();
        assert_eq!(doc.front_matter.memory_id, memory_id);
        assert_eq!(doc.front_matter.owner_agent_group_id, "ag_orchestrator");
        assert!(doc.body.contains("user prefers terse replies"));
        assert!(doc.body.contains("# reply style"));
    }

    #[test]
    fn write_memory_without_title_omits_heading() {
        let conn = catalog_db();
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");

        write_memory(&conn, &groups, "ag_orchestrator", 1, r#"{"content":"plain note"}"#, None, None)
            .unwrap();

        let rows = entries_for_agent(&conn, 1).unwrap();
        let abs = groups
            .join("orchestrator")
            .join("memory")
            .join(&rows[0].rel_path);
        let doc = MemoryDocument::parse(&std::fs::read_to_string(&abs).unwrap()).unwrap();
        assert!(!doc.body.contains('#'));
        assert_eq!(doc.body.trim(), "plain note");
    }

    #[test]
    fn write_memory_records_provenance_in_front_matter_and_catalog() {
        let conn = catalog_db();
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        let source_ref = SourceRef {
            channel: Some(SourceChannel::Slack),
            chat_id: Some("C123".to_string()),
            thread_id: Some("1700.1".to_string()),
            message_id: None,
            permalink: None,
        };

        write_memory(
            &conn,
            &groups,
            "ag_orchestrator",
            1,
            r#"{"content":"deploy via atlas-deploy"}"#,
            Some(source_ref.clone()),
            Some("U42".to_string()),
        )
        .unwrap();

        // Catalog row carries the provenance projection.
        let rows = entries_for_agent(&conn, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_user_id.as_deref(), Some("U42"));
        assert_eq!(rows[0].source_ref.as_ref(), Some(&source_ref));
        // Scope stays unscoped: provenance is recorded, never filtered on.
        assert_eq!(rows[0].scope, Scope::AllChats);

        // On-disk front matter round-trips the same provenance.
        let abs = groups
            .join("orchestrator")
            .join("memory")
            .join(&rows[0].rel_path);
        let doc = MemoryDocument::parse(&std::fs::read_to_string(&abs).unwrap()).unwrap();
        assert_eq!(doc.front_matter.source_ref.as_ref(), Some(&source_ref));
        assert_eq!(doc.front_matter.source_user_id.as_deref(), Some("U42"));
    }

    #[test]
    fn hydrate_attaches_body_as_snippet() {
        let conn = catalog_db();
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        write_memory(
            &conn,
            &groups,
            "ag_orchestrator",
            1,
            r#"{"content":"remember the milk"}"#,
            None,
            None,
        )
        .unwrap();

        let envelopes = retrieve(&conn, 1, &RetrievalContext::default(), 5).unwrap();
        let hydrated = hydrate_snippets(envelopes, &groups, "ag_orchestrator");

        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].snippet.as_deref(), Some("remember the milk"));
    }

    #[test]
    fn hydrate_is_fail_open_when_file_missing() {
        let conn = catalog_db();
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        write_memory(&conn, &groups, "ag_orchestrator", 1, r#"{"content":"on disk"}"#, None, None)
            .unwrap();

        // Retrieve, then delete the backing file: hydration must keep the
        // envelope (snippet-less) rather than drop it.
        let envelopes = retrieve(&conn, 1, &RetrievalContext::default(), 5).unwrap();
        let rel = envelopes[0].path.clone();
        let abs = groups.join("orchestrator").join("memory").join(&rel);
        std::fs::remove_file(&abs).unwrap();

        let hydrated = hydrate_snippets(envelopes, &groups, "ag_orchestrator");
        assert_eq!(hydrated.len(), 1);
        assert!(hydrated[0].snippet.is_none());
    }
}
