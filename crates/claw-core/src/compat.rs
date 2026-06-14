//! Filesystem loading plus orchestration of the compatibility check.
//!
//! [`run_compatibility_check`] is what both product binaries invoke against the
//! pinned platform path: it loads the platform release manifest, prompt-fragment
//! manifest, product manifest, and module contracts, runs every validation, and
//! returns a [`CompatibilityReport`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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
pub fn run_compatibility_check(
    platform_root: &Path,
    product_root: &Path,
) -> Result<CompatibilityReport, LoadError> {
    let platform = load_platform_manifest(platform_root)?;
    let fragments = load_prompt_fragment_manifest(platform_root)?;
    let product = load_product_manifest(product_root)?;
    let contracts = scan_contracts(platform_root)?;

    let mut diagnostics = Vec::new();
    validate::validate_module_graph(&platform, &mut diagnostics);
    validate::validate_enabled_modules(&product, &platform, &mut diagnostics);

    let known_modules: BTreeSet<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    validate::validate_prompt_fragments(&fragments, &known_modules, &mut diagnostics);

    let module_ids: Vec<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    validate::check_contract_presence(&module_ids, &contracts, &mut diagnostics);

    validate::check_version_compatibility(&product, &platform, &mut diagnostics);

    Ok(CompatibilityReport {
        ok: diagnostics.is_empty(),
        product_id: product.product_id,
        product_version: product.product_version,
        platform_id: platform.platform_id,
        platform_version: platform.platform_version,
        diagnostics,
    })
}
