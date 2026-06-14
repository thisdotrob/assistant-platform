//! The `approval-policy` prompt fragment.
//!
//! assistant-approvals owns the wording that tells the orchestrator when to stop and
//! request human approval, and how an approval card is rendered and matched back
//! to its response. Like every owning crate, assistant-approvals exposes only the
//! fragment's id, order, parameters, and body — it does NOT depend on
//! assistant-agent-protocol; the assembler wraps these. The manifest entry is the
//! source of truth for the id, order, and parameter set; the constants mirror
//! it.

/// Fragment id, matching `manifests/prompt-fragments.toml`.
pub const APPROVAL_POLICY_ID: &str = "approval-policy";

/// Render order, after `output-protocol` (20) and before `scheduling-wording`
/// (40), matching the manifest.
pub const APPROVAL_POLICY_ORDER: u32 = 30;

/// The single parameter: how approval cards should be styled for the channel.
pub const APPROVAL_POLICY_PARAMS: &[&str] = &["approval_card_style"];

/// The fragment body with `approval_card_style` substituted. A simple inline
/// replacement keeps the crate free of a dependency on assistant-agent-protocol's
/// substituter while honoring the module boundary.
pub fn approval_policy_body(approval_card_style: &str) -> String {
    APPROVAL_POLICY_TEMPLATE.replace("{approval_card_style}", approval_card_style)
}

const APPROVAL_POLICY_TEMPLATE: &str = "## Approvals\n\
     - Request approval BEFORE executing any high-impact or irreversible action \
     (spending, sending on a user's behalf, deleting, granting access, or using \
     credentials); never act first and ask later.\n\
     - Emit one approval card per request in the {approval_card_style} style, \
     stating the exact action, who it affects, and what changes if granted.\n\
     - A response is only valid for the approval it names; do not treat a \
     response to one request as approval for another, and do not proceed on an \
     ambiguous or partial answer.\n\
     - Treat an expired or denied approval as a hard stop — never honor it — and \
     re-request if the action is still needed.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_substitutes_style_and_leaves_no_placeholder() {
        let body = approval_policy_body("slack-blocks");
        assert!(body.contains("slack-blocks style"));
        assert!(!body.contains("{approval_card_style}"));
    }

    #[test]
    fn body_covers_request_before_execute_and_matching() {
        let body = approval_policy_body("cli");
        let lower = body.to_lowercase();
        assert!(lower.contains("before executing"));
        assert!(lower.contains("only valid for the approval it names"));
        assert!(lower.contains("expired or denied"));
    }

    #[test]
    fn declared_params_match_template_placeholder() {
        assert_eq!(APPROVAL_POLICY_PARAMS, &["approval_card_style"]);
        let body = approval_policy_body("X");
        assert!(!body.contains('{') && !body.contains('}'));
    }
}
