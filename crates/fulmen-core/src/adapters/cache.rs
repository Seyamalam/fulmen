//! `CacheStore` (DESIGN.md §15.1): the response-cache backend.
//!
//! The cache plugin (M5.12) calls it from the `on_response` phase when
//! storing, and from `guard` when answering a hit; a native
//! implementation answers without a Python crossing, a Python
//! `CacheStore` implementation is called through the executor bridge.

use std::sync::Mutex;
use std::time::Duration;

use super::lru::{DEFAULT_CAPACITY, ExpiringLru};
use super::{StoreFuture, ready};

/// The cache backend. Keys and values are opaque bytes — the owning
/// plugin owns the key layout (route + vary headers) and the payload
/// encoding.
pub trait CacheStore: Send + Sync + 'static {
    /// The cached payload under `key`, or `None` when absent or expired.
    fn get(&self, key: &str) -> StoreFuture<Option<Vec<u8>>>;

    /// Cache `value` under `key`, replacing any prior entry. `ttl` bounds
    /// the entry's lifetime; `None` means no expiry. Stores are bounded:
    /// a `set` past capacity evicts least-recently-used entries.
    fn set(&self, key: &str, value: Vec<u8>, ttl: Option<Duration>) -> StoreFuture<()>;

    /// Drop `key`. Deleting an absent key is a no-op, not an error.
    fn delete(&self, key: &str) -> StoreFuture<()>;
}

/// The default `CacheStore` (§15.2): an in-process bounded LRU behind a
/// `Mutex`.
///
/// `Send + Sync` by construction — safe to hit from any dispatch thread
/// with no GIL involvement. Cache contents are per-process and lost on
/// restart; the redis adapter (M4.15) shares them across workers.
#[derive(Debug)]
pub struct MemoryCacheStore {
    inner: Mutex<ExpiringLru>,
}

impl MemoryCacheStore {
    /// A store bounded to [`DEFAULT_CAPACITY`] entries.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A store evicting least-recently-used entries past `capacity`.
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

impl Default for MemoryCacheStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheStore for MemoryCacheStore {
    fn get(&self, key: &str) -> StoreFuture<Option<Vec<u8>>> {
        ready(Ok(self.lock().get(key)))
    }

    fn set(&self, key: &str, value: Vec<u8>, ttl: Option<Duration>) -> StoreFuture<()> {
        self.lock().insert(key, value, ttl);
        ready(Ok(()))
    }

    fn delete(&self, key: &str) -> StoreFuture<()> {
        self.lock().remove(key);
        ready(Ok(()))
    }
}
