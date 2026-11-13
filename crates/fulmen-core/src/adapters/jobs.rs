//! `JobQueue` (DESIGN.md §15.1, §19.2): the background-job backend.
//!
//! The `background` plugin (M5.10) enqueues task calls; `fulmen worker`
//! (M5.11) dequeues them. Delivery is at-least-once: a dequeued job is
//! *leased* for its `visibility` window and must be `ack`ed or `nack`ed
//! before the lease expires, or it becomes deliverable again — a dead
//! worker never loses a job.
//!
//! The store sees opaque bytes: the plugin owns argument serialization
//! (the schema engine) and retry policy (it picks `nack`'s `retry_at`
//! from the task's backoff). A native store answers without a Python
//! crossing; a Python `JobQueue` implementation is called through the
//! executor bridge.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::{AdapterError, StoreFuture, StoreResult};

/// The default lease for a dequeued job: redelivered when left
/// unacknowledged for this long (§19.2 at-least-once).
const DEFAULT_VISIBILITY: Duration = Duration::from_secs(30);

/// The default bound on pending + in-flight jobs — `enqueue` fails with
/// [`AdapterError::Full`] past it (§3: secure defaults; backpressure is
/// the honest answer to an unbounded in-process queue).
const DEFAULT_CAPACITY: usize = 65_536;

/// A job identifier assigned by the store (`enqueue` returns it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(Box<str>);

impl JobId {
    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One unit of background work (§19.2).
///
/// A job is submitted through `enqueue` — the store assigns `id` and
/// `enqueued_at`, and `dequeue` returns it with `attempts` bumped.
/// `payload` is the schema engine's serialization of the task
/// arguments; the store treats it as opaque.
#[derive(Debug, Clone)]
pub struct Job {
    /// The store-assigned id; `None` in a job being submitted.
    pub id: Option<JobId>,
    /// The queue this job belongs to; `dequeue` selects by queue names.
    pub queue: String,
    /// Serialized task arguments (opaque to the store).
    pub payload: Vec<u8>,
    /// When the job first becomes deliverable — §19.2's `enqueue_at`
    /// for scheduled jobs. `None` means immediately.
    pub run_at: Option<SystemTime>,
    /// How long a dequeued delivery may stay unacknowledged before the
    /// job is redelivered — the at-least-once lease. `None` uses the
    /// store's default visibility.
    pub visibility: Option<Duration>,
    /// Uniqueness key (§19.2): while a job with this key exists
    /// (pending or in-flight), another `enqueue` with the same key
    /// returns its `JobId` instead of creating a duplicate.
    pub unique: Option<String>,
    /// Delivery count, maintained by the store: `1` on first `dequeue`.
    pub attempts: u32,
    /// When the store accepted the job.
    pub enqueued_at: Option<SystemTime>,
}

impl Job {
    /// A job for `queue` carrying `payload`.
    pub fn new(queue: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            id: None,
            queue: queue.into(),
            payload,
            run_at: None,
            visibility: None,
            unique: None,
            attempts: 0,
            enqueued_at: None,
        }
    }

    /// Schedule the job: deliverable no earlier than `run_at`.
    #[must_use]
    pub fn run_at(mut self, run_at: SystemTime) -> Self {
        self.run_at = Some(run_at);
        self
    }

    /// Set the delivery lease — how long an unacknowledged delivery may
    /// run before the job is redelivered.
    #[must_use]
    pub fn visibility(mut self, visibility: Duration) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Set the uniqueness key — a duplicate `enqueue` returns the
    /// existing job's id.
    #[must_use]
    pub fn unique(mut self, key: impl Into<String>) -> Self {
        self.unique = Some(key.into());
        self
    }
}

/// The job backend (§15.1): FIFO, at-least-once.
pub trait JobQueue: Send + Sync + 'static {
    /// Enqueue `job`; returns its assigned id.
    ///
    /// If `job.unique` matches a pending or in-flight job, the existing
    /// `JobId` is returned — enqueue is idempotent, never duplicating.
    /// A full store answers [`AdapterError::Full`] (backpressure).
    fn enqueue(&self, job: Job) -> StoreFuture<JobId>;

    /// Take the oldest deliverable job on any of `queues` — FIFO by
    /// enqueue order — waiting up to `timeout` for one to become
    /// available; `None` after the deadline.
    ///
    /// The returned job is *leased*: it must be `ack`ed or `nack`ed
    /// within its visibility window or it becomes deliverable again
    /// (at-least-once). `attempts` counts deliveries.
    fn dequeue(&self, queues: &[String], timeout: Duration) -> StoreFuture<Option<Job>>;

    /// Mark `job_id` done — it is never redelivered. An unknown or
    /// expired lease is [`AdapterError::NotFound`].
    fn ack(&self, job_id: &JobId) -> StoreFuture<()>;

    /// Return `job_id` to the queue: deliverable at `retry_at`
    /// (`Some`) or immediately (`None`). The job keeps its payload and
    /// its uniqueness; `attempts` keeps counting deliveries. Same
    /// `NotFound` contract as `ack`.
    fn nack(&self, job_id: &JobId, retry_at: Option<SystemTime>) -> StoreFuture<()>;
}

/// What the timer thread wakes: one specific waiter when its dequeue
/// deadline hits (`Waiter`), or every waiter when a lease expiry or a
/// `run_at`/`retry_at` boundary may have made a job eligible (`All`).
#[derive(Debug)]
enum TimerEvent {
    /// A dequeue waiter's own deadline — wake it so it can return `None`.
    Waiter(u64),
    /// A job may have become deliverable — wake all waiters to re-check.
    All,
}

/// A waiting `dequeue` call.
struct Waiter {
    queues: Box<[String]>,
    waker: Waker,
}

impl fmt::Debug for Waiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter")
            .field("queues", &self.queues)
            .finish_non_exhaustive()
    }
}

/// A leased job: out for delivery until `deadline`.
#[derive(Debug)]
struct InFlight {
    entry: JobEntry,
    deadline: Instant,
}

/// A pending job plus its global enqueue order.
#[derive(Debug)]
struct JobEntry {
    seq: u64,
    job: Job,
}

/// The store's mutable state — everything lives behind one `Mutex`.
#[derive(Debug, Default)]
struct JobState {
    /// Per-queue FIFO deques: a job waits behind earlier jobs on the
    /// same queue but never behind another queue's backlog.
    pending: HashMap<Box<str>, VecDeque<JobEntry>>,
    /// Leased jobs by id — a delivery in progress.
    inflight: HashMap<JobId, InFlight>,
    /// Uniqueness key → job id, while the keyed job exists.
    unique: HashMap<String, JobId>,
    /// Waiting dequeue calls by id.
    waiters: HashMap<u64, Waiter>,
    /// Scheduled wakes for the timer thread, in fire order.
    events: BTreeMap<Instant, Vec<TimerEvent>>,
    next_job: u64,
    next_waiter: u64,
    seq: u64,
    /// Set by the owning `MemoryJobQueue`'s `Drop` — ends the timer loop.
    shutdown: bool,
}

#[derive(Debug)]
struct JobInner {
    state: Mutex<JobState>,
    /// Tells the timer thread a new earlier event exists.
    wake_timer: Condvar,
    default_visibility: Duration,
    capacity: usize,
}

impl JobInner {
    fn lock(&self) -> MutexGuard<'_, JobState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `Instant` `duration` from now, saturating on overflow instead of
/// panicking — an absurd `timeout`/`visibility` means "far future".
fn deadline_in(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(|| Instant::now() + Duration::from_hours(24))
}

/// `SystemTime` → `Instant`: a wall-clock instant becomes the monotonic
/// instant `duration_since(now)` out; past times saturate to now.
fn to_instant(time: SystemTime) -> Instant {
    let offset = time
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    deadline_in(offset)
}

/// The default `JobQueue` (§15.2): an in-process multi-queue FIFO
/// behind a `Mutex`, with a dedicated timer thread that wakes
/// deadline-bound `dequeue` calls and lets waiters observe lease
/// expiries and `run_at`/`retry_at` boundaries.
///
/// Runtime-agnostic — the timer thread honours `dequeue` deadlines on
/// any executor or none, which keeps the store usable from plain Rust
/// tests and the executor bridge alike. Jobs live only in this process
/// and are lost on restart; the redis adapter (M4.15) shares them
/// across workers.
#[derive(Debug)]
pub struct MemoryJobQueue {
    inner: Arc<JobInner>,
}

impl MemoryJobQueue {
    /// A store with the default visibility and capacity bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_VISIBILITY, DEFAULT_CAPACITY)
    }

    /// A store leasing deliveries for `default_visibility` (when a job
    /// carries no `visibility` of its own) and holding at most
    /// `capacity` pending + in-flight jobs.
    #[must_use]
    pub fn with_limits(default_visibility: Duration, capacity: usize) -> Self {
        let inner = Arc::new(JobInner {
            state: Mutex::new(JobState::default()),
            wake_timer: Condvar::new(),
            default_visibility,
            capacity: capacity.max(1),
        });
        let weak = Arc::downgrade(&inner);
        // Detached thread: it holds a `Weak` and exits on the `shutdown`
        // flag the `Drop` below sets, so no join is needed — joining
        // would deadlock if the store is dropped on a worker thread the
        // loop is currently waking.
        let _ = thread::Builder::new()
            .name("fulmen-job-timer".into())
            .spawn(move || timer_loop(&weak));
        Self { inner }
    }

    /// How many jobs are tracked (pending + in-flight) — test support.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        let st = self.inner.lock();
        st.pending.values().map(VecDeque::len).sum::<usize>() + st.inflight.len()
    }
}

impl Default for MemoryJobQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MemoryJobQueue {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        st.shutdown = true;
        self.inner.wake_timer.notify_all();
    }
}

/// The timer loop: fire every due `events` entry, then sleep until the
/// next event or a notify that an earlier one arrived. Holds the
/// store's `Arc` only inside each iteration so `shutdown` ends it even
/// while parked in `wait_timeout`.
fn timer_loop(weak: &Weak<JobInner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let wakers = {
            let mut st = inner.lock();
            loop {
                if st.shutdown {
                    return;
                }
                let now = Instant::now();
                // Due = every event keyed at or before `now`. `split_off`
                // returns the `>= next_tick` tail; what remains *in* the
                // map after the swap is the due part.
                let next_tick = now.checked_add(Duration::from_nanos(1)).unwrap_or(now);
                let later = st.events.split_off(&next_tick);
                let due: Vec<TimerEvent> = std::mem::replace(&mut st.events, later)
                    .into_values()
                    .flatten()
                    .collect();
                let mut fired: Vec<Waker> = Vec::new();
                for event in due {
                    match event {
                        TimerEvent::Waiter(id) => {
                            if let Some(waiter) = st.waiters.get(&id) {
                                fired.push(waiter.waker.clone());
                            }
                        }
                        TimerEvent::All => {
                            fired.extend(st.waiters.values().map(|waiter| waiter.waker.clone()));
                        }
                    }
                }
                if !fired.is_empty() {
                    break fired;
                }
                // Nothing due: sleep until the earliest pending event —
                // or until a notify says an earlier one arrived.
                st = match st.events.keys().next().copied() {
                    Some(at) => {
                        let dur = at.saturating_duration_since(Instant::now());
                        inner
                            .wake_timer
                            .wait_timeout(st, dur)
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0
                    }
                    None => inner
                        .wake_timer
                        .wait(st)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                };
            }
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

/// Move every expired lease back to the front of its queue. Runs at the
/// top of each `dequeue` poll — expiry is lazy, evaluated whenever a
/// consumer asks (the `All` timer event ensures someone asks).
fn expire_inflight(st: &mut JobState, now: Instant) {
    let expired: Vec<JobId> = st
        .inflight
        .iter()
        .filter(|(_, flight)| flight.deadline <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        if let Some(flight) = st.inflight.remove(&id) {
            let queue: Box<str> = flight.entry.job.queue.as_str().into();
            st.pending
                .entry(queue)
                .or_default()
                .push_front(flight.entry);
        }
    }
}

/// Take the eligible job with the smallest enqueue `seq` across
/// `queues`, lease it, and return a copy for the caller.
fn take_job(
    st: &mut JobState,
    queues: &[String],
    now: Instant,
    default_visibility: Duration,
    wake_timer: &Condvar,
) -> Option<Job> {
    expire_inflight(st, now);
    let wall = SystemTime::now();
    let mut best: Option<(Box<str>, usize, u64)> = None;
    for queue in queues {
        let Some(deque) = st.pending.get(queue.as_str()) else {
            continue;
        };
        for (index, entry) in deque.iter().enumerate() {
            if entry.job.run_at.is_some_and(|at| at > wall) {
                continue;
            }
            if best.as_ref().is_none_or(|(_, _, seq)| entry.seq < *seq) {
                best = Some((queue.as_str().into(), index, entry.seq));
            }
        }
    }
    let (queue, index, _) = best?;
    let mut entry = st.pending.get_mut(&queue)?.remove(index)?;
    if entry.job.visibility.is_none() {
        entry.job.visibility = Some(default_visibility);
    }
    entry.job.attempts += 1;
    let lease = deadline_in(entry.job.visibility.unwrap_or(default_visibility));
    let job = entry.job.clone();
    let id = job.id.clone()?;
    st.events.entry(lease).or_default().push(TimerEvent::All);
    wake_timer.notify_one();
    st.inflight.insert(
        id,
        InFlight {
            entry,
            deadline: lease,
        },
    );
    Some(job)
}

/// Collect the wakers of every waiter listening to `queue`. Called with
/// the lock held; the wakers are invoked after it is dropped.
fn waiters_for(st: &JobState, queue: &str) -> Vec<Waker> {
    st.waiters
        .values()
        .filter(|waiter| waiter.queues.iter().any(|q| q == queue))
        .map(|waiter| waiter.waker.clone())
        .collect()
}

/// A `dequeue` call's wait: resolves to a leased job, `None` at its
/// deadline, or pends on the store's waiter list — woken by `enqueue`
/// (a matching job arrived), a `TimerEvent::All` (a lease expired or a
/// `run_at`/`retry_at` boundary passed), or its own `TimerEvent::Waiter`
/// deadline.
struct DequeueFut {
    inner: Arc<JobInner>,
    queues: Box<[String]>,
    deadline: Instant,
    waiter: Option<u64>,
}

impl Future for DequeueFut {
    type Output = StoreResult<Option<Job>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let now = Instant::now();
        let mut st = this.inner.lock();
        if let Some(job) = take_job(
            &mut st,
            &this.queues,
            now,
            this.inner.default_visibility,
            &this.inner.wake_timer,
        ) {
            if let Some(id) = this.waiter.take() {
                st.waiters.remove(&id);
            }
            return Poll::Ready(Ok(Some(job)));
        }
        if now >= this.deadline {
            if let Some(id) = this.waiter.take() {
                st.waiters.remove(&id);
            }
            return Poll::Ready(Ok(None));
        }
        let id = if let Some(id) = this.waiter {
            id
        } else {
            let id = st.next_waiter;
            st.next_waiter += 1;
            this.waiter = Some(id);
            st.events
                .entry(this.deadline)
                .or_default()
                .push(TimerEvent::Waiter(id));
            this.inner.wake_timer.notify_one();
            id
        };
        st.waiters.insert(
            id,
            Waiter {
                queues: this.queues.clone(),
                waker: cx.waker().clone(),
            },
        );
        Poll::Pending
    }
}

impl Drop for DequeueFut {
    fn drop(&mut self) {
        if let Some(id) = self.waiter {
            self.inner.lock().waiters.remove(&id);
        }
    }
}

impl JobQueue for MemoryJobQueue {
    fn enqueue(&self, mut job: Job) -> StoreFuture<JobId> {
        let mut st = self.inner.lock();
        if let Some(key) = job.unique.as_deref()
            && let Some(existing) = st.unique.get(key)
        {
            return super::ready(Ok(existing.clone()));
        }
        let pending: usize = st.pending.values().map(VecDeque::len).sum();
        if pending + st.inflight.len() >= self.inner.capacity {
            return super::ready(Err(AdapterError::Full("job queue is at capacity".into())));
        }
        let id = JobId(format!("job-{}", st.next_job).into_boxed_str());
        st.next_job += 1;
        job.id = Some(id.clone());
        job.enqueued_at = Some(SystemTime::now());
        job.attempts = 0;
        let entry = JobEntry { seq: st.seq, job };
        st.seq += 1;
        let queue: Box<str> = entry.job.queue.clone().into_boxed_str();
        let run_at = entry.job.run_at;
        let unique = entry.job.unique.clone();
        st.pending
            .entry(queue.clone())
            .or_default()
            .push_back(entry);
        if let Some(key) = unique {
            st.unique.insert(key, id.clone());
        }
        // Deliverable now → wake matching waiters; scheduled → an `All`
        // event wakes them when `run_at` passes.
        let wakers = if run_at.is_some_and(|at| at > SystemTime::now()) {
            st.events
                .entry(to_instant(run_at.unwrap_or_else(SystemTime::now)))
                .or_default()
                .push(TimerEvent::All);
            self.inner.wake_timer.notify_one();
            Vec::new()
        } else {
            waiters_for(&st, &queue)
        };
        drop(st);
        for waker in wakers {
            waker.wake();
        }
        super::ready(Ok(id))
    }

    fn dequeue(&self, queues: &[String], timeout: Duration) -> StoreFuture<Option<Job>> {
        Box::pin(DequeueFut {
            inner: Arc::clone(&self.inner),
            queues: queues.into(),
            deadline: deadline_in(timeout),
            waiter: None,
        })
    }

    fn ack(&self, job_id: &JobId) -> StoreFuture<()> {
        let mut st = self.inner.lock();
        let Some(flight) = st.inflight.remove(job_id) else {
            return super::ready(Err(AdapterError::NotFound(format!(
                "unknown or expired lease {job_id}"
            ))));
        };
        if let Some(key) = flight.entry.job.unique {
            st.unique.remove(&key);
        }
        super::ready(Ok(()))
    }

    fn nack(&self, job_id: &JobId, retry_at: Option<SystemTime>) -> StoreFuture<()> {
        let mut st = self.inner.lock();
        let Some(flight) = st.inflight.remove(job_id) else {
            return super::ready(Err(AdapterError::NotFound(format!(
                "unknown or expired lease {job_id}"
            ))));
        };
        let mut entry = flight.entry;
        entry.job.run_at = retry_at;
        let queue: Box<str> = entry.job.queue.clone().into_boxed_str();
        let immediate = retry_at.is_none_or(|at| at <= SystemTime::now());
        if !immediate {
            st.events
                .entry(to_instant(retry_at.unwrap_or_else(SystemTime::now)))
                .or_default()
                .push(TimerEvent::All);
            self.inner.wake_timer.notify_one();
        }
        st.pending
            .entry(queue.clone())
            .or_default()
            .push_back(entry);
        let wakers = if immediate {
            waiters_for(&st, &queue)
        } else {
            Vec::new()
        };
        drop(st);
        for waker in wakers {
            waker.wake();
        }
        super::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;
    use std::task::Wake;

    /// Drive a future to completion on the current thread — a minimal
    /// `block_on` for the store's unit tests (the conformance suite has
    /// its own).
    fn block_on<F: Future>(fut: F) -> F::Output {
        struct Parker(thread::Thread);
        impl Wake for Parker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Parker(thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => thread::park(),
            }
        }
    }

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn object_safe_send_sync() {
        let store: Arc<dyn JobQueue> = Arc::new(MemoryJobQueue::new());
        assert_send_sync(&store);
    }

    #[test]
    fn enqueue_dequeue_ack_roundtrip() {
        let store = MemoryJobQueue::new();
        let id = block_on(store.enqueue(Job::new("emails", b"payload".to_vec()))).unwrap();
        let job = block_on(store.dequeue(&["emails".to_string()], Duration::ZERO))
            .unwrap()
            .unwrap();
        assert_eq!(job.id, Some(id.clone()));
        assert_eq!(job.payload, b"payload");
        assert_eq!(job.attempts, 1);
        block_on(store.ack(&id)).unwrap();
        assert_eq!(store.tracked(), 0);
    }

    #[test]
    fn unacked_job_redelivers_after_lease() {
        let store = MemoryJobQueue::with_limits(Duration::from_millis(30), 16);
        block_on(store.enqueue(Job::new("q", vec![1]))).unwrap();
        let first = block_on(store.dequeue(&["q".to_string()], Duration::ZERO))
            .unwrap()
            .unwrap();
        assert_eq!(first.attempts, 1);
        thread::sleep(Duration::from_millis(50));
        let second = block_on(store.dequeue(&["q".to_string()], Duration::ZERO))
            .unwrap()
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.attempts, 2);
    }

    #[test]
    fn dequeue_waits_for_enqueue() {
        let store = Arc::new(MemoryJobQueue::new());
        let worker = {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                block_on(store.dequeue(&["q".to_string()], Duration::from_secs(5)))
                    .unwrap()
                    .unwrap()
            })
        };
        thread::sleep(Duration::from_millis(20));
        block_on(store.enqueue(Job::new("q", vec![7]))).unwrap();
        let job = worker.join().unwrap();
        assert_eq!(job.payload, vec![7]);
    }

    #[test]
    fn dequeue_times_out() {
        let store = MemoryJobQueue::new();
        let start = Instant::now();
        let none = block_on(store.dequeue(&["q".to_string()], Duration::from_millis(40))).unwrap();
        assert!(none.is_none());
        assert!(start.elapsed() >= Duration::from_millis(35));
    }

    #[test]
    fn capacity_bound_fails_enqueue() {
        let store = MemoryJobQueue::with_limits(Duration::from_secs(1), 2);
        block_on(store.enqueue(Job::new("q", vec![]))).unwrap();
        block_on(store.enqueue(Job::new("q", vec![]))).unwrap();
        match block_on(store.enqueue(Job::new("q", vec![]))) {
            Err(AdapterError::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn store_drop_stops_timer() {
        let store = MemoryJobQueue::new();
        drop(store); // must not hang: the timer exits on `shutdown`.
    }
}
