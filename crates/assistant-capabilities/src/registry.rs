//! Capability/profile descriptors, the in-memory registry, and product
//! assembly.
//!
//! A capability module declares its identity, the readiness checks it requires,
//! and the prompt fragments it owns. A profile is composed of capabilities. The
//! registry collects registered capabilities and assembles a profile into a
//! concrete product: it rejects unknown or duplicate capability IDs and gathers
//! the union of declared readiness checks and fragments. The enabled selection
//! comes from assistant-config; capability modules never touch the central DB here,
//! so the registry holds no DB handle.

use std::collections::BTreeMap;

use assistant_config::Config;
use assistant_core::ProfileMetadata;

/// A capability module: a versioned unit that declares the readiness checks it
/// needs and the prompt fragments it owns. New methods carry defaults so
/// existing capability modules need not change.
pub trait CapabilityDescriptor {
    fn id(&self) -> &'static str;
    fn version(&self) -> &'static str;

    /// The module that owns this capability. Defaults to the capability id for
    /// modules that are their own owner.
    fn module_id(&self) -> &'static str {
        self.id()
    }

    /// Readiness check ids this capability requires be run.
    fn readiness_check_ids(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Prompt-fragment ids this capability owns.
    fn prompt_fragment_ids(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// A product profile composed of capabilities.
pub trait ProfileDescriptor {
    fn metadata(&self) -> ProfileMetadata;

    /// The capability ids this profile is composed of. Defaults to empty for
    /// profiles that are themselves a single capability.
    fn capability_ids(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// A capability as recorded in the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredCapability {
    pub id: String,
    pub version: String,
    pub module_id: String,
    pub readiness_check_ids: Vec<String>,
    pub prompt_fragment_ids: Vec<String>,
}

/// The concrete result of assembling a profile from registered capabilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembledProduct {
    pub profile_id: String,
    pub profile_version: String,
    pub capability_ids: Vec<String>,
    /// Union of the readiness checks declared by the assembled capabilities,
    /// in first-seen order — the discoverable set the host must run.
    pub readiness_check_ids: Vec<String>,
    /// Union of the prompt-fragment ids declared by the assembled capabilities.
    pub prompt_fragment_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CapabilityError {
    /// A capability id was registered more than once.
    DuplicateCapability { id: String },
    /// A profile referenced a capability id more than once.
    DuplicateInProfile { id: String },
    /// A profile referenced a capability that is not registered.
    UnknownCapability { id: String },
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityError::DuplicateCapability { id } => {
                write!(f, "capability {id:?} is already registered")
            }
            CapabilityError::DuplicateInProfile { id } => {
                write!(f, "profile references capability {id:?} more than once")
            }
            CapabilityError::UnknownCapability { id } => {
                write!(f, "profile references unregistered capability {id:?}")
            }
        }
    }
}

impl std::error::Error for CapabilityError {}

/// An in-memory registry of capabilities, keyed by id.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    capabilities: BTreeMap<String, RegisteredCapability>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capability. Rejects a duplicate id so two modules cannot claim
    /// the same capability.
    pub fn register(
        &mut self,
        descriptor: &dyn CapabilityDescriptor,
    ) -> Result<(), CapabilityError> {
        let id = descriptor.id().to_string();
        if self.capabilities.contains_key(&id) {
            return Err(CapabilityError::DuplicateCapability { id });
        }
        self.capabilities.insert(
            id.clone(),
            RegisteredCapability {
                id,
                version: descriptor.version().to_string(),
                module_id: descriptor.module_id().to_string(),
                readiness_check_ids: descriptor
                    .readiness_check_ids()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                prompt_fragment_ids: descriptor
                    .prompt_fragment_ids()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        );
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&RegisteredCapability> {
        self.capabilities.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.capabilities.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Assemble a profile into a concrete product. Rejects a capability the
    /// profile names twice and any capability not registered; gathers the union
    /// of declared readiness checks and fragments in first-seen order.
    pub fn assemble_profile(
        &self,
        profile: &dyn ProfileDescriptor,
    ) -> Result<AssembledProduct, CapabilityError> {
        let meta = profile.metadata();
        let mut capability_ids: Vec<String> = Vec::new();
        let mut readiness_check_ids: Vec<String> = Vec::new();
        let mut prompt_fragment_ids: Vec<String> = Vec::new();

        for raw in profile.capability_ids() {
            let id = raw.to_string();
            if capability_ids.contains(&id) {
                return Err(CapabilityError::DuplicateInProfile { id });
            }
            let cap = self
                .get(&id)
                .ok_or(CapabilityError::UnknownCapability { id: id.clone() })?;
            for check in &cap.readiness_check_ids {
                if !readiness_check_ids.contains(check) {
                    readiness_check_ids.push(check.clone());
                }
            }
            for fragment in &cap.prompt_fragment_ids {
                if !prompt_fragment_ids.contains(fragment) {
                    prompt_fragment_ids.push(fragment.clone());
                }
            }
            capability_ids.push(id);
        }

        Ok(AssembledProduct {
            profile_id: meta.id.to_string(),
            profile_version: meta.version.to_string(),
            capability_ids,
            readiness_check_ids,
            prompt_fragment_ids,
        })
    }
}

/// The enabled-capability selection from config. The host registers capabilities
/// it wants based on this list.
pub fn enabled_capability_ids(config: &Config) -> Vec<String> {
    config.modules.enabled.clone()
}

/// Whether a capability id is in the config's enabled selection.
pub fn is_capability_enabled(config: &Config, id: &str) -> bool {
    config.modules.enabled.iter().any(|e| e == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cap {
        id: &'static str,
        checks: Vec<&'static str>,
        fragments: Vec<&'static str>,
    }
    impl CapabilityDescriptor for Cap {
        fn id(&self) -> &'static str {
            self.id
        }
        fn version(&self) -> &'static str {
            "0.1.0"
        }
        fn readiness_check_ids(&self) -> Vec<&'static str> {
            self.checks.clone()
        }
        fn prompt_fragment_ids(&self) -> Vec<&'static str> {
            self.fragments.clone()
        }
    }

    struct Profile {
        caps: Vec<&'static str>,
    }
    impl ProfileDescriptor for Profile {
        fn metadata(&self) -> ProfileMetadata {
            ProfileMetadata {
                id: "test-profile",
                version: "0.1.0",
                kind: "orchestrator",
            }
        }
        fn capability_ids(&self) -> Vec<&'static str> {
            self.caps.clone()
        }
    }

    #[test]
    fn register_rejects_duplicate_capability_id() {
        let mut reg = CapabilityRegistry::new();
        let cap = Cap { id: "memory", checks: vec![], fragments: vec![] };
        reg.register(&cap).unwrap();
        assert_eq!(
            reg.register(&cap),
            Err(CapabilityError::DuplicateCapability { id: "memory".into() })
        );
    }

    #[test]
    fn assembly_gathers_readiness_and_fragments_and_dedups() {
        let mut reg = CapabilityRegistry::new();
        reg.register(&Cap {
            id: "memory",
            checks: vec!["mem-index-fresh"],
            fragments: vec!["rag-injection"],
        })
        .unwrap();
        reg.register(&Cap {
            id: "scheduler",
            checks: vec!["sweep-running", "mem-index-fresh"],
            fragments: vec!["scheduling-wording"],
        })
        .unwrap();

        let product = reg
            .assemble_profile(&Profile { caps: vec!["memory", "scheduler"] })
            .unwrap();
        assert_eq!(product.capability_ids, vec!["memory", "scheduler"]);
        // mem-index-fresh appears once despite both capabilities declaring it.
        assert_eq!(
            product.readiness_check_ids,
            vec!["mem-index-fresh", "sweep-running"]
        );
        assert_eq!(
            product.prompt_fragment_ids,
            vec!["rag-injection", "scheduling-wording"]
        );
    }

    #[test]
    fn assembly_rejects_unknown_capability() {
        let reg = CapabilityRegistry::new();
        assert_eq!(
            reg.assemble_profile(&Profile { caps: vec!["ghost"] }),
            Err(CapabilityError::UnknownCapability { id: "ghost".into() })
        );
    }

    #[test]
    fn assembly_rejects_duplicate_in_profile() {
        let mut reg = CapabilityRegistry::new();
        reg.register(&Cap { id: "memory", checks: vec![], fragments: vec![] }).unwrap();
        assert_eq!(
            reg.assemble_profile(&Profile { caps: vec!["memory", "memory"] }),
            Err(CapabilityError::DuplicateInProfile { id: "memory".into() })
        );
    }
}
