//! `CacheStore` conformance: `get`/`set`/`delete` semantics and TTL —
//! what the response-cache plugin relies on.
//!
//! Eviction is deliberately *not* in the suite: the bound is a
//! constructor choice outside the trait's contract, so a third-party
//! store answers it under its own policy.
#![expect(
    clippy::unwrap_used,
    reason = "conformance checks assert the adapter contract via panics — they are tests"
)]

use std::time::Duration;

use fulmen_core::adapters::CacheStore;

use crate::block_on;

/// Run every `CacheStore` check against `store`.
///
/// # Panics
/// On any contract violation — that is the point of a suite.
pub fn suite(store: &dyn CacheStore) {
    set_then_get(store);
    get_missing_is_none(store);
    set_overwrites(store);
    delete_drops_key(store);
    delete_absent_is_noop(store);
    ttl_expires(store);
    no_ttl_persists(store);
}

/// `set` then `get` returns the stored bytes.
///
/// # Panics
/// On contract violation.
pub fn set_then_get(store: &dyn CacheStore) {
    block_on(store.set("k1", b"value".to_vec(), None)).unwrap();
    assert_eq!(
        block_on(store.get("k1")).unwrap().as_deref(),
        Some(b"value".as_slice())
    );
}

/// `get` of a key never set is `None`, not an error.
///
/// # Panics
/// On contract violation.
pub fn get_missing_is_none(store: &dyn CacheStore) {
    assert!(block_on(store.get("missing-cache")).unwrap().is_none());
}

/// A second `set` under the same key replaces the value.
///
/// # Panics
/// On contract violation.
pub fn set_overwrites(store: &dyn CacheStore) {
    block_on(store.set("k2", b"first".to_vec(), None)).unwrap();
    block_on(store.set("k2", b"second".to_vec(), None)).unwrap();
    assert_eq!(
        block_on(store.get("k2")).unwrap().as_deref(),
        Some(b"second".as_slice())
    );
}

/// `delete` drops the key — a later `get` misses.
///
/// # Panics
/// On contract violation.
pub fn delete_drops_key(store: &dyn CacheStore) {
    block_on(store.set("k3", b"x".to_vec(), None)).unwrap();
    block_on(store.delete("k3")).unwrap();
    assert!(block_on(store.get("k3")).unwrap().is_none());
}

/// `delete` of an absent key is a no-op, not an error.
///
/// # Panics
/// On contract violation.
pub fn delete_absent_is_noop(store: &dyn CacheStore) {
    block_on(store.delete("never-cached")).unwrap();
}

/// An entry past its `ttl` gets as absent.
///
/// # Panics
/// On contract violation.
pub fn ttl_expires(store: &dyn CacheStore) {
    block_on(store.set("k4", b"x".to_vec(), Some(Duration::from_millis(30)))).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert!(block_on(store.get("k4")).unwrap().is_none());
}

/// A `None` TTL entry does not expire on its own — it stays until
/// evicted or deleted.
///
/// # Panics
/// On contract violation.
pub fn no_ttl_persists(store: &dyn CacheStore) {
    block_on(store.set("k5", b"x".to_vec(), None)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        block_on(store.get("k5")).unwrap().as_deref(),
        Some(b"x".as_slice())
    );
}
