pub mod event;
pub mod fragment;
pub mod lease;
pub mod model;
pub mod projection;
pub mod readiness;
pub mod repair;

pub use event::{SchedulerEvent, SchedulerEventSink, VecEventSink};
pub use fragment::{
    scheduling_wording_body, SCHEDULING_WORDING_ID, SCHEDULING_WORDING_ORDER,
    SCHEDULING_WORDING_PARAMS,
};
pub use lease::{
    acquire_lease, claim_due, complete_occurrence, link_run_to_occurrence,
    next_claimable_occurrence, run_for_occurrence, Lease,
};
pub use model::{
    generate_scheduled_item_id, ContextPolicy, EpochSecs, LifecycleTransition, Occurrence,
    Recurrence, Revision, ScheduleError, ScheduleIntent, ScheduleStatus, ScheduledMessageMeta,
};
pub use projection::{
    advance_recurrence, complete_item, item_status, list_items, migrations, upsert_item,
    upsert_occurrence, OccurrenceStatus, ProjectedItem, ProjectionError,
};
pub use readiness::{
    no_work_stuck_past_lease, projections_consistent, sweep_loop_running, CheckStatus,
};
pub use repair::{reconcile_item, repair_session_projection, RepairError, RepairReport};

pub const MODULE_ID: &str = "claw-scheduler";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

