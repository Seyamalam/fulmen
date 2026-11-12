//! The asyncio bridge (DESIGN.md §7.4, decision D4).
//!
//! One dedicated OS thread per compiled app runs a real `asyncio` event
//! loop (uvloop when installed and enabled — see [`LoopImpl`]). Dispatch
//! submits a [`AsyncJob`] onto a bounded queue; when the queue goes from
//! empty to non-empty the submitter takes the GIL once to run
//! `loop.call_soon_threadsafe(pump)`, where `pump` is an internal
//! `#[pyclass]` living on the loop. The pump drains the queue into
//! `loop.create_task`, registers each job's `tokio::sync::oneshot` in a
//! `job_id -> sender` map, and attaches `partial(pump.complete, job_id)`
//! as the task's `done` callback. `complete` calls `task.result()`,
//! converts it with the same machinery as the sync path, and fires the
//! oneshot the dispatch future is awaiting.
//!
//! GIL discipline: the loop thread holds the GIL only while asyncio runs
//! its callbacks; the submitter holds it only for the `call_soon_threadsafe`
//! wake (skipped entirely when the queue is non-empty, which is most of
//! the time under load); the dispatch future itself never touches Python.
//! Queue-deadline and capacity shedding answer `503` + `Retry-After`
//! exactly like the sync pool (§14.4).

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use pyo3::types::PyModule;
use tokio::sync::oneshot;

use crate::app::StartupError;
use crate::convert::{self, CallOutcome, ErrorTable, InvokeError, JobAnswer, ParamArg, PyCtx};
use crate::dispatch::ResponseContract;

/// How long [`AsyncLoop::start`] waits for the loop thread to report ready.
const LOOP_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Most jobs one pump call turns into tasks. The pump runs *inside* the
/// event loop, so an unbounded drain under sustained load would never
/// yield: `call_soon_threadsafe` keeps the queue non-empty forever and the
/// loop wedges (no task ever runs). After this many jobs the pump reschedules
/// itself, letting the loop iterate — start tasks, fire callbacks — between
/// chunks.
const PUMP_DRAIN_LIMIT: usize = 64;

/// Which event-loop implementation the bridge runs (§7.4, M1.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopImpl {
    /// `asyncio.new_event_loop()`.
    Asyncio,
    /// `uvloop.new_event_loop()`.
    Uvloop,
}

/// Resolve the tri-state `uvloop` flag into a [`LoopImpl`] (§7.4, M1.16):
/// `None` is auto — uvloop when importable, plain asyncio otherwise;
/// `Some(true)` requires uvloop and fails startup when it is missing;
/// `Some(false)` forces asyncio.
///
/// # Errors
/// [`StartupError`] when `uvloop` is `Some(true)` but `uvloop` is not
/// importable.
pub(crate) fn resolve_loop_impl(py: Python<'_>, uvloop: Option<bool>) -> PyResult<LoopImpl> {
    match uvloop {
        Some(false) => Ok(LoopImpl::Asyncio),
        Some(true) => PyModule::import(py, "uvloop").map_or_else(
            |_| {
                Err(StartupError::new_err(
                    "uvloop=True but the `uvloop` package is not installed",
                ))
            },
            |_| Ok(LoopImpl::Uvloop),
        ),
        None => Ok(if PyModule::import(py, "uvloop").is_ok() {
            LoopImpl::Uvloop
        } else {
            LoopImpl::Asyncio
        }),
    }
}

/// Why [`AsyncLoop::submit`] rejected a job.
#[derive(Debug)]
pub(crate) enum AsyncSubmitError {
    /// The submission queue is full or the bridge is closing; shed with
    /// `503` + `Retry-After` (§14.4).
    Full,
    /// The loop thread is gone (or was never built); answer `500`.
    Unavailable,
}

/// A submitted unit of work: run `callable(*args)` as a coroutine and
/// answer on `respond`.
struct AsyncJob {
    /// Unique id linking the task to its [`Pending`] entry.
    id: u64,
    /// The `async def` handler object.
    callable: Arc<Py<PyAny>>,
    /// Ordered call arguments (already kw-flagged).
    args: Box<[ParamArg]>,
    /// `"{METHOD} {path}"` for log lines.
    label: Box<str>,
    /// When the request entered the queue (queue-deadline basis).
    enqueued: Instant,
    /// The route's declared response contract — picks the encode
    /// descriptor by actual status.
    contract: Arc<ResponseContract>,
    /// `-> None`/`-> NoContent` declared: `None` returns answer `204`.
    none_ok: bool,
    /// `false` on `on_error`-handler jobs: their failures never defer again.
    route_errors: bool,
    /// Completes with the wire response or a deferred `on_error` call.
    respond: oneshot::Sender<JobAnswer>,
}

/// The `job_id -> completion` entry kept while a task runs.
struct Pending {
    /// `"{METHOD} {path}"` for log lines in `complete`.
    label: Box<str>,
    /// The handler object, kept for error log lines.
    callable: Arc<Py<PyAny>>,
    /// The route's declared response contract, for `complete`'s ctx.
    contract: Arc<ResponseContract>,
    /// The job's `none_ok`/`route_errors` flags, for `complete`'s ctx.
    none_ok: bool,
    /// `false` on `on_error`-handler jobs.
    route_errors: bool,
    /// Completes with the wire response or a deferred `on_error` call.
    respond: oneshot::Sender<JobAnswer>,
}

struct AsyncQueue {
    jobs: VecDeque<AsyncJob>,
    closed: bool,
}

/// Python objects the pump needs to drive the loop. Stored once the loop
/// thread has built them; cleared on [`AsyncLoop`] drop so the
/// `shared -> runtime -> pump -> shared` reference cycle breaks.
struct AsyncRuntime {
    /// `loop.call_soon_threadsafe`.
    call_soon: Py<PyAny>,
    /// The `AsyncPump` instance (the `call_soon_threadsafe` callback).
    pump: Py<PyAny>,
    /// `pump.complete` bound method, for `partial(completer, job_id)`.
    completer: Py<PyAny>,
}

/// Startup handshake from the loop thread: `slot` carries the result and
/// `condvar` wakes the waiting `start`. `Mutex` + `Condvar`, not
/// `std::sync::mpsc`: `mpsc::recv` parks through libdispatch on macOS,
/// which aborts processes forked from a multi-threaded parent.
struct Ready {
    slot: Mutex<Option<Result<(), String>>>,
    condvar: Condvar,
}

impl Ready {
    /// Record the loop thread's startup result and wake `start`.
    fn report(&self, result: Result<(), String>) {
        *lock(&self.slot) = Some(result);
        self.condvar.notify_one();
    }

    /// Wait for [`Ready::report`] up to `timeout`; `None` on timeout.
    fn wait(&self, timeout: Duration) -> Option<Result<(), String>> {
        let mut slot = lock(&self.slot);
        let deadline = Instant::now() + timeout;
        loop {
            if slot.is_some() {
                return slot.take();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self
                .condvar
                .wait_timeout(slot, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            slot = guard;
        }
    }
}

/// Everything shared between the loop thread and the submitting dispatch
/// futures.
struct AsyncShared {
    queue: Mutex<AsyncQueue>,
    pending: Mutex<HashMap<u64, Pending>>,
    runtime: Mutex<Option<AsyncRuntime>>,
    next_id: AtomicU64,
    capacity: usize,
    deadline: Duration,
    logger: Arc<Py<PyAny>>,
    response_cls: Arc<Py<PyAny>>,
    /// The app's `@app.on_error` table (§17).
    errors: Arc<ErrorTable>,
    /// `Fulmen(debug=True)`/`serve(debug=…)`: every handler result is
    /// validated against its declared response descriptor (§14.2).
    debug: bool,
}

/// Lock a mutex, recovering from poison.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AsyncShared {
    fn lock_queue(&self) -> MutexGuard<'_, AsyncQueue> {
        lock(&self.queue)
    }

    fn lock_pending(&self) -> MutexGuard<'_, HashMap<u64, Pending>> {
        lock(&self.pending)
    }

    fn is_closed(&self) -> bool {
        self.lock_queue().closed
    }

    /// `pump.complete` bound method, cloned under the GIL.
    fn completer(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        lock(&self.runtime)
            .as_ref()
            .map(|rt| rt.completer.clone_ref(py))
    }

    /// Wake the loop with `call_soon_threadsafe(pump)`. `None` when the
    /// runtime is gone (loop thread dead, or [`AsyncLoop`] dropped).
    ///
    /// Lock discipline: the `runtime` guard is dropped *before* the Python
    /// call. `call1` can release the GIL mid-call (`call_soon_threadsafe`
    /// blocks on a self-pipe `send`), so holding a mutex across it is a
    /// GIL↔mutex deadlock against a second submitter that attached first.
    fn wake(&self, py: Python<'_>) -> Option<PyResult<()>> {
        let (call_soon, pump) = {
            let runtime = lock(&self.runtime);
            let rt = runtime.as_ref()?;
            (rt.call_soon.clone_ref(py), rt.pump.clone_ref(py))
        };
        Some(call_soon.call1(py, (pump,)).map(|_| ()))
    }

    /// Close the queue and drop every queued job and pending sender; the
    /// matching dispatch futures answer `500` on their closed oneshot.
    fn close_and_drain(&self) {
        let mut queue = self.lock_queue();
        queue.closed = true;
        queue.jobs.clear();
        drop(queue);
        self.lock_pending().clear();
    }
}

/// Handle to the asyncio loop thread backing one compiled app.
///
/// Dropping closes the queue and wakes the loop once more so the pump sees
/// `closed` and stops `run_forever`; the thread exits after draining.
pub(crate) struct AsyncLoop {
    shared: Arc<AsyncShared>,
}

impl AsyncLoop {
    /// Spawn the loop thread, build the loop there, and block until it is
    /// ready (or reports failure). The GIL is released while waiting so the
    /// thread can attach.
    ///
    /// # Errors
    /// [`StartupError`] when the thread cannot spawn, `attach` fails, or
    /// building the loop raises.
    pub(crate) fn start(
        py: Python<'_>,
        loop_impl: LoopImpl,
        capacity: usize,
        deadline: Duration,
        logger: Arc<Py<PyAny>>,
        response_cls: Arc<Py<PyAny>>,
        errors: Arc<ErrorTable>,
        debug: bool,
    ) -> PyResult<Self> {
        let shared = Arc::new(AsyncShared {
            queue: Mutex::new(AsyncQueue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            pending: Mutex::new(HashMap::new()),
            runtime: Mutex::new(None),
            next_id: AtomicU64::new(0),
            capacity,
            deadline,
            logger,
            response_cls,
            errors,
            debug,
        });
        let ready = Arc::new(Ready {
            slot: Mutex::new(None),
            condvar: Condvar::new(),
        });
        thread::Builder::new()
            .name("fulmen-asyncio".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                let ready = Arc::clone(&ready);
                move || loop_thread_main(&shared, loop_impl, &ready)
            })
            .map_err(|err| {
                StartupError::new_err(format!("failed to spawn the asyncio loop thread: {err}"))
            })?;

        // The loop thread needs the GIL to build its loop; release ours.
        match py.detach(move || ready.wait(LOOP_READY_TIMEOUT)) {
            Some(Ok(())) => Ok(Self { shared }),
            Some(Err(message)) => Err(StartupError::new_err(format!(
                "failed to start the asyncio loop: {message}"
            ))),
            None => Err(StartupError::new_err(
                "the asyncio loop thread did not report ready in time",
            )),
        }
    }

    /// Queue `callable` for the loop, answering `respond` when the coroutine
    /// completes. Wakes the loop via `call_soon_threadsafe` only on the
    /// empty-to-non-empty transition (the pump drains the rest itself).
    ///
    /// # Errors
    /// [`AsyncSubmitError::Full`] when the bounded queue has no room;
    /// [`AsyncSubmitError::Unavailable`] when the loop is gone.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per job field; the job itself is the loop's type"
    )]
    pub(crate) fn submit(
        &self,
        callable: Arc<Py<PyAny>>,
        args: Box<[ParamArg]>,
        label: Box<str>,
        contract: Arc<ResponseContract>,
        none_ok: bool,
        route_errors: bool,
        respond: oneshot::Sender<JobAnswer>,
    ) -> Result<(), AsyncSubmitError> {
        let wake = {
            let mut queue = self.shared.lock_queue();
            if queue.closed || queue.jobs.len() >= self.shared.capacity {
                return Err(AsyncSubmitError::Full);
            }
            let was_empty = queue.jobs.is_empty();
            queue.jobs.push_back(AsyncJob {
                id: self.shared.next_id.fetch_add(1, Ordering::Relaxed),
                callable,
                args,
                label,
                enqueued: Instant::now(),
                contract,
                none_ok,
                route_errors,
                respond,
            });
            was_empty
        };
        if !wake {
            return Ok(());
        }
        if let Some(Some(Ok(()))) = Python::try_attach(|py| self.shared.wake(py)) {
            return Ok(());
        }
        // Loop not built yet (impossible after `start` returns) or torn
        // down, the GIL is gone, or `call_soon_threadsafe` raised: the job
        // can never run, so close and drop everything queued.
        self.shared.close_and_drain();
        Err(AsyncSubmitError::Unavailable)
    }
}

impl Drop for AsyncLoop {
    /// Close the queue and wake the loop so the pump calls `loop.stop()`;
    /// clears the runtime handle so the `shared <-> pump` cycle breaks.
    fn drop(&mut self) {
        self.shared.lock_queue().closed = true;
        let runtime = lock(&self.shared.runtime).take();
        if let Some(rt) = runtime {
            let _ = Python::try_attach(|py| rt.call_soon.call1(py, (rt.pump.clone_ref(py),)));
        }
    }
}

/// The loop thread body: build the loop and pump, publish them, run the
/// loop until the pump stops it, then close the loop and shed whatever is
/// still queued.
fn loop_thread_main(shared: &Arc<AsyncShared>, loop_impl: LoopImpl, ready: &Ready) {
    let attached = Python::try_attach(|py| match setup_loop(py, shared, loop_impl) {
        Ok(loop_obj) => {
            ready.report(Ok(()));
            if let Err(err) = loop_obj.call_method0("run_forever") {
                convert::log_message(
                    py,
                    &shared.logger,
                    format!("asyncio loop thread failed: {err}"),
                );
            }
            let _ = loop_obj.call_method0("close");
        }
        Err(err) => {
            ready.report(Err(err.to_string()));
        }
    });
    if attached.is_none() {
        ready.report(Err("the Python interpreter is unavailable".to_owned()));
    }
    // Whatever is left (early failure or shutdown) cannot run.
    shared.close_and_drain();
}

/// Build the event loop, install it, and wrap the pump. The single
/// loop-creation site for the bridge (§7.4: "keep the loop creation in one
/// place so M1.28 can flip loop policy").
fn setup_loop<'py>(
    py: Python<'py>,
    shared: &Arc<AsyncShared>,
    loop_impl: LoopImpl,
) -> PyResult<Bound<'py, PyAny>> {
    let asyncio_mod = PyModule::import(py, "asyncio")?;
    let loop_obj = match loop_impl {
        LoopImpl::Asyncio => asyncio_mod.call_method0("new_event_loop")?,
        LoopImpl::Uvloop => PyModule::import(py, "uvloop")?.call_method0("new_event_loop")?,
    };
    asyncio_mod.call_method1("set_event_loop", (&loop_obj,))?;
    // Register the thread with `threading` under a readable name so thread
    // dumps and `threading.enumerate()` show `fulmen-asyncio`, not a dummy
    // `Thread-N`.
    PyModule::import(py, "threading")?
        .call_method0("current_thread")?
        .setattr("name", "fulmen-asyncio")?;

    let pump = AsyncPump {
        shared: Arc::clone(shared),
        create_task: loop_obj.getattr("create_task")?.unbind(),
        stop_loop: loop_obj.getattr("stop")?.unbind(),
        partial: PyModule::import(py, "functools")?
            .getattr("partial")?
            .unbind(),
    };
    let pump_obj = Py::new(py, pump)?.into_any();
    let completer = pump_obj.bind(py).getattr("complete")?.unbind();
    let call_soon = loop_obj.getattr("call_soon_threadsafe")?.unbind();
    *lock(&shared.runtime) = Some(AsyncRuntime {
        call_soon,
        pump: pump_obj,
        completer,
    });
    Ok(loop_obj)
}

/// The `call_soon_threadsafe` callback living on the event loop. Internal
/// to the bridge; never registered on the Python module.
///
/// `__call__` drains the submission queue into tasks; `complete` is the
/// `done` callback finishing the matching [`Pending`] entry.
#[pyclass(module = "fulmen._fulmen")]
struct AsyncPump {
    shared: Arc<AsyncShared>,
    /// `loop.create_task`.
    create_task: Py<PyAny>,
    /// `loop.stop`.
    stop_loop: Py<PyAny>,
    /// `functools.partial`.
    partial: Py<PyAny>,
}

#[pymethods]
impl AsyncPump {
    /// Runs on the loop thread: drain up to [`PUMP_DRAIN_LIMIT`] queued jobs
    /// into loop tasks, then reschedule while work remains so the loop still
    /// iterates under load. After the drain, a closed bridge stops
    /// `run_forever`.
    fn __call__(&self, py: Python<'_>) {
        for _ in 0..PUMP_DRAIN_LIMIT {
            let job = self.shared.lock_queue().jobs.pop_front();
            let Some(job) = job else { break };
            let _ = catch_unwind(AssertUnwindSafe(|| self.start_job(py, job)));
        }
        if self.shared.is_closed() {
            let _ = self.stop_loop.call0(py);
            return;
        }
        if !self.shared.lock_queue().jobs.is_empty() {
            // More jobs than one pass allowed: wake the pump again through
            // `call_soon_threadsafe` (safe on the loop thread too). A submit
            // landing between this check and the next wake can only add
            // work — never lose it, because a non-empty queue means the
            // submitter skipped its own wake and relies on this one.
            // The `runtime` guard is dropped before `call1` for the same
            // GIL-releasing reason documented on `wake`.
            let rewake = {
                let runtime = lock(&self.shared.runtime);
                runtime
                    .as_ref()
                    .map(|rt| (rt.call_soon.clone_ref(py), rt.pump.clone_ref(py)))
            };
            if let Some((call_soon, pump)) = rewake {
                let _ = call_soon.call1(py, (pump,));
            }
        }
    }

    /// `done` callback (invoked as `complete(job_id, task)`): pull
    /// `task.result()`, convert it, and fire the job's oneshot. Never
    /// raises: a failed task becomes a `500`, not a Python error.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pymethod arguments must own their extracted value; `&Py` is not extractable"
    )]
    fn complete(&self, py: Python<'_>, job_id: u64, task: Py<PyAny>) {
        let Some(pending) = self.shared.lock_pending().remove(&job_id) else {
            return;
        };
        let ctx = PyCtx {
            logger: &self.shared.logger,
            response_cls: &self.shared.response_cls,
            errors: &self.shared.errors,
            none_ok: pending.none_ok,
            route_errors: pending.route_errors,
            label: &pending.label,
            contract: &pending.contract,
            debug: self.shared.debug,
        };
        let outcome = match task.call_method0(py, "result") {
            Ok(obj) => convert::settle(
                py,
                convert::convert_result(py, obj.bind(py), &ctx).map_err(InvokeError::Internal),
                &pending.callable,
                &ctx,
            ),
            Err(err) => {
                convert::settle(py, Err(InvokeError::Handler(err)), &pending.callable, &ctx)
            }
        };
        let answer = match outcome {
            CallOutcome::Return(ret) => JobAnswer::Response(convert::into_response(ret)),
            CallOutcome::Deferred(call) => JobAnswer::Deferred(call),
            CallOutcome::Failed => JobAnswer::Response(convert::internal_error()),
        };
        let _ = pending.respond.send(answer);
    }
}

impl AsyncPump {
    /// Turn one queued job into a running loop task. Every failure mode
    /// answers the job's oneshot instead of propagating.
    fn start_job(&self, py: Python<'_>, job: AsyncJob) {
        let ctx = PyCtx {
            logger: &self.shared.logger,
            response_cls: &self.shared.response_cls,
            errors: &self.shared.errors,
            none_ok: job.none_ok,
            route_errors: job.route_errors,
            label: &job.label,
            contract: &job.contract,
            debug: self.shared.debug,
        };
        if job.enqueued.elapsed() > self.shared.deadline {
            let _ = job
                .respond
                .send(JobAnswer::Response(convert::service_unavailable()));
            return;
        }
        // Build arguments and call the async def -> coroutine object. A
        // raise here is a handler failure, identical to the sync path.
        let coro = match convert::build_args(py, &job.args).and_then(|(pos, kw)| {
            job.callable
                .call(py, pos, Some(&kw))
                .map_err(InvokeError::Handler)
        }) {
            Ok(coro) => coro,
            Err(err) => {
                let outcome = convert::settle(py, Err(err), &job.callable, &ctx);
                Self::answer(job, outcome);
                return;
            }
        };
        let task = match self.create_task.call1(py, (coro,)) {
            Ok(task) => task,
            Err(err) => {
                let outcome =
                    convert::settle(py, Err(InvokeError::Internal(err)), &job.callable, &ctx);
                Self::answer(job, outcome);
                return;
            }
        };
        let callback = self
            .shared
            .completer(py)
            .and_then(|completer| self.partial.call1(py, (completer, job.id)).ok());
        let Some(bound) = callback else {
            convert::log_message(
                py,
                &self.shared.logger,
                format!(
                    "{}: failed to build the task completion callback",
                    job.label
                ),
            );
            let _ = job
                .respond
                .send(JobAnswer::Response(convert::internal_error()));
            return;
        };
        if let Err(err) = task.call_method1(py, "add_done_callback", (bound,)) {
            let outcome = convert::settle(py, Err(InvokeError::Internal(err)), &job.callable, &ctx);
            Self::answer(job, outcome);
            return;
        }
        // Register after `add_done_callback`: the callback cannot fire until
        // the loop iterates, and this code IS the loop thread, so the entry
        // is always in place before `complete` can look for it.
        self.shared.lock_pending().insert(
            job.id,
            Pending {
                label: job.label.clone(),
                callable: Arc::clone(&job.callable),
                contract: Arc::clone(&job.contract),
                none_ok: job.none_ok,
                route_errors: job.route_errors,
                respond: job.respond,
            },
        );
    }

    /// Send `job`'s answer for an already-settled outcome: a wire
    /// response, or a deferred `on_error` call for the dispatcher.
    fn answer(job: AsyncJob, outcome: CallOutcome) {
        let answer = match outcome {
            CallOutcome::Return(ret) => JobAnswer::Response(convert::into_response(ret)),
            CallOutcome::Deferred(call) => JobAnswer::Deferred(call),
            CallOutcome::Failed => JobAnswer::Response(convert::internal_error()),
        };
        let _ = job.respond.send(answer);
    }
}
