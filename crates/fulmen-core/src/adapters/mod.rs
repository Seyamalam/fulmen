//! Adapter interfaces (DESIGN.md §15): the Rust traits every pluggable
//! backend implements, plus the built-in in-memory defaults.
//!
//! §15.1: "Each adapter interface is a Rust trait in core and a Python
//! `Protocol`." The traits live here — next to the plugin/adapter
//! registry (§6.1) — so every crate that consumes a backend can hold an
//! `Arc<dyn …>` handle without depending on the implementation:
//! `fulmen-security` for sessions and the rate limiter (M4), the cache
//! and background-job plugins (M5). A trait object registered with the
//! app is invoked from native hook phases without a Python crossing; a
//! Python implementation is called through the executor bridge (§7) and
//! each call is one crossing, like any Python hook.
//!
//! The four store traits are *async*: methods return [`StoreFuture`], a
//! boxed `Send` future — the same object-safety pattern as
//! `fulmen-server`'s `DispatchFuture` — so an implementation can be a
//! plain in-memory map whose futures resolve on first poll, or a network
//! client like the redis adapter (M4.15), without generics leaking into
//! the trait's users. [`Validator`] is the exception: it runs once per
//! type at registration, so its methods are synchronous.
//!
//! All types are `std`-only: byte payloads are `Vec<u8>` and times are
//! `Duration`/`SystemTime`/`Instant`, keeping this crate dependency-free
//! (the M4 redis adapter will need `bytes` — a deliberate follow-up, not
//! pre-paid for here).

mod cache;
mod jobs;
mod lru;
mod ratelimit;
mod session;
mod validator;

use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub use cache::{CacheStore, MemoryCacheStore};
pub use jobs::{Job, JobId, JobQueue, MemoryJobQueue};
pub use ratelimit::{Decision, MemoryRateLimitStore, Rate, RateLimitStore};
pub use session::{MemorySessionStore, SessionStore};
pub use validator::{SpecValue, TypeRef, Validator};

/// A boxed `Send` future returned by the async adapter calls — the same
/// shape as `fulmen_server::DispatchFuture`, keeping the traits
/// object-safe (`Arc<dyn SessionStore>`) and `'static` so returned
/// futures can be spawned as their own task.
pub type StoreFuture<T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send>>;

/// The `Result` inside a [`StoreFuture`].
pub type StoreResult<T> = Result<T, AdapterError>;

/// A [`StoreFuture`] that resolves immediately — for adapters whose work
/// completes synchronously (the in-memory defaults).
pub fn ready<T: Send + 'static>(result: StoreResult<T>) -> StoreFuture<T> {
    Box::pin(async move { result })
}

/// The error every adapter call reports.
///
/// The variants classify the failures the interface contracts name:
/// `Unavailable` is a backend outage (the owning plugin decides between
/// 500 and 503), `Invalid` a caller contract violation (a bad `Rate`, a
/// malformed spec), `Unsupported` an operation the adapter cannot honour,
/// `NotFound` an absent entity (a job id, a unique key), and `Full` a
/// bounded store's backpressure signal — which is a capacity answer, not
/// a bug.
#[derive(Debug)]
pub enum AdapterError {
    /// The backend could not be reached or timed out.
    Unavailable(String),
    /// The call itself is malformed: bad arguments, unusable state.
    Invalid(String),
    /// The adapter cannot honour this operation.
    Unsupported(String),
    /// The addressed entity does not exist.
    NotFound(String),
    /// A bounded store is at capacity — backpressure, not a bug.
    Full(String),
    /// Anything else.
    Other(String),
}

impl AdapterError {
    /// Build a variant that carries only a message.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, message) = match self {
            Self::Unavailable(m) => ("unavailable", m),
            Self::Invalid(m) => ("invalid", m),
            Self::Unsupported(m) => ("unsupported", m),
            Self::NotFound(m) => ("not found", m),
            Self::Full(m) => ("full", m),
            Self::Other(m) => ("error", m),
        };
        write!(f, "adapter {kind}: {message}")
    }
}

impl std::error::Error for AdapterError {}
