//! The sync handler thread pool (DESIGN.md §7.3, §7.5).
//!
//! A fixed set of OS threads pulls [`SyncJob`]s off a bounded queue. When the
//! queue is full, or a job waited longer than the queue deadline, dispatch
//! sheds the load with `503` + `Retry-After` instead of piling up
//! (DESIGN.md §7.6, §14.4). The tokio thread that owns the connection never
//! blocks: it awaits the job's oneshot channel.
//!
//! Workers hold the GIL only around the Python calls themselves — never
//! across I/O, the queue lock, or response construction. When more jobs are
//! queued than the pool could serve in parallel, a worker takes up to
//! `batch_size` of the surplus and runs them under ONE `Python::attach`
//! (GIL batching, DESIGN.md §7.5); `batch_size = 1` disables batching
//! (free-threaded builds). Batches assemble only from the surplus over
//! `workers - 1` queued jobs — real backlog — so a small concurrent burst
//! still spreads across the pool instead of serializing behind one
//! worker's GIL hold, and assembling never waits (a wait would only delay
//! the job already in hand). A panic inside a job is caught with
//! [`catch_unwind`] and becomes a `500` for that job only; the worker and
//! the rest of the batch keep running (DESIGN.md §14.6).

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::convert::{self, CallOutcome, DeferredCall, ErrorTable, JobAnswer, ParamArg, PyCtx};
use crate::dispatch::ResponseContract;

/// One unit of pool work: call `callable` with `args` and answer on
/// `respond`.
#[derive(Debug)]
pub(crate) struct SyncJob {
    /// The handler object, `Arc`-wrapped so dispatch can share it without
    /// taking the GIL to clone.
    pub(crate) callable: Arc<Py<PyAny>>,
    /// Ordered call arguments (path parameters, signature order).
    pub(crate) args: Box<[ParamArg]>,
    /// `"{METHOD} {path}"` for log lines.
    pub(crate) label: Box<str>,
    /// When the request entered the queue (for the queue deadline).
    pub(crate) enqueued: Instant,
    /// The route's declared response contract — picks the encode
    /// descriptor by actual status.
    pub(crate) contract: Arc<ResponseContract>,
    /// `-> None`/`-> NoContent` declared: `None` returns answer `204`.
    pub(crate) none_ok: bool,
    /// `false` on `on_error`-handler jobs: their failures never defer again.
    pub(crate) route_errors: bool,
    /// Completes with the wire response or a deferred `on_error` call.
    pub(crate) respond: oneshot::Sender<JobAnswer>,
}

/// Internal result of running one job.
enum JobResult {
    /// The handler produced a converted return value.
    Return(convert::PyReturn),
    /// The job waited past the queue deadline; answer 503 without calling.
    Overdue,
    /// The handler raised an exception the `on_error` table claims — the
    /// dispatcher re-submits the error handler onto its own executor.
    Deferred(DeferredCall),
    /// The call failed and was logged (or the interpreter is gone).
    Failed,
}

struct PoolState<J> {
    jobs: VecDeque<J>,
    closed: bool,
}

struct PoolShared {
    state: Mutex<PoolState<SyncJob>>,
    /// Wakes one worker parked in `take_batch`'s job wait.
    wait_cv: Condvar,
    /// Total worker threads; a batch only takes jobs beyond `workers - 1`
    /// queued, so a queued job can never sit while a sibling goes idle
    /// (§7.5).
    workers: usize,
    capacity: usize,
    deadline: Duration,
    batch_size: usize,
    logger: Arc<Py<PyAny>>,
    response_cls: Arc<Py<PyAny>>,
    /// The app's `@app.on_error` table (§17).
    errors: Arc<ErrorTable>,
    /// `Fulmen(debug=True)`/`serve(debug=…)`: every handler result is
    /// validated against its declared response descriptor (§14.2).
    debug: bool,
}

impl PoolShared {
    /// Lock the queue, recovering from poison so one panicking worker can
    /// never wedge the pool.
    fn lock(&self) -> MutexGuard<'_, PoolState<SyncJob>> {
        lock(&self.state)
    }
}

/// Lock a queue, recovering from poison.
fn lock<J>(state: &Mutex<PoolState<J>>) -> MutexGuard<'_, PoolState<J>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why [`SyncPool::try_submit`] rejected a job.
#[derive(Debug)]
pub(crate) enum SubmitError {
    /// The queue is full or the pool is shutting down.
    Full,
}

/// A bounded pool of OS threads running `def` handlers.
///
/// Dropping the pool closes the queue: pending submissions are rejected,
/// queued jobs are drained, and the worker threads exit. The workers are not
/// joined — a wedged handler cannot block shutdown; detached threads end on
/// their own once the queue is empty.
pub(crate) struct SyncPool {
    shared: Arc<PoolShared>,
}

impl SyncPool {
    /// Spawn `threads` workers pulling from a queue bounded to `capacity`,
    /// shedding jobs that wait longer than `deadline`.
    ///
    /// `batch_size` controls GIL batching (§7.5): a worker that finds more
    /// jobs queued than the rest of the pool could take in parallel gathers
    /// up to `batch_size` of that surplus and runs it under one
    /// `Python::attach`. `batch_size <= 1` never batches.
    ///
    /// `logger` and `response_cls` are cached Python objects the workers need
    /// when attached; keeping them here avoids a lookup per job.
    ///
    /// `debug` turns on §14.2 output validation for every job the pool runs.
    ///
    /// # Errors
    /// `io::Error` if a worker thread cannot be spawned.
    pub(crate) fn new(
        threads: usize,
        capacity: usize,
        deadline: Duration,
        batch_size: usize,
        logger: Arc<Py<PyAny>>,
        response_cls: Arc<Py<PyAny>>,
        errors: Arc<ErrorTable>,
        debug: bool,
    ) -> io::Result<Self> {
        let shared = Arc::new(PoolShared {
            state: Mutex::new(PoolState {
                jobs: VecDeque::new(),
                closed: false,
            }),
            wait_cv: Condvar::new(),
            workers: threads.max(1),
            capacity,
            deadline,
            batch_size: batch_size.max(1),
            logger,
            response_cls,
            errors,
            debug,
        });
        for index in 0..threads {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name(format!("fulmen-sync-{index}"))
                .spawn(move || worker_loop(&shared))?;
        }
        Ok(Self { shared })
    }

    /// Queue `job` for a worker, or reject when the queue is full (or the
    /// pool is closing) so the caller can answer `503`.
    ///
    /// # Errors
    /// [`SubmitError::Full`] when the bounded queue has no room.
    pub(crate) fn try_submit(&self, job: SyncJob) -> Result<(), SubmitError> {
        let mut state = self.shared.lock();
        if state.closed || state.jobs.len() >= self.shared.capacity {
            return Err(SubmitError::Full);
        }
        state.jobs.push_back(job);
        drop(state);
        self.shared.wait_cv.notify_one();
        Ok(())
    }
}

impl Drop for SyncPool {
    /// Close the queue. Queued jobs still drain; workers exit once empty.
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        drop(state);
        self.shared.wait_cv.notify_all();
    }
}

fn worker_loop(shared: &PoolShared) {
    loop {
        let jobs = take_batch(
            &shared.state,
            &shared.wait_cv,
            shared.workers,
            shared.batch_size,
        );
        if jobs.is_empty() {
            return;
        }
        run_jobs(shared, jobs);
    }
}

/// Take the next batch of jobs off the queue (DESIGN.md §7.5).
///
/// Waits for the first job on `wait_cv`; an empty queue on a closed pool
/// returns an empty `Vec` (the worker then exits). With `max_batch > 1` the
/// worker then gathers jobs queued *beyond* what the rest of the pool
/// could run in parallel — `jobs.len() - (workers - 1)` — up to
/// `max_batch` in total. Batching only kicks in under real backlog: at or
/// below one queued job per sibling the batch is a singleton, so a
/// concurrent burst still spreads across the pool. Assembly never waits —
/// a straggler belongs to whichever sibling frees next, and delaying the
/// job already in hand would be pure latency.
fn take_batch<J>(
    state: &Mutex<PoolState<J>>,
    wait_cv: &Condvar,
    workers: usize,
    max_batch: usize,
) -> Vec<J> {
    let mut state = lock(state);
    let first = loop {
        if let Some(job) = state.jobs.pop_front() {
            break job;
        }
        if state.closed {
            return Vec::new();
        }
        state = wait_cv.wait(state).unwrap_or_else(PoisonError::into_inner);
    };
    let mut jobs = vec![first];
    let surplus = state.jobs.len().saturating_sub(workers.saturating_sub(1));
    for _ in 0..surplus.min(max_batch.saturating_sub(1)) {
        match state.jobs.pop_front() {
            Some(job) => jobs.push(job),
            None => break,
        }
    }
    jobs
}

/// Run one batch: deadline check per job, one `Python::attach` for the whole
/// batch, panic boundary per job (DESIGN.md §7.5, §14.6). The wire responses
/// are built and sent after the attach is released.
fn run_jobs(shared: &PoolShared, jobs: Vec<SyncJob>) {
    let Some(mut outcomes) = Python::try_attach(|py| {
        jobs.into_iter()
            .map(|job| {
                let result = execute_job(py, shared, &job);
                (job, result)
            })
            .collect::<Vec<(SyncJob, JobResult)>>()
    }) else {
        // The interpreter is gone; dropping the jobs closes their oneshots,
        // which dispatch maps to `500`.
        return;
    };
    for (job, result) in outcomes.drain(..) {
        let answer = match result {
            JobResult::Return(ret) => JobAnswer::Response(convert::into_response(ret)),
            JobResult::Overdue => JobAnswer::Response(convert::service_unavailable()),
            JobResult::Deferred(call) => JobAnswer::Deferred(call),
            JobResult::Failed => JobAnswer::Response(convert::internal_error()),
        };
        let _ = job.respond.send(answer);
    }
}

/// Run `job` while attached: the queue-deadline check, then the call and
/// conversion, with a panic boundary so one bad handler cannot kill the
/// worker or the rest of the batch (§14.6).
fn execute_job(py: Python<'_>, shared: &PoolShared, job: &SyncJob) -> JobResult {
    if job.enqueued.elapsed() > shared.deadline {
        return JobResult::Overdue;
    }
    let ctx = PyCtx {
        logger: &shared.logger,
        response_cls: &shared.response_cls,
        errors: &shared.errors,
        none_ok: job.none_ok,
        route_errors: job.route_errors,
        label: &job.label,
        contract: &job.contract,
        debug: shared.debug,
    };
    match catch_unwind(AssertUnwindSafe(|| {
        convert::call_and_convert(py, &job.callable, &job.args, &ctx)
    })) {
        Ok(CallOutcome::Return(ret)) => JobResult::Return(ret),
        Ok(CallOutcome::Deferred(call)) => JobResult::Deferred(call),
        Ok(CallOutcome::Failed) => JobResult::Failed,
        Err(payload) => {
            // A panic here is a bug in the bridge, not in user code.
            let message = panic_message(&payload);
            convert::log_message(
                py,
                &shared.logger,
                format!("{}: panic in handler dispatch: {message}", job.label),
            );
            JobResult::Failed
        }
    }
}

/// Best-effort panic payload text for the log line.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        (*msg).to_owned()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with<J>(jobs: Vec<J>, closed: bool) -> Mutex<PoolState<J>> {
        Mutex::new(PoolState {
            jobs: jobs.into_iter().collect(),
            closed,
        })
    }

    #[test]
    fn batch_size_one_never_collects() {
        let state = state_with(vec![1, 2, 3, 4], false);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 4, 1);
        assert_eq!(batch, vec![1]);
        assert_eq!(lock(&state).jobs.len(), 3);
    }

    #[test]
    fn batch_drains_available_up_to_size() {
        // One worker, so every queued job is surplus it may take.
        let state = state_with(vec![1, 2, 3, 4, 5], false);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 1, 3);
        assert_eq!(batch, vec![1, 2, 3]);
        assert_eq!(lock(&state).jobs.len(), 2);
    }

    #[test]
    fn batch_leaves_jobs_for_siblings() {
        // Two workers, two jobs: the surplus over `workers - 1` is zero,
        // so the batch is a singleton and the sibling gets the second job.
        let state = state_with(vec![1, 2], false);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 2, 8);
        assert_eq!(batch, vec![1]);
        assert_eq!(lock(&state).jobs.len(), 1);
    }

    #[test]
    fn batch_takes_only_surplus_over_siblings() {
        // Three workers, four jobs: after the first job the surplus is
        // 3 - 2 = 1, so the batch is two jobs, leaving one per sibling.
        let state = state_with(vec![1, 2, 3, 4], false);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 3, 8);
        assert_eq!(batch, vec![1, 2]);
        assert_eq!(lock(&state).jobs.len(), 2);
    }

    #[test]
    fn batch_caps_at_max_batch() {
        // Deep backlog: the batch still stops at `max_batch`.
        let state = state_with(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10], false);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 2, 4);
        assert_eq!(batch, vec![1, 2, 3, 4]);
        assert_eq!(lock(&state).jobs.len(), 6);
    }

    #[test]
    fn batch_empty_closed_queue_returns_empty() {
        let state = state_with::<i32>(vec![], true);
        let cv = Condvar::new();
        assert!(take_batch(&state, &cv, 4, 8).is_empty());
    }

    #[test]
    fn batch_closed_pool_still_drains_queued_jobs() {
        let state = state_with(vec![1, 2, 3], true);
        let cv = Condvar::new();
        let batch = take_batch(&state, &cv, 1, 8);
        assert_eq!(batch, vec![1, 2, 3]);
    }
}
