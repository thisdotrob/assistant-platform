//! Pure validation over the manifest schemas. No filesystem or other I/O —
//! every function takes already-loaded data and appends [`Diagnostic`]s.
//!
//! This is where the PR 1 acceptance failure modes are enforced: missing module
//! IDs, duplicate module IDs, missing/under-covered contracts, invalid
//! prompt-fragment ordering, and incompatible product/platform versions.

use std::collections::{BTreeMap, BTreeSet};

use crate::manifest::{PlatformManifest, ProductManifest, PromptFragmentManifest};

/// The contract sections every module contract must cover.
pub const REQUIRED_CONTRACT_SECTIONS: &[&str] = &[
    "Public API",
    "Persistence Ownership",
    "Config",
    "Events",
    "CLI/Web Surfaces",
    "Prompt Fragments",
    "Readiness Checks",
    "Conformance Tests",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    DuplicateModuleId,
    MissingModuleId,
    ModuleGraphCycle,
    UnknownEnabledModule,
    DuplicateFragmentId,
    UnknownFragmentOwnerModule,
    UnknownFragmentDependency,
    InvalidFragmentOrdering,
    FragmentDependencyCycle,
    MissingContractFile,
    ContractMissingSection,
    IncompatiblePlatformVersion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub message: String,
}

impl Diagnostic {
    fn new(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}", self.code, self.message)
    }
}

/// Coverage extracted from a single contract file: which required sections it
/// declares.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContractCoverage {
    pub present_sections: BTreeSet<String>,
}

/// Validate the generated module graph: duplicate IDs, dependency edges that
/// reference unknown module IDs, and dependency cycles.
pub fn validate_module_graph(manifest: &PlatformManifest, diags: &mut Vec<Diagnostic>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for module in &manifest.modules {
        if !seen.insert(module.id.as_str()) {
            diags.push(Diagnostic::new(
                DiagnosticCode::DuplicateModuleId,
                format!("duplicate module id `{}`", module.id),
            ));
        }
    }

    let known: BTreeSet<&str> = manifest.modules.iter().map(|m| m.id.as_str()).collect();
    for module in &manifest.modules {
        for dep in &module.depends_on {
            if !known.contains(dep.as_str()) {
                diags.push(Diagnostic::new(
                    DiagnosticCode::MissingModuleId,
                    format!(
                        "module `{}` depends on unknown module `{}`",
                        module.id, dep
                    ),
                ));
            }
        }
    }

    let edges: BTreeMap<&str, Vec<&str>> = manifest
        .modules
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                m.depends_on
                    .iter()
                    .map(String::as_str)
                    .filter(|d| known.contains(d))
                    .collect(),
            )
        })
        .collect();
    if let Some(node) = first_cycle_node(&edges) {
        diags.push(Diagnostic::new(
            DiagnosticCode::ModuleGraphCycle,
            format!("module dependency cycle reachable from `{node}`"),
        ));
    }
}

/// Validate that every module a product enables exists in the platform graph.
pub fn validate_enabled_modules(
    product: &ProductManifest,
    platform: &PlatformManifest,
    diags: &mut Vec<Diagnostic>,
) {
    let known: BTreeSet<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    for module in &product.enabled_modules {
        if !known.contains(module.as_str()) {
            diags.push(Diagnostic::new(
                DiagnosticCode::UnknownEnabledModule,
                format!(
                    "product `{}` enables unknown module `{}`",
                    product.product_id, module
                ),
            ));
        }
    }
}

/// Validate prompt fragments: duplicate IDs, unknown owner modules, unknown
/// dependency IDs, ordering consistency (a fragment must order strictly after
/// its dependencies), and dependency cycles.
pub fn validate_prompt_fragments(
    manifest: &PromptFragmentManifest,
    known_modules: &BTreeSet<&str>,
    diags: &mut Vec<Diagnostic>,
) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for fragment in &manifest.fragments {
        if !seen.insert(fragment.id.as_str()) {
            diags.push(Diagnostic::new(
                DiagnosticCode::DuplicateFragmentId,
                format!("duplicate prompt fragment id `{}`", fragment.id),
            ));
        }
    }

    let order_by_id: BTreeMap<&str, u32> = manifest
        .fragments
        .iter()
        .map(|f| (f.id.as_str(), f.order))
        .collect();

    for fragment in &manifest.fragments {
        if !known_modules.contains(fragment.owner_module.as_str()) {
            diags.push(Diagnostic::new(
                DiagnosticCode::UnknownFragmentOwnerModule,
                format!(
                    "prompt fragment `{}` is owned by unknown module `{}`",
                    fragment.id, fragment.owner_module
                ),
            ));
        }

        for dep in &fragment.depends_on {
            match order_by_id.get(dep.as_str()) {
                None => diags.push(Diagnostic::new(
                    DiagnosticCode::UnknownFragmentDependency,
                    format!(
                        "prompt fragment `{}` depends on unknown fragment `{}`",
                        fragment.id, dep
                    ),
                )),
                Some(&dep_order) if dep_order >= fragment.order => diags.push(Diagnostic::new(
                    DiagnosticCode::InvalidFragmentOrdering,
                    format!(
                        "prompt fragment `{}` (order {}) must render after dependency `{}` (order {})",
                        fragment.id, fragment.order, dep, dep_order
                    ),
                )),
                Some(_) => {}
            }
        }
    }

    let edges: BTreeMap<&str, Vec<&str>> = manifest
        .fragments
        .iter()
        .map(|f| {
            (
                f.id.as_str(),
                f.depends_on
                    .iter()
                    .map(String::as_str)
                    .filter(|d| order_by_id.contains_key(d))
                    .collect(),
            )
        })
        .collect();
    if let Some(node) = first_cycle_node(&edges) {
        diags.push(Diagnostic::new(
            DiagnosticCode::FragmentDependencyCycle,
            format!("prompt fragment dependency cycle reachable from `{node}`"),
        ));
    }
}

/// Validate that every module in the graph has a contract file and that each
/// contract covers all required sections.
pub fn check_contract_presence(
    module_ids: &[&str],
    contracts: &BTreeMap<String, ContractCoverage>,
    diags: &mut Vec<Diagnostic>,
) {
    for &module_id in module_ids {
        match contracts.get(module_id) {
            None => diags.push(Diagnostic::new(
                DiagnosticCode::MissingContractFile,
                format!("module `{module_id}` has no contract file"),
            )),
            Some(coverage) => {
                for section in REQUIRED_CONTRACT_SECTIONS {
                    if !coverage.present_sections.contains(*section) {
                        diags.push(Diagnostic::new(
                            DiagnosticCode::ContractMissingSection,
                            format!(
                                "contract for `{module_id}` is missing required section `{section}`"
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// Validate that the product was built against the current platform version.
///
/// The first release uses a single coordinated platform version, so the
/// product's pinned `compatible_platform_version` must match exactly.
pub fn check_version_compatibility(
    product: &ProductManifest,
    platform: &PlatformManifest,
    diags: &mut Vec<Diagnostic>,
) {
    if product.compatible_platform_version != platform.platform_version {
        diags.push(Diagnostic::new(
            DiagnosticCode::IncompatiblePlatformVersion,
            format!(
                "product `{}` pins platform {} but platform is {}",
                product.product_id, product.compatible_platform_version, platform.platform_version
            ),
        ));
    }
}

/// Return the first node from which a directed cycle is reachable, if any.
fn first_cycle_node<'a>(edges: &BTreeMap<&'a str, Vec<&'a str>>) -> Option<&'a str> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit<'a>(
        node: &'a str,
        edges: &BTreeMap<&'a str, Vec<&'a str>>,
        marks: &mut BTreeMap<&'a str, Mark>,
    ) -> bool {
        marks.insert(node, Mark::Visiting);
        if let Some(neighbours) = edges.get(node) {
            for &next in neighbours {
                match marks.get(next) {
                    Some(Mark::Visiting) => return true,
                    Some(Mark::Done) => {}
                    None => {
                        if visit(next, edges, marks) {
                            return true;
                        }
                    }
                }
            }
        }
        marks.insert(node, Mark::Done);
        false
    }

    let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
    edges
        .keys()
        .copied()
        .find(|&node| !marks.contains_key(node) && visit(node, edges, &mut marks))
}
