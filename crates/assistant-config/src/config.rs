//! `config.toml` schema, load/write, and environment overlay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{self, InstanceLayout, PathError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub product: ProductConfig,
    #[serde(default)]
    pub modules: ModulesConfig,
    #[serde(default)]
    pub web: WebConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductConfig {
    pub namespace: String,
    pub product_id: String,
    pub product_version: String,
    pub platform_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_handle: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModulesConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8787,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    Serialize(toml::ser::Error),
    InvalidEnv {
        key: String,
        value: String,
    },
    Path(PathError),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "failed to access {}: {source}", path.display())
            }
            ConfigError::Toml { path, source } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
            ConfigError::Serialize(source) => write!(f, "failed to serialize config: {source}"),
            ConfigError::InvalidEnv { key, value } => {
                write!(f, "invalid value {value:?} for environment override {key}")
            }
            ConfigError::Path(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<PathError> for ConfigError {
    fn from(value: PathError) -> Self {
        ConfigError::Path(value)
    }
}

impl Config {
    /// Validate the namespace/instance shape so a derived layout is well formed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        paths::validate_namespace(&self.product.namespace)?;
        if let Some(instance) = &self.product.instance {
            paths::validate_instance(instance)?;
        }
        Ok(())
    }

    pub fn instance_layout(&self, home: &Path) -> Result<InstanceLayout, ConfigError> {
        Ok(InstanceLayout::derive(
            home,
            &self.product.namespace,
            self.product.instance.as_deref(),
        )?)
    }
}

pub fn parse_config(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

pub fn render_config(config: &Config) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(config)
}

pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_config(&text).map_err(|source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `config.toml`. The parent directory must already exist; directory
/// creation is the bootstrap step's responsibility, not config's.
pub fn write_config(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let text = render_config(config).map_err(ConfigError::Serialize)?;
    std::fs::write(path, text).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Apply recognized `ASSISTANT_*` overrides onto a parsed config.
pub fn apply_env_overlay(
    config: &mut Config,
    env: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    if let Some(value) = env.get("ASSISTANT_INSTANCE") {
        config.product.instance = Some(value.clone());
    }
    if let Some(value) = env.get("ASSISTANT_OWNER_HANDLE") {
        config.product.owner_handle = Some(value.clone());
    }
    if let Some(value) = env.get("ASSISTANT_WEB_PORT") {
        config.web.port = value.parse().map_err(|_| ConfigError::InvalidEnv {
            key: "ASSISTANT_WEB_PORT".to_string(),
            value: value.clone(),
        })?;
    }
    if let Some(value) = env.get("ASSISTANT_WEB_ENABLED") {
        config.web.enabled = parse_bool(value).ok_or_else(|| ConfigError::InvalidEnv {
            key: "ASSISTANT_WEB_ENABLED".to_string(),
            value: value.clone(),
        })?;
    }
    Ok(())
}

/// Collect `ASSISTANT_*` variables from the live process environment.
pub fn env_overlay_from_process() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("ASSISTANT_"))
        .collect()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            product: ProductConfig {
                namespace: "assistant".to_string(),
                product_id: "assistant".to_string(),
                product_version: "0.1.0".to_string(),
                platform_version: "0.1.0".to_string(),
                instance: None,
                owner_handle: None,
            },
            modules: ModulesConfig {
                enabled: vec!["assistant-core".to_string(), "assistant-session".to_string()],
            },
            web: WebConfig::default(),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let config = sample();
        let text = render_config(&config).unwrap();
        let parsed = parse_config(&text).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn defaults_fill_missing_sections() {
        let text = r#"
            [product]
            namespace = "cleoclaw"
            product_id = "cleoclaw"
            product_version = "0.1.0"
            platform_version = "0.1.0"
        "#;
        let parsed = parse_config(text).unwrap();
        assert_eq!(parsed.modules.enabled, Vec::<String>::new());
        assert_eq!(parsed.web, WebConfig::default());
    }

    #[test]
    fn env_overlay_overrides_fields() {
        let mut config = sample();
        let mut env = BTreeMap::new();
        env.insert("ASSISTANT_INSTANCE".to_string(), "work".to_string());
        env.insert("ASSISTANT_OWNER_HANDLE".to_string(), "rob".to_string());
        env.insert("ASSISTANT_WEB_ENABLED".to_string(), "true".to_string());
        env.insert("ASSISTANT_WEB_PORT".to_string(), "9000".to_string());
        apply_env_overlay(&mut config, &env).unwrap();
        assert_eq!(config.product.instance.as_deref(), Some("work"));
        assert_eq!(config.product.owner_handle.as_deref(), Some("rob"));
        assert!(config.web.enabled);
        assert_eq!(config.web.port, 9000);
    }

    #[test]
    fn env_overlay_rejects_bad_port() {
        let mut config = sample();
        let mut env = BTreeMap::new();
        env.insert("ASSISTANT_WEB_PORT".to_string(), "not-a-port".to_string());
        assert!(matches!(
            apply_env_overlay(&mut config, &env),
            Err(ConfigError::InvalidEnv { .. })
        ));
    }

    #[test]
    fn validate_rejects_bad_namespace() {
        let mut config = sample();
        config.product.namespace = "Assistant".to_string();
        assert!(matches!(config.validate(), Err(ConfigError::Path(_))));
    }

    #[test]
    fn instance_layout_uses_namespace_and_instance() {
        let mut config = sample();
        config.product.instance = Some("work".to_string());
        let layout = config.instance_layout(Path::new("/home/test")).unwrap();
        assert_eq!(layout.root, Path::new("/home/test/.assistant-work"));
    }
}
