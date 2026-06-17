//! The operator web UI, wired to the live instance.
//!
//! `assistant-web` is transport-free and domain-free by design: it supplies the
//! router, the auth choke point, the page/memory handlers, and the view models,
//! but reads every datum through the [`WebApp`](assistant_web::WebApp) /
//! [`MemoryApp`](assistant_web::MemoryApp) provider traits. This module is the
//! host side the platform deferred:
//!
//! - [`listener`] — a `std::net`-only synchronous HTTP/1.1 listener (no async, no
//!   extra deps) that authenticates every request before dispatch.
//! - [`app`] — [`HostWebApp`], a `WebApp` impl backed by the real central DB.

pub mod app;
pub mod listener;
pub mod serve;

pub use app::HostWebApp;
pub use listener::{bind, serve as serve_loop};
pub use serve::{build_router, ensure_token};
