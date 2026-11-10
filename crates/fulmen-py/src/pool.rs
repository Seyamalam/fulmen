//! The sync handler thread pool (DESIGN.md §7.3).
//!
//! A fixed set of OS threads pulls [`SyncJob`]s off a bounded queue. When the
//! queue is full, or a job waited longer than the queue deadline, dispatch
//! sheds the load with `503` + `Retry-After` instead of piling up
//! (DESIGN.md §7.6, §14.4). The tokio thread that owns the connection never
//! blocks: it awaits the job's oneshot channel.
//!
//! Workers hold the GIL only around the Python call itself — never across
//! I/O, the queue lock, or response construction. A panic inside a job is
//! caught with [`catch_unwind`] and becomes a `500`; the worker keeps
//! running (DESIGN.md §14.6).

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::convert::{self, CallOutcome, ParamArg, PyCtx};

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
    /// Completes with the wire response.
    pub(crate) respond: oneshot::Sender<fulmen_server::Response>,
}

/// Internal result of running one job.
enum JobResult {
    /// The handler produced a converted return value.
    Return(convert::PyReturn),
    /// The job waited past the queue deadline; answer 503 without calling.
    Overdue,
    /// The call failed and was logged (or the interpreter is gone).
    Failed,
}

struct PoolState {
    jobs: VecDeque<SyncJob>,
    closed: bool,
}

struct PoolShared {
    state: Mutex<PoolState>,
    condvar: Condvar,
    capacity: usize,
    deadline: Duration,
    logger: Arc<Py<PyAny>>,
    response_cls: Arc<Py<PyAny>>,
}

impl PoolShared {
    /// Lock the queue, recovering from poison so one panicking worker can
    /// never wedge the pool.
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
    /// `logger` and `response_cls` are cached Python objects the workers need
    /// when attached; keeping them here avoids a lookup per job.
    ///
    /// # Errors
    /// `io::Error` if a worker thread cannot be spawned.
    pub(crate) fn new(
        threads: usize,
        capacity: usize,
        deadline: Duration,
        logger: Arc<Py<PyAny>>,
        response_cls: Arc<Py<PyAny>>,
    ) -> io::Result<Self> {
        let shared = Arc::new(PoolShared {
            state: Mutex::new(PoolState {
                jobs: VecDeque::new(),
                closed: false,
            }),
            condvar: Condvar::new(),
            capacity,
            deadline,
            logger,
            response_cls,
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
        self.shared.condvar.notify_one();
        Ok(())
    }
}

impl Drop for SyncPool {
    /// Close the queue. Queued jobs still drain; workers exit once empty.
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        drop(state);
        self.shared.condvar.notify_all();
    }
}

fn worker_loop(shared: &PoolShared) {
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                if state.closed {
                    return;
                }
                state = shared
                    .condvar
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        run_job(shared, job);
    }
}

/// Run one job: deadline check, GIL attach around the Python call only, wire
/// response built outside the attach, panic boundary per DESIGN.md §14.6.
fn run_job(shared: &PoolShared, job: SyncJob) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if job.enqueued.elapsed() > shared.deadline {
            return JobResult::Overdue;
        }
        match Python::try_attach(|py| {
            convert::call_and_convert(
                py,
                &job.callable,
                &job.args,
                &PyCtx {
                    logger: &shared.logger,
                    response_cls: &shared.response_cls,
                    label: &job.label,
                },
            )
        }) {
            Some(CallOutcome::Return(ret)) => JobResult::Return(ret),
            Some(CallOutcome::Failed) | None => JobResult::Failed,
        }
    }));
    let response = match outcome {
        Ok(JobResult::Return(ret)) => convert::into_response(ret),
        Ok(JobResult::Overdue) => service_unavailable(),
        Ok(JobResult::Failed) => convert::internal_error(),
        Err(payload) => {
            // A panic here is a bug in the bridge, not in user code; log it
            // through Python when the interpreter is still reachable.
            let message = panic_message(&payload);
            let _ = Python::try_attach(|py| {
                convert::log_message(
                    py,
                    &shared.logger,
                    format!("{}: panic in handler dispatch: {message}", job.label),
                );
            });
            convert::internal_error()
        }
    };
    let _ = job.respond.send(response);
}

/// `503` + `Retry-After` for shed load (DESIGN.md §14.4).
fn service_unavailable() -> fulmen_server::Response {
    let mut response = convert::internal_error();
    response.status = http::StatusCode::SERVICE_UNAVAILABLE;
    response.body = bytes::Bytes::from_static(b"Service Unavailable");
    response.headers.insert(
        http::header::RETRY_AFTER,
        http::header::HeaderValue::from_static("1"),
    );
    response
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
