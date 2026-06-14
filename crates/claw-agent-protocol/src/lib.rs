//! Shared agent protocol: typed outbound actions, inbound/outbound envelopes,
//! product-neutral prompt-fragment rendering, the final-text fallback safety
//! policy, the Rust-owned generated runner manifest, and a stub agent loop.
//!
//! This crate owns no database tables and no product policy. Typed actions are
//! the canonical delivery mechanism; tagged text in model prose never routes a
//! message. The runner manifest is the single artifact a replaceable TypeScript
//! shim consumes, so product prompt text stays out of the shim and out of this
//! crate. The stub agent loop writes through an [`OutboundSink`] trait, which is
//! why this crate depends only on `claw-core` in the platform manifest.

pub mod action;
pub mod envelope;
pub mod fallback;
pub mod fragment;
pub mod manifest;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod runner;

pub use action::OutboundAction;
pub use envelope::{AgentKind, InboundEnvelope, RunContext, RunResult};
pub use fallback::{decide_fallback, FallbackDecision, FallbackLedger, SuppressReason};
pub use fragment::{
    render_prompt, shared_fragments, substitute, RenderableFragment, OUTPUT_PROTOCOL_ID,
    SHARED_SAFETY_ID, SPECIALIST_OUTPUT_PROTOCOL_ID,
};
pub use manifest::{
    ManifestInputs, McpServerDecl, MemoryMount, ProfileIdentity, PromptFragmentRef, RunnerManifest,
    SessionSchemaSupport, ToolPolicy,
};
pub use process::{process_run, Delivery, ProcessedRun};
pub use protocol::{check_runner_protocol, UnsupportedProtocol, RUNNER_PROTOCOL_VERSION};
pub use provider::{AgentProvider, StubProvider};
pub use runner::{run_once, OutboundSink};

pub const MODULE_ID: &str = "claw-agent-protocol";
pub const MODULE_VERSION: &str = env!("CARGO_PKG_VERSION");
