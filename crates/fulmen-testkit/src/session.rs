//! `SessionStore` conformance: the contract of `load`/`save`/`delete`
//! and the rolling TTL — what §16.5's sessions plugin relies on.
#![expect(
    clippy::unwrap_used,
    reason = "conformance checks assert the adapter contract via panics — they are tests"
)]

use std::time::Duration;

use fulmen_core::adapters::SessionStore;

use crate::block_on;

/// Run every `SessionStore` check against `store`.
///
/// # Panics
/// On any contract violation — that is the point of a suite.
pub fn suite(store: &dyn SessionStore) {
    save_then_load(store);
    load_missing_is_none(store);
    save_overwrites(store);
    delete_drops_key(store);
    delete_absent_is_noop(store);
    ttl_expires(store);
    save_refreshes_ttl(store);
    keys_are_independent(store);
}

/// `save` then `load` returns the stored bytes.
///
/// # Panics
/// On contract violation.
pub fn save_then_load(store: &dyn SessionStore) {
    block_on(store.save("s1", b"payload".to_vec(), Duration::from_mins(1))).unwrap();
    let data = block_on(store.load("s1")).unwrap();
    assert_eq!(data.as_deref(), Some(b"payload".as_slice()));
}

/// `load` of a key never saved is `None`, not an error.
///
/// # Panics
/// On contract violation.
pub fn load_missing_is_none(store: &dyn SessionStore) {
    assert!(block_on(store.load("missing-session")).unwrap().is_none());
}

/// A second `save` under the same id replaces the value.
///
/// # Panics
/// On contract violation.
pub fn save_overwrites(store: &dyn SessionStore) {
    block_on(store.save("s2", b"first".to_vec(), Duration::from_mins(1))).unwrap();
    block_on(store.save("s2", b"second".to_vec(), Duration::from_mins(1))).unwrap();
    let data = block_on(store.load("s2")).unwrap();
    assert_eq!(data.as_deref(), Some(b"second".as_slice()));
}

/// `delete` drops the key — a later `load` misses.
///
/// # Panics
/// On contract violation.
pub fn delete_drops_key(store: &dyn SessionStore) {
    block_on(store.save("s3", b"x".to_vec(), Duration::from_mins(1))).unwrap();
    block_on(store.delete("s3")).unwrap();
    assert!(block_on(store.load("s3")).unwrap().is_none());
}

/// `delete` of an absent id is a no-op, not an error — logout races and
/// double-deletes must not 500 the request.
///
/// # Panics
/// On contract violation.
pub fn delete_absent_is_noop(store: &dyn SessionStore) {
    block_on(store.delete("never-existed")).unwrap();
}

/// An entry past its `ttl` loads as absent.
///
/// # Panics
/// On contract violation.
pub fn ttl_expires(store: &dyn SessionStore) {
    block_on(store.save("s4", b"x".to_vec(), Duration::from_millis(30))).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert!(block_on(store.load("s4")).unwrap().is_none());
}

/// Every `save` refreshes the deadline — the rolling session lifetime
/// §16.5 depends on.
///
/// # Panics
/// On contract violation.
pub fn save_refreshes_ttl(store: &dyn SessionStore) {
    let ttl = Duration::from_millis(80);
    block_on(store.save("s5", b"x".to_vec(), ttl)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    block_on(store.save("s5", b"y".to_vec(), ttl)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    // 100ms since the first save but only 50ms since the last.
    let data = block_on(store.load("s5")).unwrap();
    assert_eq!(data.as_deref(), Some(b"y".as_slice()));
    std::thread::sleep(Duration::from_millis(60));
    assert!(block_on(store.load("s5")).unwrap().is_none());
}

/// One session's lifecycle never bleeds into another's.
///
/// # Panics
/// On contract violation.
pub fn keys_are_independent(store: &dyn SessionStore) {
    block_on(store.save("s6a", b"a".to_vec(), Duration::from_mins(1))).unwrap();
    block_on(store.save("s6b", b"b".to_vec(), Duration::from_mins(1))).unwrap();
    block_on(store.delete("s6a")).unwrap();
    assert!(block_on(store.load("s6a")).unwrap().is_none());
    assert_eq!(
        block_on(store.load("s6b")).unwrap().as_deref(),
        Some(b"b".as_slice())
    );
}
