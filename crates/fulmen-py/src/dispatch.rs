//! The dispatcher: `fulmen_server::Dispatcher` implemented over the compiled
//! route table and the sync pool.
//!
//! The server is dumb transport; this file owns the HTTP semantics
//! (DESIGN.md §8, §9.2): `404`/`405`+`Allow`, automatic `OPTIONS`, automatic
//! `HEAD` (the `GET` response with its body dropped), `308` redirects under
//! the redirect trailing-slash policy, load shedding (`503` + `Retry-After`),
//! and the panic boundary of §14.6. Every error answer is an RFC 9457
//! `application/problem+json` body (§17).
//!
//! After routing, [`crate::extract`] builds each handler's call arguments
//! from its `call_spec` (path/query/header/cookie/body/form) — parameter
//! validation runs here on the I/O thread, so `4xx` answers never enter
//! Python. A handler failure that an `@app.on_error` registration claims
//! comes back as [`JobAnswer::Deferred`]; the dispatcher re-submits that
//! error handler onto the matching executor with error routing disabled
//! (a failing error handler answers `500`, it never defers again).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use fulmen_core::{CompiledApp, RouteLookup};
use fulmen_schema::Validator;
use fulmen_schema::desc::Kind;
use fulmen_schema::errors::Source;
use fulmen_server::{DispatchFuture, Dispatcher, Request, Response};
use http::header::{ALLOW, CONTENT_LENGTH, LOCATION};
use http::{HeaderValue, Method, StatusCode};
use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::aloop::{AsyncLoop, AsyncSubmitError};
use crate::convert::{self, ArgKind, JobAnswer, ParamArg, ParamValue};
use crate::descriptor::Descriptor;
use crate::extract;
use crate::pool::{SubmitError, SyncJob, SyncPool};

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
            Self::Body => Source::Body,
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
/// `(None, None)` is the dynamic `Response` escape. Stored for the
/// `OpenAPI` and debug-check tasks; dispatch does not read it.
#[derive(Debug)]
pub(crate) struct ResponseDesc {
    /// The declared status (`None` — dynamic `Response`).
    #[expect(
        dead_code,
        reason = "consumed by the M2.11 `OpenAPI` encoder, not dispatch"
    )]
    pub(crate) status: Option<u16>,
    /// The declared body's descriptor, when the variant carries one.
    #[expect(
        dead_code,
        reason = "consumed by the M2.11 `OpenAPI` encoder, not dispatch"
    )]
    pub(crate) desc: Option<Arc<Py<Descriptor>>>,
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
    /// The declared response variants (`OpenAPI`/debug checking).
    #[expect(
        dead_code,
        reason = "consumed by the M2.11 `OpenAPI` encoder, not dispatch"
    )]
    pub(crate) response_descs: Box<[ResponseDesc]>,
}

/// Everything one running server needs on top of the frozen route table.
/// Shared by every dispatch via `Arc`.
pub(crate) struct AppInner {
    /// The compiled routes.
    pub(crate) app: CompiledApp,
    /// Handler table indexed by `handler_index`.
    pub(crate) handlers: Vec<HandlerEntry>,
    /// `logging.getLogger("fulmen")` — cached for workers and error paths.
    pub(crate) logger: Arc<Py<PyAny>>,
    /// The bounded sync-handler pool.
    pub(crate) pool: SyncPool,
    /// The asyncio loop thread; `Some` only when the app has async routes.
    pub(crate) async_loop: Option<AsyncLoop>,
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

/// What the route lookup decided, with all borrows resolved to owned data so
/// nothing ties the future to the request lifetime.
enum Action {
    /// Call `handlers[index]` with `args`; `implicit_head` drops the body.
    Call {
        /// Index into the handler table.
        index: u32,
        /// Owned call arguments.
        args: Box<[ParamArg]>,
        /// `HEAD` answered by a `GET` route.
        implicit_head: bool,
    },
    /// Answer directly without a handler.
    Reply(Response),
}

impl AppInner {
    /// Route, dispatch, and produce the wire response. Must never panic:
    /// routing runs under [`catch_unwind`], handler calls are isolated on
    /// the pool (§14.6), and every lookup uses checked access.
    async fn handle(&self, request: Request) -> Response {
        let method = request.method.clone();
        let path = request.uri.path().to_owned();

        let resolved = catch_unwind(AssertUnwindSafe(|| self.resolve(&request)));
        let Ok(action) = resolved else {
            log_message(&self.logger, format!("router panicked on {method} {path}"));
            return convert::internal_error();
        };

        let (index, args, implicit_head) = match action {
            Action::Reply(response) => return response,
            Action::Call {
                index,
                args,
                implicit_head,
            } => (index, args, implicit_head),
        };

        let Some(entry) = self.handlers.get(index as usize) else {
            log_message(
                &self.logger,
                format!("{method} {path}: handler index {index} out of range"),
            );
            return convert::internal_error();
        };

        let mut response = self.run(entry, args, method, path).await;

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
        response
    }

    /// Turn the route lookup into an owned [`Action`] (runs under
    /// `catch_unwind` by the caller).
    fn resolve(&self, request: &Request) -> Action {
        match self.app.route(&request.method, request.uri.path()) {
            RouteLookup::Hit(hit) => {
                let info = self.app.route_info(hit.route);
                let Some(entry) = self.handlers.get(info.handler_index() as usize) else {
                    return Action::Reply(convert::internal_error());
                };
                match extract::extract_args(&hit, &entry.call_spec, request) {
                    extract::Extracted::Call(args) => Action::Call {
                        index: info.handler_index(),
                        args,
                        implicit_head: hit.implicit_head,
                    },
                    extract::Extracted::Reply(response) => Action::Reply(response),
                }
            }
            RouteLookup::Redirect(target) => {
                let location = match request.uri.query() {
                    Some(q) => format!("{target}?{q}"),
                    None => target,
                };
                let mut response = Response::status(StatusCode::PERMANENT_REDIRECT);
                if let Ok(value) = HeaderValue::from_str(&location) {
                    response.headers.insert(LOCATION, value);
                }
                Action::Reply(response)
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
                Action::Reply(response)
            }
            RouteLookup::NotFound => Action::Reply(convert::problem_response(
                StatusCode::NOT_FOUND,
                "about:blank",
                "Not Found",
                None,
                None,
            )),
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
        let mut answer = self
            .submit(
                entry.is_async,
                &entry.callable,
                args,
                format!("{method} {path}").into_boxed_str(),
                entry.none_ok,
                true,
            )
            .await;
        loop {
            match answer {
                JobAnswer::Response(response) => return response,
                JobAnswer::Deferred(call) => {
                    let args = Box::new([ParamArg {
                        name: "exc".to_owned(),
                        kind: ArgKind::Positional,
                        value: ParamValue::Obj(call.arg),
                    }]);
                    answer = self
                        .submit(
                            call.is_async,
                            &call.callable,
                            args,
                            call.label,
                            false,
                            false,
                        )
                        .await;
                }
            }
        }
    }

    /// Put one call on the sync pool or the asyncio loop and await the
    /// job's oneshot. `none_ok`/`route_errors` ride the job (a deferred
    /// `on_error` call always submits with both `false`).
    async fn submit(
        &self,
        is_async: bool,
        callable: &Arc<Py<PyAny>>,
        args: Box<[ParamArg]>,
        label: Box<str>,
        none_ok: bool,
        route_errors: bool,
    ) -> JobAnswer {
        if is_async {
            let Some(loop_thread) = &self.async_loop else {
                log_message(
                    &self.logger,
                    format!("{label}: async job has no loop thread"),
                );
                return JobAnswer::Response(convert::internal_error());
            };
            let (tx, rx) = oneshot::channel();
            match loop_thread.submit(Arc::clone(callable), args, label, none_ok, route_errors, tx) {
                Ok(()) => rx
                    .await
                    .unwrap_or_else(|_| JobAnswer::Response(convert::internal_error())),
                Err(AsyncSubmitError::Full) => JobAnswer::Response(convert::service_unavailable()),
                Err(AsyncSubmitError::Unavailable) => {
                    JobAnswer::Response(convert::internal_error())
                }
            }
        } else {
            let (tx, rx) = oneshot::channel();
            let job = SyncJob {
                callable: Arc::clone(callable),
                args,
                label,
                enqueued: Instant::now(),
                none_ok,
                route_errors,
                respond: tx,
            };
            match self.pool.try_submit(job) {
                Ok(()) => rx
                    .await
                    .unwrap_or_else(|_| JobAnswer::Response(convert::internal_error())),
                Err(SubmitError::Full) => JobAnswer::Response(convert::service_unavailable()),
            }
        }
    }
}

/// Log a message through the `fulmen` logger; attaches briefly and stays
/// silent when the interpreter is unreachable (shutdown).
fn log_message(logger: &Py<PyAny>, message: String) {
    let _ = Python::try_attach(|py| convert::log_message(py, logger, message));
}
