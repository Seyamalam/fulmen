//! The dispatcher: `fulmen_server::Dispatcher` implemented over the compiled
//! route table and the executors — and the request lifecycle (DESIGN.md §8).
//!
//! The server is dumb transport; this file owns the HTTP semantics and the
//! phase pipeline. Routing runs first (a pure-Rust phase with no hooks) so
//! `on_request`'s chain can be resolved per route — group-scoped hooks need
//! the match; a request matching no route still runs the app-level chains.
//! The hookable phases then execute in §8 order: `on_request` → (route) →
//! `before_route` → `guard` → `extract` → `resolve` → `before_handle` →
//! `handle` → `after_handle` → (encode) → `on_response` → `after_response`.
//! `on_error` is the exception-keyed handler table (M2), consulted by
//! `route_errors` jobs — hook failures route through it exactly like
//! handler failures (§14.8).
//!
//! Native hooks run inline without the GIL; Python hooks go through the
//! same executor machinery as handlers — `def` on the sync pool,
//! `async def` on the asyncio loop — so the dispatch future never blocks
//! on the interpreter. An empty phase chain costs nothing: resolved
//! chains are `Box<[u32]>` slices checked in order, and the `Request`
//! object materializes lazily inside the first executor job that needs
//! it, so a bare route still crosses into Python exactly once.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use fulmen_core::{CompiledApp, Phase, RouteHooks, RouteLookup};
use fulmen_schema::Validator;
use fulmen_schema::desc::Kind;
use fulmen_schema::errors::Source;
use fulmen_server::{DispatchFuture, Dispatcher, Request, Response};
use http::header::{ALLOW, CONTENT_LENGTH, LOCATION};
use http::{HeaderValue, Method, StatusCode};
use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::aloop::{AsyncLoop, AsyncSubmitError};
use crate::convert::{self, ArgKind, ErrorTable, JobAnswer, ParamArg, ParamValue, PyReturn};
use crate::descriptor::Descriptor;
use crate::extract;
use crate::hooks::{HookCx, HookDecision, HookEntry, RequestSnapshot, ResponseView};
use crate::pool::{SubmitError, SyncJob, SyncPool};

/// The per-app dependencies every execution lane needs to build a
/// [`convert::PyCtx`] — imported Python handles plus the runtime flags.
/// Bundled so `SyncPool`/`AsyncLoop` stay under the argument limit and
/// adding a flag touches one struct, not four signatures.
#[derive(Clone)]
pub(crate) struct LaneDeps {
    /// `logging.getLogger("fulmen")`.
    pub(crate) logger: Arc<Py<PyAny>>,
    /// The `fulmen.responses.Response` class object.
    pub(crate) response_cls: Arc<Py<PyAny>>,
    /// The `fulmen.request.Request` class object.
    pub(crate) request_cls: Arc<Py<PyAny>>,
    /// `Fulmen(debug=True)`/`serve(debug=…)`: output validation (§11.6).
    pub(crate) debug: bool,
}

/// Where a handler parameter's value comes from (`call_args[i].source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallSource {
    /// A `{name}` path segment, typed by the router.
    Path,
    /// A `?name=` query-string value.
    Query,
    /// A request header (case-insensitive wire name).
    Header,
    /// A `Cookie:` member.
    Cookie,
    /// The (JSON) request body.
    Body,
    /// An `application/x-www-form-urlencoded` field.
    Form,
    /// The injected `fulmen.request.Request` object (`request: Request`).
    Request,
}

impl CallSource {
    /// The `call_args` source string → its `CallSource`.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "path" => Some(Self::Path),
            "query" => Some(Self::Query),
            "header" => Some(Self::Header),
            "cookie" => Some(Self::Cookie),
            "body" => Some(Self::Body),
            "form" => Some(Self::Form),
            "request" => Some(Self::Request),
            _ => None,
        }
    }

    /// The matching `fulmen_schema` source tag for validation errors.
    pub(crate) fn schema(self) -> Source {
        match self {
            Self::Path => Source::Path,
            Self::Query => Source::Query,
            Self::Header => Source::Header,
            Self::Cookie => Source::Cookie,
            // `request` args are injected, never validated — `Request`
            // shares `Body`'s tag only for exhaustiveness.
            Self::Body | Self::Request => Source::Body,
            Self::Form => Source::Form,
        }
    }
}

/// What a parameter receives when its source supplies no value
/// (`call_args[i].missing`).
#[derive(Debug)]
pub(crate) enum MissingArg {
    /// Absence is a client error — collect a `422` `"missing"` entry.
    Required,
    /// Do not pass the argument — the signature's own default applies.
    Omit,
    /// Pass the literal default object (a marker-declared default the
    /// signature cannot express, e.g. `Query(default=10)`).
    Value(Arc<Py<PyAny>>),
    /// Call the zero-arg factory per request (`default_factory`).
    Factory(Arc<Py<PyAny>>),
}

/// A parameter's compiled schema, split so the validator is reachable
/// without the GIL.
#[derive(Debug)]
pub(crate) struct ParamSchema {
    /// The Python `Descriptor` — materialization at call time needs it.
    pub(crate) desc: Arc<Py<Descriptor>>,
    /// The descriptor's compiled validator, shared for GIL-free use.
    pub(crate) validator: Arc<Validator>,
    /// `bytes`/`Raw` kind — a `body` param taking the payload as-is.
    pub(crate) raw_bytes: bool,
}

impl ParamSchema {
    /// Split `desc` into the GIL-free half (validator) and the call-time
    /// half (the `Descriptor` for materialization).
    pub(crate) fn new(py: Python<'_>, desc: &Py<Descriptor>) -> Self {
        let borrowed = desc.bind(py).borrow();
        Self {
            desc: Arc::new(desc.clone_ref(py)),
            validator: borrowed.validator_arc(),
            raw_bytes: matches!(borrowed.type_desc().kind, Kind::Bytes(_) | Kind::Raw),
        }
    }
}

/// One handler parameter's call spec, built by
/// `python/fulmen/app.py::_inspect_handler`.
#[derive(Debug)]
pub(crate) struct CallSpecArg {
    /// The signature parameter name (also the kwargs key).
    pub(crate) name: Box<str>,
    /// Where the value comes from.
    pub(crate) source: CallSource,
    /// `pos_only` → positional binding (only ever path params).
    pub(crate) positional: bool,
    /// The name the value is looked up by — the alias, or the header's
    /// `x-api-key` form, or the parameter name.
    pub(crate) wire_name: Box<str>,
    /// The compiled descriptor, when the parameter has one.
    pub(crate) schema: Option<ParamSchema>,
    /// What an absent source value means.
    pub(crate) missing: MissingArg,
}

impl CallSpecArg {
    /// `true` for `bytes`/`Raw` body params — they take the payload
    /// octets regardless of the content type.
    pub(crate) fn takes_raw(&self) -> bool {
        self.schema.as_ref().is_some_and(|s| s.raw_bytes)
    }
}

/// One member of a handler's declared response set — `(status, body)`;
/// `(None, None)` is the dynamic `Response` escape (DESIGN.md §13).
#[derive(Debug)]
pub(crate) struct ResponseVariant {
    /// The declared status (`None` — dynamic `Response`).
    pub(crate) status: Option<u16>,
    /// The declared body's descriptor, when the variant carries one —
    /// also what `OpenAPI` reads its schema from.
    pub(crate) desc: Option<Arc<Py<Descriptor>>>,
    /// `desc`'s compiled validator, split out at compile so the debug
    /// check runs without re-borrowing the `Descriptor` per request.
    pub(crate) validator: Option<Arc<Validator>>,
}

/// A route's declared response contract: the `response_descs` set
/// `response_spec()` compiled, resolved per actual response status by
/// `convert::contract_candidates`.
#[derive(Debug, Default)]
pub(crate) struct ResponseContract {
    /// The declared variants, in declaration order.
    pub(crate) variants: Box<[ResponseVariant]>,
}

impl ResponseContract {
    /// `true` when the route declared concrete statuses (so a body that
    /// matches none deserves a debug warning); `-> Response` and
    /// unannotated routes both answer `false` — generic encoding is the
    /// declared behaviour there.
    pub(crate) fn has_status_contract(&self) -> bool {
        self.variants.iter().any(|v| v.status.is_some())
    }
}

/// One entry in the handler table, indexed by `RouteInfo::handler_index`.
#[derive(Debug)]
pub(crate) struct HandlerEntry {
    /// The Python callable, `Arc`-wrapped so dispatch can clone the handle
    /// without taking the GIL.
    pub(crate) callable: Arc<Py<PyAny>>,
    /// `async def` → the asyncio loop thread; `def` → the sync pool.
    pub(crate) is_async: bool,
    /// `-> None`/`-> NoContent` declared: `None` returns answer `204`.
    pub(crate) none_ok: bool,
    /// The parameters' extraction spec, signature order.
    pub(crate) call_spec: Box<[CallSpecArg]>,
    /// The declared response contract — descriptor-driven encoding and
    /// debug-mode output validation read it.
    pub(crate) response_contract: Arc<ResponseContract>,
    /// The route's `on_error` table — group handlers merged over the
    /// app set (index into `AppInner::error_tables`).
    pub(crate) error_table: u32,
    /// The handler declares a `request: Request` parameter — the shared
    /// request object must be materialized for it.
    pub(crate) wants_request: bool,
    /// The request-parsing limits extraction runs under (§14.3/§14.8):
    /// the app's declared bounds tightened by every plugin limit scoped
    /// to this route.
    pub(crate) limits: fulmen_schema::Limits,
}

/// One job's flags — bundled so `submit`/`enqueue` stay readable as the
/// hook machinery adds fields.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent job flags set per call kind — a state machine would conflate orthogonal axes"
)]
pub(crate) struct JobSpec {
    /// The callable to run (handler, hook, or `on_error` handler).
    pub(crate) callable: Arc<Py<PyAny>>,
    /// `async def` → the asyncio loop; `def` → the sync pool (§8: a
    /// hook's async-ness routes like a handler's).
    pub(crate) is_async: bool,
    /// The arguments, signature order.
    pub(crate) args: Box<[ParamArg]>,
    /// `"{METHOD} {path}"` for log lines.
    pub(crate) label: Box<str>,
    /// The response contract (the empty contract for hooks/error calls).
    pub(crate) contract: Arc<ResponseContract>,
    /// `None` → `204` when declared.
    pub(crate) none_ok: bool,
    /// Consult the `on_error` table on failure.
    pub(crate) route_errors: bool,
    /// `true` → the answer is the raw [`PyReturn`], not a response.
    pub(crate) hook: bool,
    /// The `on_error` table this job routes through.
    pub(crate) errors: Arc<ErrorTable>,
}

/// Everything one running server needs on top of the frozen route table.
/// Shared by every dispatch via `Arc`.
pub(crate) struct AppInner {
    /// The compiled routes.
    pub(crate) app: CompiledApp,
    /// Handler table indexed by `handler_index`.
    pub(crate) handlers: Vec<HandlerEntry>,
    /// The hook table — resolved `RouteHooks` ids index into it.
    pub(crate) hooks: Vec<HookEntry>,
    /// The app-scope chains, used for requests matching no route
    /// (a `404` still gets `on_request`/`on_response`, §8).
    pub(crate) app_hooks: RouteHooks,
    /// The app-wide `on_error` table (§17) — routes' `error_table` index
    /// `0`, and the fallback when a group index does not resolve.
    pub(crate) app_errors: Arc<ErrorTable>,
    /// Group-scoped `on_error` tables — `error_table - 1` indexes into it.
    pub(crate) error_tables: Vec<Arc<ErrorTable>>,
    /// `logging.getLogger("fulmen")` — cached for workers and error paths.
    pub(crate) logger: Arc<Py<PyAny>>,
    /// The bounded sync-handler pool.
    pub(crate) pool: SyncPool,
    /// The asyncio loop thread; `Some` only when the app has async routes.
    pub(crate) async_loop: Option<AsyncLoop>,
    /// The empty contract shared by `on_error` deferred calls — error
    /// handlers carry no response spec, so their results always encode
    /// generically.
    pub(crate) empty_contract: Arc<ResponseContract>,
    /// Loaded native plugins kept alive for the server's lifetime —
    /// hook closures hold the `LoadedPlugin`; the `Py` keeps the Python
    /// object (and thus the shared library) loaded so the entry code the
    /// hooks call is never unmapped mid-flight (§13.4).
    #[expect(
        dead_code,
        reason = "pure keep-alive: holding the Arc and Py is the entire job"
    )]
    pub(crate) native_plugins: Vec<(Arc<fulmen_plugin_sdk::host::LoadedPlugin>, Py<PyAny>)>,
}

/// The `Dispatcher` handed to `fulmen_server::serve`.
pub(crate) struct PyDispatcher {
    inner: Arc<AppInner>,
}

impl PyDispatcher {
    /// Wrap shared state for serving.
    pub(crate) fn new(inner: Arc<AppInner>) -> Self {
        Self { inner }
    }
}

impl Dispatcher for PyDispatcher {
    fn dispatch(&self, request: Request) -> DispatchFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.handle(request).await })
    }
}

impl AppInner {
    /// Route, dispatch, and produce the wire response — the §8 pipeline.
    /// Must never panic: routing runs under [`catch_unwind`], native hook
    /// calls too, and Python work is isolated on the executors (§14.6).
    #[expect(
        clippy::too_many_lines,
        reason = "one linear walk through the §8 phase pipeline — splitting the phases into helpers would obscure the order it documents"
    )]
    async fn handle(&self, request: Request) -> Response {
        let method = request.method.clone();
        let path = request.uri.path().to_owned();

        // `route` (§8 phase 4) is a pure-Rust lookup with no hooks and no
        // side effects; running it first resolves the request's hook
        // chains — group-scoped hooks are meaningless without the match.
        // Requests matching no route still run the app-scope chains.
        let lookup = catch_unwind(AssertUnwindSafe(|| {
            self.app.route(&request.method, request.uri.path())
        }));
        let Ok(lookup) = lookup else {
            log_message(&self.logger, format!("router panicked on {method} {path}"));
            return convert::internal_error();
        };

        let (hooks, errors, implicit_head) = match &lookup {
            RouteLookup::Hit(hit) => {
                let info = self.app.route_info(hit.route);
                (
                    info.hooks(),
                    self.error_table(info.error_table()),
                    hit.implicit_head,
                )
            }
            _ => (&self.app_hooks, self.error_table(0), false),
        };

        // The shared `Request` object, built on first need — only ever
        // when a Python hook or a `request` parameter asks for it.
        let mut snap: Option<Arc<RequestSnapshot>> = None;

        // §8 phase 3: `on_request` — before the route's own work begins.
        if let Some(response) = self
            .exec_decision(Phase::OnRequest, hooks, errors, &mut snap, &request)
            .await
        {
            return self
                .finish(response, hooks, errors, snap, &request, false)
                .await;
        }

        let RouteLookup::Hit(hit) = lookup else {
            // §8 phases 4/16 for unmatched requests: the `404`/`405`/
            // redirect answer still flows through `on_response`/
            // `after_response` (app-scope chains).
            let early = early_response(&request, &lookup);
            return self
                .finish(early, hooks, errors, snap, &request, false)
                .await;
        };

        let info = self.app.route_info(hit.route);
        let Some(entry) = self.handlers.get(info.handler_index() as usize) else {
            log_message(
                &self.logger,
                format!("{method} {path}: handler index out of range"),
            );
            return self
                .finish(
                    convert::internal_error(),
                    hooks,
                    errors,
                    snap,
                    &request,
                    implicit_head,
                )
                .await;
        };

        // §8 phase 5: `before_route` — CORS preflight position.
        if let Some(response) = self
            .exec_decision(Phase::BeforeRoute, hooks, errors, &mut snap, &request)
            .await
        {
            return self
                .finish(response, hooks, errors, snap, &request, implicit_head)
                .await;
        }

        // §8 phase 6: `guard` — rate limit, auth, CSRF, custom guards.
        if let Some(response) = self
            .exec_decision(Phase::Guard, hooks, errors, &mut snap, &request)
            .await
        {
            return self
                .finish(response, hooks, errors, snap, &request, implicit_head)
                .await;
        }

        // §8 phase 7: `extract` — parameter decoding/validation in Rust.
        if entry.wants_request {
            ensure_snap(&mut snap, &request);
        }
        let args = match extract::extract_args(
            &hit,
            &entry.call_spec,
            &request,
            snap.as_ref(),
            &entry.limits,
        ) {
            extract::Extracted::Call(args) => args,
            extract::Extracted::Reply(response) => {
                return self
                    .finish(response, hooks, errors, snap, &request, implicit_head)
                    .await;
            }
        };

        // §8 phase 8: `resolve` — the DI slot (native hooks later).
        if let Some(response) = self
            .exec_decision(Phase::Resolve, hooks, errors, &mut snap, &request)
            .await
        {
            return self
                .finish(response, hooks, errors, snap, &request, implicit_head)
                .await;
        }

        // §8 phase 9: `before_handle`.
        if let Some(response) = self
            .exec_decision(Phase::BeforeHandle, hooks, errors, &mut snap, &request)
            .await
        {
            return self
                .finish(response, hooks, errors, snap, &request, implicit_head)
                .await;
        }

        // §8 phase 10: `handle` — the handler itself (the one crossing a
        // bare route always pays).
        let mut response = self.run(entry, args, method, path).await;

        // §8 phase 11: `after_handle` — a non-`None` hook return replaces
        // the response.
        response = self
            .exec_post(
                Phase::AfterHandle,
                hooks,
                errors,
                &mut snap,
                &request,
                response,
            )
            .await;

        self.finish(response, hooks, errors, snap, &request, implicit_head)
            .await
    }

    /// The tail of the pipeline every produced response flows through:
    /// §8 phase 13 `on_response` (header mutation over *any* answer —
    /// `4xx`, short-circuits and handler output alike), the implicit-`HEAD`
    /// body drop, and phase 16 `after_response` (fire-and-forget).
    async fn finish(
        &self,
        response: Response,
        hooks: &RouteHooks,
        errors: &Arc<ErrorTable>,
        mut snap: Option<Arc<RequestSnapshot>>,
        request: &Request,
        implicit_head: bool,
    ) -> Response {
        let mut response = self
            .exec_post(
                Phase::OnResponse,
                hooks,
                errors,
                &mut snap,
                request,
                response,
            )
            .await;

        if implicit_head {
            // §9.2: headers of the GET response, no body. Content-Length
            // keeps the value a GET would have produced.
            let length = response.body.len();
            response
                .headers
                .entry(CONTENT_LENGTH)
                .or_insert_with(|| HeaderValue::from(u64::try_from(length).unwrap_or(u64::MAX)));
            response.body = Bytes::new();
        }

        self.exec_after_response(hooks, errors, snap, request, &response);
        response
    }

    /// Run one decision phase's chain (`on_request`, `before_route`,
    /// `guard`, `resolve`, `before_handle`): the first hook that produces
    /// a response short-circuits the rest of the request pipeline (§8).
    /// `None` means every hook continued.
    async fn exec_decision(
        &self,
        phase: Phase,
        hooks: &RouteHooks,
        errors: &Arc<ErrorTable>,
        snap: &mut Option<Arc<RequestSnapshot>>,
        request: &Request,
    ) -> Option<Response> {
        for id in hooks.chain(phase) {
            let Some(entry) = self.hooks.get(*id as usize) else {
                continue;
            };
            match entry {
                HookEntry::Native { name, hook } => {
                    match self.call_native(hook, name, phase, request, None) {
                        HookDecision::Continue => {}
                        HookDecision::Respond(response) => return Some(response),
                    }
                }
                HookEntry::Python {
                    callable, is_async, ..
                } => {
                    let snapshot = ensure_snap(snap, request);
                    let args = hook_args(phase, snapshot, None);
                    match self
                        .submit(JobSpec {
                            callable: Arc::clone(callable),
                            is_async: *is_async,
                            args,
                            label: hook_label(request, phase),
                            contract: Arc::clone(&self.empty_contract),
                            none_ok: false,
                            route_errors: true,
                            hook: true,
                            errors: Arc::clone(errors),
                        })
                        .await
                    {
                        JobAnswer::HookReturn(PyReturn::NoneReturned) => {}
                        JobAnswer::HookReturn(ret) => {
                            return Some(convert::into_response(ret));
                        }
                        JobAnswer::Response(response) => return Some(response),
                        JobAnswer::Deferred(call) => {
                            return Some(self.drain_deferred(call, errors).await);
                        }
                    }
                }
            }
        }
        None
    }

    /// Run one post-handler phase (`after_handle`, `on_response`) over the
    /// in-flight response: hooks receive `(request, response)` and a
    /// non-`None` return — including in-place header mutation — replaces
    /// it (§8).
    async fn exec_post(
        &self,
        phase: Phase,
        hooks: &RouteHooks,
        errors: &Arc<ErrorTable>,
        snap: &mut Option<Arc<RequestSnapshot>>,
        request: &Request,
        mut response: Response,
    ) -> Response {
        for id in hooks.chain(phase) {
            let Some(entry) = self.hooks.get(*id as usize) else {
                continue;
            };
            match entry {
                HookEntry::Native { name, hook } => {
                    let mut current = response;
                    match self.call_native(hook, name, phase, request, Some(&mut current)) {
                        HookDecision::Continue => response = current,
                        HookDecision::Respond(replacement) => response = replacement,
                    }
                }
                HookEntry::Python {
                    callable, is_async, ..
                } => {
                    let snapshot = ensure_snap(snap, request);
                    let args = hook_args(phase, snapshot, Some(&response));
                    match self
                        .submit(JobSpec {
                            callable: Arc::clone(callable),
                            is_async: *is_async,
                            args,
                            label: hook_label(request, phase),
                            contract: Arc::clone(&self.empty_contract),
                            none_ok: false,
                            route_errors: true,
                            hook: true,
                            errors: Arc::clone(errors),
                        })
                        .await
                    {
                        JobAnswer::HookReturn(PyReturn::NoneReturned) => {}
                        JobAnswer::HookReturn(ret) => response = convert::into_response(ret),
                        JobAnswer::Response(answer) => response = answer,
                        JobAnswer::Deferred(call) => {
                            response = self.drain_deferred(call, errors).await;
                        }
                    }
                }
            }
        }
        response
    }

    /// §8 phase 16: `after_response` — access log, metrics, background
    /// work. Native hooks run inline on a clone of the produced response;
    /// Python hooks are submitted fire-and-forget (the response is
    /// already being written — a hook can never change it), so they
    /// never gate the reply.
    fn exec_after_response(
        &self,
        hooks: &RouteHooks,
        errors: &Arc<ErrorTable>,
        mut snap: Option<Arc<RequestSnapshot>>,
        request: &Request,
        response: &Response,
    ) {
        for id in hooks.chain(Phase::AfterResponse) {
            let Some(entry) = self.hooks.get(*id as usize) else {
                continue;
            };
            match entry {
                HookEntry::Native { name, hook } => {
                    let mut copy = response.clone();
                    let _ = self.call_native(
                        hook,
                        name,
                        Phase::AfterResponse,
                        request,
                        Some(&mut copy),
                    );
                }
                HookEntry::Python {
                    callable,
                    is_async,
                    name,
                } => {
                    let snapshot = ensure_snap(&mut snap, request);
                    let args = hook_args(Phase::AfterResponse, snapshot, Some(response));
                    let (tx, rx) = oneshot::channel();
                    let submitted = self.enqueue(
                        JobSpec {
                            callable: Arc::clone(callable),
                            is_async: *is_async,
                            args,
                            label: hook_label(request, Phase::AfterResponse),
                            contract: Arc::clone(&self.empty_contract),
                            none_ok: false,
                            route_errors: false,
                            hook: true,
                            errors: Arc::clone(errors),
                        },
                        tx,
                    );
                    match submitted {
                        Ok(()) => {
                            tokio::spawn(async move {
                                // Errors inside settle are already logged;
                                // the answer itself is discarded.
                                let _ = rx.await;
                            });
                        }
                        Err(answer) => {
                            log_message(
                                &self.logger,
                                format!(
                                    "after_response hook `{name}` dropped: executor queue full"
                                ),
                            );
                            drop(answer);
                        }
                    }
                }
            }
        }
    }

    /// Run one native hook inside a panic boundary; a panicking hook logs
    /// and answers `500` (post phases drop that answer — the response
    /// being observed is already committed).
    fn call_native(
        &self,
        hook: &crate::hooks::NativeHook,
        name: &str,
        phase: Phase,
        request: &Request,
        response: Option<&mut Response>,
    ) -> HookDecision {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut cx = HookCx {
                phase,
                method: &request.method,
                path: request.uri.path(),
                query: request.uri.query(),
                headers: &request.headers,
                body: &request.body,
                remote_addr: request.remote_addr,
                response,
            };
            hook(&mut cx)
        }));
        if let Ok(decision) = result {
            decision
        } else {
            log_message(
                &self.logger,
                format!("native hook `{name}` panicked in {phase}"),
            );
            HookDecision::Respond(convert::internal_error())
        }
    }

    /// Submit `args` to the handler's executor and await the answer — then
    /// keep re-submitting while the answer defers to an `on_error`
    /// handler (§17). A deferred call always returns a `Response`: its
    /// own failures never defer again (`route_errors = false`).
    async fn run(
        &self,
        entry: &HandlerEntry,
        args: Box<[ParamArg]>,
        method: Method,
        path: String,
    ) -> Response {
        let errors = Arc::clone(self.error_table(entry.error_table));
        let mut answer = self
            .submit(JobSpec {
                callable: Arc::clone(&entry.callable),
                is_async: entry.is_async,
                args,
                label: format!("{method} {path}").into_boxed_str(),
                contract: Arc::clone(&entry.response_contract),
                none_ok: entry.none_ok,
                route_errors: true,
                hook: false,
                errors,
            })
            .await;
        loop {
            match answer {
                JobAnswer::Response(response) => return response,
                JobAnswer::HookReturn(_) => return convert::internal_error(),
                JobAnswer::Deferred(call) => {
                    answer = self.deferred_answer(call).await;
                }
            }
        }
    }

    /// Resubmit a deferred `on_error` call until it answers: error
    /// handlers never defer again (`route_errors = false`), so the loop
    /// can only re-enter on a pathological bridge bug — the response it
    /// produces then is a `500`.
    async fn drain_deferred(
        &self,
        call: convert::DeferredCall,
        errors: &Arc<ErrorTable>,
    ) -> Response {
        let mut answer = self
            .submit(JobSpec {
                callable: Arc::clone(&call.callable),
                is_async: call.is_async,
                args: Box::new([ParamArg {
                    name: "exc".to_owned(),
                    kind: ArgKind::Positional,
                    value: ParamValue::Obj(call.arg),
                }]),
                label: call.label,
                contract: Arc::clone(&self.empty_contract),
                none_ok: false,
                route_errors: false,
                hook: false,
                errors: Arc::clone(errors),
            })
            .await;
        loop {
            match answer {
                JobAnswer::Response(response) => return response,
                JobAnswer::HookReturn(_) => return convert::internal_error(),
                JobAnswer::Deferred(call) => {
                    answer = self.deferred_answer(call).await;
                }
            }
        }
    }

    /// One deferred `on_error` resubmission, re-routed through the app's
    /// default table — the origin route's table already ran.
    async fn deferred_answer(&self, call: convert::DeferredCall) -> JobAnswer {
        self.submit(JobSpec {
            callable: Arc::clone(&call.callable),
            is_async: call.is_async,
            args: Box::new([ParamArg {
                name: "exc".to_owned(),
                kind: ArgKind::Positional,
                value: ParamValue::Obj(call.arg),
            }]),
            label: call.label,
            contract: Arc::clone(&self.empty_contract),
            none_ok: false,
            route_errors: false,
            hook: false,
            errors: Arc::clone(self.error_table(0)),
        })
        .await
    }

    /// Put one call on the sync pool or the asyncio loop and await the
    /// job's oneshot. `contract`/`none_ok`/`route_errors` ride the job (a
    /// deferred `on_error` call always submits the empty contract with
    /// both flags `false`; a hook job gets `hook = true` so the answer is
    /// the raw [`PyReturn`]).
    async fn submit(&self, spec: JobSpec) -> JobAnswer {
        let (tx, rx) = oneshot::channel();
        match self.enqueue(spec, tx) {
            Ok(()) => rx
                .await
                .unwrap_or_else(|_| JobAnswer::Response(convert::internal_error())),
            Err(answer) => answer,
        }
    }

    /// The submission half of [`submit`] — enqueue without awaiting, so
    /// fire-and-forget `after_response` hooks can drop the receiver.
    #[expect(
        clippy::result_large_err,
        reason = "the Err arm travels the same oneshot-sized path as Ok — boxing it would add an allocation to save nothing"
    )]
    fn enqueue(&self, spec: JobSpec, respond: oneshot::Sender<JobAnswer>) -> Result<(), JobAnswer> {
        let JobSpec {
            callable,
            is_async,
            args,
            label,
            contract,
            none_ok,
            route_errors,
            hook,
            errors,
        } = spec;
        if is_async {
            let Some(loop_thread) = &self.async_loop else {
                log_message(
                    &self.logger,
                    format!("{label}: async job has no loop thread"),
                );
                return Err(JobAnswer::Response(convert::internal_error()));
            };
            match loop_thread.submit(
                callable,
                args,
                label,
                contract,
                none_ok,
                route_errors,
                hook,
                errors,
                respond,
            ) {
                Ok(()) => Ok(()),
                Err(AsyncSubmitError::Full) => {
                    Err(JobAnswer::Response(convert::service_unavailable()))
                }
                Err(AsyncSubmitError::Unavailable) => {
                    Err(JobAnswer::Response(convert::internal_error()))
                }
            }
        } else {
            let job = SyncJob {
                callable,
                args,
                label,
                enqueued: Instant::now(),
                contract,
                none_ok,
                route_errors,
                hook,
                errors,
                respond,
            };
            match self.pool.try_submit(job) {
                Ok(()) => Ok(()),
                Err(SubmitError::Full) => Err(JobAnswer::Response(convert::service_unavailable())),
            }
        }
    }

    /// The route's `on_error` table; `0` is the app-wide table.
    fn error_table(&self, index: u32) -> &Arc<ErrorTable> {
        if index == 0 {
            &self.app_errors
        } else {
            self.error_tables
                .get(index as usize - 1)
                .unwrap_or(&self.app_errors)
        }
    }
}

/// Build the `404`/`405`/redirect/`OPTIONS` answer for a lookup that did
/// not hit (§8 phase 4, §9.2).
fn early_response(request: &Request, lookup: &RouteLookup<'_>) -> Response {
    match lookup {
        RouteLookup::Hit(_) => convert::internal_error(),
        RouteLookup::Redirect(target) => {
            let location = match request.uri.query() {
                Some(q) => format!("{target}?{q}"),
                None => target.clone(),
            };
            let mut response = Response::status(StatusCode::PERMANENT_REDIRECT);
            if let Ok(value) = HeaderValue::from_str(&location) {
                response.headers.insert(LOCATION, value);
            }
            response
        }
        RouteLookup::MethodNotAllowed(allow) => {
            let allow = allow.header();
            let mut response = if request.method == Method::OPTIONS {
                // §9.2: automatic OPTIONS answers with the Allow set.
                Response::status(StatusCode::NO_CONTENT)
            } else {
                convert::problem_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "about:blank",
                    "Method Not Allowed",
                    None,
                    None,
                )
            };
            if let Ok(value) = HeaderValue::from_str(&allow) {
                response.headers.insert(ALLOW, value);
            }
            response
        }
        RouteLookup::NotFound => convert::problem_response(
            StatusCode::NOT_FOUND,
            "about:blank",
            "Not Found",
            None,
            None,
        ),
    }
}

/// The shared [`RequestSnapshot`] — built on first need, so routes
/// without Python hooks or `request` params never pay for it.
fn ensure_snap<'a>(
    snap: &'a mut Option<Arc<RequestSnapshot>>,
    request: &Request,
) -> &'a Arc<RequestSnapshot> {
    snap.get_or_insert_with(|| Arc::new(RequestSnapshot::new(request)))
}

/// A hook job's arguments: `(request)` for decision phases,
/// `(request, response)` for post-handler phases (§8).
fn hook_args(
    phase: Phase,
    snap: &Arc<RequestSnapshot>,
    response: Option<&Response>,
) -> Box<[ParamArg]> {
    let request_arg = ParamArg {
        name: "request".to_owned(),
        kind: ArgKind::Positional,
        value: ParamValue::Request(Arc::clone(snap)),
    };
    match (phase.takes_response(), response) {
        (true, Some(response)) => Box::new([
            request_arg,
            ParamArg {
                name: "response".to_owned(),
                kind: ArgKind::Positional,
                value: ParamValue::ResponseView(Arc::new(ResponseView::new(response))),
            },
        ]),
        _ => Box::new([request_arg]),
    }
}

/// `"{method} {path}#{phase}"` — the job label hook submissions log under.
fn hook_label(request: &Request, phase: Phase) -> Box<str> {
    format!("{} {}#{phase}", request.method, request.uri.path()).into_boxed_str()
}

/// Log a message through the `fulmen` logger; attaches briefly and stays
/// silent when the interpreter is unreachable (shutdown).
pub(crate) fn log_message(logger: &Py<PyAny>, message: String) {
    let _ = Python::try_attach(|py| convert::log_message(py, logger, message));
}
