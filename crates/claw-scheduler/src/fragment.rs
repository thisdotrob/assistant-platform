//! The `scheduling-wording` prompt fragment.
//!
//! claw-scheduler owns the wording the orchestrator uses when it talks about
//! scheduled work: it standardizes one-off vs recurring phrasing and the
//! lifecycle verbs that mirror this crate's state machine (scheduled, queued,
//! started, running, completed, blocked, paused, cancelled). Like every owning
//! crate, claw-scheduler exposes only the fragment's id, order, and body — it
//! does NOT depend on claw-agent-protocol. The agent-protocol assembler (or the
//! host) wraps these into a `RenderableFragment` and renders them alongside the
//! shared fragments. The manifest entry is the source of truth for the id,
//! order, and parameter set; the constants here mirror it.

/// Fragment id, matching `manifests/prompt-fragments.toml`.
pub const SCHEDULING_WORDING_ID: &str = "scheduling-wording";

/// Render order, after `output-protocol` (20) and `approval-policy` (30) and
/// before `rag-injection` (50), matching the manifest.
pub const SCHEDULING_WORDING_ORDER: u32 = 40;

/// The single parameter this fragment substitutes: the timezone recurrence and
/// due-time descriptions are anchored to, so phrasing is never ambiguous.
pub const SCHEDULING_WORDING_PARAMS: &[&str] = &["timezone"];

/// The fragment body with `timezone` substituted. A simple inline replacement
/// keeps the crate free of a dependency on claw-agent-protocol's substituter
/// while honoring the module boundary.
pub fn scheduling_wording_body(timezone: &str) -> String {
    SCHEDULING_WORDING_TEMPLATE.replace("{timezone}", timezone)
}

const SCHEDULING_WORDING_TEMPLATE: &str = "## Scheduling\n\
     - A one-off schedule fires once at its due time; a recurring schedule fires \
     repeatedly on its interval until cancelled. Name which kind you are creating.\n\
     - State every due time and recurrence in the {timezone} timezone; never \
     leave a time or interval timezone-ambiguous.\n\
     - Use the lifecycle verbs exactly: a schedule is `scheduled` when created, \
     `queued` when due and claimed, `started` then `running` while its work \
     executes, `completed` when that work finishes, and `blocked` if it cannot \
     proceed.\n\
     - A schedule may be `paused` (suspended, resumable), resumed back to \
     `scheduled`, or `cancelled` (terminal). Do not say \"stopped\", \"deleted\", \
     or \"done\" — map to the verb above.\n\
     - Report the next occurrence of a recurring schedule, not just that it \
     recurs, so the user can anticipate it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_substitutes_timezone_and_leaves_no_placeholder() {
        let body = scheduling_wording_body("Europe/London");
        assert!(body.contains("Europe/London timezone"));
        assert!(!body.contains("{timezone}"));
    }

    #[test]
    fn body_carries_every_lifecycle_verb() {
        let body = scheduling_wording_body("UTC");
        for verb in [
            "scheduled",
            "queued",
            "started",
            "running",
            "completed",
            "blocked",
            "paused",
            "cancelled",
        ] {
            assert!(body.contains(verb), "lifecycle verb missing: {verb}");
        }
    }

    #[test]
    fn body_distinguishes_one_off_from_recurring() {
        let body = scheduling_wording_body("UTC");
        assert!(body.contains("one-off"));
        assert!(body.contains("recurring"));
    }

    #[test]
    fn declared_params_match_template_placeholder() {
        // The only placeholder in the template is the one declared parameter, so
        // substituting it leaves no stray braces behind.
        assert_eq!(SCHEDULING_WORDING_PARAMS, &["timezone"]);
        let body = scheduling_wording_body("X");
        assert!(!body.contains('{') && !body.contains('}'));
    }
}
