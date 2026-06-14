pub mod auth;
pub mod http;
pub mod memory_api;
pub mod memoryfs;
pub mod pages;
pub mod router;
pub mod server;
pub mod view;

pub use auth::{
    bearer_token, query_token, redact, strip_token_query, AuthOutcome, AuthReject, TokenStore,
    WebToken,
};
pub use http::{Method, Request, Response};
pub use memory_api::MemoryApp;
pub use memoryfs::{
    DirEntryInfo, EditorPolicy, MemoryEditor, MemoryFile, MemoryFsError, SaveResult,
};
pub use router::{Handler, Params, Router};
pub use server::{authenticate, AuthDecision, ServerSettings};
pub use view::{
    ApprovalView, ArtifactRefView, CapabilityView, ChannelView, GroupView, Overview,
    OverviewCounts, QueueItem, ReadinessCheckView, ReadinessReportView, RunDetail, RunView,
    ScheduledItem, SessionView, SpecialistStatusView, TimelineEntry, UserView, WebApp,
};

pub const MODULE_ID: &str = "claw-web";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");

