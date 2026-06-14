pub mod bootstrap;
pub mod error;
pub mod pipeline;
pub mod readiness;
pub mod state;
mod support;

pub use bootstrap::{
    assert_outside_protected, plan, run, BootstrapInput, BootstrapOptions, BootstrapOutcome,
    BootstrapPlan, PlannedStep, StepId,
};
pub use error::SetupError;
pub use pipeline::{
    readiness_gate, FnStep, PipelineOptions, ProgressSink, SetupContext, SetupPipeline, SetupRun,
    SetupStep, SilentProgress,
};
pub use readiness::{
    load_registry, save_registry, CheckKind, CheckStatus, EnabledSurface, ReadinessCheck,
    ReadinessRegistry,
};
pub use state::{load_state, save_state, SetupState};

pub const MODULE_ID: &str = "assistant-setup";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
