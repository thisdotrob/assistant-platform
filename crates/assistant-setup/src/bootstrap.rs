//! Resumable bootstrap initialization: create the instance directory, write
//! `config.toml`, and migrate the central DB. Writes only under the instance
//! root; refuses to run if that root falls inside a protected source repo.

use std::fs;
use std::path::{Path, PathBuf};

use assistant_config::{Config, InstanceLayout};
use assistant_db::{apply, open_central, record_versions, MigrationSet, VersionRecord};

use crate::error::SetupError;
use crate::readiness::{save_registry, EnabledSurface, ReadinessRegistry};
use crate::state::load_state;
use crate::support::{append_setup_log, guard_writable, persist_state_if_possible, write_step_log};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepId {
    CreateDirectories,
    WriteConfig,
    MigrateDb,
    WriteReadiness,
}

impl StepId {
    pub const ALL: [StepId; 4] = [
        StepId::CreateDirectories,
        StepId::WriteConfig,
        StepId::MigrateDb,
        StepId::WriteReadiness,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            StepId::CreateDirectories => "create_directories",
            StepId::WriteConfig => "write_config",
            StepId::MigrateDb => "migrate_db",
            StepId::WriteReadiness => "write_readiness",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedStep {
    pub id: StepId,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct BootstrapPlan {
    pub instance_root: PathBuf,
    pub config_path: PathBuf,
    pub central_db_path: PathBuf,
    pub managed_dirs: Vec<PathBuf>,
    pub steps: Vec<PlannedStep>,
}

pub struct BootstrapInput {
    pub config: Config,
    pub home: PathBuf,
    pub migrations: MigrationSet,
    pub version_record: VersionRecord,
    /// Source repos the bootstrap must never write into.
    pub protected_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct BootstrapOptions {
    pub dry_run: bool,
    /// Stop before this step runs (models a crash for resume testing / staged
    /// setup). The remaining steps run on a later resume.
    pub stop_before: Option<StepId>,
}

#[derive(Debug)]
pub struct BootstrapOutcome {
    pub plan: BootstrapPlan,
    pub executed: Vec<StepId>,
    pub skipped: Vec<StepId>,
    pub dry_run: bool,
}

/// Build the plan without touching the filesystem. Validates the config and the
/// derived instance root.
pub fn plan(input: &BootstrapInput) -> Result<BootstrapPlan, SetupError> {
    input.config.validate()?;
    let layout = input.config.instance_layout(&input.home)?;
    assert_outside_protected(&layout.root, &input.protected_roots)?;

    let managed_dirs = layout.managed_dirs();
    let steps = vec![
        PlannedStep {
            id: StepId::CreateDirectories,
            description: format!(
                "Create {} instance directories under {}",
                managed_dirs.len(),
                layout.root.display()
            ),
        },
        PlannedStep {
            id: StepId::WriteConfig,
            description: format!("Write config.toml at {}", layout.config_path().display()),
        },
        PlannedStep {
            id: StepId::MigrateDb,
            description: format!(
                "Open {} and apply baseline central-DB migrations",
                layout.central_db_path().display()
            ),
        },
        PlannedStep {
            id: StepId::WriteReadiness,
            description: format!(
                "Write readiness registry for enabled checks at {}",
                layout.readiness_path().display()
            ),
        },
    ];

    Ok(BootstrapPlan {
        instance_root: layout.root.clone(),
        config_path: layout.config_path(),
        central_db_path: layout.central_db_path(),
        managed_dirs,
        steps,
    })
}

/// Run bootstrap. In dry-run mode nothing is written and the plan is returned.
/// Otherwise steps run in order, persisting resumable state and logs after each.
pub fn run(input: &BootstrapInput, opts: &BootstrapOptions) -> Result<BootstrapOutcome, SetupError> {
    let plan = plan(input)?;
    let layout = input.config.instance_layout(&input.home)?;

    if opts.dry_run {
        return Ok(BootstrapOutcome {
            plan,
            executed: Vec::new(),
            skipped: Vec::new(),
            dry_run: true,
        });
    }

    let state_path = layout.setup_state_path();
    let mut state = load_state(&state_path)?;
    let mut executed = Vec::new();
    let mut skipped = Vec::new();

    for step in StepId::ALL {
        if opts.stop_before == Some(step) {
            break;
        }
        if state.is_completed(step.as_str()) {
            skipped.push(step);
            continue;
        }

        match execute_step(step, input, &layout) {
            Ok(detail) => {
                state.mark_completed(step.as_str());
                persist_state_if_possible(&layout, &state)?;
                write_step_log(&layout, step.as_str(), &detail);
                append_setup_log(&layout, &format!("step {} ok: {detail}", step.as_str()));
                executed.push(step);
            }
            Err(e) => {
                state.record_error(e.to_string());
                let _ = persist_state_if_possible(&layout, &state);
                append_setup_log(&layout, &format!("step {} failed: {e}", step.as_str()));
                return Err(e);
            }
        }
    }

    Ok(BootstrapOutcome {
        plan,
        executed,
        skipped,
        dry_run: false,
    })
}

fn execute_step(
    step: StepId,
    input: &BootstrapInput,
    layout: &InstanceLayout,
) -> Result<String, SetupError> {
    match step {
        StepId::CreateDirectories => {
            let dirs = layout.managed_dirs();
            for dir in &dirs {
                guard_writable(&layout.root, dir, &input.protected_roots)?;
                fs::create_dir_all(dir).map_err(|source| SetupError::Io {
                    path: dir.clone(),
                    source,
                })?;
            }
            Ok(format!("created {} directories", dirs.len()))
        }
        StepId::WriteConfig => {
            let path = layout.config_path();
            guard_writable(&layout.root, &path, &input.protected_roots)?;
            assistant_config::write_config(&path, &input.config)?;
            Ok(format!("wrote {}", path.display()))
        }
        StepId::MigrateDb => {
            let path = layout.central_db_path();
            guard_writable(&layout.root, &path, &input.protected_roots)?;
            let mut conn = open_central(&path)?;
            let report = apply(&mut conn, &input.migrations)?;
            record_versions(&conn, &input.version_record)?;
            Ok(format!(
                "applied {} migrations, skipped {}",
                report.applied.len(),
                report.skipped.len()
            ))
        }
        StepId::WriteReadiness => {
            let path = layout.readiness_path();
            guard_writable(&layout.root, &path, &input.protected_roots)?;
            let surface = EnabledSurface {
                modules: input.config.modules.enabled.clone(),
                profile: None,
                capabilities: Vec::new(),
            };
            let registry = ReadinessRegistry::for_enabled(&surface);
            save_registry(&path, &registry)?;
            Ok(format!("registered {} readiness check(s)", registry.checks.len()))
        }
    }
}

/// Verify the instance root does not live inside any protected source repo.
pub fn assert_outside_protected(
    instance_root: &Path,
    protected_roots: &[PathBuf],
) -> Result<(), SetupError> {
    for protected in protected_roots {
        if instance_root.starts_with(protected) {
            return Err(SetupError::SourceMutation {
                path: instance_root.to_path_buf(),
            });
        }
    }
    Ok(())
}
