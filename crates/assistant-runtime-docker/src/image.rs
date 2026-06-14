//! Docker image naming and the `assistant-base` image contract.
//!
//! Product images layer on a shared `assistant-base` image; this module names
//! and references those images. It does not build images — building happens in
//! product CI/setup — but it pins the base image contract version and the base
//! runtime so the host and setup agree on what "the base image" is.

use serde::{Deserialize, Serialize};

/// The repository name of the shared base image all agent runners layer on.
pub const BASE_IMAGE_REPOSITORY: &str = "assistant-base";

/// The base runtime the Claude Agent SDK runs inside, confirmed by the
/// 2026-06-01 auth spike.
pub const BASE_IMAGE_RUNTIME: &str = "node:22-slim";

/// A fully qualified image reference: `repository:tag`, or pinned by digest as
/// `repository@sha256:...` when a digest is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub repository: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn new(repository: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            tag: tag.into(),
            digest: None,
        }
    }

    pub fn pinned(
        repository: impl Into<String>,
        tag: impl Into<String>,
        digest: impl Into<String>,
    ) -> Self {
        Self {
            repository: repository.into(),
            tag: tag.into(),
            digest: Some(digest.into()),
        }
    }

    /// The reference string passed to `docker run`. A digest, when present,
    /// pins the exact image and takes precedence over the tag.
    pub fn reference(&self) -> String {
        match &self.digest {
            Some(digest) => format!("{}@{}", self.repository, digest),
            None => format!("{}:{}", self.repository, self.tag),
        }
    }

    pub fn is_pinned(&self) -> bool {
        self.digest.is_some()
    }
}

/// The base image contract: its reference plus the contract version that moves
/// with the coordinated platform release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseImageContract {
    pub image: ImageRef,
    pub runtime: String,
    pub contract_version: String,
}

impl BaseImageContract {
    /// The base image for a given platform version and contract version.
    pub fn for_platform(platform_version: &str, contract_version: &str) -> Self {
        Self {
            image: ImageRef::new(BASE_IMAGE_REPOSITORY, platform_version),
            runtime: BASE_IMAGE_RUNTIME.to_string(),
            contract_version: contract_version.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_reference_when_unpinned() {
        let image = ImageRef::new("assistant-base", "0.1.0");
        assert_eq!(image.reference(), "assistant-base:0.1.0");
        assert!(!image.is_pinned());
    }

    #[test]
    fn digest_takes_precedence_when_pinned() {
        let image = ImageRef::pinned("assistant-base", "0.1.0", "sha256:abc123");
        assert_eq!(image.reference(), "assistant-base@sha256:abc123");
        assert!(image.is_pinned());
    }

    #[test]
    fn base_contract_uses_platform_version_as_tag() {
        let contract = BaseImageContract::for_platform("0.1.0", "0.1.0");
        assert_eq!(contract.image.reference(), "assistant-base:0.1.0");
        assert_eq!(contract.runtime, "node:22-slim");
    }

    #[test]
    fn image_ref_round_trips_json() {
        let image = ImageRef::pinned("r", "t", "sha256:d");
        let json = serde_json::to_string(&image).unwrap();
        let back: ImageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(image, back);
    }
}
