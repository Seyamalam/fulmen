//! Calling the Python handler and converting its return value into the wire
//! [`Response`] (DESIGN.md §10.2, §13, §17).
//!
//! Everything in this file that touches Python objects runs under
//! [`Python::attach`]; the wire `Response` is built from owned Rust values
//! afterwards, so the GIL is never held across response construction.
//!
//! Error routing (§17): a handler raising `fulmen.errors.HTTPError` or
//! `fulmen.errors.ValidationError` answers with `application/problem+json`;
//! any other exception is looked up in the app's `on_error` table (closest
//! MRO class wins) and — when a handler is registered — the request is
//! *deferred*: the executor answers [`JobAnswer::Deferred`] and the
//! dispatcher re-submits the error handler onto the matching executor.

use std::sync::Arc;

use bytes::Bytes;
use fulmen_schema::errors::ValidationErrors;
use fulmen_schema::value::{LiteralValue, Value};
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple};

use crate::descriptor::{self, Descriptor};
use crate::json::{self, JsonValue};

/// `text/plain; charset=utf-8`, the §10.2 content type for `str` results.
const CT_TEXT: &str = "text/plain; charset=utf-8";
/// `application/json`, for `dict`/`list`/`tuple` results.
const CT_JSON: &str = "application/json";
/// `application/octet-stream`, for `bytes` results.
const CT_OCTETS: &str = "application/octet-stream";
/// `application/problem+json`, the RFC 9457 error body (§17).
const CT_PROBLEM: &str = "application/problem+json";

/// One call argument for the handler, owned by Rust before the GIL is taken
/// (the request path is read into Rust types on the I/O thread).
#[derive(Debug)]
pub(crate) struct ParamArg {
    /// The parameter name (matches the handler's parameter).
    pub(crate) name: String,
    /// How the argument is bound.
    pub(crate) kind: ArgKind,
    /// The argument's value (or how to produce it at call time).
    pub(crate) value: ParamValue,
}

/// How a call argument binds into the callable's signature.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ArgKind {
    /// `pos_only` — passed positionally (they are always path-sourced, so
    /// never `Omitted`, so the tuple never shifts).
    Positional,
    /// `normal`/`kw_only` — passed by name.
    Named,
}

/// A typed call-argument value in a form cheap to materialize as Python.
#[derive(Debug)]
pub(crate) enum ParamValue {
    /// A ready Python object — a literal default, or the exception an
    /// `on_error` handler receives.
    Obj(Arc<Py<PyAny>>),
    /// A zero-arg factory called at call time (a `default_factory`
    /// default, which must produce a fresh value per request).
    CallDefault(Arc<Py<PyAny>>),
    /// A decoded wire value for a parameter with no descriptor — the raw
    /// `Value` materializes at call time without validation.
    Wire(Value<'static>),
    /// `value` passed `desc`'s validator at dispatch (lax for text
    /// sources, strict for the body): materialization alone needs the
    /// GIL, so extraction-time `422`s never enter Python.
    Validated {
        /// The parameter's compiled descriptor.
        desc: Arc<Py<Descriptor>>,
        /// The already-validated value.
        value: Value<'static>,
    },
}

/// The handler's return value converted to owned Rust data.
#[derive(Debug)]
pub(crate) enum PyReturn {
    /// `str` → `200 text/plain; charset=utf-8`.
    Text(String),
    /// `bytes` → `200 application/octet-stream`.
    Octets(Vec<u8>),
    /// JSON-serializable (`dict`/`list`/`tuple`/`Model`/scalars) → `200
    /// application/json`.
    Json(JsonValue),
    /// `fulmen.Response`: explicit status/headers plus a converted body
    /// (`None` body means an empty response body) and optional
    /// `content_type` override.
    Built {
        /// The response status, already validated.
        status: StatusCode,
        /// User-supplied headers, already validated and in insertion order.
        headers: Vec<(HeaderName, HeaderValue)>,
        /// `Response.content_type` — wins over body-type inference.
        content_type: Option<HeaderValue>,
        /// The converted `body` member, when not `None`.
        body: Option<Box<PyReturn>>,
    },
    /// A typed error → RFC 9457 `application/problem+json` (§17).
    Problem {
        /// HTTP status.
        status: StatusCode,
        /// Extra headers the error carried (e.g. `WWW-Authenticate`).
        headers: Vec<(HeaderName, HeaderValue)>,
        /// The problem document.
        body: JsonValue,
    },
    /// The handler returned `None`.
    NoneReturned,
    /// The handler returned a type the bridge cannot encode; carries a
    /// description for the log line.
    Unsupported(String),
}

/// What failed inside [`invoke`], for the log message.
#[derive(Debug)]
pub(crate) enum InvokeError {
    /// The handler call itself raised a Python exception.
    Handler(PyErr),
    /// Building arguments or converting the result raised.
    Internal(PyErr),
}

/// One `@app.on_error` registration: an exception class and its handler.
#[derive(Debug)]
pub(crate) struct ErrorHandler {
    /// The registered exception class (matched by MRO position).
    pub(crate) class_: Arc<Py<PyAny>>,
    /// The handler callable, taking the exception instance.
    pub(crate) handler: Arc<Py<PyAny>>,
    /// `inspect.iscoroutinefunction(handler)` — picks the executor.
    pub(crate) is_async: bool,
}

/// The app's `on_error` table (§17). Shared by every executor.
#[derive(Debug, Default)]
pub(crate) struct ErrorTable {
    /// Registrations in declaration order; MRO distance decides conflicts.
    pub(crate) handlers: Vec<ErrorHandler>,
}

impl ErrorTable {
    /// The registered handler whose class sits closest to `err`'s type in
    /// the MRO (§17: "chosen by the closest class in the MRO").
    pub(crate) fn lookup<'a>(&'a self, py: Python<'a>, err: &PyErr) -> Option<&'a ErrorHandler> {
        if self.handlers.is_empty() {
            return None;
        }
        let mro = err
            .get_type(py)
            .getattr("__mro__")
            .ok()?
            .cast_into::<PyTuple>()
            .ok()?;
        let mut best: Option<(usize, &ErrorHandler)> = None;
        for h in &self.handlers {
            let class = h.class_.bind(py);
            if let Some(pos) = mro.iter().position(|c| c.is(class))
                && best.is_none_or(|(bp, _)| pos < bp)
            {
                best = Some((pos, h));
            }
        }
        best.map(|(_, h)| h)
    }
}

/// A call the dispatcher must still make — an `on_error` handler invoked
/// with the raised exception. Produced by an executor's `settle`; the
/// dispatcher re-submits it onto the matching executor (sync pool or
/// asyncio loop) with error routing disabled, so an error handler that
/// itself raises answers `500` rather than ping-ponging between handlers.
#[derive(Debug)]
pub(crate) struct DeferredCall {
    /// The error-handler callable.
    pub(crate) callable: Arc<Py<PyAny>>,
    /// The exception instance, passed as the sole positional argument.
    pub(crate) arg: Arc<Py<PyAny>>,
    /// `true` → the asyncio loop; `false` → the sync pool.
    pub(crate) is_async: bool,
    /// `"{METHOD} {path}"` for log lines.
    pub(crate) label: Box<str>,
}

/// What an executor sends back on a job's oneshot: either a finished wire
/// response, or a deferred error-handler call for the dispatcher to route.
#[derive(Debug)]
pub(crate) enum JobAnswer {
    /// The request is answered.
    Response(fulmen_server::Response),
    /// Route this `on_error` call through the matching executor.
    Deferred(DeferredCall),
}

/// Context the worker needs to log through Python's `logging`, recognise
/// `fulmen.Response`, and route typed errors (all cached at compile time).
pub(crate) struct PyCtx<'a> {
    /// `logging.getLogger("fulmen")`.
    pub(crate) logger: &'a Py<PyAny>,
    /// The `fulmen.responses.Response` class object.
    pub(crate) response_cls: &'a Py<PyAny>,
    /// The app's `on_error` table.
    pub(crate) errors: &'a ErrorTable,
    /// `-> None`/`-> NoContent` declared: `None` returns answer `204`.
    pub(crate) none_ok: bool,
    /// `false` on error-handler jobs: their failures never defer again.
    pub(crate) route_errors: bool,
    /// `"{METHOD} {path}"` identifying the route in log lines.
    pub(crate) label: &'a str,
}

/// The outcome of [`call_and_convert`], produced inside [`Python::attach`].
#[derive(Debug)]
pub(crate) enum CallOutcome {
    /// The call produced a convertible return value.
    Return(PyReturn),
    /// The call failed in a way the `on_error` table handles: the
    /// dispatcher must run `handler(exc)` on the matching executor.
    Deferred(DeferredCall),
    /// The call failed and the failure was already logged; the caller owes
    /// the client a `500`.
    Failed,
}

/// Call the handler and convert its result, logging every failure through
/// the `fulmen` logger while attached. Python errors never propagate; a Rust
/// panic still does, for [`std::panic::catch_unwind`] at the worker.
pub(crate) fn call_and_convert(
    py: Python<'_>,
    callable: &Py<PyAny>,
    args: &[ParamArg],
    ctx: &PyCtx<'_>,
) -> CallOutcome {
    settle(py, invoke(py, callable, args, ctx), callable, ctx)
}

/// Resolve an invoke result into a [`CallOutcome`]: converted returns pass
/// through, typed errors become problem responses, table-matched exceptions
/// defer to their `on_error` handler, everything else is logged and becomes
/// [`CallOutcome::Failed`]. Shared by the sync path (via
/// [`call_and_convert`]) and the async path, which feeds it the completed
/// task's result or exception. `callable` is the handler object, used only
/// for log lines.
pub(crate) fn settle(
    py: Python<'_>,
    result: Result<PyReturn, InvokeError>,
    callable: &Py<PyAny>,
    ctx: &PyCtx<'_>,
) -> CallOutcome {
    match result {
        Ok(PyReturn::Unsupported(ty)) => {
            log_message(
                py,
                ctx.logger,
                format!(
                    "{}: handler returned unsupported type `{ty}`",
                    handler_name(py, callable)
                ),
            );
            CallOutcome::Failed
        }
        Ok(PyReturn::NoneReturned) => {
            if ctx.none_ok {
                return CallOutcome::Return(PyReturn::Built {
                    status: StatusCode::NO_CONTENT,
                    headers: Vec::new(),
                    content_type: None,
                    body: None,
                });
            }
            log_message(
                py,
                ctx.logger,
                format!("{}: handler returned None", handler_name(py, callable)),
            );
            CallOutcome::Failed
        }
        Ok(ret) => CallOutcome::Return(ret),
        Err(InvokeError::Handler(err)) => {
            route_error(py, err, callable, ctx, "unhandled exception in handler")
        }
        Err(InvokeError::Internal(err)) => route_error(
            py,
            err,
            callable,
            ctx,
            "error preparing or converting result",
        ),
    }
}

/// Typed errors → problem responses; table-matched exceptions → deferred
/// `on_error` calls; everything else → log + `Failed`.
fn route_error(
    py: Python<'_>,
    err: PyErr,
    callable: &Py<PyAny>,
    ctx: &PyCtx<'_>,
    what: &str,
) -> CallOutcome {
    // A registered `@app.on_error` handler wins even for HTTPError /
    // ValidationError — that is how apps override the built-in problem
    // rendering for specific statuses (§17).
    if ctx.route_errors
        && let Some(h) = ctx.errors.lookup(py, &err)
    {
        return CallOutcome::Deferred(DeferredCall {
            callable: Arc::clone(&h.handler),
            arg: Arc::new(err.value(py).clone().unbind().into_any()),
            is_async: h.is_async,
            label: ctx.label.into(),
        });
    }
    if let Some(ret) = problem_from_err(py, &err) {
        return CallOutcome::Return(ret);
    }
    log_exception(
        py,
        ctx.logger,
        format!("{}: {what}", handler_name(py, callable)),
        err,
    );
    CallOutcome::Failed
}

/// `err` is a `fulmen.errors.HTTPError`/`ValidationError` → build its RFC
/// 9457 problem response via `fulmen.errors.problem_for`. Anything else (or
/// any Python failure along the way) returns `None` — never raises.
fn problem_from_err(py: Python<'_>, err: &PyErr) -> Option<PyReturn> {
    let http_cls = descriptor::http_error_class(py).ok()?;
    let val_cls = descriptor::validation_error_class(py).ok()?;
    if !err.matches(py, http_cls.bind(py)).is_ok_and(|m| m)
        && !err.matches(py, val_cls.bind(py)).is_ok_and(|m| m)
    {
        return None;
    }
    let fun = py
        .import("fulmen.errors")
        .ok()?
        .getattr("problem_for")
        .ok()?;
    let out = fun.call1((err.value(py),)).ok()?;
    let (status, body, headers): (u16, Bound<'_, PyAny>, Bound<'_, PyAny>) = out.extract().ok()?;
    let json = json::extract(&body).ok()?;
    let mut hdrs = Vec::new();
    if let Ok(d) = headers.cast::<PyDict>() {
        for (k, v) in d.iter() {
            let name: String = k.extract().ok()?;
            let value: String = v.extract().ok()?;
            let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
            let value = HeaderValue::from_str(&value).ok()?;
            hdrs.push((name, value));
        }
    }
    Some(PyReturn::Problem {
        status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        headers: hdrs,
        body: json,
    })
}

/// Materialize `args` into the call's positional tuple and kwargs dict.
/// Shared by the sync path (invoke → result conversion) and the async path
/// (build the coroutine object), so both build arguments identically.
///
/// `pos_only` parameters go in the tuple; `normal`/`kw_only` go by name —
/// an `Omitted` middle parameter can never shift a positional binding.
pub(crate) fn build_args<'py>(
    py: Python<'py>,
    args: &[ParamArg],
) -> Result<(Bound<'py, PyTuple>, Bound<'py, PyDict>), InvokeError> {
    let mut positional = Vec::new();
    let kwargs = PyDict::new(py);
    for arg in args {
        let value = resolve_value(py, &arg.value)?;
        match arg.kind {
            ArgKind::Positional => positional.push(value),
            ArgKind::Named => kwargs
                .set_item(&arg.name, value)
                .map_err(InvokeError::Internal)?,
        }
    }
    let tuple = PyTuple::new(py, positional).map_err(InvokeError::Internal)?;
    Ok((tuple, kwargs))
}

/// A `ParamValue` → the Python object passed to the callable.
/// `Validated` values only materialize (validation ran at dispatch);
/// `CallDefault` invokes the parameter's default factory.
fn resolve_value<'py>(
    py: Python<'py>,
    value: &ParamValue,
) -> Result<Bound<'py, PyAny>, InvokeError> {
    match value {
        ParamValue::Obj(o) => Ok(o.bind(py).clone()),
        ParamValue::CallDefault(factory) => factory.bind(py).call0().map_err(InvokeError::Internal),
        ParamValue::Wire(v) => descriptor::value_to_py(py, v)
            .map(|o| o.into_bound(py))
            .map_err(InvokeError::Internal),
        ParamValue::Validated { desc, value } => desc
            .bind(py)
            .borrow()
            .do_materialize(py, value)
            .map(|o| o.into_bound(py))
            .map_err(InvokeError::Internal),
    }
}

/// Build the argument tuple/kwargs and call the handler.
fn invoke(
    py: Python<'_>,
    callable: &Py<PyAny>,
    args: &[ParamArg],
    ctx: &PyCtx<'_>,
) -> Result<PyReturn, InvokeError> {
    let (tuple, kwargs) = build_args(py, args)?;
    let result = callable
        .call(py, tuple, Some(&kwargs))
        .map_err(InvokeError::Handler)?;
    convert_result(py, result.bind(py), ctx).map_err(InvokeError::Internal)
}

/// Convert a Python return value into [`PyReturn`].
pub(crate) fn convert_result(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    ctx: &PyCtx<'_>,
) -> Result<PyReturn, PyErr> {
    if obj.is_none() {
        return Ok(PyReturn::NoneReturned);
    }
    if obj.is_instance(ctx.response_cls.bind(py))? {
        return convert_response(py, obj, ctx);
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(PyReturn::Text(s.to_str()?.to_owned()));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(PyReturn::Octets(b.as_bytes().to_vec()));
    }
    match json::extract(obj) {
        Ok(v) => Ok(PyReturn::Json(v)),
        Err(json::ExtractError::Unsupported(what)) => Ok(PyReturn::Unsupported(what)),
        Err(json::ExtractError::Python(err)) => Err(err),
    }
}

/// Convert a `fulmen.Response` instance from its `status`, `headers`,
/// `content_type` and `body` members.
fn convert_response(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    ctx: &PyCtx<'_>,
) -> Result<PyReturn, PyErr> {
    let status: u16 = obj.getattr("status")?.extract()?;
    let status = StatusCode::from_u16(status).map_err(|_| {
        PyValueError::new_err(format!(
            "Response.status {status} is not a valid HTTP status"
        ))
    })?;

    let headers_obj = obj.getattr("headers")?;
    let headers_map = headers_obj
        .cast::<PyDict>()
        .map_err(|_| PyValueError::new_err("Response.headers must be a dict of str to str"))?;
    let mut headers = Vec::with_capacity(headers_map.len());
    for (key, value) in headers_map.iter() {
        let name: String = key.extract()?;
        let text: String = value.extract()?;
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            PyValueError::new_err(format!("invalid header name `{name}` in Response.headers"))
        })?;
        let value = HeaderValue::from_str(&text).map_err(|_| {
            PyValueError::new_err(format!(
                "invalid value for header `{name}` in Response.headers"
            ))
        })?;
        headers.push((name, value));
    }

    let ct_obj = obj.getattr("content_type")?;
    let content_type = if ct_obj.is_none() {
        None
    } else {
        let text: String = ct_obj.extract()?;
        Some(HeaderValue::from_str(&text).map_err(|_| {
            PyValueError::new_err(format!("invalid content_type {text:?} on Response"))
        })?)
    };

    let body_obj = obj.getattr("body")?;
    let body = if body_obj.is_none() {
        None
    } else {
        match convert_result(py, &body_obj, ctx)? {
            ret @ (PyReturn::Text(_) | PyReturn::Octets(_) | PyReturn::Json(_)) => {
                Some(Box::new(ret))
            }
            // `Stream` bodies are sync iterables of bytes/str — buffered
            // (native streaming is M5.x, DESIGN.md deviation note).
            PyReturn::Unsupported(_) if body_obj.hasattr("__iter__")? => {
                Some(Box::new(buffer_iter(py, &body_obj)?))
            }
            PyReturn::Built { .. } => {
                return Ok(PyReturn::Unsupported("Response".to_owned()));
            }
            other => return Ok(other),
        }
    };
    Ok(PyReturn::Built {
        status,
        headers,
        content_type,
        body,
    })
}

/// Join a sync `Iterable[bytes|str]` body into one octet buffer.
fn buffer_iter(_py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<PyReturn, PyErr> {
    let mut out = Vec::new();
    for item in obj.try_iter()? {
        let item = item?;
        if let Ok(b) = item.cast::<PyBytes>() {
            out.extend_from_slice(b.as_bytes());
        } else if let Ok(s) = item.cast::<PyString>() {
            out.extend_from_slice(s.to_str()?.as_bytes());
        } else {
            return Err(PyTypeError::new_err(format!(
                "Stream chunks must be bytes or str, got {}",
                type_name(&item)
            )));
        }
    }
    Ok(PyReturn::Octets(out))
}

/// Build the wire [`Response`] from a converted return value. Runs without
/// the GIL: everything it reads is owned Rust data.
pub(crate) fn into_response(ret: PyReturn) -> fulmen_server::Response {
    match ret {
        PyReturn::Text(s) => typed(StatusCode::OK, CT_TEXT, Bytes::from(s)),
        PyReturn::Octets(b) => typed(StatusCode::OK, CT_OCTETS, Bytes::from(b)),
        PyReturn::Json(v) => typed(StatusCode::OK, CT_JSON, Bytes::from(json::write(&v))),
        PyReturn::Problem {
            status,
            headers,
            body,
        } => {
            let mut response = fulmen_server::Response::status(status);
            for (name, value) in headers {
                response.headers.append(name, value);
            }
            response
                .headers
                .entry(CONTENT_TYPE)
                .or_insert_with(|| HeaderValue::from_static(CT_PROBLEM));
            response.body = Bytes::from(json::write(&body));
            response
        }
        PyReturn::Built {
            status,
            headers,
            content_type,
            body,
        } => {
            let mut response = fulmen_server::Response::status(status);
            for (name, value) in headers {
                response.headers.append(name, value);
            }
            if let Some(ct) = content_type {
                response.headers.entry(CONTENT_TYPE).or_insert(ct);
            }
            if let Some(body) = body {
                let (content_type, bytes) = render(*body);
                response
                    .headers
                    .entry(CONTENT_TYPE)
                    .or_insert_with(|| content_type);
                response.body = bytes;
            }
            response
        }
        // `None`/unsupported results are logged and mapped to 500 by the
        // caller; these arms only fire as a defensive fallback.
        PyReturn::NoneReturned | PyReturn::Unsupported(_) => internal_error(),
    }
}

/// `(content-type, body)` for the convertible `PyReturn` variants.
fn render(ret: PyReturn) -> (HeaderValue, Bytes) {
    match ret {
        PyReturn::Text(s) => (HeaderValue::from_static(CT_TEXT), Bytes::from(s)),
        PyReturn::Octets(b) => (HeaderValue::from_static(CT_OCTETS), Bytes::from(b)),
        PyReturn::Json(v) => (
            HeaderValue::from_static(CT_JSON),
            Bytes::from(json::write(&v)),
        ),
        // Unreachable: `Response.body` conversion only keeps the three
        // variants above (see `convert_response`).
        _ => (HeaderValue::from_static(CT_OCTETS), Bytes::new()),
    }
}

/// `200` with a content type and body.
fn typed(status: StatusCode, content_type: &'static str, body: Bytes) -> fulmen_server::Response {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    fulmen_server::Response {
        status,
        headers,
        body,
    }
}

/// The problem-document `type` for request-validation failures (§11.7).
pub(crate) const VALIDATION_PROBLEM_TYPE: &str = "https://fulmen.dev/problems/validation";

/// An RFC 9457 `application/problem+json` response for the bridge's own
/// error paths — the same shape `fulmen.errors.problem_for` produces for
/// raised `HTTPError`s (§17: native and Python errors share the format).
pub(crate) fn problem_response(
    status: StatusCode,
    type_uri: &str,
    title: &str,
    detail: Option<String>,
    errors: Option<&ValidationErrors>,
) -> fulmen_server::Response {
    let mut pairs = vec![
        ("type".to_owned(), JsonValue::Str(type_uri.to_owned())),
        ("title".to_owned(), JsonValue::Str(title.to_owned())),
        (
            "status".to_owned(),
            JsonValue::Int(i64::from(status.as_u16())),
        ),
    ];
    if let Some(detail) = detail {
        pairs.push(("detail".to_owned(), JsonValue::Str(detail)));
    }
    if let Some(errors) = errors {
        pairs.push(("errors".to_owned(), errors_json(errors)));
    }
    let mut response = fulmen_server::Response::status(status);
    response
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(CT_PROBLEM));
    response.body = Bytes::from(json::write(&JsonValue::Dict(pairs)));
    response
}

/// The §11.7 `errors` array as a [`JsonValue`] — the same encoding
/// [`ValidationErrors::to_json`] produces, built from the typed entries.
fn errors_json(errors: &ValidationErrors) -> JsonValue {
    JsonValue::List(
        errors
            .to_problem_entries()
            .iter()
            .map(|e| {
                let mut entry = vec![
                    ("loc".to_owned(), JsonValue::Str(e.loc_pointer.clone())),
                    ("code".to_owned(), JsonValue::Str(e.code.to_owned())),
                    ("message".to_owned(), JsonValue::Str(e.message.to_owned())),
                ];
                if let Some(limit) = e.limit {
                    entry.push(("limit".to_owned(), literal_json(limit)));
                }
                JsonValue::Dict(entry)
            })
            .collect(),
    )
}

/// A `LiteralValue` bound (`ge`/`le`/…) as a [`JsonValue`], matching
/// [`LiteralValue::to_json`]'s rendering.
fn literal_json(value: &LiteralValue) -> JsonValue {
    match value {
        LiteralValue::Bool(b) => JsonValue::Bool(*b),
        LiteralValue::Int(i) => JsonValue::Int(*i),
        LiteralValue::UInt(u) => JsonValue::UInt(*u),
        LiteralValue::BigInt(t) => JsonValue::BigInt(t.clone()),
        LiteralValue::Float(f) if f.is_finite() => JsonValue::Float(*f),
        // JSON has no NaN/Infinity literal — `to_json` renders `null`.
        LiteralValue::Null | LiteralValue::Float(_) => JsonValue::Null,
        LiteralValue::Str(s) => JsonValue::Str(s.to_string()),
        LiteralValue::Bytes(b) => JsonValue::Str(b.escape_ascii().to_string()),
    }
}

/// A `422` for the extracted-parameter error set (§11.7).
pub(crate) fn validation_problem(errors: &ValidationErrors) -> fulmen_server::Response {
    problem_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        VALIDATION_PROBLEM_TYPE,
        "Request validation failed",
        None,
        Some(errors),
    )
}

/// A minimal `500` used wherever the dispatcher must answer an error.
pub(crate) fn internal_error() -> fulmen_server::Response {
    problem_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "about:blank",
        "Internal Server Error",
        None,
        None,
    )
}

/// `503` + `Retry-After` for shed load (DESIGN.md §14.4): queue full, or a
/// job that waited past its queue deadline.
pub(crate) fn service_unavailable() -> fulmen_server::Response {
    let mut response = problem_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "about:blank",
        "Service Unavailable",
        None,
        None,
    );
    response
        .headers
        .insert(http::header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// A displayable type name for error messages.
fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .qualname()
        .map_or_else(|_| "<unknown>".to_owned(), |q| q.to_string())
}

/// `handler.__qualname__`, for log lines.
fn handler_name(py: Python<'_>, callable: &Py<PyAny>) -> String {
    callable
        .getattr(py, "__qualname__")
        .and_then(|q| q.extract::<String>(py))
        .unwrap_or_else(|_| "<handler>".to_owned())
}

/// `logger.error(msg)` — best effort; a logging failure goes to
/// `sys.unraisablehook` instead of being silently dropped.
pub(crate) fn log_message(py: Python<'_>, logger: &Py<PyAny>, message: String) {
    if let Err(err) = logger.call_method(py, "error", (message,), None) {
        err.write_unraisable(py, None);
    }
}

/// `logger.error(msg, exc_info=(type, value, tb))` so the traceback lands in
/// the `fulmen` log, never on the wire (DESIGN.md §14.6).
pub(crate) fn log_exception(py: Python<'_>, logger: &Py<PyAny>, message: String, err: PyErr) {
    let kwargs = PyDict::new(py);
    let exc_info = (
        err.get_type(py).into_any(),
        err.value(py).clone().into_any(),
        err.traceback(py)
            .map_or_else(|| py.None().into_bound(py), Bound::into_any),
    );
    if kwargs.set_item("exc_info", exc_info).is_err()
        || logger
            .call_method(py, "error", (message,), Some(&kwargs))
            .is_err()
    {
        err.write_unraisable(py, None);
    }
}
