pub mod config;
pub mod paths;
pub mod specialists;

pub use config::{
    apply_env_overlay, env_overlay_from_process, load_config, parse_config, render_config,
    write_config, Config, ConfigError, ModulesConfig, ProductConfig, WebConfig,
};
pub use paths::{
    home_dir, instance_dir_name, validate_instance, validate_namespace, InstanceLayout, PathError,
};
pub use specialists::{resolve_specialists, SpecialistEntry, SpecialistOverrides};

pub const MODULE_ID: &str = "assistant-config";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
