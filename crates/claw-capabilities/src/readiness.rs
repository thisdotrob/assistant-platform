//! Capability readiness checks: every enabled capability is registered, and a
//! product profile assembles cleanly. Both are pure in-memory checks over the
//! registry and config, so they take no injected probe.
//!
//! `CheckStatus` is duplicated per crate (not shared) to honor the module
//! dependency boundary, matching the other platform crates.

use claw_config::Config;
use serde::{Deserialize, Serialize};

use crate::registry::{enabled_capability_ids, CapabilityRegistry, ProfileDescriptor};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    Skipped { detail: String },
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass)
    }

    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckStatus::Fail { .. })
    }
}

/// Every capability the config enables is present in the registry — a missing
/// one means the host did not register a module it was configured to run.
pub fn enabled_capabilities_registered(config: &Config, registry: &CapabilityRegistry) -> CheckStatus {
    let missing: Vec<String> = enabled_capability_ids(config)
        .into_iter()
        .filter(|id| !registry.contains(id))
        .collect();
    if missing.is_empty() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail {
            detail: format!("enabled capabilities not registered: {}", missing.join(", ")),
        }
    }
}

/// The profile assembles into a concrete product — every capability it names is
/// registered and named at most once, so its readiness checks and fragments are
/// discoverable.
pub fn profile_assembles(registry: &CapabilityRegistry, profile: &dyn ProfileDescriptor) -> CheckStatus {
    match registry.assemble_profile(profile) {
        Ok(_) => CheckStatus::Pass,
        Err(e) => CheckStatus::Fail {
            detail: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CapabilityDescriptor;
    use claw_core::ProfileMetadata;

    struct Cap {
        id: &'static str,
    }
    impl CapabilityDescriptor for Cap {
        fn id(&self) -> &'static str {
            self.id
        }
        fn version(&self) -> &'static str {
            "0.1.0"
        }
    }

    struct Profile {
        caps: Vec<&'static str>,
    }
    impl ProfileDescriptor for Profile {
        fn metadata(&self) -> ProfileMetadata {
            ProfileMetadata { id: "p", version: "0.1.0", kind: "orchestrator" }
        }
        fn capability_ids(&self) -> Vec<&'static str> {
            self.caps.clone()
        }
    }

    #[test]
    fn profile_with_unknown_capability_fails() {
        let reg = CapabilityRegistry::new();
        assert!(profile_assembles(&reg, &Profile { caps: vec!["ghost"] }).is_blocking_failure());
    }

    #[test]
    fn profile_with_registered_capability_passes() {
        let mut reg = CapabilityRegistry::new();
        reg.register(&Cap { id: "memory" }).unwrap();
        assert!(profile_assembles(&reg, &Profile { caps: vec!["memory"] }).is_pass());
    }
}
