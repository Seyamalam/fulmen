//! `fulmen-server`: the pure-Rust HTTP/1.1 transport for Fulmen.
//!
//! This crate owns the socket and nothing else. It knows nothing about
//! Python and nothing about routing: every request is handed to a
//! [`Dispatcher`], the trait another crate implements to route and call
//! handlers (`docs/DESIGN.md` §6.2). Because the server only sees the
//! wire-level [`Request`] and [`Response`] types, it can be benchmarked end
//! to end with [`testing::TestDispatcher`], which is the upper bound on
//! throughput before any interpreter work is counted.
//!
//! # Serving
//!
//! [`serve`] binds a socket, builds a multi-threaded tokio runtime
//! ([`ServerConfig::worker_threads`], default: CPU cores), and returns a
//! [`Server`] handle with the bound address and the shutdown controls.
//! Connections are served with hyper 1.x HTTP/1.1; keep-alive and
//! pipelining are always on. HTTP/2, TLS, compression and `WebSockets` are
//! later milestones (`docs/ROADMAP.md` M5).
//!
//! # The `Dispatcher` choice
//!
//! [`Dispatcher::dispatch`] returns a boxed, `'static`, `Send` future.
//! That makes the trait object-safe, so one `Arc<dyn Dispatcher>` is shared
//! by every connection and embedders do not have to propagate generics
//! through their own types. The cost is one heap allocation per request,
//! which is noise next to parsing and I/O; a generic signature could avoid
//! it but would leak the implementation type into every user of the trait.
//! Each dispatch is spawned as its own tokio task, so a panic in a
//! dispatcher becomes a `500` for that request instead of poisoning the
//! connection task (`docs/DESIGN.md` §14.6).
//!
//! # Limits and timeouts
//!
//! All values are configurable through [`ServerConfig`] and default to
//! `docs/DESIGN.md` §14.3/§14.4. Head limits (URL length, header count,
//! header block size) reject with real `414`/`431` responses; the body
//! limit rejects with `413`; read timeouts answer `408`; the connection
//! limit refuses the socket. Keep-alive connections get a separate idle
//! timeout, enforced per connection by an instrumented I/O wrapper.

mod config;
mod conn;
mod date;
mod dispatcher;
mod server;
mod service;
pub mod testing;
mod wire;

pub use config::ServerConfig;
pub use dispatcher::{DispatchFuture, Dispatcher};
pub use server::{Server, serve};
pub use wire::{Request, Response};

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "server";
