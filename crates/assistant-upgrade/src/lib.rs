//! Upgrade and conformance flow for long-lived instances.
//!
//! Hardens the upgrade path for instances created by this platform: a read-only
//! instance schema inventory, an idempotent upgrade runner, and a runtime
//! compatibility matrix. Owns no tables of its own — it reads and advances
//! assistant-db's central meta tables and assistant-session's per-session schema through
//! their public APIs, confining all writes to the instance root.

pub mod error;
pub mod inventory;
pub mod matrix;
pub mod runner;

pub use error::UpgradeError;
pub use inventory::{inventory, InstanceInventory, SessionSchema};
pub use matrix::{
    compatibility_matrix, conformance, CompatCode, CompatFinding, CompatibilityMatrix,
    ConformanceReport, RuntimeVersions,
};
pub use runner::{upgrade_instance, SessionUpgrade, UpgradeOptions, UpgradeReport};

pub const MODULE_ID: &str = "assistant-upgrade";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
