pub mod db;
pub mod migrate;
pub mod schema;

pub use db::{open_central, open_in_memory};
pub use migrate::{
    applied_versions, apply, ensure_meta_tables, read_version_record, record_versions, DbError,
    Migration, MigrationReport, MigrationSet, VersionRecord,
};
pub use schema::{baseline_migrations, baseline_owner_modules, BASELINE_TABLES};

pub const MODULE_ID: &str = "claw-db";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
