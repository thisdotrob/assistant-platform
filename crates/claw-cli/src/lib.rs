use std::path::{Path, PathBuf};

use claw_config::{Config, InstanceLayout, ModulesConfig, ProductConfig, WebConfig};
use claw_core::platform_metadata;
use claw_db::{baseline_migrations, VersionRecord};
use claw_setup::{
    run, BootstrapInput, BootstrapOptions, PipelineOptions, ProgressSink, SetupPipeline, SetupStep,
};
use claw_upgrade::{
    upgrade_instance, ConformanceReport, RuntimeVersions, UpgradeOptions, UpgradeReport,
};

pub mod access;
pub mod bridge;
pub mod command;
pub mod output;
pub mod readiness;
pub mod registry;

pub use access::{dispatch, AccessDecision, AccessPolicy, AccessRequest, OperatorOnly};
pub use bridge::{
    decode_response, encode_request, serve_pending, BridgeError, BridgeRequest, BridgeResponse,
    BridgeTransport, SessionBridge, REQUEST_KIND, RESPONSE_KIND,
};
pub use command::{
    ActionKind, Caller, CommandOutcome, CommandRequest, ResultTable,
};
pub use output::{render, OutputFormat, Verbosity};
pub use readiness::{render_readiness, CheckResult, CheckState, ReadinessReport};
pub use registry::{ActionSpec, CommandRegistry, RegistryError, Resource};

// Re-exported so a product can assemble its setup pipeline against `claw-cli`
// alone, without taking a direct dependency on `claw-setup`.
pub use claw_setup::{
    readiness_gate, CheckStatus, FnStep, SetupContext, SetupError, SetupRun,
};

pub const MODULE_ID: &str = "claw-cli";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the platform/product compatibility check and print a human-readable
/// summary. Returns a process exit code: 0 on success, 1 on failure.
pub fn doctor_compatibility(platform_root: &Path, product_root: &Path) -> i32 {
    match claw_core::run_compatibility_check(platform_root, product_root) {
        Err(load_error) => {
            eprintln!("error: {}", load_error);
            1
        }
        Ok(report) => {
            println!("product: {} {}", report.product_id, report.product_version);
            println!(
                "platform: {} {}",
                report.platform_id, report.platform_version
            );
            if report.ok {
                println!("compatibility: OK");
                0
            } else {
                eprintln!("compatibility: FAILED ({} issue(s))", report.diagnostics.len());
                for diagnostic in &report.diagnostics {
                    eprintln!("{}", diagnostic);
                }
                1
            }
        }
    }
}

/// Render an aggregated readiness report and return a process exit code: 0 when
/// ready, 1 when any check is a blocking failure. The host collects the report
/// from each registered module before calling this.
pub fn doctor_readiness(
    report: &readiness::ReadinessReport,
    format: output::OutputFormat,
    verbosity: output::Verbosity,
) -> i32 {
    println!("{}", readiness::render_readiness(report, format, verbosity));
    if report.is_ready() {
        0
    } else {
        1
    }
}

/// Product-supplied inputs for a bootstrap run. The platform supplies module
/// graph and version; this carries only product identity and run options, so no
/// product policy lives in the shared crate.
pub struct BootstrapRequest {
    pub namespace: String,
    pub product_id: String,
    pub product_version: String,
    pub instance: Option<String>,
    pub enabled_modules: Vec<String>,
    pub home: Option<PathBuf>,
    pub protected_roots: Vec<PathBuf>,
    pub dry_run: bool,
}

/// Build the bootstrap input from a product request. The platform supplies the
/// module graph and version; the request carries only product identity. Returns
/// the derived `home` alongside the input so callers can derive the layout.
fn build_bootstrap_input(request: &BootstrapRequest) -> Result<BootstrapInput, String> {
    let platform = platform_metadata();
    let module_order: Vec<String> = platform.module_ids.iter().map(|s| s.to_string()).collect();

    let config = Config {
        product: ProductConfig {
            namespace: request.namespace.clone(),
            product_id: request.product_id.clone(),
            product_version: request.product_version.clone(),
            platform_version: platform.version.to_string(),
            instance: request.instance.clone(),
            owner_handle: None,
        },
        modules: ModulesConfig {
            enabled: request.enabled_modules.clone(),
        },
        web: WebConfig::default(),
    };

    let home = match request.home.clone().or_else(claw_config::home_dir) {
        Some(home) => home,
        None => return Err("HOME is not set; pass --home <path>".to_string()),
    };

    let version_record = VersionRecord {
        product_id: request.product_id.clone(),
        product_version: request.product_version.clone(),
        platform_version: platform.version.to_string(),
        modules: platform
            .module_ids
            .iter()
            .map(|m| (m.to_string(), platform.version.to_string()))
            .collect(),
    };

    Ok(BootstrapInput {
        config,
        home,
        migrations: baseline_migrations(module_order),
        version_record,
        protected_roots: request.protected_roots.clone(),
    })
}

/// Run bootstrap initialization for a product and print a human-readable
/// summary. Returns a process exit code: 0 on success, 1 on failure.
pub fn bootstrap(request: BootstrapRequest) -> i32 {
    let input = match build_bootstrap_input(&request) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let options = BootstrapOptions {
        dry_run: request.dry_run,
        stop_before: None,
    };

    match run(&input, &options) {
        Ok(outcome) => {
            println!("instance root: {}", outcome.plan.instance_root.display());
            println!("config: {}", outcome.plan.config_path.display());
            println!("central db: {}", outcome.plan.central_db_path.display());
            if outcome.dry_run {
                println!("mode: dry-run (no changes written)");
                println!("planned steps:");
                for step in &outcome.plan.steps {
                    println!("  - {}", step.description);
                }
            } else {
                println!("mode: apply");
                println!(
                    "executed: {} step(s), skipped: {} step(s)",
                    outcome.executed.len(),
                    outcome.skipped.len()
                );
            }
            0
        }
        Err(e) => {
            eprintln!("setup error: {e}");
            1
        }
    }
}

/// Run full product setup: the foundational bootstrap (idempotent + resumable)
/// followed by the product's own ordered setup steps. The platform owns the
/// runner, resumable state, logging, dry-run, progress, and gate enforcement;
/// the product supplies the domain/external steps (create owner, build images,
/// configure OneCLI/channels, install service, channel delivery checks, …) as
/// `SetupStep`s — and should append [`readiness_gate`] last so a readiness
/// failure blocks completion. Returns a process exit code: 0 on success, 1 on
/// any failure (including a gate that did not pass).
pub fn setup(request: BootstrapRequest, steps: Vec<Box<dyn SetupStep>>) -> i32 {
    let dry_run = request.dry_run;
    let input = match build_bootstrap_input(&request) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    // Phase 1 — foundational bootstrap (create dirs, write config, migrate DB,
    // write the readiness registry). Idempotent and resumable.
    let boot = BootstrapOptions {
        dry_run,
        stop_before: None,
    };
    let outcome = match run(&input, &boot) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("setup error: {e}");
            return 1;
        }
    };
    println!("instance root: {}", outcome.plan.instance_root.display());

    // Phase 2 — product setup pipeline.
    let layout = match input.config.instance_layout(&input.home) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("setup error: {e}");
            return 1;
        }
    };

    let mut pipeline = SetupPipeline::new();
    for step in steps {
        pipeline.push(step);
    }
    if pipeline.is_empty() {
        if dry_run {
            println!("mode: dry-run (no changes written)");
        }
        println!("no additional setup steps configured");
        return 0;
    }

    let mut progress = StdoutProgress;
    match pipeline.run(
        &layout,
        &input.protected_roots,
        &PipelineOptions { dry_run },
        &mut progress,
    ) {
        Ok(run) => {
            if run.dry_run {
                println!("mode: dry-run (no changes written)");
            } else {
                println!(
                    "setup complete: {} step(s) executed, {} skipped",
                    run.executed.len(),
                    run.skipped.len()
                );
            }
            0
        }
        Err(e) => {
            eprintln!("setup error: {e}");
            1
        }
    }
}

/// Upgrade an existing instance to the current platform schema and print a
/// human-readable summary. Applies any pending central migrations and sweeps
/// every per-session DB forward; the run is idempotent and resumable. With
/// `dry_run`, reports what would migrate without writing. Returns a process exit
/// code: 0 on success, 1 on failure (including an instance that was never
/// bootstrapped).
pub fn upgrade(request: BootstrapRequest) -> i32 {
    let dry_run = request.dry_run;
    let input = match build_bootstrap_input(&request) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let layout = match input.config.instance_layout(&input.home) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("upgrade error: {e}");
            return 1;
        }
    };

    match upgrade_instance(
        &layout,
        &input.migrations,
        &input.version_record,
        UpgradeOptions { dry_run },
    ) {
        Ok(report) => {
            print_upgrade_report(&layout, &report);
            0
        }
        Err(e) => {
            eprintln!("upgrade error: {e}");
            1
        }
    }
}

/// Run the full conformance suite a product invokes after a version bump: the
/// repo-level compatibility check, the runtime/instance matrix, and a dry-run
/// upgrade preview. The product passes its running version identity as plain
/// strings, so products stay decoupled from `claw-upgrade`'s own types. Returns
/// a process exit code: 0 when conformant, 1 otherwise.
#[allow(clippy::too_many_arguments)]
pub fn conformance(
    request: BootstrapRequest,
    platform_root: &Path,
    product_root: &Path,
    platform_version: String,
    base_container_image_contract_version: String,
    orchestrator_profile_id: String,
    orchestrator_profile_version: String,
) -> i32 {
    let input = match build_bootstrap_input(&request) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let layout = match input.config.instance_layout(&input.home) {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("conformance error: {e}");
            return 1;
        }
    };

    let runtime = RuntimeVersions {
        platform_version,
        base_container_image_contract_version,
        orchestrator_profile_id,
        orchestrator_profile_version,
    };

    match claw_upgrade::conformance(
        platform_root,
        product_root,
        &layout,
        &runtime,
        &input.migrations,
        &input.version_record,
    ) {
        Ok(report) => {
            print_conformance_report(&report);
            if report.ok {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("conformance error: {e}");
            1
        }
    }
}

fn print_upgrade_report(layout: &InstanceLayout, report: &UpgradeReport) {
    println!("instance root: {}", layout.root.display());
    println!(
        "mode: {}",
        if report.dry_run {
            "dry-run (no changes written)"
        } else {
            "apply"
        }
    );
    let verb = if report.dry_run { "would apply" } else { "applied" };

    if report.central_applied.is_empty() {
        println!("central migrations: none pending");
    } else {
        println!("central migrations {verb}:");
        for (module, version) in &report.central_applied {
            println!("  - {module} v{version}");
        }
    }

    if report.sessions.is_empty() {
        println!("sessions: all at current schema");
    } else {
        println!("sessions {verb}:");
        for session in &report.sessions {
            println!(
                "  - {}/{} inbound={:?} outbound={:?}",
                session.agent_group_id,
                session.session_id,
                session.inbound_applied,
                session.outbound_applied
            );
        }
    }
}

fn print_conformance_report(report: &ConformanceReport) {
    let matrix = &report.matrix;
    println!(
        "product: {} {}",
        matrix.repo.product_id, matrix.repo.product_version
    );
    println!(
        "platform: {} {}",
        matrix.repo.platform_id, matrix.repo.platform_version
    );

    if matrix.repo.ok {
        println!("repo compatibility: OK");
    } else {
        eprintln!(
            "repo compatibility: FAILED ({} issue(s))",
            matrix.repo.diagnostics.len()
        );
        for diagnostic in &matrix.repo.diagnostics {
            eprintln!("  {diagnostic}");
        }
    }

    if matrix.findings.is_empty() {
        println!("runtime compatibility: OK");
    } else {
        eprintln!("runtime compatibility: {} finding(s)", matrix.findings.len());
        for finding in &matrix.findings {
            eprintln!("  {:?}: {}", finding.code, finding.message);
        }
    }

    match &report.upgrade_preview {
        Some(preview) if preview.central_applied.is_empty() && preview.sessions.is_empty() => {
            println!("upgrade preview: instance already at current schema");
        }
        Some(preview) => {
            println!("upgrade preview: pending migrations");
            for (module, version) in &preview.central_applied {
                println!("  central {module} v{version}");
            }
            for session in &preview.sessions {
                println!(
                    "  session {}/{} inbound={:?} outbound={:?}",
                    session.agent_group_id,
                    session.session_id,
                    session.inbound_applied,
                    session.outbound_applied
                );
            }
        }
        None => println!("upgrade preview: instance not bootstrapped"),
    }

    if report.ok {
        println!("conformance: OK");
    } else {
        eprintln!("conformance: FAILED");
    }
}

/// Streams setup progress to stdout/stderr, one line per step transition.
struct StdoutProgress;

impl ProgressSink for StdoutProgress {
    fn step_started(&mut self, index: usize, total: usize, id: &str, description: &str) {
        println!("[{}/{}] {id}: {description}", index + 1, total);
    }
    fn step_completed(&mut self, _index: usize, _total: usize, id: &str, detail: &str) {
        println!("  ok: {id}: {detail}");
    }
    fn step_skipped(&mut self, _index: usize, _total: usize, id: &str) {
        println!("  skip: {id} (already complete)");
    }
    fn step_failed(&mut self, _index: usize, _total: usize, id: &str, error: &str) {
        eprintln!("  FAILED: {id}: {error}");
    }
}

