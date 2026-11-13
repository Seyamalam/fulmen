//! `RateLimitStore` (DESIGN.md §15.1, §16.3, decision D11): the
//! rate-limiter's decision backend.
//!
//! `acquire` asks the store to take `cost` cells out of `key`'s budget
//! under `rate`. The algorithm is GCRA — one theoretical arrival time
//! per key, exact, no bursts beyond the configured tolerance — because a
//! single timestamp is also what a redis store can update atomically
//! (the M4 redis adapter mirrors it in Lua).
//!
//! The rate-limit plugin (M4.11) calls `acquire` once per configured
//! limit from the `guard` phase; a native store answers without a
//! Python crossing, a Python `RateLimitStore` implementation is called
//! through the executor bridge.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{AdapterError, StoreFuture, StoreResult, ready};

/// The default bound on tracked keys — see §3 (secure defaults): a
/// key-flooding client cannot grow the map without bound. Expired keys
/// (a TAT in the past is semantically identical to an absent key) are
/// reclaimed first, so the bound only bites under sustained distinct-key
/// pressure.
const DEFAULT_MAX_KEYS: usize = 128_000;

/// One rate-limit declaration: `count` requests per `per`, with `burst`
/// requests allowed in a single instant on top of the sustained drip.
///
/// In GCRA terms `per / count` is the emission interval `T` — the
/// spacing between requests the steady state allows — and `burst * T`
/// is the burst tolerance `τ` the theoretical arrival time may run
/// ahead of the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    count: u64,
    per: Duration,
    burst: u64,
}

impl Rate {
    /// `count` requests per `per`; a full period's worth (`count`) may
    /// arrive in a single instant — the usual `"N/period"` reading.
    ///
    /// # Errors
    /// [`AdapterError::Invalid`] when `count` is zero or `per` is empty.
    pub fn new(count: u64, per: Duration) -> StoreResult<Self> {
        Self::with_burst(count, per, count)
    }

    /// `count` requests per `per` with an explicit instant budget of
    /// `burst`.
    ///
    /// # Errors
    /// [`AdapterError::Invalid`] when `count` is zero or `per` is empty.
    pub fn with_burst(count: u64, per: Duration, burst: u64) -> StoreResult<Self> {
        if count == 0 {
            return Err(AdapterError::invalid("rate count must be > 0"));
        }
        if per.is_zero() {
            return Err(AdapterError::invalid("rate period must be > 0"));
        }
        Ok(Self { count, per, burst })
    }

    /// The sustained rate's numerator.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The sustained rate's period.
    pub fn per(&self) -> Duration {
        self.per
    }

    /// The instant budget in requests.
    pub fn burst(&self) -> u64 {
        self.burst
    }

    /// The GCRA emission interval `T = per / count`, in nanoseconds.
    fn interval_nanos(&self) -> u128 {
        self.per.as_nanos() / u128::from(self.count)
    }

    /// The GCRA burst tolerance `τ = burst * T`, in nanoseconds.
    fn tolerance_nanos(&self) -> u128 {
        self.interval_nanos().saturating_mul(u128::from(self.burst))
    }
}

/// A rate-limit verdict. `remaining` feeds the `RateLimit` header's
/// remaining field, `reset_after` its reset field, and `retry_after`
/// the `Retry-After` header (§16.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request may proceed: `remaining` more requests fit within the
    /// burst tolerance right now, and the budget is fully replenished
    /// `reset_after` from now.
    Allowed {
        /// Requests still available under the burst tolerance.
        remaining: u64,
        /// Time until the budget is completely full again.
        reset_after: Duration,
    },
    /// The request must wait: `retry_after` is the earliest a retry of
    /// this same `cost` could pass; `reset_after` is when the budget is
    /// fully replenished.
    Denied {
        /// Earliest the denied request would be admitted.
        retry_after: Duration,
        /// Time until the budget is completely full again.
        reset_after: Duration,
    },
}

impl Decision {
    /// Whether the request may proceed.
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

/// The rate-limiter's decision backend: `acquire` atomically checks and
/// updates `key`'s budget.
pub trait RateLimitStore: Send + Sync + 'static {
    /// Try to consume `cost` cells of `key`'s budget under `rate`.
    ///
    /// The call is atomic per key: concurrent `acquire`s never
    /// double-spend. A [`Decision::Denied`] consumes nothing — denied
    /// requests do not tighten the budget for the next caller.
    /// `rate` travels with each call (a route can hold several limits,
    /// §16.3) so the store keeps no per-key configuration.
    fn acquire(&self, key: &str, cost: u64, rate: Rate) -> StoreFuture<Decision>;
}

/// The default `RateLimitStore` (§15.2): an in-process GCRA map behind a
/// `Mutex` — one `Instant` (theoretical arrival time) per key.
///
/// `Send + Sync` by construction — a decision is a mutex, one hashmap
/// lookup and integer math, measured in tens of nanoseconds. Per-process
/// state: limits are per worker (M4 documents this for multi-process
/// deployments); the redis adapter gives a cluster-wide budget.
#[derive(Debug)]
pub struct MemoryRateLimitStore {
    tats: Mutex<HashMap<Box<str>, Instant>>,
    max_keys: usize,
}

impl MemoryRateLimitStore {
    /// A store tracking up to [`DEFAULT_MAX_KEYS`] keys.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_keys(DEFAULT_MAX_KEYS)
    }

    /// A store bounded to `max_keys` tracked keys.
    ///
    /// Keys whose theoretical arrival time has passed carry no state a
    /// fresh request would notice, so they are reclaimed first; when
    /// every key is still live, the store evicts the most-drained
    /// budget (the smallest TAT — the least user-visible loss).
    #[must_use]
    pub fn with_max_keys(max_keys: usize) -> Self {
        Self {
            tats: Mutex::new(HashMap::new()),
            max_keys: max_keys.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Box<str>, Instant>> {
        self.tats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// GCRA over `key`'s theoretical arrival time.
    fn acquire_inner(&self, key: &str, cost: u64, rate: Rate, now: Instant) -> Decision {
        let interval = rate.interval_nanos();
        let tolerance = rate.tolerance_nanos();
        let charge = interval.saturating_mul(u128::from(cost));

        let mut tats = self.lock();
        let stored = tats.get(key).copied().unwrap_or(now);
        let base = stored.max(now);
        let new_tat = base + nanos_to_duration(charge);
        let ahead = new_tat.saturating_duration_since(now).as_nanos();

        if ahead <= tolerance {
            self.insert_bounded(&mut tats, key, new_tat);
            let remaining = u64::try_from((tolerance - ahead) / interval).unwrap_or(u64::MAX);
            return Decision::Allowed {
                remaining,
                reset_after: nanos_to_duration(ahead),
            };
        }
        // Denied: state untouched, so the attempt consumed nothing.
        Decision::Denied {
            retry_after: nanos_to_duration(ahead - tolerance),
            reset_after: stored.saturating_duration_since(now),
        }
    }

    /// Insert `key`'s TAT, honouring `max_keys`: dead keys are reclaimed
    /// first, then the most-drained budget.
    fn insert_bounded(&self, tats: &mut HashMap<Box<str>, Instant>, key: &str, tat: Instant) {
        if tats.len() < self.max_keys || tats.contains_key(key) {
            tats.insert(key.into(), tat);
            return;
        }
        let now = Instant::now();
        tats.retain(|_, t| *t > now);
        if tats.len() >= self.max_keys
            && let Some(victim) = tats.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone())
        {
            tats.remove(&victim);
        }
        tats.insert(key.into(), tat);
    }
}

impl Default for MemoryRateLimitStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimitStore for MemoryRateLimitStore {
    fn acquire(&self, key: &str, cost: u64, rate: Rate) -> StoreFuture<Decision> {
        ready(Ok(self.acquire_inner(key, cost, rate, Instant::now())))
    }
}

/// `u128` nanoseconds → `Duration`, saturating at `Duration::MAX`.
fn nanos_to_duration(nanos: u128) -> Duration {
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn rate_validation() {
        assert!(Rate::new(10, Duration::from_secs(1)).is_ok());
        assert!(Rate::new(0, Duration::from_secs(1)).is_err());
        assert!(Rate::new(10, Duration::ZERO).is_err());
        let rate = Rate::with_burst(10, Duration::from_secs(1), 3).unwrap();
        assert_eq!(rate.burst(), 3);
    }

    #[test]
    fn gcra_allows_burst_then_denies() {
        let store = MemoryRateLimitStore::new();
        let rate = Rate::new(5, Duration::from_secs(1)).unwrap();
        for _ in 0..5 {
            assert!(store.acquire_inner("k", 1, rate, Instant::now()).allowed());
        }
        assert!(!store.acquire_inner("k", 1, rate, Instant::now()).allowed());
    }

    #[test]
    fn denied_consumes_nothing() {
        let store = MemoryRateLimitStore::new();
        let rate = Rate::new(1, Duration::from_millis(50)).unwrap();
        let now = Instant::now();
        assert!(store.acquire_inner("k", 1, rate, now).allowed());
        let denied = store.acquire_inner("k", 1, rate, now);
        assert!(!denied.allowed());
        // The denied call left no charge: after one interval the next is allowed.
        let later = now + Duration::from_millis(60);
        assert!(store.acquire_inner("k", 1, rate, later).allowed());
    }

    #[test]
    fn retry_after_matches_emission_interval() {
        let store = MemoryRateLimitStore::new();
        // burst = 1: a second request retries after exactly T.
        let rate = Rate::with_burst(1, Duration::from_millis(100), 1).unwrap();
        let now = Instant::now();
        assert!(store.acquire_inner("k", 1, rate, now).allowed());
        match store.acquire_inner("k", 1, rate, now) {
            Decision::Denied { retry_after, .. } => {
                assert_eq!(retry_after, Duration::from_millis(100));
            }
            Decision::Allowed { .. } => panic!("second acquire must be denied"),
        }
    }

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn object_safe_send_sync() {
        let store: Arc<dyn RateLimitStore> = Arc::new(MemoryRateLimitStore::new());
        assert_send_sync(&store);
    }
}
