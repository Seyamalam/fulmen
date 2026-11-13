//! A bounded string-keyed LRU map with per-entry expiry — the shared
//! core of `MemorySessionStore` and `MemoryCacheStore`.
//!
//! Recency is a monotonically increasing sequence stamped per entry; a
//! binary min-heap of `(seq, key)` approximates LRU order. Heap entries
//! superseded by a newer `seq` for the same key are stale and skipped
//! on eviction — the lazy-LRU trick that avoids an intrusive linked
//! list while keeping every operation `O(log n)` or better.
//!
//! Expiry is lazy: a dead entry is dropped on access and opportunistically
//! when an insert needs room, so a store holding only expired entries
//! never evicts a live one to make space.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

/// The default entry bound. Generous enough that a legitimate app never
/// hits it, small enough that a `set`-flooding client cannot grow the
/// map without bound (§3: secure defaults).
pub(crate) const DEFAULT_CAPACITY: usize = 10_000;

pub(crate) struct ExpiringLru {
    map: HashMap<Box<str>, Entry>,
    heap: BinaryHeap<Reverse<(u64, Box<str>)>>,
    seq: u64,
    capacity: usize,
}

struct Entry {
    value: Vec<u8>,
    expires: Option<Instant>,
    seq: u64,
}

impl Entry {
    fn alive(&self, now: Instant) -> bool {
        self.expires.is_none_or(|deadline| deadline > now)
    }
}

impl ExpiringLru {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            heap: BinaryHeap::new(),
            seq: 0,
            capacity: capacity.max(1),
        }
    }

    /// Live + dead entries — used by unit tests.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// The live value under `key`, promoted to most-recently-used.
    /// An expired entry is removed and reported as absent.
    pub fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let now = Instant::now();
        if self.map.get(key).is_some_and(|entry| !entry.alive(now)) {
            self.remove(key);
            return None;
        }
        let entry = self.map.get_mut(key)?;
        self.seq += 1;
        entry.seq = self.seq;
        self.heap.push(Reverse((self.seq, key.into())));
        Some(entry.value.clone())
    }

    /// Upsert `key`, evicting the least-recently-used entries when the
    /// map outgrows its capacity. `ttl` of `None` never expires.
    pub fn insert(&mut self, key: &str, value: Vec<u8>, ttl: Option<Duration>) {
        let expires = ttl.map(|ttl| Instant::now() + ttl);
        self.seq += 1;
        let seq = self.seq;
        self.map.insert(
            key.into(),
            Entry {
                value,
                expires,
                seq,
            },
        );
        self.heap.push(Reverse((seq, key.into())));
        if self.map.len() > self.capacity {
            self.purge_expired();
        }
        while self.map.len() > self.capacity {
            self.evict_one();
        }
    }

    /// Drop `key`; returns whether a live entry was removed. Stale heap
    /// entries are skipped lazily by [`Self::evict_one`].
    pub fn remove(&mut self, key: &str) -> bool {
        self.map.remove(key).is_some()
    }

    /// Drop every entry past its deadline.
    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.map.retain(|_, entry| entry.alive(now));
    }

    /// Remove the least-recently-used *live* entry, discarding stale
    /// heap entries and expired entries first.
    fn evict_one(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((seq, key))) = self.heap.pop() {
            match self.map.get(key.as_ref()) {
                // Stale heap entry: the key was touched after this seq —
                // keep scanning.
                Some(entry) if entry.seq != seq => {}
                // Expired entries clean themselves up for free.
                Some(entry) if !entry.alive(now) => {
                    self.map.remove(key.as_ref());
                }
                Some(_) => {
                    self.map.remove(key.as_ref());
                    return;
                }
                // Key was deleted already — keep scanning.
                None => {}
            }
        }
    }
}

impl std::fmt::Debug for ExpiringLru {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExpiringLru")
            .field("len", &self.map.len())
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lru(capacity: usize) -> ExpiringLru {
        ExpiringLru::new(capacity)
    }

    #[test]
    fn get_insert_remove() {
        let mut l = lru(4);
        l.insert("a", b"1".to_vec(), None);
        assert_eq!(l.get("a"), Some(b"1".to_vec()));
        assert!(l.remove("a"));
        assert_eq!(l.get("a"), None);
        assert!(!l.remove("a"));
    }

    #[test]
    fn eviction_is_lru_order() {
        let mut l = lru(3);
        l.insert("a", vec![0], None);
        l.insert("b", vec![0], None);
        l.insert("c", vec![0], None);
        // Touch "a": eviction order becomes b, c, a.
        l.get("a");
        l.insert("d", vec![0], None);
        assert_eq!(l.get("a"), Some(vec![0]));
        assert_eq!(l.get("b"), None);
        assert_eq!(l.get("c"), Some(vec![0]));
        assert_eq!(l.get("d"), Some(vec![0]));
    }

    #[test]
    fn expired_entries_make_room_first() {
        let mut l = lru(2);
        l.insert("old", vec![0], Some(Duration::from_millis(1)));
        l.insert("live", vec![0], None);
        std::thread::sleep(Duration::from_millis(5));
        l.insert("new", vec![0], None);
        // "old" expired — it is purged instead of evicting "live".
        assert_eq!(l.get("old"), None);
        assert_eq!(l.get("live"), Some(vec![0]));
        assert_eq!(l.get("new"), Some(vec![0]));
    }

    #[test]
    fn stale_heap_entries_do_not_confuse_eviction() {
        let mut l = lru(2);
        l.insert("a", vec![0], None);
        l.insert("b", vec![0], None);
        // Repeatedly touch "a": heaps up stale seqs for it.
        for _ in 0..10 {
            l.get("a");
        }
        l.insert("c", vec![0], None);
        assert_eq!(l.get("b"), None);
        assert_eq!(l.get("a"), Some(vec![0]));
        assert_eq!(l.get("c"), Some(vec![0]));
    }

    #[test]
    fn upsert_refreshes_recency_not_capacity() {
        let mut l = lru(2);
        l.insert("a", vec![0], None);
        l.insert("a", vec![1], None);
        assert_eq!(l.len(), 1);
        assert_eq!(l.get("a"), Some(vec![1]));
    }
}
