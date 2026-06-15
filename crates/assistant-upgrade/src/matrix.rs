//! Runtime compatibility matrix and the reusable conformance entrypoint.
//!
//! The repo-level check (module graph, contract coverage, product↔platform
//! version) is delegated to [`assistant_core::run_compatibility_check`]. On top of
//! that this module compares the *running* system against the release manifest
//! and the instance on disk: the platform binary version, the base container
//! image contract version, the orchestrator profile, and the instance DB schema
//! state. The instance checks are deliberately one-directional — they refuse to
//! run an instance written by a *newer* platform (a downgrade) and flag any
//! per-session schema beyond what this build ships.

use std::path::Path;

use assistant_config::InstanceLayout;
use assistant_core::{CompatibilityReport, PlatformManifest, ProductManifest};
use assistant_db::{MigrationSet, VersionRecord};
use assistant_session::{CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION};

use crate::error::UpgradeError;
use crate::inventory::{inventory, InstanceInventory};
use crate::runner::{upgrade_instance, UpgradeOptions, UpgradeReport};

/// The actual versions the running system presents, supplied by the caller, to
/// be verified against the manifests on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVersions {
    pub platform_version: String,
    pub base_container_image_contract_version: String,
    pub orchestrator_profile_id: String,
    pub orchestrator_profile_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatCode {
    /// Running platform binary version diverged from the release manifest.
    PlatformVersionMismatch,
    /// Base container image implements a different contract version than the
    /// manifest declares.
    ContainerImageMismatch,
    /// Running orchestrator profile id/version diverged from the product.
    OrchestratorProfileMismatch,
    /// The instance was last written by a platform newer than the running one;
    /// refusing to downgrade.
    InstanceNewerThanRunner,
    /// A per-session schema version exceeds what this build can read.
    UnsupportedSessionSchema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatFinding {
    pub code: CompatCode,
    pub message: String,
}

/// The combined repo-level report plus the runtime/instance findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityMatrix {
    /// True only when the repo check passed and there are no runtime findings.
    pub ok: bool,
    pub repo: CompatibilityReport,
    pub findings: Vec<CompatFinding>,
}

/// The full conformance outcome: the compatibility matrix plus a dry-run upgrade
/// preview (absent when the instance has not been bootstrapped yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub ok: bool,
    pub matrix: CompatibilityMatrix,
    pub upgrade_preview: Option<UpgradeReport>,
}

/// Build the runtime compatibility matrix for one instance.
///
/// `platform_root` is the disk override for the platform-side inputs; `None`
/// reads them from the binary's compiled-in copy (the checkout-free default).
pub fn compatibility_matrix(
    platform_root: Option<&Path>,
    product_root: &Path,
    layout: &InstanceLayout,
    runtime: &RuntimeVersions,
) -> Result<CompatibilityMatrix, UpgradeError> {
    let (repo, platform) = match platform_root {
        Some(root) => (
            assistant_core::run_compatibility_check(root, product_root)?,
            assistant_core::compat::load_platform_manifest(root)?,
        ),
        None => (
            assistant_core::run_compatibility_check_embedded(product_root)?,
            assistant_core::compat::embedded_platform_manifest()?,
        ),
    };
    let product = assistant_core::compat::load_product_manifest(product_root)?;
    let instance = inventory(layout)?;

    let findings = evaluate_runtime(&platform, &product, &instance, runtime);
    let ok = repo.ok && findings.is_empty();
    Ok(CompatibilityMatrix {
        ok,
        repo,
        findings,
    })
}

/// Run the full conformance suite a product invokes after a version bump: the
/// repo-level check, the runtime/instance matrix, and a dry-run upgrade preview.
pub fn conformance(
    platform_root: Option<&Path>,
    product_root: &Path,
    layout: &InstanceLayout,
    runtime: &RuntimeVersions,
    central_migrations: &MigrationSet,
    version_record: &VersionRecord,
) -> Result<ConformanceReport, UpgradeError> {
    let matrix = compatibility_matrix(platform_root, product_root, layout, runtime)?;

    // Only an already-bootstrapped instance has anything to preview; previewing
    // never creates the central DB.
    let upgrade_preview = if layout.central_db_path().exists() {
        Some(upgrade_instance(
            layout,
            central_migrations,
            version_record,
            UpgradeOptions { dry_run: true },
        )?)
    } else {
        None
    };

    Ok(ConformanceReport {
        ok: matrix.ok,
        matrix,
        upgrade_preview,
    })
}

/// Pure runtime/instance comparison. No I/O; takes already-loaded manifests and
/// the instance inventory.
fn evaluate_runtime(
    platform: &PlatformManifest,
    product: &ProductManifest,
    instance: &InstanceInventory,
    runtime: &RuntimeVersions,
) -> Vec<CompatFinding> {
    let mut findings = Vec::new();

    if runtime.platform_version != platform.platform_version {
        findings.push(CompatFinding {
            code: CompatCode::PlatformVersionMismatch,
            message: format!(
                "running platform {} does not match manifest platform {}",
                runtime.platform_version, platform.platform_version
            ),
        });
    }

    if runtime.base_container_image_contract_version
        != platform.base_container_image_contract_version
    {
        findings.push(CompatFinding {
            code: CompatCode::ContainerImageMismatch,
            message: format!(
                "container image contract {} does not match manifest {}",
                runtime.base_container_image_contract_version,
                platform.base_container_image_contract_version
            ),
        });
    }

    if runtime.orchestrator_profile_id != product.orchestrator_profile_id
        || runtime.orchestrator_profile_version != product.orchestrator_profile_version
    {
        findings.push(CompatFinding {
            code: CompatCode::OrchestratorProfileMismatch,
            message: format!(
                "running profile {}:{} does not match product profile {}:{}",
                runtime.orchestrator_profile_id,
                runtime.orchestrator_profile_version,
                product.orchestrator_profile_id,
                product.orchestrator_profile_version
            ),
        });
    }

    if let Some(recorded) = &instance.recorded {
        let recorded_v = recorded.platform_version.as_str();
        let running_v = runtime.platform_version.as_str();
        match (parse_version(recorded_v), parse_version(running_v)) {
            // Both versions are comparable: refuse only a genuine downgrade,
            // i.e. an instance written by a strictly newer platform.
            (Some(rec), Some(run)) if rec > run => {
                findings.push(CompatFinding {
                    code: CompatCode::InstanceNewerThanRunner,
                    message: format!(
                        "instance was written by platform {recorded_v} which is newer than the running platform {running_v}; refusing to downgrade"
                    ),
                });
            }
            (Some(_), Some(_)) => {}
            // Either side is unparseable: the ordering cannot be established, so
            // we cannot prove this is not a downgrade. Refuse rather than risk
            // running against a newer instance with an unreadable schema.
            _ => {
                findings.push(CompatFinding {
                    code: CompatCode::InstanceNewerThanRunner,
                    message: format!(
                        "cannot compare instance platform version {recorded_v:?} with running platform {running_v:?}; refusing until the instance is verified not newer than the runner"
                    ),
                });
            }
        }
    }

    for session in &instance.sessions {
        if let Some(v) = session.inbound_version
            && v > CURRENT_INBOUND_VERSION
        {
            findings.push(CompatFinding {
                code: CompatCode::UnsupportedSessionSchema,
                message: format!(
                    "session {}/{} inbound schema v{v} exceeds supported v{CURRENT_INBOUND_VERSION}",
                    session.agent_group_id, session.session_id
                ),
            });
        }
        if let Some(v) = session.outbound_version
            && v > CURRENT_OUTBOUND_VERSION
        {
            findings.push(CompatFinding {
                code: CompatCode::UnsupportedSessionSchema,
                message: format!(
                    "session {}/{} outbound schema v{v} exceeds supported v{CURRENT_OUTBOUND_VERSION}",
                    session.agent_group_id, session.session_id
                ),
            });
        }
    }

    findings
}

/// Parse a dotted numeric version into comparable components. Returns `None` for
/// anything that is not strictly `N(.N)*`.
fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.split('.').map(|part| part.parse::<u64>().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::SessionSchema;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn platform_manifest() -> PlatformManifest {
        PlatformManifest {
            platform_id: "assistant-platform".to_string(),
            platform_version: "0.1.0".to_string(),
            runner_protocol_version: "0.1.0".to_string(),
            generated_manifest_schema_version: "0.1.0".to_string(),
            base_container_image_contract_version: "0.1.0".to_string(),
            conformance_suite_version: "0.1.0".to_string(),
            modules: Vec::new(),
        }
    }

    fn product_manifest() -> ProductManifest {
        ProductManifest {
            product_id: "testprod".to_string(),
            product_name: "Test Product".to_string(),
            product_version: "0.1.0".to_string(),
            compatible_platform_version: "0.1.0".to_string(),
            orchestrator_profile_id: "test-profile".to_string(),
            orchestrator_profile_version: "0.1.0".to_string(),
            enabled_modules: Vec::new(),
        }
    }

    fn runtime_ok() -> RuntimeVersions {
        RuntimeVersions {
            platform_version: "0.1.0".to_string(),
            base_container_image_contract_version: "0.1.0".to_string(),
            orchestrator_profile_id: "test-profile".to_string(),
            orchestrator_profile_version: "0.1.0".to_string(),
        }
    }

    fn inventory_at(platform_version: &str, sessions: Vec<SessionSchema>) -> InstanceInventory {
        InstanceInventory {
            central_present: true,
            recorded: Some(VersionRecord {
                product_id: "testprod".to_string(),
                product_version: "0.1.0".to_string(),
                platform_version: platform_version.to_string(),
                modules: Vec::new(),
            }),
            applied_central_versions: Vec::new(),
            sessions,
        }
    }

    #[test]
    fn clean_runtime_has_no_findings() {
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.1.0", Vec::new()),
            &runtime_ok(),
        );
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn container_image_mismatch_is_flagged() {
        let mut runtime = runtime_ok();
        runtime.base_container_image_contract_version = "9.9.9".to_string();
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.1.0", Vec::new()),
            &runtime,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, CompatCode::ContainerImageMismatch);
    }

    #[test]
    fn profile_mismatch_is_flagged() {
        let mut runtime = runtime_ok();
        runtime.orchestrator_profile_version = "0.2.0".to_string();
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.1.0", Vec::new()),
            &runtime,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, CompatCode::OrchestratorProfileMismatch);
    }

    #[test]
    fn instance_written_by_newer_platform_is_refused() {
        // Instance recorded platform 0.2.0, runner is 0.1.0 => downgrade refusal.
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.2.0", Vec::new()),
            &runtime_ok(),
        );
        assert!(
            findings
                .iter()
                .any(|f| f.code == CompatCode::InstanceNewerThanRunner),
            "expected downgrade refusal: {findings:?}"
        );
    }

    #[test]
    fn session_schema_beyond_current_is_unsupported() {
        let sessions = vec![SessionSchema {
            agent_group_id: "groupone".to_string(),
            session_id: "sessone".to_string(),
            inbound_version: Some(CURRENT_INBOUND_VERSION + 1),
            outbound_version: Some(CURRENT_OUTBOUND_VERSION),
        }];
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.1.0", sessions),
            &runtime_ok(),
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, CompatCode::UnsupportedSessionSchema);
    }

    #[test]
    fn version_ordering_handles_multi_digit_components() {
        // Vec<u64> components compare numerically, not lexically by digit.
        assert!(parse_version("0.10.0") > parse_version("0.9.0"));
        assert!(parse_version("0.1.0") == parse_version("0.1.0"));
        assert!(parse_version("0.1.0") < parse_version("0.2.0"));
        // Anything that is not strictly dotted-numeric is unparseable.
        assert!(parse_version("nightly").is_none());
        assert!(parse_version("0.2.0-rc1").is_none());
    }

    #[test]
    fn instance_with_unparseable_recorded_version_is_refused() {
        // A recorded version we cannot order against the runner must be refused
        // rather than silently accepted as "not newer".
        let findings = evaluate_runtime(
            &platform_manifest(),
            &product_manifest(),
            &inventory_at("0.2.0-rc1", Vec::new()),
            &runtime_ok(),
        );
        assert!(
            findings
                .iter()
                .any(|f| f.code == CompatCode::InstanceNewerThanRunner),
            "expected refusal for unparseable recorded version: {findings:?}"
        );
    }

    const PRODUCT_DESCRIPTOR: &str = "\
product_id = \"testprod\"
product_name = \"Test Product\"
product_version = \"0.1.0\"
compatible_platform_version = \"0.1.0\"
orchestrator_profile_id = \"test-profile\"
orchestrator_profile_version = \"0.1.0\"
enabled_modules = []
";

    fn platform_root() -> PathBuf {
        // .../crates/assistant-upgrade -> .../crates -> .../assistant-platform
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn current_central_set() -> MigrationSet {
        let mut set = MigrationSet::new(vec!["assistant-db".to_string()]);
        set.add(assistant_db::Migration::new(
            "assistant-db",
            1,
            "base",
            "CREATE TABLE t (id INTEGER);",
        ));
        set
    }

    fn version_record() -> VersionRecord {
        VersionRecord {
            product_id: "testprod".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            modules: vec![("assistant-db".to_string(), "0.1.0".to_string())],
        }
    }

    #[test]
    fn conformance_is_clean_on_a_fresh_instance() {
        let platform_root = platform_root();
        let tmp = TempDir::new().unwrap();

        // Minimal product fixture; manifests/contracts come from the real repo.
        let product_root = tmp.path().join("product-root");
        fs::create_dir_all(product_root.join("product")).unwrap();
        fs::write(
            product_root.join("product/descriptor.toml"),
            PRODUCT_DESCRIPTOR,
        )
        .unwrap();

        // A bootstrapped instance already at the current schema.
        let layout = InstanceLayout::derive(tmp.path(), "testns", None).unwrap();
        fs::create_dir_all(&layout.root).unwrap();
        let set = current_central_set();
        let mut conn = assistant_db::open_central(&layout.central_db_path()).unwrap();
        assistant_db::apply(&mut conn, &set).unwrap();
        assistant_db::record_versions(&conn, &version_record()).unwrap();
        drop(conn);

        let report = conformance(
            Some(&platform_root),
            &product_root,
            &layout,
            &runtime_ok(),
            &set,
            &version_record(),
        )
        .unwrap();

        assert!(
            report.matrix.repo.ok,
            "repo diagnostics: {:?}",
            report.matrix.repo.diagnostics
        );
        assert!(
            report.matrix.findings.is_empty(),
            "runtime findings: {:?}",
            report.matrix.findings
        );
        assert!(report.ok);

        let preview = report.upgrade_preview.expect("instance is bootstrapped");
        assert!(preview.dry_run);
        assert!(preview.central_applied.is_empty());
        assert!(preview.sessions.is_empty());
    }
}
