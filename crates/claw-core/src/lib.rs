pub mod compat;
pub mod manifest;
pub mod validate;

pub use compat::{CompatibilityReport, LoadError, run_compatibility_check};
pub use manifest::{PlatformManifest, ProductManifest, PromptFragment, PromptFragmentManifest};
pub use validate::{Diagnostic, DiagnosticCode};

pub const PLATFORM_ID: &str = "assistant-platform";
pub const PLATFORM_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const PLATFORM_MODULES: &[&str] = &[
    "claw-core",
    "claw-config",
    "claw-db",
    "claw-session",
    "claw-router",
    "claw-agent-protocol",
    "claw-runtime-docker",
    "claw-memory",
    "claw-scheduler",
    "claw-agent-graph",
    "claw-permissions",
    "claw-approvals",
    "claw-cli",
    "claw-web",
    "claw-setup",
    "claw-upgrade",
    "claw-capabilities",
    "claw-channel-cli",
    "claw-channel-slack",
    "claw-channel-telegram",
    "claw-host",
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

