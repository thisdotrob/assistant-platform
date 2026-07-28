//! Config-driven specialist registration: the `[[specialists]]` schema and the
//! loader that turns each entry into a [`SpecialistSpec`].
//!
//! A product compiles in a baseline set of specialists; this lets an instance
//! add more by referencing a bundle file (a serialized `SpecialistSpec` the
//! specialist's build publishes alongside its image) plus a few operational
//! overrides. The security-bearing fields — the system prompt and the
//! `allowed_tools` auto-approve patterns — come from the reviewed bundle, never
//! from hand-authored config; overrides are limited to capacity/pinning knobs.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use assistant_specialist_spec::SpecialistSpec;

use crate::config::ConfigError;

/// One `[[specialists]]` table: a bundle reference plus operator-owned overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistEntry {
    /// Path (relative to the instance's `specialists/` dir) of the bundle file —
    /// the serialized [`SpecialistSpec`] to register. Absolute paths and `..`
    /// components are rejected so config can't read outside that dir.
    pub bundle: PathBuf,
    /// Whether this entry is registered. Absent means enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Operational overrides applied on top of the bundle. Capacity/pinning only:
    /// nothing here can widen what the specialist may execute.
    #[serde(default)]
    pub overrides: SpecialistOverrides,
}

fn default_true() -> bool {
    true
}

/// Operator-owned overrides. Deliberately excludes `system_prompt`, `tools`, and
/// `allowed_tools`: those are the specialist's security contract and stay with
/// the reviewed bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistOverrides {
    /// Pin the image to an exact digest (overriding the bundle's, if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_specialists: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_artifact_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

impl SpecialistOverrides {
    fn apply(&self, spec: &mut SpecialistSpec) {
        if let Some(digest) = &self.image_digest {
            spec.image_digest = Some(digest.clone());
        }
        if let Some(n) = self.max_specialists {
            spec.max_specialists = n;
        }
        if let Some(n) = self.max_concurrent_jobs {
            spec.max_concurrent_jobs = n;
        }
        if let Some(n) = self.max_artifact_bytes {
            spec.max_artifact_bytes = n;
        }
        if let Some(n) = self.max_turns {
            spec.max_turns = n;
        }
    }
}

/// Resolve each enabled entry into a [`SpecialistSpec`]: read the bundle under
/// `dir`, apply overrides, and validate the image reference. The host merges the
/// result with its compiled specialists.
pub fn resolve_specialists(
    entries: &[SpecialistEntry],
    dir: &Path,
) -> Result<Vec<SpecialistSpec>, ConfigError> {
    let mut out = Vec::new();
    for entry in entries {
        if !entry.enabled {
            continue;
        }
        let path = resolve_bundle_path(dir, &entry.bundle)?;
        let text = std::fs::read_to_string(&path).map_err(|source| {
            ConfigError::SpecialistBundleIo {
                path: path.clone(),
                source,
            }
        })?;
        let mut spec: SpecialistSpec =
            serde_json::from_str(&text).map_err(|source| ConfigError::SpecialistBundleParse {
                path: path.clone(),
                source,
            })?;
        entry.overrides.apply(&mut spec);
        validate_image_ref(&spec, &entry.bundle)?;
        out.push(spec);
    }
    Ok(out)
}

/// Join `bundle` under `dir`, rejecting absolute paths and any `..` traversal so
/// a bundle reference can never escape the instance's `specialists/` dir.
fn resolve_bundle_path(dir: &Path, bundle: &Path) -> Result<PathBuf, ConfigError> {
    let escapes = bundle.is_absolute()
        || bundle
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)));
    if escapes {
        return Err(ConfigError::SpecialistBundlePathEscape {
            bundle: bundle.display().to_string(),
        });
    }
    Ok(dir.join(bundle))
}

/// A registrable spec must name an image the host can pull: a repository plus a
/// tag or a digest. Config can reference an image but cannot produce one, so a
/// dangling reference fails at load rather than at first delegation.
fn validate_image_ref(spec: &SpecialistSpec, bundle: &Path) -> Result<(), ConfigError> {
    let has_ref = !spec.image_repository.trim().is_empty()
        && (!spec.image_tag.trim().is_empty() || spec.image_digest.is_some());
    if has_ref {
        Ok(())
    } else {
        Err(ConfigError::SpecialistMissingImageRef {
            bundle: bundle.display().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> SpecialistSpec {
        SpecialistSpec {
            route_name: "browser".to_string(),
            description: "browses the web".to_string(),
            profile_id: "browser-specialist".to_string(),
            profile_version: "0.1.0".to_string(),
            group_slug: "browser-1".to_string(),
            image_repository: "ghcr.io/example/browser".to_string(),
            image_tag: "0.1.0".to_string(),
            image_digest: None,
            max_specialists: 1,
            max_concurrent_jobs: 8,
            max_artifact_bytes: 1024,
            system_prompt: "You browse.".to_string(),
            tools: vec!["Bash".to_string()],
            allowed_tools: vec!["Bash(agent-browser:*)".to_string()],
            max_turns: 40,
            extra_env: vec![],
        }
    }

    fn write_bundle(dir: &Path, name: &str, spec: &SpecialistSpec) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), serde_json::to_string(spec).unwrap()).unwrap();
    }

    #[test]
    fn entry_defaults_to_enabled_with_no_overrides() {
        let entry: SpecialistEntry = toml::from_str(r#"bundle = "browser.json""#).unwrap();
        assert!(entry.enabled);
        assert_eq!(entry.overrides, SpecialistOverrides::default());
    }

    #[test]
    fn resolves_a_bundle_into_a_spec() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "browser.json", &sample_spec());
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("browser.json"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        let specs = resolve_specialists(&entries, dir.path()).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].route_name, "browser");
    }

    #[test]
    fn overrides_apply_over_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "browser.json", &sample_spec());
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("browser.json"),
            enabled: true,
            overrides: SpecialistOverrides {
                image_digest: Some("sha256:abc".to_string()),
                max_specialists: Some(3),
                max_turns: Some(12),
                ..Default::default()
            },
        }];
        let specs = resolve_specialists(&entries, dir.path()).unwrap();
        assert_eq!(specs[0].image_digest.as_deref(), Some("sha256:abc"));
        assert_eq!(specs[0].max_specialists, 3);
        assert_eq!(specs[0].max_turns, 12);
        // untouched fields keep the bundle's values
        assert_eq!(specs[0].max_concurrent_jobs, 8);
    }

    #[test]
    fn disabled_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(dir.path(), "browser.json", &sample_spec());
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("browser.json"),
            enabled: false,
            overrides: SpecialistOverrides::default(),
        }];
        assert!(resolve_specialists(&entries, dir.path()).unwrap().is_empty());
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("../secrets.json"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        assert!(matches!(
            resolve_specialists(&entries, dir.path()),
            Err(ConfigError::SpecialistBundlePathEscape { .. })
        ));
    }

    #[test]
    fn rejects_absolute_bundle_path() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("/etc/passwd"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        assert!(matches!(
            resolve_specialists(&entries, dir.path()),
            Err(ConfigError::SpecialistBundlePathEscape { .. })
        ));
    }

    #[test]
    fn rejects_bundle_missing_an_image_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sample_spec();
        spec.image_repository = String::new();
        write_bundle(dir.path(), "broken.json", &spec);
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("broken.json"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        assert!(matches!(
            resolve_specialists(&entries, dir.path()),
            Err(ConfigError::SpecialistMissingImageRef { .. })
        ));
    }

    #[test]
    fn digest_only_reference_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sample_spec();
        spec.image_tag = String::new();
        spec.image_digest = Some("sha256:abc".to_string());
        write_bundle(dir.path(), "browser.json", &spec);
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("browser.json"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        assert_eq!(resolve_specialists(&entries, dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn missing_bundle_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![SpecialistEntry {
            bundle: PathBuf::from("absent.json"),
            enabled: true,
            overrides: SpecialistOverrides::default(),
        }];
        assert!(matches!(
            resolve_specialists(&entries, dir.path()),
            Err(ConfigError::SpecialistBundleIo { .. })
        ));
    }
}
