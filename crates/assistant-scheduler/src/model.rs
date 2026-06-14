//! Scheduling domain model: the data that lives in a scheduled message's
//! `messages_in.metadata` JSON, which is the source of truth for scheduled work.
//!
//! The shapes here encode the rewrite's scheduling rules directly:
//!
//! - a schedule has a stable `scheduled_item_id` that never changes across
//!   updates, runs, retries, or recurrence occurrences;
//! - the original intent is an *immutable* record; an update appends a new
//!   revision and changes the effective fields, but never rewrites the intent;
//! - recurrence advances from the occurrence's *scheduled* time, not wall-clock
//!   completion time, so a late or retried run never drifts the cadence;
//! - each occurrence carries a stable sequence and idempotency key, so a
//!   duplicate sweep recomputes the same identity and cannot double-run it;
//! - a context policy records whether the run should use current orchestrator
//!   memory (default) or an explicitly captured snapshot.
//!
//! Times are epoch seconds (`EpochSecs`). The crate never reads the wall clock
//! itself: callers pass `now`, which keeps every behavior deterministically
//! testable and lets the host control the time source.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Seconds since the Unix epoch. The scheduler's single time representation.
pub type EpochSecs = i64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    InvalidRecurrence { detail: String },
    IllegalTransition {
        from: ScheduleStatus,
        transition: LifecycleTransition,
    },
    /// An update or transition was attempted on a terminal (cancelled/completed)
    /// item.
    Terminal { status: ScheduleStatus },
    /// Metadata JSON could not be (de)serialized.
    Json { detail: String },
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleError::InvalidRecurrence { detail } => {
                write!(f, "invalid recurrence: {detail}")
            }
            ScheduleError::IllegalTransition { from, transition } => {
                write!(f, "illegal transition {transition:?} from {from:?}")
            }
            ScheduleError::Terminal { status } => {
                write!(f, "cannot modify a {status:?} scheduled item")
            }
            ScheduleError::Json { detail } => write!(f, "scheduling metadata json error: {detail}"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// How a recurring schedule repeats. Recurrence always advances from the
/// scheduled occurrence time, never from completion, to avoid drift.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recurrence {
    /// A fixed interval in seconds between occurrences.
    Every { seconds: i64 },
}

impl Recurrence {
    pub fn validate(&self) -> Result<(), ScheduleError> {
        match self {
            Recurrence::Every { seconds } if *seconds > 0 => Ok(()),
            Recurrence::Every { seconds } => Err(ScheduleError::InvalidRecurrence {
                detail: format!("interval must be positive, got {seconds}"),
            }),
        }
    }

    /// The scheduled time of the occurrence that follows one scheduled at
    /// `occurrence_scheduled`. Anchored to the scheduled time, so repeated calls
    /// reproduce the exact cadence regardless of when execution actually happened.
    pub fn next_after(&self, occurrence_scheduled: EpochSecs) -> EpochSecs {
        match self {
            Recurrence::Every { seconds } => occurrence_scheduled + seconds,
        }
    }
}

/// Whether a scheduled run uses live memory or a frozen snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextPolicy {
    /// Default: resolve current orchestrator memory at run time.
    #[default]
    CurrentMemory,
    /// Use an explicitly captured context snapshot; the run must not silently
    /// fall back to current memory.
    CapturedSnapshot { snapshot_ref: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Active,
    Paused,
    Cancelled,
    Completed,
}

impl ScheduleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleStatus::Active => "active",
            ScheduleStatus::Paused => "paused",
            ScheduleStatus::Cancelled => "cancelled",
            ScheduleStatus::Completed => "completed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(ScheduleStatus::Active),
            "paused" => Some(ScheduleStatus::Paused),
            "cancelled" => Some(ScheduleStatus::Cancelled),
            "completed" => Some(ScheduleStatus::Completed),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ScheduleStatus::Cancelled | ScheduleStatus::Completed)
    }

    /// Apply a lifecycle transition, rejecting illegal moves (e.g. resuming an
    /// active item, or any transition out of a terminal state).
    pub fn apply(self, transition: LifecycleTransition) -> Result<ScheduleStatus, ScheduleError> {
        use LifecycleTransition::*;
        use ScheduleStatus::*;
        let next = match (self, transition) {
            (Active, Pause) => Paused,
            (Paused, Resume) => Active,
            (Active, Cancel) | (Paused, Cancel) => Cancelled,
            (Active, Complete) => Completed,
            (from, transition) => {
                return Err(ScheduleError::IllegalTransition { from, transition })
            }
        };
        Ok(next)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleTransition {
    Pause,
    Resume,
    Cancel,
    Complete,
}

/// The immutable record of what was originally asked. Never rewritten; updates
/// append a [`Revision`] instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleIntent {
    pub created_by: String,
    pub summary: String,
    pub created_at: EpochSecs,
}

/// One entry in a schedule's revision history. Revision 1 is the create; each
/// subsequent revision records the effective schedule fields at that point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub revision: u32,
    pub revised_at: EpochSecs,
    pub revised_by: String,
    pub process_after: EpochSecs,
    pub recurrence: Option<Recurrence>,
    pub context_policy: ContextPolicy,
}

/// A single scheduled firing. Identity is `(scheduled_item_id, sequence)`; the
/// idempotency key is derived from that identity so any sweep recomputes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub scheduled_item_id: String,
    pub sequence: u64,
    pub scheduled_for: EpochSecs,
    pub idempotency_key: String,
}

impl Occurrence {
    pub fn new(scheduled_item_id: &str, sequence: u64, scheduled_for: EpochSecs) -> Self {
        Self {
            scheduled_item_id: scheduled_item_id.to_string(),
            sequence,
            scheduled_for,
            idempotency_key: occurrence_idempotency_key(scheduled_item_id, sequence),
        }
    }
}

/// The full scheduling state stored in a scheduled message's metadata. This is
/// the source of truth; the central projection is derived from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledMessageMeta {
    pub scheduled_item_id: String,
    pub agent_group_id: i64,
    pub intent: ScheduleIntent,
    pub process_after: EpochSecs,
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub context_policy: ContextPolicy,
    pub status: ScheduleStatus,
    pub revision: u32,
    pub revisions: Vec<Revision>,
    /// The highest occurrence sequence allocated so far (0 before the first).
    #[serde(default)]
    pub occurrence_seq: u64,
}

impl ScheduledMessageMeta {
    /// Create a new schedule at revision 1. Validates recurrence up front.
    pub fn create(
        agent_group_id: i64,
        intent: ScheduleIntent,
        process_after: EpochSecs,
        recurrence: Option<Recurrence>,
        context_policy: ContextPolicy,
    ) -> Result<Self, ScheduleError> {
        if let Some(rec) = &recurrence {
            rec.validate()?;
        }
        let scheduled_item_id = generate_scheduled_item_id(agent_group_id, &intent);
        let revision = Revision {
            revision: 1,
            revised_at: intent.created_at,
            revised_by: intent.created_by.clone(),
            process_after,
            recurrence: recurrence.clone(),
            context_policy: context_policy.clone(),
        };
        Ok(Self {
            scheduled_item_id,
            agent_group_id,
            intent,
            process_after,
            recurrence,
            context_policy,
            status: ScheduleStatus::Active,
            revision: 1,
            revisions: vec![revision],
            occurrence_seq: 0,
        })
    }

    /// Update the effective schedule fields, appending a new revision. The
    /// original `intent` is preserved untouched. Rejected on terminal items.
    pub fn update(
        &mut self,
        revised_by: impl Into<String>,
        revised_at: EpochSecs,
        process_after: EpochSecs,
        recurrence: Option<Recurrence>,
        context_policy: ContextPolicy,
    ) -> Result<(), ScheduleError> {
        if self.status.is_terminal() {
            return Err(ScheduleError::Terminal {
                status: self.status,
            });
        }
        if let Some(rec) = &recurrence {
            rec.validate()?;
        }
        self.revision += 1;
        self.process_after = process_after;
        self.recurrence = recurrence;
        self.context_policy = context_policy;
        self.revisions.push(Revision {
            revision: self.revision,
            revised_at,
            revised_by: revised_by.into(),
            process_after: self.process_after,
            recurrence: self.recurrence.clone(),
            context_policy: self.context_policy.clone(),
        });
        Ok(())
    }

    /// Apply a lifecycle transition to the item's status.
    pub fn transition(&mut self, transition: LifecycleTransition) -> Result<(), ScheduleError> {
        self.status = self.status.apply(transition)?;
        Ok(())
    }

    /// The next occurrence to fire if the item is active, else `None`. The next
    /// occurrence is always scheduled at the current effective `process_after`.
    pub fn pending_occurrence(&self) -> Option<Occurrence> {
        if self.status != ScheduleStatus::Active {
            return None;
        }
        Some(Occurrence::new(
            &self.scheduled_item_id,
            self.occurrence_seq + 1,
            self.process_after,
        ))
    }

    /// Record that `occurrence` fired. For a recurring item this advances
    /// `process_after` from the occurrence's *scheduled* time (drift-free) and
    /// bumps the occurrence sequence; for a one-off it completes the item.
    pub fn record_fired(&mut self, occurrence: &Occurrence) {
        self.occurrence_seq = occurrence.sequence;
        match &self.recurrence {
            Some(recurrence) => {
                self.process_after = recurrence.next_after(occurrence.scheduled_for);
            }
            None => {
                self.status = ScheduleStatus::Completed;
            }
        }
    }

    pub fn to_metadata_json(&self) -> Result<String, ScheduleError> {
        serde_json::to_string(self).map_err(|e| ScheduleError::Json { detail: e.to_string() })
    }

    pub fn from_metadata_json(json: &str) -> Result<Self, ScheduleError> {
        serde_json::from_str(json).map_err(|e| ScheduleError::Json { detail: e.to_string() })
    }
}

/// A stable, content-derived scheduled item ID. Generated once at creation and
/// then carried unchanged for the schedule's whole life.
pub fn generate_scheduled_item_id(agent_group_id: i64, intent: &ScheduleIntent) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_group_id.to_le_bytes());
    hasher.update(b"|");
    hasher.update(intent.created_by.as_bytes());
    hasher.update(b"|");
    hasher.update(intent.summary.as_bytes());
    hasher.update(b"|");
    hasher.update(intent.created_at.to_le_bytes());
    let digest = hasher.finalize();
    let mut id = String::from("sched_");
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

fn occurrence_idempotency_key(scheduled_item_id: &str, sequence: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scheduled_item_id.as_bytes());
    hasher.update(b"|");
    hasher.update(sequence.to_le_bytes());
    let digest = hasher.finalize();
    let mut key = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> ScheduleIntent {
        ScheduleIntent {
            created_by: "U1".into(),
            summary: "remind me to stretch".into(),
            created_at: 1_000,
        }
    }

    #[test]
    fn create_is_active_at_revision_one_with_stable_id() {
        let meta =
            ScheduledMessageMeta::create(7, intent(), 2_000, None, ContextPolicy::CurrentMemory)
                .unwrap();
        assert_eq!(meta.status, ScheduleStatus::Active);
        assert_eq!(meta.revision, 1);
        assert_eq!(meta.revisions.len(), 1);
        assert!(meta.scheduled_item_id.starts_with("sched_"));
        // Regenerating from the same agent + intent yields the same ID.
        assert_eq!(meta.scheduled_item_id, generate_scheduled_item_id(7, &intent()));
    }

    #[test]
    fn update_appends_revision_and_preserves_intent() {
        let mut meta =
            ScheduledMessageMeta::create(1, intent(), 2_000, None, ContextPolicy::CurrentMemory)
                .unwrap();
        let original_intent = meta.intent.clone();
        let original_id = meta.scheduled_item_id.clone();
        meta.update(
            "U2",
            3_000,
            5_000,
            Some(Recurrence::Every { seconds: 3_600 }),
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        assert_eq!(meta.revision, 2);
        assert_eq!(meta.revisions.len(), 2);
        assert_eq!(meta.process_after, 5_000);
        // Identity and original intent are untouched by the update.
        assert_eq!(meta.scheduled_item_id, original_id);
        assert_eq!(meta.intent, original_intent);
        assert_eq!(meta.revisions[0].process_after, 2_000);
        assert_eq!(meta.revisions[1].revised_by, "U2");
    }

    #[test]
    fn terminal_items_reject_updates() {
        let mut meta =
            ScheduledMessageMeta::create(1, intent(), 2_000, None, ContextPolicy::CurrentMemory)
                .unwrap();
        meta.transition(LifecycleTransition::Cancel).unwrap();
        let err = meta
            .update("U2", 3_000, 5_000, None, ContextPolicy::CurrentMemory)
            .unwrap_err();
        assert_eq!(
            err,
            ScheduleError::Terminal {
                status: ScheduleStatus::Cancelled
            }
        );
    }

    #[test]
    fn lifecycle_transitions_are_validated() {
        use LifecycleTransition::*;
        use ScheduleStatus::*;
        assert_eq!(Active.apply(Pause).unwrap(), Paused);
        assert_eq!(Paused.apply(Resume).unwrap(), Active);
        assert_eq!(Active.apply(Cancel).unwrap(), Cancelled);
        assert_eq!(Paused.apply(Cancel).unwrap(), Cancelled);
        assert_eq!(Active.apply(Complete).unwrap(), Completed);
        // Illegal: resume an active item, or anything out of a terminal state.
        assert!(matches!(
            Active.apply(Resume),
            Err(ScheduleError::IllegalTransition { .. })
        ));
        assert!(matches!(
            Cancelled.apply(Resume),
            Err(ScheduleError::IllegalTransition { .. })
        ));
        assert!(matches!(
            Completed.apply(Pause),
            Err(ScheduleError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn recurrence_advances_from_scheduled_time_without_drift() {
        let mut meta = ScheduledMessageMeta::create(
            1,
            intent(),
            1_000,
            Some(Recurrence::Every { seconds: 60 }),
            ContextPolicy::CurrentMemory,
        )
        .unwrap();
        let mut scheduled = Vec::new();
        for _ in 0..5 {
            let occ = meta.pending_occurrence().unwrap();
            scheduled.push(occ.scheduled_for);
            // Fire arbitrarily late — must not affect the next scheduled time.
            meta.record_fired(&occ);
        }
        // base + n*interval exactly, no creep from late firing.
        assert_eq!(scheduled, vec![1_000, 1_060, 1_120, 1_180, 1_240]);
        // Still active (recurring never auto-completes).
        assert_eq!(meta.status, ScheduleStatus::Active);
    }

    #[test]
    fn one_off_completes_after_firing() {
        let mut meta =
            ScheduledMessageMeta::create(1, intent(), 1_000, None, ContextPolicy::CurrentMemory)
                .unwrap();
        let occ = meta.pending_occurrence().unwrap();
        assert_eq!(occ.sequence, 1);
        meta.record_fired(&occ);
        assert_eq!(meta.status, ScheduleStatus::Completed);
        assert!(meta.pending_occurrence().is_none());
    }

    #[test]
    fn occurrence_idempotency_key_is_stable_per_identity() {
        let a = Occurrence::new("sched_x", 3, 1_000);
        let b = Occurrence::new("sched_x", 3, 9_999);
        // Key depends only on (item, sequence), not the scheduled time.
        assert_eq!(a.idempotency_key, b.idempotency_key);
        let c = Occurrence::new("sched_x", 4, 1_000);
        assert_ne!(a.idempotency_key, c.idempotency_key);
    }

    #[test]
    fn update_does_not_silently_switch_captured_context() {
        let mut meta = ScheduledMessageMeta::create(
            1,
            intent(),
            2_000,
            None,
            ContextPolicy::CapturedSnapshot { snapshot_ref: "snap_1".into() },
        )
        .unwrap();
        // Re-passing the captured policy on an unrelated reschedule keeps it; the
        // model never defaults a captured snapshot back to current memory on its
        // own — a switch is only ever an explicit caller choice.
        meta.update(
            "U2",
            3_000,
            6_000,
            None,
            ContextPolicy::CapturedSnapshot { snapshot_ref: "snap_1".into() },
        )
        .unwrap();
        assert_eq!(
            meta.context_policy,
            ContextPolicy::CapturedSnapshot { snapshot_ref: "snap_1".into() }
        );
        // It survives a metadata round trip too, so a repair re-read cannot lose it.
        let reparsed = ScheduledMessageMeta::from_metadata_json(&meta.to_metadata_json().unwrap()).unwrap();
        assert_eq!(reparsed.context_policy, meta.context_policy);
    }

    #[test]
    fn captured_context_round_trips_through_metadata_json() {
        let meta = ScheduledMessageMeta::create(
            2,
            intent(),
            2_000,
            None,
            ContextPolicy::CapturedSnapshot {
                snapshot_ref: "snap_42".into(),
            },
        )
        .unwrap();
        let json = meta.to_metadata_json().unwrap();
        let parsed = ScheduledMessageMeta::from_metadata_json(&json).unwrap();
        assert_eq!(parsed, meta);
        // The captured-context choice survives the round trip and is not lost.
        assert_eq!(
            parsed.context_policy,
            ContextPolicy::CapturedSnapshot {
                snapshot_ref: "snap_42".into()
            }
        );
    }
}
