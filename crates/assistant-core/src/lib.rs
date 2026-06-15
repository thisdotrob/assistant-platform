pub mod compat;
pub mod manifest;
pub mod validate;

pub use compat::{
    CompatibilityReport, LoadError, run_compatibility_check, run_compatibility_check_embedded,
};
pub use manifest::{PlatformManifest, ProductManifest, PromptFragment, PromptFragmentManifest};
pub use validate::{Diagnostic, DiagnosticCode};

pub const PLATFORM_ID: &str = "assistant-platform";
pub const PLATFORM_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const PLATFORM_MODULES: &[&str] = &[
    "assistant-core",
    "assistant-config",
    "assistant-db",
    "assistant-session",
    "assistant-router",
    "assistant-agent-protocol",
    "assistant-runtime-docker",
    "assistant-memory",
    "assistant-scheduler",
    "assistant-agent-graph",
    "assistant-permissions",
    "assistant-approvals",
    "assistant-cli",
    "assistant-web",
    "assistant-setup",
    "assistant-upgrade",
    "assistant-capabilities",
    "assistant-channel-cli",
    "assistant-channel-slack",
    "assistant-channel-telegram",
    "assistant-host",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformMetadata {
    pub id: &'static str,
    pub version: &'static str,
    pub module_ids: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductMetadata {
    pub id: &'static str,
    pub version: &'static str,
    pub compatible_platform_version: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileMetadata {
    pub id: &'static str,
    pub version: &'static str,
    pub kind: &'static str,
}

pub fn platform_metadata() -> PlatformMetadata {
    PlatformMetadata {
        id: PLATFORM_ID,
        version: PLATFORM_VERSION,
        module_ids: PLATFORM_MODULES,
    }
}

