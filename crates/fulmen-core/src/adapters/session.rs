//! `SessionStore` (DESIGN.md §15.1, §16.5): the server-side session
//! backend.
//!
//! The sessions plugin (M4.13) calls it from the `resolve` phase while
//! the cookie itself carries only a random 256-bit id, so a `load` miss
//! means "no session". A native implementation registered with the app
//! is invoked without a Python crossing; a Python `SessionStore`
//! protocol implementation is called through the executor bridge.

use std::sync::Mutex;
use std::time::Duration;

use super::lru::{DEFAULT_CAPACITY, ExpiringLru};
use super::{StoreFuture, ready};

/// The session-state backend.
///
/// All methods are one async call on the hot path; implementors must be
/// `Send + Sync` — the same store serves every worker thread.
pub trait SessionStore: Send + Sync + 'static {
    /// The stored payload for `session_id`, or `None` when absent or
    /// expired. Payloads are opaque bytes (the sessions plugin owns the
    /// session encoding).
    fn load(&self, session_id: &str) -> StoreFuture<Option<Vec<u8>>>;

    /// Store `data` under `session_id`, replacing any prior value. The
    /// entry expires `ttl` after this call — each `save` refreshes the
    /// deadline, which is how the rolling session lifetime works.
    fn save(&self, session_id: &str, data: Vec<u8>, ttl: Duration) -> StoreFuture<()>;

    /// Drop `session_id`. Deleting an absent id is a no-op, not an error.
    fn delete(&self, session_id: &str) -> StoreFuture<()>;
}

/// The default `SessionStore` (§15.2): an in-process map behind a
/// `Mutex`, bounded and LRU-evicted at `capacity` entries.
///
/// `Send + Sync` by construction — safe to hit from any dispatch thread
/// with no GIL involvement. A second process or a restart loses all
/// sessions; apps that need shared or durable sessions plug in the
/// redis adapter (M4.15) instead.
#[derive(Debug)]
pub struct MemorySessionStore {
    inner: Mutex<ExpiringLru>,
}

impl MemorySessionStore {
    /// A store bounded to [`DEFAULT_CAPACITY`] live sessions.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A store evicting least-recently-used sessions past `capacity`.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(ExpiringLru::new(capacity)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ExpiringLru> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for MemorySessionStore {
    fn load(&self, session_id: &str) -> StoreFuture<Option<Vec<u8>>> {
        ready(Ok(self.lock().get(session_id)))
    }

    fn save(&self, session_id: &str, data: Vec<u8>, ttl: Duration) -> StoreFuture<()> {
        self.lock().insert(session_id, data, Some(ttl));
        ready(Ok(()))
    }

    fn delete(&self, session_id: &str) -> StoreFuture<()> {
        self.lock().remove(session_id);
        ready(Ok(()))
    }
}

/// Object-safety proof: a `SessionStore` must be registrable as an
/// `Arc<dyn …>` backend (§6.1 registry).
#[cfg(test)]
mod object_safety {
    use super::*;
    use std::sync::Arc;

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn trait_is_object_safe_and_shared() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        assert_send_sync(&store);
    }
}

/// Constructor used by `fulmen-testkit`'s suite and unit tests here —
/// kept small; the conformance suite owns the contract checks.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{AdapterError, StoreResult};

    fn lock(s: &MemorySessionStore) -> std::sync::MutexGuard<'_, ExpiringLru> {
        s.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn capacity_bound_evicts_lru() {
        let store = MemorySessionStore::with_capacity(2);
        let mut map = lock(&store);
        map.insert("a", vec![0], None);
        map.insert("b", vec![0], None);
        map.get("a");
        map.insert("c", vec![0], None);
        assert!(map.get("b").is_none());
        assert!(map.get("a").is_some());
        assert!(map.get("c").is_some());
    }

    #[test]
    fn error_display_covers_variants() {
        let err = AdapterError::Unavailable("down".into());
        assert_eq!(err.to_string(), "adapter unavailable: down");
        let res: StoreResult<()> = Err(err);
        assert!(res.is_err());
    }
}
