//! The `NativeApp` object shared between the Python `Fulmen` class and the
//! serving entry points, plus [`compile_app`], which turns the recorded
//! declarations into a frozen [`CompiledApp`] and a handler table.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use fulmen_core::{AppDecl, CompiledApp, Method, RouteDecl};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::aloop::{self, AsyncLoop};
use crate::convert::ErrorTable;
use crate::descriptor::Descriptor;
use crate::dispatch::{
    AppInner, CallSource, CallSpecArg, HandlerEntry, MissingArg, ParamSchema, PyDispatcher,
    ResponseContract, ResponseVariant,
};
use crate::pool::SyncPool;

pyo3::create_exception!(
    fulmen._fulmen,
    StartupError,
    pyo3::exceptions::PyException,
    "The application failed to start; lists every declaration or binding problem found."
);

/// The Python shape of a route spec tuple (`NativeApp.add_route` argument):
/// `(method, path, param_types, call_args, handler, is_async, none_ok,
/// response_descs)` — see `python/fulmen/app.py::_inspect_handler` and
/// docs/SPEC-FORMAT.md.
type RouteSpec = (
    // Uppercase HTTP method.
    String,
    // The route pattern.
    String,
    // `(name, "int"|"str")` path params, handler-signature order (router).
    Vec<(String, String)>,
    // `(name, source, kind, wire_name, Descriptor|None, missing)` for
    // every parameter, signature order; `missing` is
    // `("required"|"omit"|"literal"|"factory", object)`.
    Vec<CallArgSpec>,
    // The callable.
    Py<PyAny>,
    // `inspect.iscoroutinefunction(handler)`.
    bool,
    // `None` return is legal (`-> None`/`-> NoContent` declared).
    bool,
    // `(status|None, Descriptor|None)` declared response set (OpenAPI).
    Vec<(Option<u16>, Option<Py<Descriptor>>)>,
);

/// One `call_args` element of a route spec: the parameter name, its
/// `CallSource` name, `pos_only`/`normal`/`kw_only`, the wire name the
/// value is looked up by (alias or `x-api-key` header form), the compiled
/// descriptor, and `("required"|"omit"|"literal"|"factory", object)` —
/// what an absent source value means (`literal` passes the object,
/// `factory` calls it, `omit` lets the signature default apply).
type CallArgSpec = (
    String,
    String,
    String,
    String,
    Option<Py<Descriptor>>,
    (String, Py<PyAny>),
);

/// One route registration passed from Python.
struct RouteRegistration {
    /// Uppercase HTTP method name.
    method: String,
    /// The route pattern.
    path: String,
    /// `(name, type-name)` in the route's declaration order.
    param_types: Vec<(String, String)>,
    /// The parameters' call spec, signature order.
    call_args: Vec<CallArgSpec>,
    /// The callable.
    handler: Py<PyAny>,
    /// `inspect.iscoroutinefunction(handler)`.
    is_async: bool,
    /// Whether a `None` return answers `204`.
    none_ok: bool,
    /// The declared response variants.
    response_descs: Vec<(Option<u16>, Option<Py<Descriptor>>)>,
}

impl RouteRegistration {
    /// Extract one registration from the Python side of `NativeApp.add_route`.
    fn extract(bound: &Bound<'_, PyTuple>) -> PyResult<Self> {
        let (method, path, param_types, call_args, handler, is_async, none_ok, response_descs): RouteSpec =
            bound.extract()?;
        Ok(Self {
            method,
            path,
            param_types,
            call_args,
            handler,
            is_async,
            none_ok,
            response_descs,
        })
    }
}

/// One `@app.on_error` registration recorded by Python.
struct ErrorRegistration {
    /// The exception class.
    class_: Py<PyAny>,
    /// The handler callable.
    handler: Py<PyAny>,
    /// `inspect.iscoroutinefunction(handler)`.
    is_async: bool,
}

/// The native half of a `Fulmen` application: route and error-handler
/// declarations recorded until [`Fulmen.run`](python/fulmen/app.py) compiles
/// them.
///
/// Instances are not exposed to user code directly — the Python wrapper
/// keeps one as `app._native` and hands it to `serve`/`serve_for_test`.
#[pyclass(name = "NativeApp", module = "fulmen._fulmen")]
pub(crate) struct NativeApp {
    routes: Vec<RouteRegistration>,
    error_handlers: Vec<ErrorRegistration>,
    /// `Fulmen(debug=True)` — §14.2 output validation; OR-ed with the
    /// serving-level `debug` flag at compile.
    pub(crate) debug: bool,
}

#[pymethods]
impl NativeApp {
    /// A new, empty declaration set. `debug` turns on output validation.
    #[new]
    #[pyo3(signature = (debug=false))]
    fn new(debug: bool) -> Self {
        Self {
            routes: Vec::new(),
            error_handlers: Vec::new(),
            debug,
        }
    }

    /// Record one route. `spec` is the eight-tuple described on
    /// [`RouteSpec`]; the Python layer builds it from the handler's
    /// signature and type hints.
    ///
    /// # Errors
    /// [`TypeError`] when `spec` is malformed.
    fn add_route(&mut self, spec: &Bound<'_, PyTuple>) -> PyResult<()> {
        let registration = RouteRegistration::extract(spec)?;
        self.routes.push(registration);
        Ok(())
    }

    /// Record one `@app.on_error(exc_cls)` registration.
    fn add_error_handler(&mut self, exc_class: Py<PyAny>, handler: Py<PyAny>, is_async: bool) {
        self.error_handlers.push(ErrorRegistration {
            class_: exc_class,
            handler,
            is_async,
        });
    }
}

/// A compiled application ready to serve: the frozen route table plus the
/// per-handler dispatch entries in the same order they were declared.
pub(crate) struct CompiledServe {
    /// Shared state handed to the [`PyDispatcher`].
    pub(crate) inner: Arc<AppInner>,
}

/// Knobs the serving entry points collect from the Python call and hand to
/// [`compile_app`].
pub(crate) struct BridgeConfig {
    /// uvloop tri-state: `None` auto, `Some(true)` required, `Some(false)`
    /// plain asyncio (§7.4, M1.16).
    pub(crate) uvloop: Option<bool>,
    /// GIL batching size for the sync pool (§7.5); ignored on free-threaded
    /// builds, where batching never pays.
    pub(crate) gil_batch_size: usize,
    /// Serving-level debug flag (`serve(debug=…)`); the compiled app's
    /// effective debug is `config.debug || native.debug` (§14.2).
    pub(crate) debug: bool,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            uvloop: None,
            gil_batch_size: 32,
            debug: false,
        }
    }
}

/// Compile `native`'s recorded declarations into [`CompiledServe`].
///
/// Python-side declaration problems (a bad source name, a param spec count
/// mismatch, a missing route param type, …) are merged with
/// [`fulmen_core::CompileErrors`] into one [`StartupError`] listing
/// everything found (DESIGN.md §14.1).
///
/// # Errors
/// [`StartupError`] listing every problem found.
pub(crate) fn compile_app(
    py: Python<'_>,
    native: &NativeApp,
    config: &BridgeConfig,
) -> PyResult<CompiledServe> {
    let response_cls = import_response_cls(py)?;
    let logger = import_logger(py)?;

    let mut decls = AppDecl::new();
    let mut entries: Vec<HandlerEntry> = Vec::with_capacity(native.routes.len());
    let mut problems: Vec<String> = Vec::new();
    let mut has_async = native.error_handlers.iter().any(|e| e.is_async);

    for registration in &native.routes {
        let label = format!("{} {}", registration.method, registration.path);
        let Some(method) = parse_method(&registration.method, &label, &mut problems) else {
            continue;
        };
        let Some(call_spec) = check_spec(py, registration, &label, &mut problems) else {
            continue;
        };
        has_async |= registration.is_async;

        let mut decl = RouteDecl::new(method, &registration.path, index_to_u32(entries.len()));
        for (name, ty) in &registration.param_types {
            decl = decl.param(name.clone(), ty.clone());
        }
        decls.route(decl);
        entries.push(HandlerEntry {
            callable: Arc::new(registration.handler.clone_ref(py)),
            is_async: registration.is_async,
            none_ok: registration.none_ok,
            call_spec: call_spec.into_boxed_slice(),
            response_contract: Arc::new(ResponseContract {
                variants: registration
                    .response_descs
                    .iter()
                    .map(|(status, desc)| {
                        let (desc, validator) = match desc {
                            Some(d) => {
                                let borrowed = d.bind(py).borrow();
                                (
                                    Some(Arc::new(d.clone_ref(py))),
                                    Some(borrowed.validator_arc()),
                                )
                            }
                            None => (None, None),
                        };
                        ResponseVariant {
                            status: *status,
                            desc,
                            validator,
                        }
                    })
                    .collect(),
            }),
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

    let errors = Arc::new(ErrorTable {
        handlers: native
            .error_handlers
            .iter()
            .map(|e| crate::convert::ErrorHandler {
                class_: Arc::new(e.class_.clone_ref(py)),
                handler: Arc::new(e.handler.clone_ref(py)),
                is_async: e.is_async,
            })
            .collect(),
    });

    // §14.2: debug output validation activates from the serving flag or
    // the app's own `Fulmen(debug=True)` declaration.
    let debug = config.debug || native.debug;

    // §7.3: pool size defaults to 4× cores under the GIL, 2× free-threaded;
    // the queue is bounded at 1024 with a 5 s queue deadline (§14.4).
    let threads = default_pool_threads(py);
    let gil = gil_enabled(py);
    // §7.5: GIL batching only pays when the GIL exists; free-threaded
    // builds take one job at a time.
    let batch_size = if gil { config.gil_batch_size } else { 1 };
    let pool = SyncPool::new(
        threads,
        1024,
        Duration::from_secs(5),
        batch_size,
        logger.clone(),
        response_cls.clone(),
        Arc::clone(&errors),
        debug,
    )
    .map_err(|err| StartupError::new_err(format!("failed to spawn the sync pool: {err}")))?;

    // §7.4: the asyncio loop thread only exists when the app declares
    // `async def` handlers or async `on_error` handlers.
    let async_loop = if has_async {
        let loop_impl = aloop::resolve_loop_impl(py, config.uvloop)?;
        Some(AsyncLoop::start(
            py,
            loop_impl,
            1024,
            Duration::from_secs(5),
            logger.clone(),
            response_cls.clone(),
            Arc::clone(&errors),
            debug,
        )?)
    } else {
        None
    };

    Ok(CompiledServe {
        inner: Arc::new(AppInner {
            app,
            handlers: entries,
            logger,
            pool,
            async_loop,
            empty_contract: Arc::new(ResponseContract::default()),
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

/// Turn the registration's `call_args` into the call spec, validating
/// source names and path-param consistency; problems go to `problems`.
fn check_spec(
    py: Python<'_>,
    registration: &RouteRegistration,
    label: &str,
    problems: &mut Vec<String>,
) -> Option<Vec<CallSpecArg>> {
    let handler_name = registration
        .handler
        .bind(py)
        .getattr("__qualname__")
        .and_then(|q| q.extract::<String>())
        .unwrap_or_else(|_| "<handler>".to_owned());

    let mut spec = Vec::with_capacity(registration.call_args.len());
    let mut path_seen = 0usize;
    for (name, source, kind, wire_name, desc, missing) in &registration.call_args {
        let Some(source) = CallSource::parse(source) else {
            problems.push(format!(
                "{label} ({handler_name}): parameter `{name}` has unknown source `{source}`"
            ));
            return None;
        };
        let positional = match kind.as_str() {
            "pos_only" => true,
            "normal" | "kw_only" => false,
            other => {
                problems.push(format!(
                    "{label} ({handler_name}): parameter `{name}` has unknown kind `{other}`"
                ));
                return None;
            }
        };
        if source == CallSource::Path {
            if !registration.param_types.iter().any(|(n, _)| n == name) {
                problems.push(format!(
                    "{label} ({handler_name}): `{name}` is marked path but is not a route parameter"
                ));
                return None;
            }
            path_seen += 1;
        }
        if positional && source != CallSource::Path {
            problems.push(format!(
                "{label} ({handler_name}): positional-only parameter `{name}` must be a path parameter"
            ));
            return None;
        }
        if source == CallSource::Header
            && http::header::HeaderName::from_bytes(wire_name.as_bytes()).is_err()
        {
            problems.push(format!(
                "{label} ({handler_name}): header parameter `{name}` has invalid wire name `{wire_name}`"
            ));
            return None;
        }
        let missing = match missing {
            (m, _) if m == "required" => MissingArg::Required,
            (m, _) if m == "omit" => MissingArg::Omit,
            (m, obj) if m == "literal" => MissingArg::Value(Arc::new(obj.clone_ref(py))),
            (m, f) if m == "factory" => MissingArg::Factory(Arc::new(f.clone_ref(py))),
            (m, _) => {
                problems.push(format!(
                    "{label} ({handler_name}): parameter `{name}` has unknown missing kind `{m}`"
                ));
                return None;
            }
        };
        spec.push(CallSpecArg {
            name: name.clone().into_boxed_str(),
            source,
            positional,
            wire_name: wire_name.clone().into_boxed_str(),
            schema: desc.as_ref().map(|d| ParamSchema::new(py, d)),
            missing,
        });
    }
    if path_seen != registration.param_types.len() {
        problems.push(format!(
            "{label} ({handler_name}): the route declares {} path parameter(s) but {} are marked path-sourced",
            registration.param_types.len(),
            path_seen,
        ));
        return None;
    }
    Some(spec)
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
