//! Typed schemas for the platform release manifest, product manifest, and
//! prompt-fragment manifest, plus TOML parsing.
//!
//! These types are the executable form of the module-boundary contracts: they
//! are consumed by the compatibility check, by setup, and by both product
//! binaries. They own no database tables.

use serde::Deserialize;

/// The coordinated platform release manifest (`manifests/platform.toml`).
///
/// Carries the single coordinated platform version plus the protocol/schema/
/// image/conformance versions that move with it, and the generated module
/// graph used for validation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PlatformManifest {
    pub platform_id: String,
    pub platform_version: String,
    pub runner_protocol_version: String,
    pub generated_manifest_schema_version: String,
    pub base_container_image_contract_version: String,
    pub conformance_suite_version: String,
    #[serde(default)]
    pub modules: Vec<ModuleNode>,
}

/// One node in the generated module graph: a module ID plus the IDs of the
/// modules it depends on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ModuleNode {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// The product manifest (`product/descriptor.toml`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ProductManifest {
    pub product_id: String,
    pub product_name: String,
    pub product_version: String,
    pub compatible_platform_version: String,
    pub orchestrator_profile_id: String,
    pub orchestrator_profile_version: String,
    #[serde(default)]
    pub enabled_modules: Vec<String>,
}

/// The prompt-fragment manifest (`manifests/prompt-fragments.toml`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct PromptFragmentManifest {
    #[serde(default, rename = "fragment")]
    pub fragments: Vec<PromptFragment>,
}

/// A single prompt fragment declaration.
///
/// `order` plus `depends_on` express ordering: a fragment must render after
/// every fragment it depends on, so its `order` must be strictly greater than
/// each dependency's `order`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PromptFragment {
    pub id: String,
    pub owner_module: String,
    pub version: String,
    pub target_agent_kind: String,
    #[serde(default)]
    pub parameters: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub order: u32,
    #[serde(default)]
    pub override_rules: String,
    #[serde(default)]
    pub conformance_assertions: Vec<String>,
}

pub fn parse_platform_manifest(input: &str) -> Result<PlatformManifest, toml::de::Error> {
    toml::from_str(input)
}

pub fn parse_product_manifest(input: &str) -> Result<ProductManifest, toml::de::Error> {
    toml::from_str(input)
}

pub fn parse_prompt_fragment_manifest(
    input: &str,
) -> Result<PromptFragmentManifest, toml::de::Error> {
    toml::from_str(input)
}
