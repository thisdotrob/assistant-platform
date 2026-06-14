//! Runner protocol version compatibility.
//!
//! The runner manifest declares the runner protocol version a run expects. The
//! configured TypeScript shim advertises which protocol versions it implements.
//! A host must refuse to start a runner whose declared version the shim does
//! not support, so the two sides never speak mismatched protocols.

/// The runner protocol version this build speaks. Kept in step with the
/// coordinated platform manifest's `runner_protocol_version`.
pub const RUNNER_PROTOCOL_VERSION: &str = "0.1.0";

/// A declared protocol version the configured shim does not implement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedProtocol {
    pub declared: String,
    pub supported: Vec<String>,
}

/// Verify a declared runner protocol version is supported by the shim. Returns
/// the mismatch on refusal so the host can report exactly what was expected.
pub fn check_runner_protocol(
    declared: &str,
    shim_supported: &[&str],
) -> Result<(), UnsupportedProtocol> {
    if shim_supported.contains(&declared) {
        Ok(())
    } else {
        Err(UnsupportedProtocol {
            declared: declared.to_string(),
            supported: shim_supported.iter().map(|s| s.to_string()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_version_is_accepted() {
        assert!(check_runner_protocol(RUNNER_PROTOCOL_VERSION, &["0.1.0"]).is_ok());
    }

    #[test]
    fn unsupported_version_is_refused() {
        let err = check_runner_protocol("0.2.0", &["0.1.0"]).unwrap_err();
        assert_eq!(err.declared, "0.2.0");
        assert_eq!(err.supported, vec!["0.1.0".to_string()]);
    }

    #[test]
    fn empty_shim_support_refuses_everything() {
        assert!(check_runner_protocol("0.1.0", &[]).is_err());
    }

    #[test]
    fn const_matches_coordinated_platform_manifest() {
        // The protocol version is one coordinated value; this guards against the
        // const drifting from the platform manifest.
        let manifest_toml = include_str!("../../../manifests/platform.toml");
        let manifest = assistant_core::manifest::parse_platform_manifest(manifest_toml).unwrap();
        assert_eq!(manifest.runner_protocol_version, RUNNER_PROTOCOL_VERSION);
    }
}
