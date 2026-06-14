//! Capability and profile composition for the platform.
//!
//! A capability module declares its identity, the readiness checks it requires,
//! and the prompt fragments it owns. A profile is composed of capabilities. The
//! registry collects registered capabilities and assembles a profile into a
//! concrete product. The enabled selection comes from assistant-config; capability
//! modules never touch the central DB here, so the registry holds no DB handle.

pub mod event;
pub mod readiness;
pub mod registry;

pub use event::{CapabilityEvent, CapabilityEventSink, VecEventSink};
pub use readiness::{enabled_capabilities_registered, profile_assembles, CheckStatus};
pub use registry::{
    enabled_capability_ids, is_capability_enabled, AssembledProduct, CapabilityDescriptor,
    CapabilityError, CapabilityRegistry, ProfileDescriptor, RegisteredCapability,
};

pub const MODULE_ID: &str = "assistant-capabilities";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
