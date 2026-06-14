//! The predefined specialist registry and creation/admission policy.
//!
//! Specialists may only be created from profiles registered ahead of time — the
//! freeform `create_agent` of v1 is replaced by a constrained registry. Profiles
//! are supplied to this crate as plain data by the host (capability modules like
//! `assistant-specialist-browser` own the real profile; the host translates its
//! identity and limits into a [`RegisteredProfile`]). This keeps the agent graph
//! free of any dependency on concrete specialist crates.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-profile limits the host enforces before creating specialists or starting
/// jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLimits {
    pub max_specialists: u32,
    pub max_concurrent_jobs: u32,
}

impl ProfileLimits {
    pub fn new(max_specialists: u32, max_concurrent_jobs: u32) -> Self {
        Self {
            max_specialists,
            max_concurrent_jobs,
        }
    }
}

/// A profile the host has approved for specialist creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredProfile {
    pub profile_id: String,
    pub profile_version: String,
    pub kind: String,
    /// Whether agents of this profile may own external channel destinations.
    /// Browser (and other internal specialists) must be false.
    pub allows_external_destinations: bool,
    pub limits: ProfileLimits,
}

impl RegisteredProfile {
    /// A specialist profile with no external destinations (the common case).
    pub fn specialist(
        profile_id: impl Into<String>,
        profile_version: impl Into<String>,
        limits: ProfileLimits,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            profile_version: profile_version.into(),
            kind: "specialist".to_string(),
            allows_external_destinations: false,
            limits,
        }
    }

    pub fn is_specialist(&self) -> bool {
        self.kind == "specialist"
    }
}

/// The set of profiles a host has approved.
#[derive(Clone, Debug, Default)]
pub struct SpecialistRegistry {
    profiles: BTreeMap<String, RegisteredProfile>,
}

impl SpecialistRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, profile: RegisteredProfile) -> &mut Self {
        self.profiles.insert(profile.profile_id.clone(), profile);
        self
    }

    pub fn get(&self, profile_id: &str) -> Option<&RegisteredProfile> {
        self.profiles.get(profile_id)
    }

    pub fn is_registered(&self, profile_id: &str) -> bool {
        self.profiles.contains_key(profile_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RegisteredProfile> {
        self.profiles.values()
    }
}

/// Why a creation or admission request was refused by policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    UnknownProfile { profile_id: String },
    NotASpecialist { profile_id: String },
    SpecialistLimitReached { profile_id: String, max: u32 },
    ConcurrencyLimitReached { profile_id: String, max: u32 },
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::UnknownProfile { profile_id } => {
                write!(f, "profile {profile_id:?} is not in the specialist registry")
            }
            PolicyError::NotASpecialist { profile_id } => {
                write!(f, "profile {profile_id:?} is not a specialist profile")
            }
            PolicyError::SpecialistLimitReached { profile_id, max } => {
                write!(f, "profile {profile_id:?} already has the maximum {max} specialists")
            }
            PolicyError::ConcurrencyLimitReached { profile_id, max } => {
                write!(f, "profile {profile_id:?} already has the maximum {max} concurrent jobs")
            }
        }
    }
}

impl std::error::Error for PolicyError {}

/// Authorize creating a new specialist from a profile. Rejects unknown or
/// non-specialist profiles, and refuses once the per-profile specialist limit
/// is reached.
pub fn authorize_create<'r>(
    registry: &'r SpecialistRegistry,
    profile_id: &str,
    existing_specialists: u32,
) -> Result<&'r RegisteredProfile, PolicyError> {
    let profile = registry.get(profile_id).ok_or_else(|| PolicyError::UnknownProfile {
        profile_id: profile_id.to_string(),
    })?;
    if !profile.is_specialist() {
        return Err(PolicyError::NotASpecialist {
            profile_id: profile_id.to_string(),
        });
    }
    if existing_specialists >= profile.limits.max_specialists {
        return Err(PolicyError::SpecialistLimitReached {
            profile_id: profile_id.to_string(),
            max: profile.limits.max_specialists,
        });
    }
    Ok(profile)
}

/// Authorize starting a new job for a profile, given how many of its jobs are
/// already in flight.
pub fn authorize_job_start(profile: &RegisteredProfile, in_flight_jobs: u32) -> Result<(), PolicyError> {
    if in_flight_jobs >= profile.limits.max_concurrent_jobs {
        return Err(PolicyError::ConcurrencyLimitReached {
            profile_id: profile.profile_id.clone(),
            max: profile.limits.max_concurrent_jobs,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser_registry() -> SpecialistRegistry {
        let mut r = SpecialistRegistry::new();
        r.register(RegisteredProfile::specialist(
            "browser-specialist",
            "0.1.0",
            ProfileLimits::new(1, 2),
        ));
        r
    }

    #[test]
    fn can_create_registered_specialist() {
        let r = browser_registry();
        let p = authorize_create(&r, "browser-specialist", 0).unwrap();
        assert_eq!(p.profile_id, "browser-specialist");
        assert!(!p.allows_external_destinations);
    }

    #[test]
    fn cannot_create_unknown_profile() {
        let r = browser_registry();
        assert_eq!(
            authorize_create(&r, "made-up", 0),
            Err(PolicyError::UnknownProfile { profile_id: "made-up".into() })
        );
    }

    #[test]
    fn cannot_create_non_specialist_profile() {
        let mut r = SpecialistRegistry::new();
        r.register(RegisteredProfile {
            profile_id: "personal-orchestrator".into(),
            profile_version: "0.1.0".into(),
            kind: "orchestrator".into(),
            allows_external_destinations: true,
            limits: ProfileLimits::new(1, 1),
        });
        assert_eq!(
            authorize_create(&r, "personal-orchestrator", 0),
            Err(PolicyError::NotASpecialist { profile_id: "personal-orchestrator".into() })
        );
    }

    #[test]
    fn specialist_count_limit_is_enforced() {
        let r = browser_registry();
        // max_specialists == 1
        assert!(authorize_create(&r, "browser-specialist", 0).is_ok());
        assert_eq!(
            authorize_create(&r, "browser-specialist", 1),
            Err(PolicyError::SpecialistLimitReached { profile_id: "browser-specialist".into(), max: 1 })
        );
    }

    #[test]
    fn concurrency_limit_is_enforced() {
        let r = browser_registry();
        let p = r.get("browser-specialist").unwrap();
        // max_concurrent_jobs == 2
        assert!(authorize_job_start(p, 0).is_ok());
        assert!(authorize_job_start(p, 1).is_ok());
        assert_eq!(
            authorize_job_start(p, 2),
            Err(PolicyError::ConcurrencyLimitReached { profile_id: "browser-specialist".into(), max: 2 })
        );
    }
}
