//! The pre-compaction memory nudge.
//!
//! Before a long-running agent's context is compacted, durable facts that only
//! live in the conversation would be lost. The host injects this nudge so the
//! agent writes anything worth keeping to its memory root first. It is advisory
//! prompt text, not an action: it never writes memory itself.

/// Marker the host injects ahead of a compaction so the agent persists durable
/// facts to memory before context is summarized away.
pub const PRE_COMPACTION_NUDGE: &str = "<memory_nudge>this conversation is about to be compacted; \
     before that happens, write any durable facts, decisions, or follow-ups worth keeping to your \
     memory root (with appropriate scope and source), then continue.</memory_nudge>";

/// The pre-compaction nudge text.
pub fn pre_compaction_nudge() -> &'static str {
    PRE_COMPACTION_NUDGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_is_a_marker_and_mentions_persistence() {
        assert!(PRE_COMPACTION_NUDGE.starts_with("<memory_nudge>"));
        assert!(PRE_COMPACTION_NUDGE.ends_with("</memory_nudge>"));
        assert!(PRE_COMPACTION_NUDGE.contains("memory root"));
        assert_eq!(pre_compaction_nudge(), PRE_COMPACTION_NUDGE);
    }
}
