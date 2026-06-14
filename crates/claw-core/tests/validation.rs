//! Failure-mode coverage for the PR 1 manifest validations, using in-memory
//! fixtures so the assertions are deterministic and independent of repo state.

use std::collections::{BTreeMap, BTreeSet};

use claw_core::manifest::{
    parse_platform_manifest, parse_product_manifest, parse_prompt_fragment_manifest,
};
use claw_core::validate::{
    ContractCoverage, DiagnosticCode, REQUIRED_CONTRACT_SECTIONS, check_contract_presence,
    check_version_compatibility, validate_module_graph, validate_prompt_fragments,
};

fn codes(diags: &[claw_core::Diagnostic]) -> Vec<DiagnosticCode> {
    diags.iter().map(|d| d.code).collect()
}

fn platform(modules_toml: &str) -> claw_core::PlatformManifest {
    let src = format!(
        r#"
platform_id = "claw-platform"
platform_version = "0.1.0"
runner_protocol_version = "0.1.0"
generated_manifest_schema_version = "0.1.0"
base_container_image_contract_version = "0.1.0"
conformance_suite_version = "0.1.0"
{modules_toml}
"#
    );
    parse_platform_manifest(&src).expect("platform manifest parses")
}

fn full_coverage() -> ContractCoverage {
    ContractCoverage {
        present_sections: REQUIRED_CONTRACT_SECTIONS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

#[test]
fn clean_module_graph_passes() {
    let manifest = platform(
        r#"
[[modules]]
id = "claw-core"
[[modules]]
id = "claw-db"
depends_on = ["claw-core"]
"#,
    );
    let mut diags = Vec::new();
    validate_module_graph(&manifest, &mut diags);
    assert!(diags.is_empty(), "expected clean graph, got {diags:?}");
}

#[test]
fn duplicate_module_id_fails() {
    let manifest = platform(
        r#"
[[modules]]
id = "claw-core"
[[modules]]
id = "claw-core"
"#,
    );
    let mut diags = Vec::new();
    validate_module_graph(&manifest, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::DuplicateModuleId));
}

#[test]
fn dependency_on_unknown_module_fails() {
    let manifest = platform(
        r#"
[[modules]]
id = "claw-db"
depends_on = ["claw-core"]
"#,
    );
    let mut diags = Vec::new();
    validate_module_graph(&manifest, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::MissingModuleId));
}

#[test]
fn module_dependency_cycle_fails() {
    let manifest = platform(
        r#"
[[modules]]
id = "a"
depends_on = ["b"]
[[modules]]
id = "b"
depends_on = ["a"]
"#,
    );
    let mut diags = Vec::new();
    validate_module_graph(&manifest, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::ModuleGraphCycle));
}

#[test]
fn well_ordered_fragments_pass() {
    let manifest = parse_prompt_fragment_manifest(
        r#"
[[fragment]]
id = "safety"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 10

[[fragment]]
id = "identity"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 20
depends_on = ["safety"]
"#,
    )
    .expect("fragment manifest parses");
    let known: BTreeSet<&str> = ["claw-agent-protocol"].into_iter().collect();
    let mut diags = Vec::new();
    validate_prompt_fragments(&manifest, &known, &mut diags);
    assert!(diags.is_empty(), "expected clean fragments, got {diags:?}");
}

#[test]
fn fragment_ordered_before_its_dependency_fails() {
    let manifest = parse_prompt_fragment_manifest(
        r#"
[[fragment]]
id = "safety"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 30

[[fragment]]
id = "identity"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 20
depends_on = ["safety"]
"#,
    )
    .expect("fragment manifest parses");
    let known: BTreeSet<&str> = ["claw-agent-protocol"].into_iter().collect();
    let mut diags = Vec::new();
    validate_prompt_fragments(&manifest, &known, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::InvalidFragmentOrdering));
}

#[test]
fn fragment_with_unknown_owner_module_fails() {
    let manifest = parse_prompt_fragment_manifest(
        r#"
[[fragment]]
id = "safety"
owner_module = "claw-nonexistent"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 10
"#,
    )
    .expect("fragment manifest parses");
    let known: BTreeSet<&str> = ["claw-agent-protocol"].into_iter().collect();
    let mut diags = Vec::new();
    validate_prompt_fragments(&manifest, &known, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::UnknownFragmentOwnerModule));
}

#[test]
fn fragment_dependency_cycle_fails() {
    let manifest = parse_prompt_fragment_manifest(
        r#"
[[fragment]]
id = "a"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 10
depends_on = ["b"]

[[fragment]]
id = "b"
owner_module = "claw-agent-protocol"
version = "0.1.0"
target_agent_kind = "orchestrator"
order = 20
depends_on = ["a"]
"#,
    )
    .expect("fragment manifest parses");
    let known: BTreeSet<&str> = ["claw-agent-protocol"].into_iter().collect();
    let mut diags = Vec::new();
    validate_prompt_fragments(&manifest, &known, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::FragmentDependencyCycle));
}

#[test]
fn missing_contract_file_fails() {
    let mut contracts: BTreeMap<String, ContractCoverage> = BTreeMap::new();
    contracts.insert("claw-core".to_string(), full_coverage());
    let mut diags = Vec::new();
    check_contract_presence(&["claw-core", "claw-db"], &contracts, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::MissingContractFile));
}

#[test]
fn contract_missing_required_section_fails() {
    let mut coverage = full_coverage();
    coverage.present_sections.remove("Conformance Tests");
    let mut contracts = BTreeMap::new();
    contracts.insert("claw-core".to_string(), coverage);
    let mut diags = Vec::new();
    check_contract_presence(&["claw-core"], &contracts, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::ContractMissingSection));
}

#[test]
fn full_contract_coverage_passes() {
    let mut contracts = BTreeMap::new();
    contracts.insert("claw-core".to_string(), full_coverage());
    let mut diags = Vec::new();
    check_contract_presence(&["claw-core"], &contracts, &mut diags);
    assert!(diags.is_empty(), "expected clean coverage, got {diags:?}");
}

#[test]
fn incompatible_platform_version_fails() {
    let platform = platform(
        r#"
[[modules]]
id = "claw-core"
"#,
    );
    let product = parse_product_manifest(
        r#"
product_id = "assistant"
product_name = "Assistant"
product_version = "0.1.0"
compatible_platform_version = "0.2.0"
orchestrator_profile_id = "assistant.orchestrator"
orchestrator_profile_version = "0.1.0"
enabled_modules = ["claw-core"]
"#,
    )
    .expect("product manifest parses");
    let mut diags = Vec::new();
    check_version_compatibility(&product, &platform, &mut diags);
    assert!(codes(&diags).contains(&DiagnosticCode::IncompatiblePlatformVersion));
}

#[test]
fn matching_platform_version_passes() {
    let platform = platform(
        r#"
[[modules]]
id = "claw-core"
"#,
    );
    let product = parse_product_manifest(
        r#"
product_id = "assistant"
product_name = "Assistant"
product_version = "0.1.0"
compatible_platform_version = "0.1.0"
orchestrator_profile_id = "assistant.orchestrator"
orchestrator_profile_version = "0.1.0"
enabled_modules = ["claw-core"]
"#,
    )
    .expect("product manifest parses");
    let mut diags = Vec::new();
    check_version_compatibility(&product, &platform, &mut diags);
    assert!(diags.is_empty(), "expected compatible versions, got {diags:?}");
}
