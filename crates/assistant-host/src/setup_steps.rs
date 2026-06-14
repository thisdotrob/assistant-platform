//! Reusable product setup steps for a local live turn.
//!
//! Both products append [`setup_steps`] to their `assistant_cli::setup` call so they
//! inherit the same host-side preparation: build the shared agent image and
//! validate the OneCLI Claude path. Each step records its own module readiness
//! so the readiness surface reflects reality; the products own whether to wire a
//! hard [`assistant_setup::readiness_gate`] last (deferred here while per-module
//! readiness reporting is still partial).

use std::process::Command;

use assistant_db::{apply, open_central, Migration, MigrationSet};
use assistant_runtime_docker::BASE_IMAGE_REPOSITORY;
use assistant_setup::{CheckStatus, FnStep, SetupStep};

/// Env var naming the build context (directory containing the agent
/// `Dockerfile`). When unset, the build step reports a skip rather than failing.
pub const AGENT_IMAGE_DIR_ENV: &str = "ASSISTANT_AGENT_IMAGE_DIR";
/// Env var overriding the image tag to build/run. Defaults to
/// `assistant-base:<platform version>`.
pub const AGENT_IMAGE_TAG_ENV: &str = "ASSISTANT_AGENT_IMAGE_TAG";

fn default_image_tag() -> String {
    format!("{BASE_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// The ordered host setup steps a product appends before its readiness gate.
pub fn setup_steps() -> Vec<Box<dyn SetupStep>> {
    vec![migrate_domain_db(), build_agent_image(), configure_onecli()]
}

/// The domain subsystems the run-loop composes (memory/RAG, permissions, the
/// scheduler, and router sticky-engagement) each ship a `v2` migration in their
/// own module namespace. The M1 bootstrap applies only the baseline (`v1`)
/// tables, so without this step a real install is missing the catalog,
/// standing-instruction, lease, and sticky tables those subsystems read. Applied
/// in module-dependency order on top of the baseline; `apply` is per-`(module,
/// version)` and idempotent, so a resume/rerun is a no-op.
fn domain_migrations() -> MigrationSet {
    let mut set = MigrationSet::new(vec![
        assistant_router::MODULE_ID.to_string(),
        assistant_permissions::MODULE_ID.to_string(),
        assistant_scheduler::MODULE_ID.to_string(),
        assistant_memory::MODULE_ID.to_string(),
        assistant_agent_graph::MODULE_ID.to_string(),
    ]);
    let v2s: Vec<Migration> = assistant_router::migrations()
        .into_iter()
        .chain(assistant_permissions::migrations())
        .chain(assistant_scheduler::migrations())
        .chain(assistant_memory::migrations())
        .chain(assistant_agent_graph::store::migrations())
        .collect();
    for migration in v2s {
        set.add(migration);
    }
    set
}

/// Apply the domain-subsystem central-DB migrations on top of the M1 baseline.
fn migrate_domain_db() -> Box<dyn SetupStep> {
    FnStep::new(
        "migrate_domain_db",
        "Apply domain (memory/permissions/scheduler/router) central-DB migrations",
        |ctx| {
            let path = ctx.layout().central_db_path();
            if ctx.dry_run() {
                return Ok(format!("would apply domain migrations to {}", path.display()));
            }
            ctx.guard_writable(&path)?;
            let mut conn = open_central(&path)?;
            let report = apply(&mut conn, &domain_migrations())?;
            Ok(format!(
                "applied {} migration(s), skipped {}",
                report.applied.len(),
                report.skipped.len()
            ))
        },
    )
    .boxed()
}

/// Build the shared `assistant-base` image from a `Dockerfile` context. Runs
/// `docker build` only outside the sandbox; in dry-run (or with no context set)
/// it reports what it would do without touching Docker.
fn build_agent_image() -> Box<dyn SetupStep> {
    FnStep::new(
        "build_agent_image",
        "Build the assistant-base agent container image",
        |ctx| {
            let tag = std::env::var(AGENT_IMAGE_TAG_ENV).unwrap_or_else(|_| default_image_tag());
            let context = std::env::var(AGENT_IMAGE_DIR_ENV).ok();

            if ctx.dry_run() {
                return Ok(match &context {
                    Some(dir) => format!("would build {tag} from {dir}"),
                    None => format!("would build {tag} (set {AGENT_IMAGE_DIR_ENV} to a context)"),
                });
            }

            let Some(dir) = context else {
                return Ok(format!(
                    "skipped: set {AGENT_IMAGE_DIR_ENV} to the agent image context to build {tag}"
                ));
            };

            let output = Command::new("docker")
                .args(["build", "-t", &tag, &dir])
                .output()
                .map_err(|source| std::io::Error::new(
                    source.kind(),
                    format!("failed to launch docker build: {source}"),
                ))
                .map_err(|source| assistant_setup::SetupError::Io {
                    path: std::path::PathBuf::from(&dir),
                    source,
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(assistant_setup::SetupError::Io {
                    path: std::path::PathBuf::from(&dir),
                    source: std::io::Error::other(format!(
                        "docker build failed: {}",
                        stderr.trim()
                    )),
                });
            }
            ctx.record_readiness(
                "module:assistant-runtime-docker",
                CheckStatus::Pass,
                Some(format!("built {tag}")),
            );
            Ok(format!("built {tag}"))
        },
    )
    .boxed()
}

/// Provision this installation's OneCLI material for the Claude path and record
/// host readiness. Each installation has its own gateway (its own agent), so the
/// step is scoped to the instance's agent identifier. It never fails the pipeline
/// (stub-mode setup legitimately leaves OneCLI unconfigured; the Claude spawn
/// itself refuses if not ready) and never reads or logs the Anthropic secret —
/// only its presence is probed. Standing up the gateway stack itself is an
/// operator step (the `init-onecli` skill); this validates and persists the
/// host-side trust material from the running gateway.
fn configure_onecli() -> Box<dyn SetupStep> {
    FnStep::new(
        "configure_onecli",
        "Provision the OneCLI gateway material for the Claude path",
        |ctx| {
            let layout = ctx.layout();
            let agent = crate::onecli::agent_identifier(layout);

            if ctx.dry_run() {
                return Ok(match crate::onecli::gateway_url() {
                    Some(url) => {
                        format!("would provision OneCLI agent {agent:?} against gateway {url}")
                    }
                    None => format!(
                        "would provision OneCLI agent {agent:?} (set {} — default port {})",
                        crate::onecli::ONECLI_URL_ENV,
                        crate::onecli::default_gateway_port(layout),
                    ),
                });
            }

            // Live only when a gateway URL is configured; offline/stub setup has
            // none and falls through to a plain probe (all-false → Pending).
            let readiness = match crate::onecli::gateway_url() {
                Some(_) => match crate::onecli::provision_from_gateway(layout, &agent) {
                    Ok(readiness) => readiness,
                    Err(err) => {
                        // A configured-but-unreachable gateway is a real problem,
                        // but we record Pending rather than aborting the whole
                        // pipeline — the Claude spawn refuses anyway.
                        let detail = format!("onecli provisioning incomplete: {err}");
                        ctx.record_readiness(
                            "module:assistant-host",
                            CheckStatus::Pending,
                            Some(detail.clone()),
                        );
                        return Ok(detail);
                    }
                },
                None => crate::onecli::probe(layout),
            };

            let detail = format!(
                "agent={agent} proxy_configured={} anthropic_secret_present={} placeholder_injection_ok={}",
                readiness.proxy_configured,
                readiness.anthropic_secret_present,
                readiness.placeholder_injection_ok,
            );
            let status = if readiness.is_ready() {
                CheckStatus::Pass
            } else {
                CheckStatus::Pending
            };
            ctx.record_readiness("module:assistant-host", status, Some(detail.clone()));
            Ok(detail)
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_config::InstanceLayout;
    use assistant_db::{
        applied_versions, baseline_migrations, baseline_owner_modules, open_in_memory,
    };
    use assistant_setup::{
        save_registry, EnabledSurface, PipelineOptions, ReadinessRegistry, SetupPipeline,
        SilentProgress,
    };

    fn baseline_set() -> MigrationSet {
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        baseline_migrations(order)
    }

    /// The five domain v2 migrations apply cleanly on top of the baseline (the
    /// scheduler one ALTERs a baseline table, so it would fail if the baseline
    /// were missing), and a rerun is a no-op.
    #[test]
    fn domain_migrations_layer_v2_on_baseline() {
        let mut conn = open_in_memory().unwrap();
        apply(&mut conn, &baseline_set()).unwrap();

        let report = apply(&mut conn, &domain_migrations()).unwrap();
        assert_eq!(report.applied.len(), 5);

        let versions = applied_versions(&conn).unwrap();
        for module in [
            assistant_router::MODULE_ID,
            assistant_permissions::MODULE_ID,
            assistant_scheduler::MODULE_ID,
            assistant_memory::MODULE_ID,
            assistant_agent_graph::MODULE_ID,
        ] {
            assert!(
                versions.contains(&(module.to_string(), 2)),
                "expected {module} v2 applied, got {versions:?}"
            );
        }

        // Idempotent: a second apply skips all five.
        let rerun = apply(&mut conn, &domain_migrations()).unwrap();
        assert!(rerun.applied.is_empty());
        assert_eq!(rerun.skipped.len(), 5);
    }

    /// A layout under a fresh temp home with the instance dirs and a readiness
    /// registry already in place, as the M1 bootstrap would leave them.
    fn prepared() -> (tempfile::TempDir, InstanceLayout) {
        let home = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(home.path(), "assistant", Some("test")).unwrap();
        for dir in layout.managed_dirs() {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let surface = EnabledSurface {
            modules: vec!["assistant-host".to_string()],
            profile: None,
            capabilities: Vec::new(),
        };
        save_registry(&layout.readiness_path(), &ReadinessRegistry::for_enabled(&surface)).unwrap();
        (home, layout)
    }

    /// The setup step opens this installation's central DB and applies the domain
    /// migrations on top of the baseline the bootstrap already wrote.
    #[test]
    fn migrate_domain_db_step_applies_to_central_db() {
        let (_home, layout) = prepared();
        // Simulate the bootstrap's MigrateDb step having run first.
        {
            let mut conn = open_central(&layout.central_db_path()).unwrap();
            apply(&mut conn, &baseline_set()).unwrap();
        }

        let pipeline = SetupPipeline::new().with_step(migrate_domain_db());
        let mut sink = SilentProgress;
        let run = pipeline
            .run(&layout, &[], &PipelineOptions::default(), &mut sink)
            .unwrap();
        assert_eq!(run.executed, vec!["migrate_domain_db"]);

        let conn = open_central(&layout.central_db_path()).unwrap();
        let versions = applied_versions(&conn).unwrap();
        assert!(versions.contains(&(assistant_memory::MODULE_ID.to_string(), 2)));
        assert!(versions.contains(&(assistant_permissions::MODULE_ID.to_string(), 2)));
        assert!(versions.contains(&(assistant_scheduler::MODULE_ID.to_string(), 2)));
        assert!(versions.contains(&(assistant_router::MODULE_ID.to_string(), 2)));
        assert!(versions.contains(&(assistant_agent_graph::MODULE_ID.to_string(), 2)));
    }
}
