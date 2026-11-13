//! `JobQueue` conformance: §19.2's at-least-once contract —
//! enqueue/dequeue FIFO, delivery leases, `ack` finality, `nack`
//! redelivery (immediate and `retry_at`-deferred), `attempts`
//! counting, and unique-key dedup.
//!
//! Waits stay under ~200 ms; the lease test uses a `Job`-level
//! `visibility` so it needs no store-constructor knowledge.
#![expect(
    clippy::unwrap_used,
    reason = "conformance checks assert the adapter contract via panics — they are tests"
)]

use std::time::{Duration, SystemTime};

use fulmen_core::adapters::{Job, JobQueue};

use crate::block_on;

/// Run every `JobQueue` check against `store`.
///
/// # Panics
/// On any contract violation — that is the point of a suite.
pub fn suite(store: &dyn JobQueue) {
    enqueue_returns_distinct_ids(store);
    dequeue_fifo_per_queue(store);
    dequeue_selects_queues(store);
    dequeue_times_out(store);
    dequeue_waits_for_enqueue(store);
    payload_roundtrip(store);
    ack_is_final(store);
    unacked_job_redelivers(store);
    nack_redelivers_immediately(store);
    nack_retry_at_delays(store);
    attempts_counts_deliveries(store);
    scheduled_job_not_early(store);
    unique_dedups_enqueue(store);
    finished_lease_errors(store);
}

/// `enqueue` assigns an id; two jobs get distinct ones.
///
/// # Panics
/// On contract violation.
pub fn enqueue_returns_distinct_ids(store: &dyn JobQueue) {
    let a = block_on(store.enqueue(Job::new("q", vec![1]))).unwrap();
    let b = block_on(store.enqueue(Job::new("q", vec![2]))).unwrap();
    assert!(!a.as_str().is_empty());
    assert_ne!(a, b);
}

/// Jobs come out oldest-first within a queue.
///
/// # Panics
/// On contract violation.
pub fn dequeue_fifo_per_queue(store: &dyn JobQueue) {
    block_on(store.enqueue(Job::new("fifo", b"one".to_vec()))).unwrap();
    block_on(store.enqueue(Job::new("fifo", b"two".to_vec()))).unwrap();
    let first = deq(store, "fifo");
    let second = deq(store, "fifo");
    assert_eq!(first.payload, b"one");
    assert_eq!(second.payload, b"two");
    block_on(store.ack(first.id.as_ref().unwrap())).unwrap();
    block_on(store.ack(second.id.as_ref().unwrap())).unwrap();
}

/// A dequeue names its queues — jobs on other queues stay put.
///
/// # Panics
/// On contract violation.
pub fn dequeue_selects_queues(store: &dyn JobQueue) {
    block_on(store.enqueue(Job::new("select-a", vec![]))).unwrap();
    let none =
        block_on(store.dequeue(&["select-b".to_string()], Duration::from_millis(30))).unwrap();
    assert!(none.is_none());
    let job = deq(store, "select-a");
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
}

/// An empty store's `dequeue` resolves to `None` after `timeout`.
///
/// # Panics
/// On contract violation.
pub fn dequeue_times_out(store: &dyn JobQueue) {
    let start = std::time::Instant::now();
    let none = block_on(store.dequeue(&["empty".to_string()], Duration::from_millis(50))).unwrap();
    assert!(none.is_none());
    assert!(start.elapsed() >= Duration::from_millis(40));
}

/// A pending `dequeue` resolves when a job lands.
///
/// # Panics
/// On contract violation.
pub fn dequeue_waits_for_enqueue(store: &dyn JobQueue) {
    // `&dyn JobQueue` is `Send` (the trait is `Sync`), so scoped
    // threads share the store directly.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(40));
            block_on(store.enqueue(Job::new("waiter", vec![9]))).unwrap();
        });
        let job = block_on(store.dequeue(&["waiter".to_string()], Duration::from_secs(2)))
            .unwrap()
            .unwrap();
        assert_eq!(job.payload, vec![9]);
        block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
    });
}

/// The dequeued job carries the enqueued payload byte-for-byte.
///
/// # Panics
/// On contract violation.
pub fn payload_roundtrip(store: &dyn JobQueue) {
    let payload = b"\x00\xffbinary-payload".to_vec();
    block_on(store.enqueue(Job::new("payload", payload.clone()))).unwrap();
    let job = deq(store, "payload");
    assert_eq!(job.payload, payload);
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
}

/// An `ack`ed job is never delivered again.
///
/// # Panics
/// On contract violation.
pub fn ack_is_final(store: &dyn JobQueue) {
    block_on(store.enqueue(Job::new("ack", vec![]))).unwrap();
    let job = deq(store, "ack");
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
    let none = block_on(store.dequeue(&["ack".to_string()], Duration::from_millis(30))).unwrap();
    assert!(none.is_none());
}

/// §19.2 at-least-once: a delivery left unacknowledged past its
/// `visibility` lease is redelivered.
///
/// # Panics
/// On contract violation.
pub fn unacked_job_redelivers(store: &dyn JobQueue) {
    let job = Job::new("lease", vec![]).visibility(Duration::from_millis(40));
    block_on(store.enqueue(job)).unwrap();
    let first = deq(store, "lease");
    std::thread::sleep(Duration::from_millis(80));
    let second = deq(store, "lease");
    assert_eq!(second.id, first.id, "the same job must come back");
    block_on(store.ack(second.id.as_ref().unwrap())).unwrap();
}

/// `nack(None)` makes the job deliverable immediately.
///
/// # Panics
/// On contract violation.
pub fn nack_redelivers_immediately(store: &dyn JobQueue) {
    block_on(store.enqueue(Job::new("nack-now", vec![]))).unwrap();
    let first = deq(store, "nack-now");
    block_on(store.nack(first.id.as_ref().unwrap(), None)).unwrap();
    let second = deq(store, "nack-now");
    assert_eq!(second.id, first.id);
    block_on(store.ack(second.id.as_ref().unwrap())).unwrap();
}

/// `nack(retry_at)` defers the redelivery: too-early dequeues miss, a
/// dequeue past `retry_at` gets the job.
///
/// # Panics
/// On contract violation.
pub fn nack_retry_at_delays(store: &dyn JobQueue) {
    block_on(store.enqueue(Job::new("nack-later", vec![]))).unwrap();
    let first = deq(store, "nack-later");
    let retry_at = SystemTime::now() + Duration::from_millis(80);
    block_on(store.nack(first.id.as_ref().unwrap(), Some(retry_at))).unwrap();
    let early =
        block_on(store.dequeue(&["nack-later".to_string()], Duration::from_millis(30))).unwrap();
    assert!(
        early.is_none(),
        "nacked job must not deliver before retry_at"
    );
    let second = block_on(store.dequeue(&["nack-later".to_string()], Duration::from_millis(200)))
        .unwrap()
        .unwrap();
    assert_eq!(second.id, first.id);
    block_on(store.ack(second.id.as_ref().unwrap())).unwrap();
}

/// `attempts` counts deliveries: 1 on first dequeue, ≥2 on redelivery.
///
/// # Panics
/// On contract violation.
pub fn attempts_counts_deliveries(store: &dyn JobQueue) {
    let job = Job::new("attempts", vec![]).visibility(Duration::from_millis(40));
    block_on(store.enqueue(job)).unwrap();
    let first = deq(store, "attempts");
    assert_eq!(first.attempts, 1);
    std::thread::sleep(Duration::from_millis(80));
    let second = deq(store, "attempts");
    assert!(second.attempts >= 2, "attempts must count deliveries");
    block_on(store.ack(second.id.as_ref().unwrap())).unwrap();
}

/// A `run_at`-scheduled job is not delivered before its time.
///
/// # Panics
/// On contract violation.
pub fn scheduled_job_not_early(store: &dyn JobQueue) {
    let job = Job::new("scheduled", vec![]).run_at(SystemTime::now() + Duration::from_millis(60));
    block_on(store.enqueue(job)).unwrap();
    let early =
        block_on(store.dequeue(&["scheduled".to_string()], Duration::from_millis(20))).unwrap();
    assert!(early.is_none(), "a scheduled job must not deliver early");
    let job = block_on(store.dequeue(&["scheduled".to_string()], Duration::from_millis(200)))
        .unwrap()
        .unwrap();
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
}

/// §19.2 uniqueness: a second `enqueue` with a live `unique` key
/// returns the existing id instead of duplicating; after `ack` the key
/// is free again.
///
/// # Panics
/// On contract violation.
pub fn unique_dedups_enqueue(store: &dyn JobQueue) {
    let first = block_on(store.enqueue(Job::new("uniq", vec![]).unique("key-1"))).unwrap();
    let dupe = block_on(store.enqueue(Job::new("uniq", vec![]).unique("key-1"))).unwrap();
    assert_eq!(first, dupe, "a live unique key must dedup, not duplicate");
    let job = deq(store, "uniq");
    assert!(
        block_on(store.dequeue(&["uniq".to_string()], Duration::from_millis(30)))
            .unwrap()
            .is_none()
    );
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
    let third = block_on(store.enqueue(Job::new("uniq", vec![]).unique("key-1"))).unwrap();
    assert_ne!(third, first);
    let job = deq(store, "uniq");
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
}

/// `ack`/`nack` on a finished lease errors — the lease is explicit, not
/// best-effort. (A third party cannot fabricate a `JobId`, so the
/// "unknown id" case is exercised through a finished one.)
///
/// # Panics
/// On contract violation.
pub fn finished_lease_errors(store: &dyn JobQueue) {
    let id = block_on(store.enqueue(Job::new("known", vec![]))).unwrap();
    let job = deq(store, "known");
    block_on(store.ack(job.id.as_ref().unwrap())).unwrap();
    assert!(block_on(store.ack(&id)).is_err(), "a re-ack must err");
    assert!(
        block_on(store.nack(&id, None)).is_err(),
        "nack after ack must err"
    );
}

/// Dequeue `queue` with a generous wait and unwrap both layers — called
/// where a job is guaranteed. Panics on a miss.
fn deq(store: &dyn JobQueue, queue: &str) -> Job {
    block_on(store.dequeue(&[queue.to_string()], Duration::from_secs(2)))
        .unwrap()
        .unwrap()
}
