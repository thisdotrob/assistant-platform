//! The generic, resumable setup-step pipeline that completes product setup on
//! top of the M1 bootstrap.
//!
//! A setup run is an ordered list of [`SetupStep`] trait objects. The pipeline
//! owns the cross-cutting concerns — resumable state, per-step raw logs and the
//! human-readable `setup.log`, dry-run, user-facing progress, and hard-gate
//! enforcement — while each step owns one unit of work. The crate links no
//! domain code, so the domain/external steps (create owner, build images,
//! configure OneCLI/channels, install service, channel delivery checks) are
//! supplied by the product as `SetupStep` impls (or built from [`FnStep`]); the
//! crate ships the runner, the step trait, and the ready-made [`readiness_gate`].
//!
//! State is shared with the bootstrap: both persist to the same `state.json`
//! keyed by step id, so a single resume continues wherever setup stopped — as
//! long as step ids do not collide with the bootstrap's
//! (`create_directories`/`write_config`/`migrate_db`/`write_readiness`).

use std::path::{Path, PathBuf};

use assistant_config::InstanceLayout;

use crate::error::SetupError;
use crate::readiness::{load_registry, save_registry, CheckStatus, ReadinessRegistry};
use crate::state::load_state;
use crate::support::{append_setup_log, guard_writable, persist_state_if_possible, write_step_log};

/// One unit of setup work. Object-safe so a product can assemble a
/// heterogeneous pipeline of `Box<dyn SetupStep>`.
pub trait SetupStep {
    /// Stable id used for resumable state and the per-step log filename. Must be
    /// unique within a pipeline and must not collide with a bootstrap step id.
    fn id(&self) -> &str;

    /// One-line human-readable description shown in progress and the plan.
    fn description(&self) -> String;

    /// Whether a failure of this step is a hard gate that blocks setup
    /// completion. Non-gate steps can also fail, but a gate's failure is the
    /// "setup cannot complete" signal the plan calls out (readiness, channel
    /// delivery, pairing).
    fn is_gate(&self) -> bool {
        false
    }

    /// Do the work, returning a short detail string for the logs. Writes must go
    /// through [`SetupContext::guard_writable`]. A gate that does not pass
    /// returns `Err(SetupError::Gate { .. })`.
    ///
    /// Execution is at-least-once: if the process crashes after a step records
    /// readiness but before its completion is persisted, the step reruns on
    /// resume, so a step's work should be idempotent. Gates are re-evaluated on
    /// every run and are never marked completed.
    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<String, SetupError>;
}

/// A step id is used both as a resumable-state key and, interpolated, as a
/// per-step log filename. Restricting it to `[a-z0-9_-]+` keeps a hostile or
/// careless id from traversing out of the logs dir (e.g. `../../config`).
fn valid_step_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// What a running step is handed: the instance layout, the dry-run flag, the
/// write-confinement guard, and the readiness registry to record results into.
pub struct SetupContext<'a> {
    layout: &'a InstanceLayout,
    dry_run: bool,
    protected_roots: &'a [PathBuf],
    registry: &'a mut ReadinessRegistry,
    registry_dirty: bool,
}

impl<'a> SetupContext<'a> {
    pub fn layout(&self) -> &InstanceLayout {
        self.layout
    }

    /// True in dry-run: a step must not write, and should report what it would
    /// do instead.
    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// Confine a write to the instance root and out of every protected source
    /// repo. Every step must call this before touching `path`.
    pub fn guard_writable(&self, path: &Path) -> Result<(), SetupError> {
        guard_writable(&self.layout.root, path, self.protected_roots)
    }

    /// Whether a readiness check is registered for this enabled surface.
    pub fn readiness_registered(&self, id: &str) -> bool {
        self.registry.contains(id)
    }

    /// Record a readiness result for a registered check. Returns false if no
    /// such check is registered (the item was not enabled), in which case
    /// nothing is recorded. The pipeline persists the registry after the step.
    pub fn record_readiness(&mut self, id: &str, status: CheckStatus, detail: Option<String>) -> bool {
        let recorded = self.registry.record(id, status, detail);
        if recorded {
            self.registry_dirty = true;
        }
        recorded
    }

    /// Read-only view of the current readiness registry (e.g. for a gate).
    pub fn readiness(&self) -> &ReadinessRegistry {
        self.registry
    }
}

/// Receives user-facing progress as the pipeline runs. All methods default to
/// no-ops so a caller can implement only what it needs; [`SilentProgress`] does
/// nothing at all.
pub trait ProgressSink {
    fn step_started(&mut self, _index: usize, _total: usize, _id: &str, _description: &str) {}
    fn step_completed(&mut self, _index: usize, _total: usize, _id: &str, _detail: &str) {}
    fn step_skipped(&mut self, _index: usize, _total: usize, _id: &str) {}
    fn step_failed(&mut self, _index: usize, _total: usize, _id: &str, _error: &str) {}
}

/// A progress sink that discards everything.
pub struct SilentProgress;
impl ProgressSink for SilentProgress {}

#[derive(Debug, Clone, Default)]
pub struct PipelineOptions {
    pub dry_run: bool,
}

/// The result of a pipeline run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRun {
    pub executed: Vec<String>,
    pub skipped: Vec<String>,
    pub dry_run: bool,
}

/// An ordered list of setup steps.
#[derive(Default)]
pub struct SetupPipeline {
    steps: Vec<Box<dyn SetupStep>>,
}

impl SetupPipeline {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn with_step(mut self, step: Box<dyn SetupStep>) -> Self {
        self.steps.push(step);
        self
    }

    pub fn push(&mut self, step: Box<dyn SetupStep>) -> &mut Self {
        self.steps.push(step);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn step_ids(&self) -> Vec<&str> {
        self.steps.iter().map(|s| s.id()).collect()
    }

    /// Run the pipeline. In dry-run nothing is written: each step is reported as
    /// planned and the registry/state are untouched. Otherwise steps run in
    /// order, already-completed steps are skipped (resume), and after each step
    /// the readiness registry (if the step recorded anything), resumable state,
    /// and logs are persisted. A failing step — gate or not — stops the run and
    /// returns its error; a resume reruns from that step.
    pub fn run(
        &self,
        layout: &InstanceLayout,
        protected_roots: &[PathBuf],
        opts: &PipelineOptions,
        progress: &mut dyn ProgressSink,
    ) -> Result<SetupRun, SetupError> {
        let total = self.steps.len();

        for step in &self.steps {
            if !valid_step_id(step.id()) {
                return Err(SetupError::InvalidStepId {
                    id: step.id().to_string(),
                });
            }
        }

        if opts.dry_run {
            for (index, step) in self.steps.iter().enumerate() {
                progress.step_started(index, total, step.id(), &step.description());
                progress.step_completed(index, total, step.id(), "(dry-run)");
            }
            return Ok(SetupRun {
                executed: Vec::new(),
                skipped: Vec::new(),
                dry_run: true,
            });
        }

        let state_path = layout.setup_state_path();
        let mut state = load_state(&state_path)?;
        let registry_path = layout.readiness_path();
        let mut registry = load_registry(&registry_path)?;

        let mut executed = Vec::new();
        let mut skipped = Vec::new();

        for (index, step) in self.steps.iter().enumerate() {
            // Gates are checkpoints, not one-shot work: they always re-evaluate
            // the live registry and are never persisted as completed, so a
            // resume (or a hand-edited state.json) cannot skip past a gate.
            if !step.is_gate() && state.is_completed(step.id()) {
                skipped.push(step.id().to_string());
                progress.step_skipped(index, total, step.id());
                continue;
            }

            progress.step_started(index, total, step.id(), &step.description());

            let mut ctx = SetupContext {
                layout,
                dry_run: false,
                protected_roots,
                registry: &mut registry,
                registry_dirty: false,
            };
            let result = step.run(&mut ctx);
            let registry_dirty = ctx.registry_dirty;
            if registry_dirty {
                // Persist whatever the step recorded, even on failure, so the
                // CLI/web surfaces show the real check states from this run.
                save_registry(&registry_path, &registry)?;
            }

            match result {
                Ok(detail) => {
                    // A gate is never recorded as completed, so it re-runs on
                    // every resume; only real work steps persist completion.
                    if !step.is_gate() {
                        state.mark_completed(step.id());
                        persist_state_if_possible(layout, &state)?;
                    }
                    write_step_log(layout, step.id(), &detail);
                    append_setup_log(layout, &format!("step {} ok: {detail}", step.id()));
                    progress.step_completed(index, total, step.id(), &detail);
                    executed.push(step.id().to_string());
                }
                Err(e) => {
                    state.record_error(e.to_string());
                    let _ = persist_state_if_possible(layout, &state);
                    append_setup_log(layout, &format!("step {} failed: {e}", step.id()));
                    progress.step_failed(index, total, step.id(), &e.to_string());
                    return Err(e);
                }
            }
        }

        Ok(SetupRun {
            executed,
            skipped,
            dry_run: false,
        })
    }
}

/// Wraps a closure as a [`SetupStep`], so a product can express a step inline
/// without a dedicated type. Use [`FnStep::gate`] for a hard gate.
pub struct FnStep<F> {
    id: String,
    description: String,
    gate: bool,
    run: F,
}

impl<F> FnStep<F>
where
    F: Fn(&mut SetupContext<'_>) -> Result<String, SetupError>,
{
    pub fn new(id: impl Into<String>, description: impl Into<String>, run: F) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            gate: false,
            run,
        }
    }

    pub fn gate(id: impl Into<String>, description: impl Into<String>, run: F) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            gate: true,
            run,
        }
    }

    pub fn boxed(self) -> Box<dyn SetupStep>
    where
        F: 'static,
    {
        Box::new(self)
    }
}

impl<F> SetupStep for FnStep<F>
where
    F: Fn(&mut SetupContext<'_>) -> Result<String, SetupError>,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn is_gate(&self) -> bool {
        self.gate
    }

    fn run(&self, ctx: &mut SetupContext<'_>) -> Result<String, SetupError> {
        (self.run)(ctx)
    }
}

/// A ready-made hard gate: setup cannot complete while any registered readiness
/// check is still failing or pending. Place it last so earlier steps have
/// recorded their results. `Skipped` checks (disabled paths) do not block.
pub fn readiness_gate(id: impl Into<String>) -> Box<dyn SetupStep> {
    let id = id.into();
    let gate_id = id.clone();
    FnStep::gate(id, "All enabled readiness checks must pass", move |ctx| {
        let registry = ctx.readiness();
        if registry.all_pass() {
            return Ok(format!("{} readiness check(s) pass", registry.checks.len()));
        }
        let unmet: Vec<&str> = registry
            .checks
            .iter()
            .filter(|c| !matches!(c.status, CheckStatus::Pass | CheckStatus::Skipped))
            .map(|c| c.id.as_str())
            .collect();
        Err(SetupError::Gate {
            id: gate_id.clone(),
            detail: format!("{} check(s) not passing: {}", unmet.len(), unmet.join(", ")),
        })
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readiness::{EnabledSurface, ReadinessRegistry};
    use crate::state::load_state;
    use std::cell::RefCell;

    /// A layout under a fresh temp home, with the instance dirs + a readiness
    /// registry already in place (as the bootstrap would leave them).
    fn prepared(modules: &[&str]) -> (tempfile::TempDir, InstanceLayout) {
        let home = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(home.path(), "assistant", Some("test")).unwrap();
        for dir in layout.managed_dirs() {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let surface = EnabledSurface {
            modules: modules.iter().map(|m| m.to_string()).collect(),
            profile: None,
            capabilities: Vec::new(),
        };
        save_registry(&layout.readiness_path(), &ReadinessRegistry::for_enabled(&surface)).unwrap();
        (home, layout)
    }

    #[derive(Default)]
    struct Recorder {
        events: RefCell<Vec<String>>,
    }
    impl ProgressSink for &Recorder {
        fn step_started(&mut self, i: usize, total: usize, id: &str, _d: &str) {
            self.events.borrow_mut().push(format!("start {i}/{total} {id}"));
        }
        fn step_completed(&mut self, _i: usize, _t: usize, id: &str, _detail: &str) {
            self.events.borrow_mut().push(format!("done {id}"));
        }
        fn step_skipped(&mut self, _i: usize, _t: usize, id: &str) {
            self.events.borrow_mut().push(format!("skip {id}"));
        }
        fn step_failed(&mut self, _i: usize, _t: usize, id: &str, _e: &str) {
            self.events.borrow_mut().push(format!("fail {id}"));
        }
    }

    fn ok_step(id: &'static str) -> Box<dyn SetupStep> {
        FnStep::new(id, format!("do {id}"), move |_ctx| Ok(format!("{id} ok"))).boxed()
    }

    #[test]
    fn dry_run_writes_nothing_and_reports_every_step() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let pipeline = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(ok_step("beta"));
        let rec = Recorder::default();
        let mut sink = &rec;
        let run = pipeline
            .run(
                &layout,
                &[],
                &PipelineOptions { dry_run: true },
                &mut sink,
            )
            .unwrap();
        assert!(run.dry_run);
        assert!(run.executed.is_empty());
        // No state file written for a dry run.
        assert!(!layout.setup_state_path().exists() || load_state(&layout.setup_state_path()).unwrap().completed.is_empty());
        // Both steps were reported.
        assert!(rec.events.borrow().iter().any(|e| e == "done alpha"));
        assert!(rec.events.borrow().iter().any(|e| e == "done beta"));
    }

    #[test]
    fn executes_in_order_and_persists_resumable_state() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let pipeline = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(ok_step("beta"));
        let mut sink = SilentProgress;
        let run = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert_eq!(run.executed, vec!["alpha", "beta"]);
        let state = load_state(&layout.setup_state_path()).unwrap();
        assert!(state.is_completed("alpha") && state.is_completed("beta"));
        assert!(layout.logs_dir().join("setup-alpha.log").exists());
        assert!(layout.setup_log_path().exists());
    }

    #[test]
    fn completed_steps_are_skipped_on_rerun() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let pipeline = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(ok_step("beta"));
        let mut sink = SilentProgress;
        pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        let second = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert!(second.executed.is_empty());
        assert_eq!(second.skipped, vec!["alpha", "beta"]);
    }

    #[test]
    fn a_failing_step_stops_the_run_and_resume_continues() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let fail_once = FnStep::new("beta", "fails first time", |_ctx| {
            Err(SetupError::Gate {
                id: "beta".to_string(),
                detail: "not ready".to_string(),
            })
        })
        .boxed();
        let pipeline = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(fail_once);
        let mut sink = SilentProgress;
        let err = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap_err();
        assert!(matches!(err, SetupError::Gate { .. }));
        // alpha completed and is recorded; beta is not.
        let state = load_state(&layout.setup_state_path()).unwrap();
        assert!(state.is_completed("alpha"));
        assert!(!state.is_completed("beta"));
        assert!(state.last_error.is_some());

        // Resume with a beta that now succeeds: alpha is skipped, beta runs.
        let pipeline2 = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(ok_step("beta"));
        let resumed = pipeline2
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert_eq!(resumed.skipped, vec!["alpha"]);
        assert_eq!(resumed.executed, vec!["beta"]);
    }

    #[test]
    fn readiness_gate_blocks_until_all_checks_pass() {
        let (_h, layout) = prepared(&["assistant-core", "assistant-session"]);
        // A step that only marks one of the two checks passing, then the gate.
        let mark_core = FnStep::new("mark_core", "pass assistant-core", |ctx| {
            assert!(ctx.record_readiness("module:assistant-core", CheckStatus::Pass, None));
            Ok("recorded".to_string())
        })
        .boxed();
        let pipeline = SetupPipeline::new()
            .with_step(mark_core)
            .with_step(readiness_gate("readiness_gate"));
        let mut sink = SilentProgress;
        let err = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap_err();
        match err {
            SetupError::Gate { detail, .. } => assert!(detail.contains("module:assistant-session")),
            other => panic!("expected gate error, got {other}"),
        }

        // Mark the remaining check and resume: the gate now passes.
        let mark_session = FnStep::new("mark_session", "pass assistant-session", |ctx| {
            assert!(ctx.record_readiness("module:assistant-session", CheckStatus::Pass, None));
            Ok("recorded".to_string())
        })
        .boxed();
        let pipeline2 = SetupPipeline::new()
            .with_step(ok_step("mark_core"))
            .with_step(mark_session)
            .with_step(readiness_gate("readiness_gate"));
        let resumed = pipeline2
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert!(resumed.executed.contains(&"readiness_gate".to_string()));
    }

    #[test]
    fn a_step_cannot_write_outside_the_instance_root() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let escaper = FnStep::new("escape", "tries to write outside", |ctx| {
            let target = std::path::Path::new("/tmp/evil-setup-target");
            ctx.guard_writable(target)?;
            Ok("should not reach".to_string())
        })
        .boxed();
        let pipeline = SetupPipeline::new().with_step(escaper);
        let mut sink = SilentProgress;
        let err = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap_err();
        assert!(matches!(err, SetupError::SourceMutation { .. }));
    }

    #[test]
    fn a_dotdot_path_under_the_root_is_still_rejected() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let root = layout.root.clone();
        let traverser = FnStep::new("traverse", "tries to escape via ..", move |ctx| {
            // Lexically prefixed by the root, but `..` resolves outside it.
            let target = root.join("..").join("escaped");
            ctx.guard_writable(&target)?;
            Ok("should not reach".to_string())
        })
        .boxed();
        let pipeline = SetupPipeline::new().with_step(traverser);
        let mut sink = SilentProgress;
        let err = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap_err();
        assert!(matches!(err, SetupError::SourceMutation { .. }));
    }

    #[test]
    fn a_step_id_that_could_traverse_the_logs_dir_is_rejected() {
        let (_h, layout) = prepared(&["assistant-core"]);
        let evil = FnStep::new("../../config", "malicious id", |_ctx| Ok("x".to_string())).boxed();
        let pipeline = SetupPipeline::new().with_step(evil);
        let mut sink = SilentProgress;
        let err = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap_err();
        assert!(matches!(err, SetupError::InvalidStepId { .. }));
    }

    #[test]
    fn a_gate_is_re_evaluated_on_every_run_and_never_persisted_completed() {
        let (_h, layout) = prepared(&[]); // no enabled checks -> gate passes trivially
        let pipeline = SetupPipeline::new()
            .with_step(ok_step("alpha"))
            .with_step(readiness_gate("readiness_gate"));
        let mut sink = SilentProgress;

        let first = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert!(first.executed.contains(&"readiness_gate".to_string()));
        // The gate is not recorded as completed even though it passed.
        let state = load_state(&layout.setup_state_path()).unwrap();
        assert!(state.is_completed("alpha"));
        assert!(!state.is_completed("readiness_gate"));

        // On a rerun the real work step is skipped but the gate runs again.
        let second = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert_eq!(second.skipped, vec!["alpha"]);
        assert_eq!(second.executed, vec!["readiness_gate"]);
    }
}
