//! The `NativeApp` object shared between the Python `Fulmen` class and the
//! serving entry points, plus [`compile_app`], which turns the recorded
//! declarations into a frozen [`CompiledApp`] and a handler table.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use fulmen_core::pipeline::resolve_hooks;
use fulmen_core::{APP_SCOPE, AppDecl, CompiledApp, HookDecl, Method, Phase, RouteDecl};
use fulmen_plugin_sdk::host::{HookArgs, HookOutcome, LoadedPlugin, ResponseMut};
use fulmen_schema::Limits;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyTuple};

use crate::aloop::{self, AsyncLoop};
use crate::convert;
use crate::convert::ErrorTable;
use crate::descriptor::Descriptor;
use crate::dispatch::{
    AppInner, CallSource, CallSpecArg, HandlerEntry, LaneDeps, MissingArg, ParamSchema,
    PyDispatcher, ResponseContract, ResponseVariant,
};
use crate::hooks::{HookCx, HookDecision, HookEntry, NativeHook};
use crate::pool::SyncPool;

pyo3::create_exception!(
    fulmen._fulmen,
    StartupError,
    pyo3::exceptions::PyException,
    "The application failed to start; lists every declaration or binding problem found."
);

/// The Python shape of a route spec tuple (`NativeApp.add_route` argument):
/// `(method, path, param_types, call_args, handler, is_async, none_ok,
/// response_descs, layers)` — see `python/fulmen/app.py::_inspect_handler`
/// and docs/SPEC-FORMAT.md.
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
    // The group scopes the route belongs to, outermost first — selects
    // the scoped hook and `on_error` registrations that apply (§8).
    Vec<u32>,
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
    /// The route's group scopes, outermost first.
    layers: Vec<u32>,
}

impl RouteRegistration {
    /// Extract one registration from the Python side of `NativeApp.add_route`.
    fn extract(bound: &Bound<'_, PyTuple>) -> PyResult<Self> {
        let (
            method,
            path,
            param_types,
            call_args,
            handler,
            is_async,
            none_ok,
            response_descs,
            layers,
        ): RouteSpec = bound.extract()?;
        Ok(Self {
            method,
            path,
            param_types,
            call_args,
            handler,
            is_async,
            none_ok,
            response_descs,
            layers,
        })
    }
}

/// One `@app.on_error` registration recorded by Python.
struct ErrorRegistration {
    /// The scope the handler was declared on (`0` = app-wide).
    scope: u32,
    /// The exception class.
    class_: Py<PyAny>,
    /// The handler callable.
    handler: Py<PyAny>,
    /// `inspect.iscoroutinefunction(handler)`.
    is_async: bool,
}

/// What backs one [`HookDecl`] — the callable half of a hook registration.
/// The `HookDecl` (phase/scope/priority/seq) is the resolver's view; this
/// is what runs when the resolved chain reaches the hook.
enum HookKind {
    /// A Python callable routed through the executors.
    Python {
        /// The callable.
        callable: Py<PyAny>,
        /// `inspect.iscoroutinefunction` — picks the lane.
        is_async: bool,
    },
    /// A Rust closure run inline without the GIL.
    Native(NativeHook),
}

/// One recorded hook registration.
struct HookRecord {
    /// The resolver-facing data.
    decl: HookDecl,
    /// The registration name (`fn.__qualname__`), for `--hooks` output.
    name: String,
    /// What the hook calls.
    kind: HookKind,
}

/// One `@app.on_startup`/`@app.on_shutdown` registration (§5.7).
pub(crate) struct LifecycleHook {
    /// The callable (zero arguments).
    pub(crate) callable: Py<PyAny>,
    /// `async def` → runs on the app's asyncio loop.
    pub(crate) is_async: bool,
}

/// The request-parsing bounds an app or plugin may declare — `None`
/// leaves a field alone (§14.3/§14.8). `max_desc_depth` is compile-time
/// only, so it is never a request limit a plugin can touch.
#[derive(Clone, Copy, Debug, Default)]
#[expect(
    clippy::struct_field_names,
    reason = "fields mirror `Limits` and the Python kwarg names — the `max_` prefix is the public vocabulary"
)]
struct LimitSpec {
    /// Input nesting depth bound.
    max_depth: Option<u32>,
    /// Items/entries bound for any `Array`/`Object` input.
    max_items: Option<usize>,
    /// Byte-length bound for `Str`/`Bytes` inputs.
    max_str_bytes: Option<usize>,
}

impl LimitSpec {
    /// The declared bounds over `Limits::default()` — undeclared fields
    /// keep the default, so the result is the caller's effective set.
    fn effective(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_depth: self.max_depth.unwrap_or(defaults.max_depth),
            max_items: self.max_items.unwrap_or(defaults.max_items),
            max_str_bytes: self.max_str_bytes.unwrap_or(defaults.max_str_bytes),
            ..defaults
        }
    }

    /// Push a problem for every declared field looser than `bound` —
    /// §14.8's tightening-only rule. `owner`/`bound_name` name the
    /// parties so the startup error identifies field, owner, the bound
    /// value and the declared value.
    fn check_tightened(&self, bound: &Limits, owner: &str, bound_name: &str, problems: &mut Vec<String>) {
        if let Some(declared) = self.max_depth {
            check_limit_field(
                problems,
                owner,
                "max_depth",
                u64::from(declared),
                u64::from(bound.max_depth),
                bound_name,
            );
        }
        if let Some(declared) = self.max_items {
            check_limit_field(
                problems,
                owner,
                "max_items",
                declared as u64,
                bound.max_items as u64,
                bound_name,
            );
        }
        if let Some(declared) = self.max_str_bytes {
            check_limit_field(
                problems,
                owner,
                "max_str_bytes",
                declared as u64,
                bound.max_str_bytes as u64,
                bound_name,
            );
        }
    }
}

/// One problem line for a limit declared looser than its bound (§14.8):
/// names the field, who declared it, the declared value and the bound.
fn check_limit_field(
    problems: &mut Vec<String>,
    owner: &str,
    field: &str,
    declared: u64,
    bound: u64,
    bound_name: &str,
) {
    if declared > bound {
        problems.push(format!(
            "{owner} declares a looser `{field}` limit: {declared} exceeds {bound_name} {bound}"
        ));
    }
}

/// One plugin's scope-bound limit declaration (§14.8): at compile the
/// spec must tighten the app's limits, then it applies to every route
/// in `scope` (the plugin's `use()` target — the app for scope `0`, or
/// the group and its children otherwise).
struct ScopeLimits {
    /// The scope the declaration applies to (`APP_SCOPE` for the app).
    scope: u32,
    /// The declaring plugin's name — reported in startup problems.
    plugin: String,
    /// The declared fields.
    spec: LimitSpec,
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
    /// Every recorded hook; the index is the hook id resolved chains use.
    hooks: Vec<HookRecord>,
    /// Next group-scope id handed out by [`add_scope`](Self::add_scope);
    /// [`APP_SCOPE`] (`0`) is the application itself.
    next_scope: u32,
    /// Global registration sequence feeding [`HookDecl::seq`].
    hook_seq: u32,
    /// `@app.on_startup`/`@app.on_shutdown` registrations.
    startup_hooks: Vec<LifecycleHook>,
    /// `@app.on_shutdown` registrations.
    shutdown_hooks: Vec<LifecycleHook>,
    /// Loaded native plugins (§13.4): the instance plus the Python object
    /// owning its shared library — both must outlive the declaration
    /// phase, so [`compile_app`] clones them into `AppInner`.
    native_plugins: Vec<(Arc<LoadedPlugin>, Py<PyAny>)>,
    /// The app's declared request limits (`Fulmen(limits=…)`, §14.3) —
    /// all-`None` keeps `Limits::default()`.
    limit_spec: LimitSpec,
    /// Plugin-declared scope limits (`PluginContext.limits`, §14.8) —
    /// checked against `limit_spec` at compile, then applied per route.
    scope_limits: Vec<ScopeLimits>,
    /// `Fulmen(debug=True)` — §11.6 output validation; OR-ed with the
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
            hooks: Vec::new(),
            next_scope: APP_SCOPE + 1,
            hook_seq: 0,
            startup_hooks: Vec::new(),
            shutdown_hooks: Vec::new(),
            native_plugins: Vec::new(),
            limit_spec: LimitSpec::default(),
            scope_limits: Vec::new(),
            debug,
        }
    }

    /// Record one route. `spec` is the nine-tuple described on
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

    /// Allocate a group scope id; hooks and `on_error` registrations
    /// recorded under it apply to routes carrying the scope in `layers`.
    fn add_scope(&mut self) -> u32 {
        let scope = self.next_scope;
        self.next_scope += 1;
        scope
    }

    /// Record one Python hook: `phase` is a §8 phase name, `priority` the
    /// ordering band (lower runs earlier), `scope` the app (`0`) or a
    /// group scope from [`add_scope`](Self::add_scope).
    ///
    /// # Errors
    /// [`ValueError`] for an unknown phase name.
    fn add_hook(
        &mut self,
        phase: &str,
        callable: Py<PyAny>,
        is_async: bool,
        name: String,
        priority: u16,
        scope: u32,
    ) -> PyResult<()> {
        let phase: Phase = phase
            .parse()
            .map_err(|_| PyValueError::new_err(format!("unknown hook phase `{phase}`")))?;
        self.push_hook(
            phase,
            scope,
            priority,
            name,
            HookKind::Python { callable, is_async },
        );
        Ok(())
    }

    /// Load a native plugin's `fulmen.plugin.v1` descriptor capsule and
    /// register its hooks (DESIGN.md §13.4): `capsule` carries the
    /// `FulmenPluginV1` pointer, `config` the plugin's JSON config,
    /// `priority`/`scope` order and scope the hooks like any other
    /// registration, and `keep` is the Python object owning the shared
    /// library (kept alive for the app's lifetime so the plugin's code
    /// is never unmapped). Every hook runs inline — zero Python
    /// crossings.
    ///
    /// # Errors
    /// [`StartupError`] for an ABI mismatch or a plugin `setup` failure;
    /// [`ValueError`] for a wrongly-named capsule.
    fn add_native_plugin(
        &mut self,
        py: Python<'_>,
        capsule: &Bound<'_, PyCapsule>,
        config: &[u8],
        priority: u16,
        scope: u32,
        keep: Py<PyAny>,
    ) -> PyResult<()> {
        // `pointer_checked` enforces the capsule name and non-null
        // pointer; `LoadedPlugin::load` does every dereference, keeping
        // all unsafe inside fulmen-plugin-sdk.
        let pointer = capsule.pointer_checked(Some(fulmen_plugin_sdk::FULMEN_PLUGIN_CAPSULE))?;
        let plugin = LoadedPlugin::load(pointer.cast())
            .and_then(|plugin| {
                plugin.create(config)?;
                Ok(plugin)
            })
            .map_err(|err| StartupError::new_err(err.to_string()))?;
        let logger = import_logger(py)?;
        let plugin = Arc::new(plugin);
        for phase in Phase::ALL {
            if !plugin.has_hook(phase.index()) {
                continue;
            }
            let name = format!("{}:{}", plugin.name(), phase.as_str());
            let hook = plugin_hook(&plugin, &logger, phase);
            self.add_native_hook(phase, &name, priority, scope, hook);
        }
        self.native_plugins.push((plugin, keep));
        Ok(())
    }

    /// Record the app's request limits (`Fulmen(limits=…)`, §14.3):
    /// `None` leaves a field at its default. Declared values are checked
    /// against the defaults at compile — limits only ever tighten, so a
    /// looser declaration is a startup problem (§14.8).
    #[pyo3(signature = (max_depth=None, max_items=None, max_str_bytes=None))]
    fn set_limits(
        &mut self,
        max_depth: Option<u32>,
        max_items: Option<usize>,
        max_str_bytes: Option<usize>,
    ) {
        self.limit_spec = LimitSpec {
            max_depth,
            max_items,
            max_str_bytes,
        };
    }

    /// Record one plugin's declared limits for `scope`
    /// (`PluginContext.limits`, §14.8). Compile validates each field
    /// against the app's limits — a looser field is a startup problem
    /// naming the field, the plugin and both values; a tighter field
    /// applies to the scope's routes.
    #[pyo3(signature = (scope, plugin, max_depth=None, max_items=None, max_str_bytes=None))]
    fn add_scope_limits(
        &mut self,
        scope: u32,
        plugin: String,
        max_depth: Option<u32>,
        max_items: Option<usize>,
        max_str_bytes: Option<usize>,
    ) {
        self.scope_limits.push(ScopeLimits {
            scope,
            plugin,
            spec: LimitSpec {
                max_depth,
                max_items,
                max_str_bytes,
            },
        });
    }

    /// Record one `@app.on_error(exc_cls)` registration under `scope`.
    fn add_error_handler(
        &mut self,
        exc_class: Py<PyAny>,
        handler: Py<PyAny>,
        is_async: bool,
        scope: u32,
    ) {
        self.error_handlers.push(ErrorRegistration {
            scope,
            class_: exc_class,
            handler,
            is_async,
        });
    }

    /// Record one `@app.on_startup`/`@app.on_shutdown` registration.
    ///
    /// # Errors
    /// [`ValueError`] for a `kind` other than `"startup"`/`"shutdown"`.
    fn add_lifecycle_hook(
        &mut self,
        kind: &str,
        callable: Py<PyAny>,
        is_async: bool,
    ) -> PyResult<()> {
        let hook = LifecycleHook { callable, is_async };
        match kind {
            "startup" => self.startup_hooks.push(hook),
            "shutdown" => self.shutdown_hooks.push(hook),
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown lifecycle hook kind `{other}`"
                )));
            }
        }
        Ok(())
    }

    /// The diagnostic route report `fulmen routes` renders (M3.4): one row
    /// per declared route — `(method, path, handler, chains, on_error,
    /// cost)` — where `chains` is `(phase, hook, kind)` triples in
    /// execution order, `on_error` the exception classes the route's
    /// merged table handles, and `cost` the Python crossings per request
    /// (`1` handler + one per Python hook in the chains).
    ///
    /// Built from the recorded declarations — the same data
    /// [`compile_app`] resolves — never by probing requests.
    fn describe_routes(&self, py: Python<'_>) -> Vec<RouteRow> {
        let decls = self.hook_decls();
        let mut rows = Vec::with_capacity(self.routes.len());
        for route in &self.routes {
            let handler_name = qualname(py, &route.handler);
            let resolved = resolve_hooks(&decls, &route.layers);
            let mut chains = Vec::new();
            let mut python = 0u32;
            for (i, chain) in resolved.chains().iter().enumerate() {
                for id in chain {
                    let Some(record) = self.hooks.get(*id as usize) else {
                        continue;
                    };
                    if matches!(record.kind, HookKind::Python { .. }) {
                        python += 1;
                    }
                    chains.push((
                        Phase::ALL[i].as_str().to_owned(),
                        record.name.clone(),
                        match record.kind {
                            HookKind::Python { .. } => "python".to_owned(),
                            HookKind::Native(_) => "native".to_owned(),
                        },
                    ));
                }
            }
            let errors = self
                .merged_error_scopes(&route.layers)
                .into_iter()
                .map(|registration| qualname(py, &registration.class_))
                .collect();
            rows.push((
                route.method.clone(),
                route.path.clone(),
                handler_name,
                chains,
                errors,
                1 + python,
            ));
        }
        rows
    }
}

/// One `describe_routes` row: `(method, path, handler, chains, on_error,
/// cost)` — `chains` itself a list of `(phase, name, kind)` triples.
type RouteRow = (
    String,
    String,
    String,
    Vec<(String, String, String)>,
    Vec<String>,
    u32,
);

impl NativeApp {
    /// Append a hook record; the id is its position in the table.
    fn push_hook(
        &mut self,
        phase: Phase,
        scope: u32,
        priority: u16,
        name: String,
        kind: HookKind,
    ) -> u32 {
        let seq = self.hook_seq;
        self.hook_seq += 1;
        self.hooks.push(HookRecord {
            decl: HookDecl {
                phase,
                scope,
                priority,
                seq,
            },
            name,
            kind,
        });
        index_to_u32(self.hooks.len() - 1)
    }

    /// Register a Rust-side native hook (loaded plugins — and, later,
    /// security plugins and DI). Runs inline on the dispatch thread:
    /// zero Python crossings.
    ///
    /// Not reachable from Python — `add_native_plugin` calls it.
    pub(crate) fn add_native_hook(
        &mut self,
        phase: Phase,
        name: &str,
        priority: u16,
        scope: u32,
        hook: NativeHook,
    ) -> u32 {
        self.push_hook(
            phase,
            scope,
            priority,
            name.to_owned(),
            HookKind::Native(hook),
        )
    }

    /// The resolver's view of every recorded hook.
    fn hook_decls(&self) -> Vec<HookDecl> {
        self.hooks.iter().map(|record| record.decl).collect()
    }

    /// The scopes whose `on_error` registrations apply to a route with
    /// `layers`, ordered for MRO ties: innermost group first, the app
    /// table last (the closest scope wins a same-distance conflict).
    fn merged_error_scopes<'a>(&'a self, layers: &'a [u32]) -> Vec<&'a ErrorRegistration> {
        let mut scopes: Vec<u32> = layers.iter().rev().copied().collect();
        scopes.push(APP_SCOPE);
        let mut merged = Vec::new();
        for scope in scopes {
            merged.extend(self.error_handlers.iter().filter(|e| e.scope == scope));
        }
        merged
    }
}

/// A compiled application ready to serve: the frozen route table plus the
/// per-handler dispatch entries in the same order they were declared.
pub(crate) struct CompiledServe {
    /// Shared state handed to the [`PyDispatcher`].
    pub(crate) inner: Arc<AppInner>,
    /// `@app.on_startup` hooks — run after compile, before bind (§6.4.8).
    pub(crate) startup_hooks: Vec<LifecycleHook>,
    /// `@app.on_shutdown` hooks — run after the server stops.
    pub(crate) shutdown_hooks: Vec<LifecycleHook>,
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
    /// effective debug is `config.debug || native.debug` (§11.6).
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
#[expect(
    clippy::too_many_lines,
    reason = "one compile pass over route/hook/error declarations — splitting would scatter the ordering invariants it enforces"
)]
pub(crate) fn compile_app(
    py: Python<'_>,
    native: &NativeApp,
    config: &BridgeConfig,
) -> PyResult<CompiledServe> {
    let response_cls = import_response_cls(py)?;
    let request_cls = import_request_cls(py)?;
    let logger = import_logger(py)?;

    let hook_decls = native.hook_decls();
    // The hook table shares the decl ids: `HookEntry` i implements `HookDecl` i.
    let hook_entries: Vec<HookEntry> = native
        .hooks
        .iter()
        .map(|record| match &record.kind {
            HookKind::Python { callable, is_async } => HookEntry::Python {
                name: record.name.clone().into_boxed_str(),
                callable: Arc::new(callable.clone_ref(py)),
                is_async: *is_async,
            },
            HookKind::Native(hook) => HookEntry::Native {
                name: record.name.clone().into_boxed_str(),
                hook: Arc::clone(hook),
            },
        })
        .collect();
    let app_hooks = fulmen_core::pipeline::resolve_scope_hooks(&hook_decls, APP_SCOPE);

    // Per-route `on_error` tables: index 0 is the app table (held
    // separately in `AppInner::app_errors`); group tables occupy 1-based
    // slots — a route in a group merges group registrations over the
    // app set (§8).
    let app_errors = Arc::new(ErrorTable {
        handlers: native
            .error_handlers
            .iter()
            .filter(|e| e.scope == APP_SCOPE)
            .map(|e| error_entry(py, e))
            .collect(),
    });
    let mut error_tables: Vec<Arc<ErrorTable>> = Vec::new();
    let mut error_index: std::collections::HashMap<Vec<u32>, u32> =
        std::collections::HashMap::new();

    let mut decls = AppDecl::new();
    let mut entries: Vec<HandlerEntry> = Vec::with_capacity(native.routes.len());
    let mut problems: Vec<String> = Vec::new();
    // §14.8: limits only ever tighten. The app can't declare looser than
    // the §14.3 defaults, and a plugin can't declare looser than the
    // app — every violation reports field, owner and both values.
    let app_limits = native.limit_spec.effective();
    native
        .limit_spec
        .check_tightened(&Limits::default(), "the app", "the default", &mut problems);
    for scoped in &native.scope_limits {
        scoped.spec.check_tightened(
            &app_limits,
            &format!("plugin `{}`", scoped.plugin),
            "the app's",
            &mut problems,
        );
    }
    let mut has_async = native.error_handlers.iter().any(|e| e.is_async)
        || native
            .hooks
            .iter()
            .any(|h| matches!(h.kind, HookKind::Python { is_async: true, .. }))
        || native.startup_hooks.iter().any(|h| h.is_async)
        || native.shutdown_hooks.iter().any(|h| h.is_async);

    for registration in &native.routes {
        let label = format!("{} {}", registration.method, registration.path);
        let Some(method) = parse_method(&registration.method, &label, &mut problems) else {
            continue;
        };
        let Some(call_spec) = check_spec(py, registration, &label, &mut problems) else {
            continue;
        };
        has_async |= registration.is_async;

        let error_table = if registration.layers.is_empty() {
            0
        } else if let Some(index) = error_index.get(&registration.layers) {
            *index
        } else {
            let table = Arc::new(ErrorTable {
                handlers: native
                    .merged_error_scopes(&registration.layers)
                    .into_iter()
                    .map(|e| error_entry(py, e))
                    .collect(),
            });
            error_tables.push(table);
            // Group tables are 1-based: `error_table - 1` indexes `error_tables`.
            let index = index_to_u32(error_tables.len());
            error_index.insert(registration.layers.clone(), index);
            index
        };

        let mut decl = RouteDecl::new(method, &registration.path, index_to_u32(entries.len()))
            .hooks(resolve_hooks(&hook_decls, &registration.layers))
            .error_table(error_table);
        for (name, ty) in &registration.param_types {
            decl = decl.param(name.clone(), ty.clone());
        }
        let wants_request = call_spec
            .iter()
            .any(|arg| arg.source == CallSource::Request);
        decls.route(decl);
        entries.push(HandlerEntry {
            callable: Arc::new(registration.handler.clone_ref(py)),
            is_async: registration.is_async,
            none_ok: registration.none_ok,
            call_spec: call_spec.into_boxed_slice(),
            response_contract: response_contract(py, registration),
            error_table,
            wants_request,
            limits: route_limits(&app_limits, &native.scope_limits, &registration.layers),
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

    // §11.6: debug output validation activates from the serving flag or
    // the app's own `Fulmen(debug=True)` declaration.
    let deps = LaneDeps {
        logger: logger.clone(),
        response_cls,
        request_cls,
        debug: config.debug || native.debug,
    };

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
        deps.clone(),
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
            deps,
        )?)
    } else {
        None
    };

    Ok(CompiledServe {
        inner: Arc::new(AppInner {
            app,
            handlers: entries,
            hooks: hook_entries,
            app_hooks,
            app_errors,
            error_tables,
            logger,
            pool,
            async_loop,
            empty_contract: Arc::new(ResponseContract::default()),
            native_plugins: native
                .native_plugins
                .iter()
                .map(|(plugin, keep)| (Arc::clone(plugin), keep.clone_ref(py)))
                .collect(),
        }),
        startup_hooks: native
            .startup_hooks
            .iter()
            .map(|h| LifecycleHook {
                callable: h.callable.clone_ref(py),
                is_async: h.is_async,
            })
            .collect(),
        shutdown_hooks: native
            .shutdown_hooks
            .iter()
            .map(|h| LifecycleHook {
                callable: h.callable.clone_ref(py),
                is_async: h.is_async,
            })
            .collect(),
    })
}

/// The `NativeHook` bridging one loaded plugin phase into the pipeline
/// (§13.4): `HookCx`'s borrows adapt to `HookArgs`, `run_hook` does the
/// cross-ABI call inside the SDK's panic boundary, and a `Panicked`
/// outcome logs and answers `500` — matching `call_native`'s own
/// containment (§14.6).
fn plugin_hook(plugin: &Arc<LoadedPlugin>, logger: &Arc<Py<PyAny>>, phase: Phase) -> NativeHook {
    let plugin = Arc::clone(plugin);
    let logger = Arc::clone(logger);
    let plugin_name = plugin.name().to_owned();
    let phase_index = phase.index();
    Arc::new(move |cx: &mut HookCx<'_>| {
        let mut args = HookArgs {
            method: cx.method.as_str(),
            path: cx.path,
            query: cx.query,
            headers: cx.headers,
            body: cx.body,
            remote_addr: cx.remote_addr,
            response: cx.response.as_mut().map(|res| ResponseMut {
                status: &mut res.status,
                headers: &mut res.headers,
                body: &mut res.body,
            }),
        };
        match plugin.run_hook(phase_index, &mut args) {
            HookOutcome::Continue => HookDecision::Continue,
            HookOutcome::Respond(parts) => HookDecision::Respond(fulmen_server::Response {
                status: parts.status,
                headers: parts.headers,
                body: parts.body,
            }),
            HookOutcome::Panicked(message) => {
                crate::dispatch::log_message(
                    &logger,
                    format!("native plugin `{plugin_name}` {} hook: {message}", cx.phase),
                );
                HookDecision::Respond(convert::internal_error())
            }
        }
    })
}

/// One `ErrorRegistration` → the compiled [`ErrorHandler`].
fn error_entry(py: Python<'_>, registration: &ErrorRegistration) -> convert::ErrorHandler {
    convert::ErrorHandler {
        class_: Arc::new(registration.class_.clone_ref(py)),
        handler: Arc::new(registration.handler.clone_ref(py)),
        is_async: registration.is_async,
    }
}

/// `obj.__qualname__` for diagnostics (`fulmen routes`).
fn qualname(py: Python<'_>, obj: &Py<PyAny>) -> String {
    obj.getattr(py, "__qualname__")
        .and_then(|q| q.extract::<String>(py))
        .unwrap_or_else(|_| "<callable>".to_owned())
}

/// Compile a route's `response_descs` into its [`ResponseContract`]:
/// a shared `Descriptor` handle plus a prebuilt [`Validator`] per
/// declared variant (declaration order preserved — same-status unions
/// try in order, status variants match exactly).
fn response_contract(py: Python<'_>, registration: &RouteRegistration) -> Arc<ResponseContract> {
    Arc::new(ResponseContract {
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

/// The `fulmen.request.Request` class, imported once at compile time —
/// materializes `request`-sourced arguments and the object hooks receive.
fn import_request_cls(py: Python<'_>) -> PyResult<Arc<Py<PyAny>>> {
    let module = PyModule::import(py, "fulmen.request")?;
    Ok(Arc::new(module.getattr("Request")?.unbind()))
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
        if source == CallSource::Request {
            // `request` is always injected — `missing` never applies.
            spec.push(CallSpecArg {
                name: name.clone().into_boxed_str(),
                source,
                positional,
                wire_name: wire_name.clone().into_boxed_str(),
                schema: None,
                missing: MissingArg::Omit,
            });
            continue;
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

/// The effective limits for one route (§14.8): the app's bounds
/// tightened by every plugin declaration whose scope covers the route —
/// an `APP_SCOPE` declaration covers all routes, a group scope covers
/// its own routes and its children's (the same rule hooks resolve by).
fn route_limits(app: &Limits, scoped: &[ScopeLimits], layers: &[u32]) -> Limits {
    scoped.iter().fold(*app, |limits, s| {
        if s.scope == APP_SCOPE || layers.contains(&s.scope) {
            limits.tightened(&s.spec.effective())
        } else {
            limits
        }
    })
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
