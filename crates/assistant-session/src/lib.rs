//! Per-session DB protocol: session folder layout, host-owned `inbound.db` and
//! runner-protocol-owned `outbound.db`, lazy per-session migrations, sequence
//! parity, the outbound recovery exception, attachment path safety, and a local
//! control test harness.

pub mod attachment;
pub mod control;
pub mod db;
pub mod error;
pub mod layout;
pub mod migrate;
pub mod recovery;
pub mod schema;
pub mod session;

pub use attachment::safe_attachment_path;
pub use control::{FakeContainer, LocalControl};
pub use error::SessionError;
pub use layout::{DbKind, SessionLayout};
pub use migrate::{
    check_runner_compatibility, lazy_migrate, schema_version, SchemaCompat, SessionMigration,
    PROTOCOL_VERSION,
};
pub use recovery::{
    container_liveness, open_outbound_recovery, Liveness, RecoveryGuard, SessionLock,
    DEFAULT_HEARTBEAT_TTL,
};
pub use schema::{
    inbound_migrations, migrations_for, migrations_v1_only, outbound_migrations,
    CURRENT_INBOUND_VERSION, CURRENT_OUTBOUND_VERSION,
};
pub use session::{
    current_inbound_compat, current_outbound_compat, enqueue_inbound, enqueue_inbound_keyed,
    init_session, mark_delivered, max_delivered_seq, open_inbound, open_outbound_read,
    read_outbound, session_exists, set_destinations, set_routing, verify_sequence_parity,
    Destination, InboundMessage, OutboundMessage,
};

pub const MODULE_ID: &str = "assistant-session";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
