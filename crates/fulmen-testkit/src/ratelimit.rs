//! `RateLimitStore` conformance: the allow/deny contract of §16.3 —
//! burst admission, denial carrying a `retry_after`, a denied call
//! consuming nothing, and per-key independence.
//!
//! The suite uses millisecond rates so a real wait can prove retry
//! semantics without slowing the run.
#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "conformance checks assert the adapter contract via panics — they are tests"
)]

use std::time::Duration;

use fulmen_core::adapters::{Decision, Rate, RateLimitStore};

use crate::block_on;

/// Run every `RateLimitStore` check against `store`.
///
/// # Panics
/// On any contract violation — that is the point of a suite.
pub fn suite(store: &dyn RateLimitStore) {
    first_acquire_allowed(store);
    burst_then_denied(store);
    denied_reports_retry_after(store);
    denied_consumes_nothing(store);
    retry_after_wait_allows(store);
    keys_are_independent(store);
    cost_consumes_budget(store);
}

/// A fresh key's first `acquire` is allowed.
///
/// # Panics
/// On contract violation.
pub fn first_acquire_allowed(store: &dyn RateLimitStore) {
    let rate = rate(10, 1_000);
    match block_on(store.acquire("rl-first", 1, rate)).unwrap() {
        Decision::Allowed { remaining, .. } => assert!(remaining < 10),
        Decision::Denied { .. } => panic!("first acquire must be allowed"),
    }
}

/// `burst` rapid acquires pass; the next is denied — §16.3's "no
/// bursts beyond the configured burst size".
///
/// # Panics
/// On contract violation.
pub fn burst_then_denied(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(4, Duration::from_millis(400), 4).unwrap();
    for attempt in 0..4 {
        assert!(
            block_on(store.acquire("rl-burst", 1, rate))
                .unwrap()
                .allowed(),
            "attempt {attempt} within burst must pass"
        );
    }
    assert!(
        !block_on(store.acquire("rl-burst", 1, rate))
            .unwrap()
            .allowed(),
        "acquire past the burst must be denied"
    );
}

/// A `Denied` verdict reports when a retry would pass.
///
/// # Panics
/// On contract violation.
pub fn denied_reports_retry_after(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(1, Duration::from_millis(200), 1).unwrap();
    block_on(store.acquire("rl-retry", 1, rate)).unwrap();
    match block_on(store.acquire("rl-retry", 1, rate)).unwrap() {
        Decision::Denied { retry_after, .. } => {
            assert!(retry_after > Duration::ZERO);
            assert!(retry_after <= Duration::from_millis(400));
        }
        Decision::Allowed { .. } => panic!("second acquire must be denied"),
    }
}

/// A denied `acquire` consumes no budget — the next call at the
/// reported `retry_after` is not pushed further out.
///
/// # Panics
/// On contract violation.
pub fn denied_consumes_nothing(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(1, Duration::from_millis(60), 1).unwrap();
    block_on(store.acquire("rl-free", 1, rate)).unwrap();
    for _ in 0..3 {
        assert!(
            !block_on(store.acquire("rl-free", 1, rate))
                .unwrap()
                .allowed()
        );
    }
    std::thread::sleep(Duration::from_millis(80));
    assert!(
        block_on(store.acquire("rl-free", 1, rate))
            .unwrap()
            .allowed(),
        "a denied call must not charge the budget"
    );
}

/// Waiting out the reported `retry_after` makes the retry pass.
///
/// # Panics
/// On contract violation.
pub fn retry_after_wait_allows(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(1, Duration::from_millis(50), 1).unwrap();
    block_on(store.acquire("rl-wait", 1, rate)).unwrap();
    let Decision::Denied { retry_after, .. } = block_on(store.acquire("rl-wait", 1, rate)).unwrap()
    else {
        panic!("second acquire must be denied")
    };
    std::thread::sleep(retry_after + Duration::from_millis(20));
    assert!(
        block_on(store.acquire("rl-wait", 1, rate))
            .unwrap()
            .allowed()
    );
}

/// Distinct keys hold distinct budgets.
///
/// # Panics
/// On contract violation.
pub fn keys_are_independent(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(1, Duration::from_millis(500), 1).unwrap();
    block_on(store.acquire("rl-a", 1, rate)).unwrap();
    assert!(!block_on(store.acquire("rl-a", 1, rate)).unwrap().allowed());
    assert!(
        block_on(store.acquire("rl-b", 1, rate)).unwrap().allowed(),
        "key b's budget must not be spent by key a"
    );
}

/// A `cost` of N prices like N single calls.
///
/// # Panics
/// On contract violation.
pub fn cost_consumes_budget(store: &dyn RateLimitStore) {
    let rate = Rate::with_burst(4, Duration::from_millis(400), 4).unwrap();
    block_on(store.acquire("rl-cost", 3, rate)).unwrap();
    assert!(
        block_on(store.acquire("rl-cost", 1, rate))
            .unwrap()
            .allowed()
    );
    assert!(
        !block_on(store.acquire("rl-cost", 1, rate))
            .unwrap()
            .allowed(),
        "cost 3 + cost 1 spends a burst of 4 — the next must deny"
    );
}

/// `count` per `millis` — the usual rate shape.
fn rate(count: u64, millis: u64) -> Rate {
    Rate::new(count, Duration::from_millis(millis)).unwrap()
}
