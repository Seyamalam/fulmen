//! The `NativeApp` object shared between the Python `Fulmen` class and the
//! serving entry points, plus [`compile_app`], which turns the recorded
//! declarations into a frozen [`CompiledApp`] and a handler table.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use fulmen_core::{AppDecl, CompiledApp, Method, RouteDecl};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::dispatch::{AppInner, HandlerEntry, HandlerKind, PyDispatcher};
use crate::pool::SyncPool;

pyo3::create_exception!(
    fulmen._fulmen,
    StartupError,
    pyo3::exceptions::PyException,
    "The application failed to start; lists every declaration or binding problem found."
);

/// The Python shape of a route spec tuple (`NativeApp.add_route` argument).
type RouteSpec = (
    String,
    String,
    Vec<(String, String)>,
    Vec<bool>,
    Py<PyAny>,
    bool,
);

/// One route registration passed from Python: `method`, `path`, the ordered
/// `(name, type_name)` path-param specs, a `kw_flags` tuple matching the
/// handler's parameter order (`True` = keyword-only), the `handler`, and
/// `is_async`.
#[derive(Debug)]
struct RouteRegistration {
    /// Uppercase HTTP method name.
    method: String,
    /// The route pattern.
    path: String,
    /// `(name, type-name)` in the route's declaration order.
    param_types: Vec<(String, String)>,
    /// Per-handler-parameter keyword-only flags, signature order.
    kw_flags: Vec<bool>,
    /// The callable.
    handler: Py<PyAny>,
    /// `inspect.iscoroutinefunction(handler)`.
    is_async: bool,
}

impl RouteRegistration {
    /// Extract one registration from the Python side of `NativeApp.add_route`.
    fn extract(bound: &Bound<'_, PyTuple>) -> PyResult<Self> {
        let (method, path, param_types, kw_flags, handler, is_async): RouteSpec =
            bound.extract()?;
        Ok(Self {
            method,
            path,
            param_types,
            kw_flags,
            handler,
            is_async,
        })
    }
}

/// The native half of a `Fulmen` application: route declarations recorded
/// until [`Fulmen.run`](python/fulmen/app.py) compiles them.
///
/// Instances are not exposed to user code directly — the Python wrapper
/// keeps one as `app._native` and hands it to `serve`/`serve_for_test`.
#[pyclass(name = "NativeApp", module = "fulmen._fulmen")]
pub(crate) struct NativeApp {
    routes: Vec<RouteRegistration>,
}

#[pymethods]
impl NativeApp {
    /// A new, empty declaration set.
    #[new]
    fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Record one route. `spec` is the six-tuple described on
    /// [`RouteRegistration`]; the Python layer builds it from the handler's
    /// signature and type hints.
    ///
    /// # Errors
    /// [`TypeError`] when `spec` is malformed.
    fn add_route(&mut self, spec: &Bound<'_, PyTuple>) -> PyResult<()> {
        let registration = RouteRegistration::extract(spec)?;
        self.routes.push(registration);
        Ok(())
    }
}

/// A compiled application ready to serve: the frozen route table plus the
/// per-handler dispatch entries in the same order they were declared.
pub(crate) struct CompiledServe {
    /// Shared state handed to the [`PyDispatcher`].
    pub(crate) inner: Arc<AppInner>,
}

/// Compile `native`'s recorded declarations into [`CompiledServe`].
///
/// Python-side declaration problems (a handler parameter that is not a path
/// parameter, a param spec count mismatch, a missing route param type, …)
/// are merged with [`fulmen_core::CompileErrors`] into one [`StartupError`]
/// listing everything found (DESIGN.md §14.1).
///
/// # Errors
/// [`StartupError`] listing every problem found.
pub(crate) fn compile_app(py: Python<'_>, native: &NativeApp) -> PyResult<CompiledServe> {
    let response_cls = import_response_cls(py)?;
    let logger = import_logger(py)?;

    let mut decls = AppDecl::new();
    let mut entries: Vec<HandlerEntry> = Vec::with_capacity(native.routes.len());
    let mut problems: Vec<String> = Vec::new();

    for registration in &native.routes {
        let label = format!("{} {}", registration.method, registration.path);
        let Some(method) = parse_method(&registration.method, &label, &mut problems) else {
            continue;
        };
        let Some(kind) = check_params(py, registration, &label, &mut problems) else {
            continue;
        };

        let mut decl = RouteDecl::new(method, &registration.path, index_to_u32(entries.len()));
        for (name, ty) in &registration.param_types {
            decl = decl.param(name.clone(), ty.clone());
        }
        decls.route(decl);
        entries.push(HandlerEntry {
            callable: Arc::new(registration.handler.clone_ref(py)),
            kind,
        });
    }

    let app = CompiledApp::compile(decls).map_err(|errors| {
        let mut all = problems.clone();
        all.extend(errors.iter().map(ToString::to_string));
        StartupError::new_err(format_startup_errors(&all))
    })?;
    if !problems.is_empty() {
        return Err(StartupError::new_err(format_startup_errors(&problems)));
    }

    // §7.3: pool size defaults to 4× cores under the GIL, 2× free-threaded;
    // the queue is bounded at 1024 with a 10 s queue deadline (§14.4).
    let threads = default_pool_threads(py);
    let pool = SyncPool::new(
        threads,
        1024,
        Duration::from_secs(10),
        logger.clone(),
        response_cls.clone(),
    )
    .map_err(|err| StartupError::new_err(format!("failed to spawn the sync pool: {err}")))?;

    Ok(CompiledServe {
        inner: Arc::new(AppInner {
            app,
            handlers: entries,
            logger,
            pool,
        }),
    })
}

/// The `fulmen.responses.Response` class, imported once at compile time.
fn import_response_cls(py: Python<'_>) -> PyResult<Arc<Py<PyAny>>> {
    let module = PyModule::import(py, "fulmen.responses")?;
    Ok(Arc::new(module.getattr("Response")?.unbind()))
}

/// `logging.getLogger("fulmen")`, cached for dispatch-time logging.
fn import_logger(py: Python<'_>) -> PyResult<Arc<Py<PyAny>>> {
    let logging = PyModule::import(py, "logging")?;
    Ok(Arc::new(
        logging.getattr("getLogger")?.call1(("fulmen",))?.unbind(),
    ))
}

/// Parse the method name; anything unknown becomes a startup problem.
fn parse_method(name: &str, label: &str, problems: &mut Vec<String>) -> Option<Method> {
    match Method::from_bytes(name.as_bytes()) {
        Ok(method) if SUPPORTED_METHODS.contains(&method) => Some(method),
        _ => {
            problems.push(format!("{label}: unsupported method `{name}`"));
            None
        }
    }
}

/// The methods M1 accepts (the router compiles more; the bridge limits the
/// surface to the documented set plus `HEAD`/`OPTIONS`).
const SUPPORTED_METHODS: [Method; 7] = [
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::HEAD,
    Method::OPTIONS,
];

/// Validate the handler's declared parameters against its route's path
/// params and pick the [`HandlerKind`]; problems go to `problems`.
fn check_params(
    py: Python<'_>,
    registration: &RouteRegistration,
    label: &str,
    problems: &mut Vec<String>,
) -> Option<HandlerKind> {
    let handler_name = registration
        .handler
        .bind(py)
        .getattr("__qualname__")
        .and_then(|q| q.extract::<String>())
        .unwrap_or_else(|_| "<handler>".to_owned());

    // Every handler parameter must be a path parameter (M1: no query/body
    // extraction). The route's declared param order defines the call order,
    // so kw_flags only decide positional-vs-keyword per handler signature.
    if registration.kw_flags.len() != registration.param_types.len() {
        problems.push(format!(
            "{label} ({handler_name}): handler has {} parameter(s) but the route declares {} path parameter(s)",
            registration.kw_flags.len(),
            registration.param_types.len(),
        ));
        return None;
    }
    if registration.is_async {
        Some(HandlerKind::Async)
    } else {
        Some(HandlerKind::Sync {
            kw_flags: registration.kw_flags.clone().into_boxed_slice(),
        })
    }
}

/// `usize` route index to `u32` without a panic path (the table is tiny).
fn index_to_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// The §7.3 pool default: `4 × cores` under the GIL, `2 ×` free-threaded.
fn default_pool_threads(py: Python<'_>) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, NonZeroUsize::get);
    let gil = gil_enabled(py);
    if gil { 4 * cores } else { 2 * cores }
}

/// Whether the interpreter is running with the GIL enabled. Reads
/// `sys._is_gil_enabled()` when present (3.13+/free-threaded builds);
/// defaults to `true` on builds without it.
fn gil_enabled(py: Python<'_>) -> bool {
    py.import("sys")
        .and_then(|sys| sys.getattr("_is_gil_enabled"))
        .and_then(|f| f.call0())
        .and_then(|v| v.extract::<bool>())
        .unwrap_or(true)
}

/// One startup error message listing every problem found.
fn format_startup_errors(problems: &[String]) -> String {
    let mut message = String::from("application failed to start:");
    for problem in problems {
        message.push_str("\n  - ");
        message.push_str(problem);
    }
    message
}

/// Build the dispatcher for a compiled app.
pub(crate) fn dispatcher(compiled: &CompiledServe) -> PyDispatcher {
    PyDispatcher::new(Arc::clone(&compiled.inner))
}
