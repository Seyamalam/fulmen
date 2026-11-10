//! The dispatcher: `fulmen_server::Dispatcher` implemented over the compiled
//! route table and the sync thread pool.
//!
//! The server is dumb transport; this file owns the HTTP semantics
//! (DESIGN.md §8, §9.2): `404`/`405`+`Allow`, automatic `OPTIONS`, automatic
//! `HEAD` (the `GET` response with its body dropped), `308` redirects under
//! the redirect trailing-slash policy, load shedding (`503` + `Retry-After`),
//! and the panic boundary of §14.6.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use fulmen_core::{CompiledApp, PathValue, RouteLookup};
use fulmen_server::{DispatchFuture, Dispatcher, Request, Response};
use http::header::{ALLOW, CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RETRY_AFTER};
use http::{HeaderValue, Method, StatusCode};
use pyo3::prelude::*;
use tokio::sync::oneshot;

use crate::convert::{self, ParamArg, ParamValue};
use crate::pool::{SubmitError, SyncJob, SyncPool};

/// The plain-text content type used for all built-in status responses.
const CT_TEXT: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");

/// How a handler is invoked.
#[derive(Debug)]
pub(crate) enum HandlerKind {
    /// `def` — runs on the sync pool. `kw_flags` marks, per parameter in
    /// handler signature order, which are keyword-only (passed by name).
    Sync { kw_flags: Box<[bool]> },
    /// `async def` — reserved for the asyncio dispatch path (M1.15). For
    /// now requests to such routes answer `500`, logged, at dispatch time;
    /// see [`dispatch_async`].
    Async,
}

/// One entry in the handler table, indexed by `RouteInfo::handler_index`.
#[derive(Debug)]
pub(crate) struct HandlerEntry {
    /// The Python callable, `Arc`-wrapped so dispatch can clone the handle
    /// without taking the GIL.
    pub(crate) callable: Arc<Py<PyAny>>,
    /// How to call it.
    pub(crate) kind: HandlerKind,
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
        let method = request.method;
        let path = request.uri.path().to_owned();
        let query = request.uri.query().map(str::to_owned);

        let resolved = catch_unwind(AssertUnwindSafe(|| {
            self.resolve(&method, &path, query.as_deref())
        }));
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

        let mut response = match &entry.kind {
            HandlerKind::Sync { .. } => self.dispatch_sync(entry, args, method, path).await,
            HandlerKind::Async => self.dispatch_async(entry, args, method, path).await,
        };

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
    fn resolve(&self, method: &Method, path: &str, query: Option<&str>) -> Action {
        match self.app.route(method, path) {
            RouteLookup::Hit(hit) => {
                let info = self.app.route_info(hit.route);
                let args: Box<[ParamArg]> = hit
                    .params
                    .iter()
                    .map(|p| ParamArg {
                        name: p.name.to_owned(),
                        keyword_only: false, // filled in by dispatch_sync
                        value: match &p.value {
                            PathValue::Int(i) => ParamValue::Int(*i),
                            PathValue::Str(s) => ParamValue::Str(s.to_string()),
                        },
                    })
                    .collect();
                Action::Call {
                    index: info.handler_index(),
                    args,
                    implicit_head: hit.implicit_head,
                }
            }
            RouteLookup::Redirect(target) => {
                let location = match query {
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
                let mut response = if method == Method::OPTIONS {
                    // §9.2: automatic OPTIONS answers with the Allow set.
                    Response::status(StatusCode::NO_CONTENT)
                } else {
                    let mut response = Response::status(StatusCode::METHOD_NOT_ALLOWED);
                    response.headers.insert(CONTENT_TYPE, CT_TEXT);
                    response.body = Bytes::from_static(b"Method Not Allowed");
                    response
                };
                if let Ok(value) = HeaderValue::from_str(&allow) {
                    response.headers.insert(ALLOW, value);
                }
                Action::Reply(response)
            }
            RouteLookup::NotFound => {
                let mut response = Response::status(StatusCode::NOT_FOUND);
                response.headers.insert(CONTENT_TYPE, CT_TEXT);
                response.body = Bytes::from_static(b"Not Found");
                Action::Reply(response)
            }
        }
    }

    /// Submit the call to the sync pool and await the oneshot.
    async fn dispatch_sync(
        &self,
        entry: &HandlerEntry,
        args: Box<[ParamArg]>,
        method: Method,
        path: String,
    ) -> Response {
        let HandlerKind::Sync { kw_flags } = &entry.kind else {
            return convert::internal_error();
        };
        if kw_flags.len() != args.len() {
            log_message(
                &self.logger,
                format!("{method} {path}: parameter spec does not match the signature"),
            );
            return convert::internal_error();
        }
        let mut args = Vec::from(args);
        for (arg, kw) in args.iter_mut().zip(kw_flags.iter()) {
            arg.keyword_only = *kw;
        }
        let (tx, rx) = oneshot::channel();
        let job = SyncJob {
            callable: Arc::clone(&entry.callable),
            args: args.into_boxed_slice(),
            label: format!("{method} {path}").into_boxed_str(),
            enqueued: Instant::now(),
            respond: tx,
        };
        match self.pool.try_submit(job) {
            Ok(()) => rx.await.unwrap_or_else(|_| convert::internal_error()),
            Err(SubmitError::Full) => {
                let mut response = Response::status(StatusCode::SERVICE_UNAVAILABLE);
                response.headers.insert(CONTENT_TYPE, CT_TEXT);
                response
                    .headers
                    .insert(RETRY_AFTER, HeaderValue::from_static("1"));
                response.body = Bytes::from_static(b"Service Unavailable");
                response
            }
        }
    }

    /// The `async def` dispatch seam (M1.15): a loop thread running
    /// `loop.call_soon_threadsafe` + a completion oneshot lands here. Until
    /// that exists, an async route answers `500` and logs at dispatch time —
    /// startup stays possible so sync-only apps are unaffected.
    #[expect(
        clippy::unused_async,
        reason = "the M1.15 asyncio bridge lands here and awaits a completion channel"
    )]
    async fn dispatch_async(
        &self,
        _entry: &HandlerEntry,
        _args: Box<[ParamArg]>,
        method: Method,
        path: String,
    ) -> Response {
        log_message(
            &self.logger,
            format!("{method} {path}: async handlers are not supported yet"),
        );
        convert::internal_error()
    }
}

/// Log a message through the `fulmen` logger; attaches briefly and stays
/// silent when the interpreter is unreachable (shutdown).
fn log_message(logger: &Py<PyAny>, message: String) {
    let _ = Python::try_attach(|py| convert::log_message(py, logger, message));
}
