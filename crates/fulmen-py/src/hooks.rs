//! Lifecycle hooks (DESIGN.md §8): the per-phase hook table, the native
//! hook extension point, and the lazy `Request`/`Response` views hooks
//! receive.
//!
//! Two hook kinds share one id space (the ids resolved `RouteHooks`
//! chains carry). [`HookEntry::Native`] entries are Rust closures
//! registered at startup — the slot security plugins and DI fill (M4+) —
//! and run inline on the dispatch thread without the GIL, so they cost no
//! Python crossing. [`HookEntry::Python`] entries are callables submitted
//! through the executor machinery exactly like handlers: `def` hooks take
//! the sync pool, `async def` hooks take the asyncio loop thread, and the
//! dispatch future never touches the GIL itself.
//!
//! [`RequestSnapshot`] and [`ResponseView`] defer building the Python
//! objects a hook receives until an executor job materializes its
//! arguments — inside the job's own `Python::attach`, so arg construction
//! never costs an extra GIL acquisition on the dispatch path.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use fulmen_core::Phase;
use fulmen_server::{Request, Response};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, DATE};
use http::{HeaderMap, Method};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};

/// What one hook invocation decided.
pub(crate) enum HookDecision {
    /// Continue the pipeline.
    #[expect(
        dead_code,
        reason = "only native-hook closures (M4+) construct it — the dispatch path matches it today"
    )]
    Continue,
    /// The phase produced a response — a short-circuit answer for the
    /// decision phases, a replacement for the post-handler phases.
    Respond(Response),
}

/// The wire-level view a native hook sees.
///
/// `response` is `Some` only for the post-handler phases
/// (`after_handle`/`on_response`/`after_response`): mutating it mutates
/// what the client receives for the first two; `after_response` gets a
/// clone, since the response is already produced.
#[expect(
    dead_code,
    reason = "native-hook closures read these fields — the M4 security plugin SDK is the first consumer"
)]
pub(crate) struct HookCx<'a> {
    /// The phase the hook is running in.
    pub(crate) phase: Phase,
    /// The request method.
    pub(crate) method: &'a Method,
    /// The request path (raw, still percent-encoded).
    pub(crate) path: &'a str,
    /// The query string, when present.
    pub(crate) query: Option<&'a str>,
    /// The request headers.
    pub(crate) headers: &'a HeaderMap,
    /// The request body.
    pub(crate) body: &'a [u8],
    /// The peer address.
    pub(crate) remote_addr: SocketAddr,
    /// The in-flight response, for post-handler phases.
    pub(crate) response: Option<&'a mut Response>,
}

/// A native hook: a Rust closure running inline on the dispatch thread.
/// `pub` inside the crate so `fulmen-security`-facing adapters (M4) can
/// register them; Python cannot construct one.
pub(crate) type NativeHook = Arc<dyn for<'a> Fn(&mut HookCx<'a>) -> HookDecision + Send + Sync>;

/// One entry of the app's hook table, indexed by the hook ids in the
/// resolved [`fulmen_core::RouteHooks`] chains.
pub(crate) enum HookEntry {
    /// A Rust closure — runs inline, no Python crossing.
    Native {
        /// The registration name, for `fulmen routes --hooks` and logs.
        name: Box<str>,
        /// The hook.
        hook: NativeHook,
    },
    /// A Python callable — one executor submission (one crossing) per call.
    Python {
        /// The registration name (`fn.__qualname__`), for diagnostics.
        name: Box<str>,
        /// The callable, `Arc`-wrapped so dispatch clones without the GIL.
        callable: Arc<Py<PyAny>>,
        /// `inspect.iscoroutinefunction` — picks the executor lane.
        is_async: bool,
    },
}

#[expect(
    dead_code,
    reason = "the M4 plugin SDK's diagnostics consume these accessors"
)]
impl HookEntry {
    /// `true` for Python hooks — the ones that cost a crossing (§8).
    pub(crate) fn is_python(&self) -> bool {
        matches!(self, Self::Python { .. })
    }

    /// The registration name, for `fulmen routes --hooks` and log lines.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Native { name, .. } | Self::Python { name, .. } => name,
        }
    }

    /// `"native"` or `"python"` — the `--hooks`/`--cost` kind label.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Native { .. } => "native",
            Self::Python { .. } => "python",
        }
    }
}

/// Everything needed to build the `fulmen.request.Request` object hooks
/// and `request`-sourced handler parameters share.
///
/// The snapshot is Rust data built once per request — but only when the
/// route actually needs a Python object (any Python hook in its resolved
/// chains, or a `request: Request` parameter). The `Request` instance
/// itself materializes lazily inside the first executor job that resolves
/// it ([`OnceLock`]), so building it never costs a separate crossing and
/// every hook plus the handler sees the *same* object — `request.state`
/// writes travel between hooks and the handler.
pub(crate) struct RequestSnapshot {
    method: Box<str>,
    path: Box<str>,
    query: Box<str>,
    headers: Vec<(Box<str>, Box<str>)>,
    body: Bytes,
    remote: Box<str>,
    obj: OnceLock<Py<PyAny>>,
}

impl RequestSnapshot {
    /// Capture the wire request into owned Rust data.
    pub(crate) fn new(request: &Request) -> Self {
        Self {
            method: request.method.as_str().into(),
            path: request.uri.path().into(),
            query: request.uri.query().unwrap_or("").into(),
            headers: request
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().into(),
                        String::from_utf8_lossy(value.as_bytes())
                            .into_owned()
                            .into(),
                    )
                })
                .collect(),
            body: request.body.clone(),
            remote: request.remote_addr.to_string().into(),
            obj: OnceLock::new(),
        }
    }

    /// The shared `Request` object, building it on first use.
    ///
    /// Runs inside an executor's `Python::attach`; a racing builder loses
    /// its object to [`OnceLock::set`] so every caller shares one instance.
    ///
    /// # Errors
    /// [`InvokeError::Internal`](crate::convert::InvokeError::Internal)
    /// when `fulmen.request.Request._from_wire` fails.
    pub(crate) fn object(
        &self,
        py: Python<'_>,
        request_cls: &Py<PyAny>,
    ) -> Result<Py<PyAny>, crate::convert::InvokeError> {
        if let Some(obj) = self.obj.get() {
            return Ok(obj.clone_ref(py));
        }
        let built = self.build(py, request_cls)?;
        let _ = self.obj.set(built);
        match self.obj.get() {
            Some(obj) => Ok(obj.clone_ref(py)),
            None => Err(crate::convert::InvokeError::Internal(
                pyo3::exceptions::PyRuntimeError::new_err("request object was not initialized"),
            )),
        }
    }

    /// `Request._from_wire(method, path, query, headers, body, remote)`.
    fn build(
        &self,
        py: Python<'_>,
        request_cls: &Py<PyAny>,
    ) -> Result<Py<PyAny>, crate::convert::InvokeError> {
        let cls = request_cls.bind(py);
        let pairs = self
            .headers
            .iter()
            .map(|(name, value)| PyTuple::new(py, [name.as_ref(), value.as_ref()]))
            .collect::<PyResult<Vec<_>>>()
            .map_err(crate::convert::InvokeError::Internal)?;
        let headers = PyList::new(py, pairs).map_err(crate::convert::InvokeError::Internal)?;
        cls.call_method1(
            "_from_wire",
            (
                self.method.as_ref(),
                self.path.as_ref(),
                self.query.as_ref(),
                headers,
                PyBytes::new(py, &self.body),
                self.remote.as_ref(),
            ),
        )
        .map(Bound::unbind)
        .map_err(crate::convert::InvokeError::Internal)
    }
}

impl std::fmt::Debug for RequestSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestSnapshot")
            .field("method", &self.method)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A wire [`Response`] about to materialize as a
/// `fulmen.responses.Response` for a post-phase hook.
///
/// The view carries status, headers and body so the hook can mutate the
/// object in place (`response.headers["x"] = "…"`) or answer a different
/// value entirely — the registration wrapper returns whichever applies
/// and the executor converts it back like any other response.
/// `Content-Length` and `Date` are dropped: the server manages both
/// (the body a hook edits makes a copied length a lie).
///
/// The materialized object is kept ([`OnceLock`]) so a hook that mutates
/// `response` in place and returns `None` still propagates its edits —
/// the dispatcher converts the stored object back to the wire response
/// (§8 post-phase semantics).
#[derive(Debug)]
pub(crate) struct ResponseView {
    status: u16,
    headers: Vec<(Box<str>, Box<str>)>,
    content_type: Option<Box<str>>,
    body: Bytes,
    obj: OnceLock<Py<PyAny>>,
}

impl ResponseView {
    /// Snapshot the in-flight wire response.
    pub(crate) fn new(response: &Response) -> Self {
        Self {
            status: response.status.as_u16(),
            headers: response
                .headers
                .iter()
                .filter(|(name, _)| *name != CONTENT_LENGTH && *name != DATE)
                .map(|(name, value)| {
                    (
                        name.as_str().into(),
                        String::from_utf8_lossy(value.as_bytes())
                            .into_owned()
                            .into(),
                    )
                })
                .collect(),
            content_type: response
                .headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(Into::into),
            body: response.body.clone(),
            obj: OnceLock::new(),
        }
    }

    /// The materialized `Response` object, when it has been built.
    pub(crate) fn object(&self) -> Option<&Py<PyAny>> {
        self.obj.get()
    }

    /// `Response(body=body, status=status, headers=dict, content_type=ct)`,
    /// built once per view.
    ///
    /// # Errors
    /// [`InvokeError::Internal`](crate::convert::InvokeError::Internal)
    /// when the `Response` constructor rejects the view.
    pub(crate) fn materialize<'py>(
        &self,
        py: Python<'py>,
        response_cls: &Py<PyAny>,
    ) -> Result<Bound<'py, PyAny>, crate::convert::InvokeError> {
        if let Some(obj) = self.obj.get() {
            return Ok(obj.bind(py).clone());
        }
        let headers = PyDict::new(py);
        for (name, value) in &self.headers {
            headers
                .set_item(name.as_ref(), value.as_ref())
                .map_err(crate::convert::InvokeError::Internal)?;
        }
        let kwargs = PyDict::new(py);
        kwargs
            .set_item("status", self.status)
            .map_err(crate::convert::InvokeError::Internal)?;
        kwargs
            .set_item("headers", headers)
            .map_err(crate::convert::InvokeError::Internal)?;
        if let Some(ct) = &self.content_type {
            kwargs
                .set_item("content_type", ct.as_ref())
                .map_err(crate::convert::InvokeError::Internal)?;
        }
        let built = response_cls
            .call(py, (PyBytes::new(py, &self.body),), Some(&kwargs))
            .map_err(crate::convert::InvokeError::Internal)?;
        let _ = self.obj.set(built);
        match self.obj.get() {
            Some(obj) => Ok(obj.bind(py).clone()),
            None => Err(crate::convert::InvokeError::Internal(
                pyo3::exceptions::PyRuntimeError::new_err("response view was not initialized"),
            )),
        }
    }
}
