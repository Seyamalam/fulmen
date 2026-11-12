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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use fulmen_schema::desc::Kind;
use fulmen_schema::encode_json;
use fulmen_schema::errors::{Source, ValidationErrors};
use fulmen_schema::value::{LiteralValue, Value};
use fulmen_schema::{Mode, Validator};
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple};

use crate::descriptor::{self, Descriptor};
use crate::dispatch::{ResponseContract, ResponseVariant};
use crate::encode::{self, EncCtx, EncodeError, ShapeError};
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
    /// A body already encoded to JSON bytes by the descriptor-driven
    /// walker (§11.6) — no `JsonValue` round-trip.
    Encoded(Vec<u8>),
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
    /// The route's declared response contract — picks the encode
    /// descriptor by actual response status.
    pub(crate) contract: &'a ResponseContract,
    /// `Fulmen(debug=True)`/`serve(debug=…)` — §14.2 output validation.
    pub(crate) debug: bool,
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
///
/// A declared response contract runs first: when `response_descs`
/// resolved a descriptor for the body, the descriptor-driven walker
/// encodes it (§11.6) and §10.2's runtime-type channels are enforced —
/// `-> str` returning an `int` is a contract violation, not a
/// relabelled body. Unannotated/`Any`/`Raw` routes, and statuses the
/// contract does not cover, keep the generic path.
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
    if let Some((status, inner)) = contract_body(py, obj, StatusCode::OK.as_u16(), ctx)? {
        return Ok(match inner {
            problem @ PyReturn::Problem { .. } => problem,
            other => PyReturn::Built {
                status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                headers: Vec::new(),
                content_type: None,
                body: Some(Box::new(other)),
            },
        });
    }
    generic_body(py, obj)
}

/// The §10.2 generic conversion — a `str` is `text/plain`, `bytes` are
/// `octet-stream`, everything else rides `json::extract` (the
/// `Any`/`Raw`/unannotated behaviour).
fn generic_body(_py: Python<'_>, obj: &Bound<'_, PyAny>) -> Result<PyReturn, PyErr> {
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

/// Encode `obj` under the contract's descriptor(s) for `status`.
///
/// Returns `Some((declared_status, return))` when a descriptor handled
/// the value, `None` when the generic path applies — either because the
/// contract declares no descriptor for `status`, or every candidate is
/// `Any`/`Raw`. Same-status unions (`-> A | B`) try each declared
/// variant in order; the first whose shape fits encodes.
///
/// §10.2's channels survive the contract: `str`-typed variants answer
/// `Text`, `bytes` answer `Octets` — the descriptor still checks the
/// shape first, so a declared `-> str` route returning `int` is a
/// contract violation (500), not a quietly text/plain'd number.
fn contract_body(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    status: u16,
    ctx: &PyCtx<'_>,
) -> Result<Option<(u16, PyReturn)>, PyErr> {
    let candidates = contract_candidates(ctx.contract, status);
    if candidates.is_empty() {
        // The route declared statuses (typed variants) but not this
        // one — generic encoding is the §13 escape; a debug warning is
        // all the contract contributes.
        if ctx.debug && ctx.contract.has_status_contract() {
            log_message(
                py,
                ctx.logger,
                format!(
                    "{}: response body at status {status} has no declared variant — encoding generically",
                    ctx.label
                ),
            );
        }
        return Ok(None);
    }
    let mut generic = false;
    let mut first_shape: Option<ShapeError> = None;
    for variant in candidates {
        let Some(desc_py) = variant.desc.as_ref() else {
            continue;
        };
        let desc = desc_py.bind(py).borrow();
        match try_variant(py, obj, desc.type_desc(), desc.callbacks(), ctx)? {
            TryOut::Generic => generic = true,
            TryOut::Shape(err) => {
                if first_shape.is_none() {
                    first_shape = Some(err);
                }
            }
            TryOut::Done(chan, value) => {
                if ctx.debug {
                    if let Err(errors) = output_check(variant, &value) {
                        log_message(
                            py,
                            ctx.logger,
                            format!(
                                "{}: response failed output validation ({} error{})",
                                ctx.label,
                                errors.len(),
                                if errors.len() == 1 { "" } else { "s" },
                            ),
                        );
                        return Ok(Some((
                            StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            PyReturn::Problem {
                                status: StatusCode::INTERNAL_SERVER_ERROR,
                                headers: Vec::new(),
                                body: problem_json(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    RESPONSE_PROBLEM_TYPE,
                                    "Response validation failed",
                                    Some(format!(
                                        "handler `{}` returned a body violating its declared response type",
                                        ctx.label
                                    )),
                                    Some(&errors),
                                ),
                            },
                        )));
                    }
                }
                let ret = match chan {
                    Chan::Text => PyReturn::Text(value_string(&value)),
                    Chan::Octets => PyReturn::Octets(value_bytes(&value)),
                    Chan::Json => PyReturn::Encoded(encode_json(&value)),
                };
                return Ok(Some((variant.status.unwrap_or(200), ret)));
            }
        }
    }
    if generic {
        return Ok(None);
    }
    contract_violation(py, first_shape, ctx)
}

/// The descriptor-bearing variants eligible for a body at `status`:
/// every exact-status variant first (so `-> A | B` picks by shape),
/// else the single declared descriptor (a bare `-> User` body travels
/// as a `User` whatever status the `Response` object chose), else the
/// route's single 200-variant.
fn contract_candidates(
    contract: &ResponseContract,
    status: u16,
) -> Vec<&ResponseVariant> {
    let exact: Vec<_> = contract
        .variants
        .iter()
        .filter(|v| v.status == Some(status) && v.desc.is_some())
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let declared: Vec<_> = contract
        .variants
        .iter()
        .filter(|v| v.desc.is_some())
        .collect();
    if declared.len() == 1 {
        return declared;
    }
    contract
        .variants
        .iter()
        .filter(|v| v.status == Some(StatusCode::OK.as_u16()) && v.desc.is_some())
        .take(1)
        .collect()
}

/// How one variant handled the body.
enum TryOut {
    /// The value encoded under the variant; carries its §10.2 channel
    /// and the wire-shaped `Value` (for the debug validation pass).
    Done(Chan, Value<'static>),
    /// The shape does not fit this variant — try the next one.
    Shape(ShapeError),
    /// The variant is `Any`/`Raw` — defer to the generic path.
    Generic,
}

/// §10.2's body channels: `str` is text, `bytes` is octets, the rest is
/// `application/json`.
enum Chan {
    /// `text/plain; charset=utf-8` — a `Value::Str` rendered raw.
    Text,
    /// `application/octet-stream` — a `Value::Bytes` rendered raw.
    Octets,
    /// `application/json` — the `Value` run through `encode_json`.
    Json,
}

/// Try `obj` against one variant's descriptor: §10.2 channels for the
/// declared scalar kinds, the §11.6 walker for everything else.
fn try_variant(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    ty: &fulmen_schema::TypeDesc,
    callbacks: &[Py<PyAny>],
    _ctx: &PyCtx<'_>,
) -> Result<TryOut, PyErr> {
    match &ty.kind {
        Kind::Any | Kind::Raw => Ok(TryOut::Generic),
        Kind::Str(_) | Kind::Email | Kind::Url => match obj.cast::<PyString>() {
            Ok(s) => Ok(TryOut::Done(
                Chan::Text,
                Value::string(s.to_str()?.to_owned()),
            )),
            Err(_) => Ok(TryOut::Shape(ShapeError::expected("str", obj))),
        },
        Kind::Bytes(_) => {
            if let Ok(b) = obj.cast::<PyBytes>() {
                return Ok(TryOut::Done(
                    Chan::Octets,
                    Value::Bytes(Cow::Owned(b.as_bytes().to_vec())),
                ));
            }
            if let Ok(b) = obj.cast::<pyo3::types::PyByteArray>() {
                return Ok(TryOut::Done(
                    Chan::Octets,
                    Value::Bytes(Cow::Owned(b.to_vec())),
                ));
            }
            Ok(TryOut::Shape(ShapeError::expected("bytes", obj)))
        }
        _ => {
            let enc_ctx = EncCtx { callbacks };
            let scope = HashMap::new();
            match encode::encode(py, obj, ty, &enc_ctx, &scope, 0) {
                Ok(value) => Ok(TryOut::Done(Chan::Json, value)),
                Err(EncodeError::Shape(err)) => Ok(TryOut::Shape(err)),
                Err(EncodeError::Python(err)) => Err(err),
            }
        }
    }
}

/// The debug output check (§14.2): the wire-shaped `Value` validated
/// against the variant's compiled validator in strict mode. Errors mean
/// the handler's return violates its declared contract.
fn output_check(
    variant: &ResponseVariant,
    value: &Value<'static>,
) -> Result<(), ValidationErrors> {
    let Some(validator) = variant.validator.as_ref() else {
        return Ok(());
    };
    match validate_response(validator, value) {
        Ok(()) => Ok(()),
        Err(errors) => Err(errors),
    }
}

/// `validator.validate_out(value, Response, Strict)` — the §14.2 output
/// pass, GIL-free.
fn validate_response(
    validator: &Arc<Validator>,
    value: &Value<'static>,
) -> Result<(), ValidationErrors> {
    validator
        .validate_out(value, Source::Response, Mode::Strict)
        .map(|_| ())
}

/// Every candidate declared a concrete shape and `obj` fit none — a
/// contract violation. Debug mode answers the §11.7-shaped problem
/// document; non-debug raises so the ordinary
/// `error converting result` path logs and answers `500`.
fn contract_violation(
    py: Python<'_>,
    first: Option<ShapeError>,
    ctx: &PyCtx<'_>,
) -> Result<Option<(u16, PyReturn)>, PyErr> {
    let shape = first.unwrap_or_else(|| ShapeError {
        loc: Vec::new(),
        code: fulmen_schema::ErrorCode::Type,
        message: "response body matches no declared variant".to_owned(),
    });
    if !ctx.debug {
        return Err(PyTypeError::new_err(format!(
            "{}: {}",
            shape.pointer(),
            shape.message
        )));
    }
    let mut errors = ValidationErrors::new();
    errors.fail(
        Source::Response,
        shape.loc.clone(),
        shape.code,
        shape.message.clone(),
        None,
    );
    log_message(
        py,
        ctx.logger,
        format!(
            "{}: response failed the declared contract: {}",
            ctx.label,
            shape.message,
        ),
    );
    Ok(Some((
        StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
        PyReturn::Problem {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: Vec::new(),
            body: problem_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                RESPONSE_PROBLEM_TYPE,
                "Response validation failed",
                Some(format!(
                    "handler `{}` returned a body violating its declared response type",
                    ctx.label
                )),
                Some(&errors),
            ),
        },
    )))
}

/// The `Value::Str` a `Chan::Text` body always carries.
fn value_string(value: &Value<'static>) -> String {
    match value {
        Value::Str(s) => s.as_ref().to_owned(),
        _ => String::new(),
    }
}

/// The `Value::Bytes` a `Chan::Octets` body always carries.
fn value_bytes(value: &Value<'static>) -> Vec<u8> {
    match value {
        Value::Bytes(b) => b.as_ref().to_vec(),
        _ => Vec::new(),
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
    } else if obj.is_instance(descriptor::stream_class(py)?.bind(py))? {
        // `Stream` bodies are sync iterables of bytes/str — buffered
        // (native streaming is M5.x, DESIGN.md deviation note).
        Some(Box::new(buffer_iter(py, &body_obj)?))
    } else if body_obj.is_instance(ctx.response_cls.bind(py))? {
        // A `Response` inside a body is a misuse — answer unsupported.
        return Ok(PyReturn::Unsupported("Response".to_owned()));
    } else {
        // The body's descriptor is selected by the *actual* status —
        // `Response(status=404, body=e)` under `-> User | NotFound[Err]`
        // encodes `e` as `Err`.
        let converted = match contract_body(py, &body_obj, status.as_u16(), ctx)? {
            Some((_declared, inner)) => inner,
            None => generic_body(py, &body_obj)?,
        };
        match converted {
            ret @ (PyReturn::Text(_)
            | PyReturn::Octets(_)
            | PyReturn::Json(_)
            | PyReturn::Encoded(_)) => Some(Box::new(ret)),
            // A plain `Response` carrying a non-JSON-able iterable buffers
            // like `Stream` does.
            PyReturn::Unsupported(_) if body_obj.hasattr("__iter__")? => {
                Some(Box::new(buffer_iter(py, &body_obj)?))
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
        PyReturn::Encoded(b) => typed(StatusCode::OK, CT_JSON, Bytes::from(b)),
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
        PyReturn::Encoded(b) => (HeaderValue::from_static(CT_JSON), Bytes::from(b)),
        // Unreachable: `Response.body` conversion only keeps the
        // text/octets/json variants above (see `convert_response`).
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

/// The problem-document `type` for response-contract failures — the
/// debug-mode `500` a handler violating its declared response type gets.
const RESPONSE_PROBLEM_TYPE: &str = "https://fulmen.dev/problems/response";

/// The problem document as a [`JsonValue`] — shared by the wire
/// [`problem_response`] builder and the debug output-violation path,
/// which wraps it in [`PyReturn::Problem`].
fn problem_json(
    status: StatusCode,
    type_uri: &str,
    title: &str,
    detail: Option<String>,
    errors: Option<&ValidationErrors>,
) -> JsonValue {
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
    JsonValue::Dict(pairs)
}

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
    let mut response = fulmen_server::Response::status(status);
    response
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static(CT_PROBLEM));
    response.body = Bytes::from(json::write(&problem_json(
        status, type_uri, title, detail, errors,
    )));
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
