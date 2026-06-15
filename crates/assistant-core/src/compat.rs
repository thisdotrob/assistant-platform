//! Filesystem loading plus orchestration of the compatibility check.
//!
//! [`run_compatibility_check`] is what both product binaries invoke against the
//! pinned platform path: it loads the platform release manifest, prompt-fragment
//! manifest, product manifest, and module contracts, runs every validation, and
//! returns a [`CompatibilityReport`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

use crate::manifest::{
    PlatformManifest, ProductManifest, PromptFragmentManifest, parse_platform_manifest,
    parse_product_manifest, parse_prompt_fragment_manifest,
};
use crate::validate::{
    self, ContractCoverage, Diagnostic, REQUIRED_CONTRACT_SECTIONS,
};

pub const PLATFORM_MANIFEST_RELPATH: &str = "manifests/platform.toml";
pub const PROMPT_FRAGMENT_MANIFEST_RELPATH: &str = "manifests/prompt-fragments.toml";
pub const PRODUCT_MANIFEST_RELPATH: &str = "product/descriptor.toml";
pub const CONTRACTS_RELDIR: &str = "contracts";

/// The platform-side compatibility inputs compiled into the binary, so a product
/// can `doctor`/`conformance` against the pinned platform with no checkout of it
/// on disk. These mirror the on-disk files the disk-based loaders read; the
/// embedded path is the default and `--platform-path` overrides it.
const EMBEDDED_PLATFORM_MANIFEST: &str = include_str!("../../../manifests/platform.toml");
const EMBEDDED_PROMPT_FRAGMENT_MANIFEST: &str =
    include_str!("../../../manifests/prompt-fragments.toml");
static EMBEDDED_CONTRACTS: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../contracts");

/// Synthetic path label for parse errors on embedded inputs (which can only fail
/// if the compiled-in bytes are themselves malformed — a build-time bug).
fn embedded_path(relpath: &str) -> PathBuf {
    PathBuf::from("<embedded>").join(relpath)
}

#[derive(Debug)]
pub enum LoadError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            LoadError::Toml { path, source } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LoadError {}

fn read_to_string(path: &Path) -> Result<String, LoadError> {
    fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub fn load_platform_manifest(platform_root: &Path) -> Result<PlatformManifest, LoadError> {
    let path = platform_root.join(PLATFORM_MANIFEST_RELPATH);
    let raw = read_to_string(&path)?;
    parse_platform_manifest(&raw).map_err(|source| LoadError::Toml { path, source })
}

pub fn load_prompt_fragment_manifest(
    platform_root: &Path,
) -> Result<PromptFragmentManifest, LoadError> {
    let path = platform_root.join(PROMPT_FRAGMENT_MANIFEST_RELPATH);
    let raw = read_to_string(&path)?;
    parse_prompt_fragment_manifest(&raw).map_err(|source| LoadError::Toml { path, source })
}

pub fn load_product_manifest(product_root: &Path) -> Result<ProductManifest, LoadError> {
    let path = product_root.join(PRODUCT_MANIFEST_RELPATH);
    let raw = read_to_string(&path)?;
    parse_product_manifest(&raw).map_err(|source| LoadError::Toml { path, source })
}

/// Read `contracts/*.md` and record which required sections each file declares,
/// keyed by file stem (the module ID).
pub fn scan_contracts(platform_root: &Path) -> Result<BTreeMap<String, ContractCoverage>, LoadError> {
    let dir = platform_root.join(CONTRACTS_RELDIR);
    let entries = fs::read_dir(&dir).map_err(|source| LoadError::Io {
        path: dir.clone(),
        source,
    })?;

    let mut coverage = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| LoadError::Io {
            path: dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = read_to_string(&path)?;
        coverage.insert(
            stem.to_string(),
            ContractCoverage {
                present_sections: section_headers(&content),
            },
        );
    }
    Ok(coverage)
}

/// Parse the compiled-in platform manifest. Errors only on a malformed embed.
pub fn embedded_platform_manifest() -> Result<PlatformManifest, LoadError> {
    parse_platform_manifest(EMBEDDED_PLATFORM_MANIFEST).map_err(|source| LoadError::Toml {
        path: embedded_path(PLATFORM_MANIFEST_RELPATH),
        source,
    })
}

/// Parse the compiled-in prompt-fragment manifest.
pub fn embedded_prompt_fragment_manifest() -> Result<PromptFragmentManifest, LoadError> {
    parse_prompt_fragment_manifest(EMBEDDED_PROMPT_FRAGMENT_MANIFEST).map_err(|source| {
        LoadError::Toml {
            path: embedded_path(PROMPT_FRAGMENT_MANIFEST_RELPATH),
            source,
        }
    })
}

/// Read the compiled-in `contracts/*.md`, recording required sections per module,
/// keyed by file stem — the embedded twin of [`scan_contracts`].
pub fn scan_embedded_contracts() -> BTreeMap<String, ContractCoverage> {
    let mut coverage = BTreeMap::new();
    for file in EMBEDDED_CONTRACTS.files() {
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(content) = file.contents_utf8() else {
            continue;
        };
        coverage.insert(
            stem.to_string(),
            ContractCoverage {
                present_sections: section_headers(content),
            },
        );
    }
    coverage
}

/// Extract markdown header texts, restricted to the required-section names so
/// the coverage set stays small and intent-revealing.
fn section_headers(content: &str) -> BTreeSet<String> {
    let required: BTreeSet<&str> = REQUIRED_CONTRACT_SECTIONS.iter().copied().collect();
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with('#') {
                return None;
            }
            let title = trimmed.trim_start_matches('#').trim();
            required.contains(title).then(|| title.to_string())
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityReport {
    pub ok: bool,
    pub product_id: String,
    pub product_version: String,
    pub platform_id: String,
    pub platform_version: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Load every manifest under the given roots and run all PR 1 validations.
///
/// Reads the platform inputs from `platform_root` on disk. Products default to
/// [`run_compatibility_check_embedded`]; this disk path backs `--platform-path`.
pub fn run_compatibility_check(
    platform_root: &Path,
    product_root: &Path,
) -> Result<CompatibilityReport, LoadError> {
    let platform = load_platform_manifest(platform_root)?;
    let fragments = load_prompt_fragment_manifest(platform_root)?;
    let product = load_product_manifest(product_root)?;
    let contracts = scan_contracts(platform_root)?;
    Ok(build_report(platform, fragments, product, contracts))
}

/// Run the compatibility check against the compiled-in platform inputs, reading
/// only the product manifest from disk. This is the checkout-free default: a
/// product binary validates against the platform it was built against without a
/// sibling `assistant-platform` checkout.
pub fn run_compatibility_check_embedded(
    product_root: &Path,
) -> Result<CompatibilityReport, LoadError> {
    let platform = embedded_platform_manifest()?;
    let fragments = embedded_prompt_fragment_manifest()?;
    let product = load_product_manifest(product_root)?;
    let contracts = scan_embedded_contracts();
    Ok(build_report(platform, fragments, product, contracts))
}

/// Run every PR 1 validation over already-loaded manifests + contracts. Shared
/// by the disk-based and embedded entry points so they stay behaviourally identical.
fn build_report(
    platform: PlatformManifest,
    fragments: PromptFragmentManifest,
    product: ProductManifest,
    contracts: BTreeMap<String, ContractCoverage>,
) -> CompatibilityReport {
    let mut diagnostics = Vec::new();
    validate::validate_module_graph(&platform, &mut diagnostics);
    validate::validate_enabled_modules(&product, &platform, &mut diagnostics);

    let known_modules: BTreeSet<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    validate::validate_prompt_fragments(&fragments, &known_modules, &mut diagnostics);

    let module_ids: Vec<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    validate::check_contract_presence(&module_ids, &contracts, &mut diagnostics);

    validate::check_version_compatibility(&product, &platform, &mut diagnostics);

    CompatibilityReport {
        ok: diagnostics.is_empty(),
        product_id: product.product_id,
        product_version: product.product_version,
        platform_id: platform.platform_id,
        platform_version: platform.platform_version,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // .../crates/assistant-core -> .../crates -> .../assistant-platform
    fn platform_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn embedded_inputs_match_the_on_disk_files() {
        let root = platform_root();
        let disk = load_platform_manifest(&root).unwrap();
        let embedded = embedded_platform_manifest().unwrap();
        assert_eq!(disk, embedded);

        let disk_fragments = load_prompt_fragment_manifest(&root).unwrap();
        let embedded_fragments = embedded_prompt_fragment_manifest().unwrap();
        assert_eq!(disk_fragments, embedded_fragments);

        let disk_contracts = scan_contracts(&root).unwrap();
        let embedded_contracts = scan_embedded_contracts();
        assert_eq!(disk_contracts, embedded_contracts);
    }

    #[test]
    fn embedded_check_equals_disk_check_for_the_same_product() {
        let root = platform_root();
        let product_root = root.join("product");
        // The platform repo carries no product descriptor; skip when absent so
        // this stays a pure embedded-vs-disk equivalence test on real inputs.
        if !product_root.join("descriptor.toml").exists() {
            // Without a product, exercise the embedded loaders directly: they must
            // parse and cover every platform module's contract.
            let platform = embedded_platform_manifest().unwrap();
            let contracts = scan_embedded_contracts();
            for module in &platform.modules {
                assert!(
                    contracts.contains_key(&module.id),
                    "embedded contracts missing module {}",
                    module.id
                );
            }
            return;
        }
        let disk = run_compatibility_check(&root, &root).unwrap();
        let embedded = run_compatibility_check_embedded(&root).unwrap();
        assert_eq!(disk, embedded);
    }
}
