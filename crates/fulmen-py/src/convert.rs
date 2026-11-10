//! Calling the Python handler and converting its return value into the wire
//! [`Response`] (DESIGN.md §10.2, M1 subset).
//!
//! Everything in this file that touches Python objects runs under
//! [`Python::attach`]; the wire `Response` is built from owned Rust values
//! afterwards, so the GIL is never held across response construction.

use bytes::Bytes;
use http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString, PyTuple};

use crate::json::{self, JsonValue};

/// `text/plain; charset=utf-8`, the §10.2 content type for `str` results.
const CT_TEXT: &str = "text/plain; charset=utf-8";
/// `application/json`, for `dict`/`list`/`tuple` results.
const CT_JSON: &str = "application/json";
/// `application/octet-stream`, for `bytes` results.
const CT_OCTETS: &str = "application/octet-stream";

/// One path-parameter argument for the handler call, owned by Rust before the
/// GIL is taken (the request path is read into Rust types on the I/O thread).
#[derive(Debug)]
pub(crate) struct ParamArg {
    /// The parameter name (matches the handler's parameter).
    pub(crate) name: String,
    /// `true` for keyword-only parameters, which are passed by name.
    pub(crate) keyword_only: bool,
    /// The typed, percent-decoded value.
    pub(crate) value: ParamValue,
}

/// A typed path-parameter value in a form cheap to materialize as Python.
#[derive(Debug)]
pub(crate) enum ParamValue {
    /// `int` parameter.
    Int(i64),
    /// `str` parameter.
    Str(String),
}

/// The handler's return value converted to owned Rust data.
#[derive(Debug)]
pub(crate) enum PyReturn {
    /// `str` → `200 text/plain; charset=utf-8`.
    Text(String),
    /// `bytes` → `200 application/octet-stream`.
    Octets(Vec<u8>),
    /// `dict`/`list`/`tuple` → `200 application/json`.
    Json(JsonValue),
    /// `fulmen.Response`: explicit status/headers plus a converted body
    /// (`None` body means an empty response body).
    Built {
        /// The response status, already validated.
        status: StatusCode,
        /// User-supplied headers, already validated and in insertion order.
        headers: Vec<(HeaderName, HeaderValue)>,
        /// The converted `body` member, when not `None`.
        body: Option<Box<PyReturn>>,
    },
    /// The handler returned `None` — a 500 in M1 (`NoContent` is M2.14).
    NoneReturned,
    /// The handler returned a type M1 cannot encode; carries a description.
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

/// Context the worker needs to log through Python's `logging` and to
/// recognise `fulmen.Response` (both cached at compile time).
pub(crate) struct PyCtx<'a> {
    /// `logging.getLogger("fulmen")`.
    pub(crate) logger: &'a Py<PyAny>,
    /// The `fulmen.responses.Response` class object.
    pub(crate) response_cls: &'a Py<PyAny>,
    /// `"{METHOD} {path}"` identifying the route in log lines.
    pub(crate) label: &'a str,
}

/// The outcome of [`call_and_convert`], produced inside [`Python::attach`].
#[derive(Debug)]
pub(crate) enum CallOutcome {
    /// The call produced a convertible return value.
    Return(PyReturn),
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
    match invoke(py, callable, args, ctx) {
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
            log_message(
                py,
                ctx.logger,
                format!("{}: handler returned None", handler_name(py, callable)),
            );
            CallOutcome::Failed
        }
        Ok(ret) => CallOutcome::Return(ret),
        Err(InvokeError::Handler(err)) => {
            log_exception(
                py,
                ctx.logger,
                format!(
                    "{}: unhandled exception in handler",
                    handler_name(py, callable)
                ),
                err,
            );
            CallOutcome::Failed
        }
        Err(InvokeError::Internal(err)) => {
            log_exception(
                py,
                ctx.logger,
                format!("{}: error preparing or converting result", ctx.label),
                err,
            );
            CallOutcome::Failed
        }
    }
}

/// Build the argument tuple/kwargs and call the handler.
fn invoke(
    py: Python<'_>,
    callable: &Py<PyAny>,
    args: &[ParamArg],
    ctx: &PyCtx<'_>,
) -> Result<PyReturn, InvokeError> {
    let mut positional = Vec::with_capacity(args.len());
    let kwargs = PyDict::new(py);
    for arg in args {
        let value: Bound<'_, PyAny> = match &arg.value {
            ParamValue::Int(i) => pyo3::types::PyInt::new(py, *i).into_any(),
            ParamValue::Str(s) => PyString::new(py, s).into_any(),
        };
        if arg.keyword_only {
            kwargs
                .set_item(&arg.name, value)
                .map_err(InvokeError::Internal)?;
        } else {
            positional.push(value);
        }
    }
    let tuple = PyTuple::new(py, positional).map_err(InvokeError::Internal)?;
    let result = callable
        .call(py, tuple, Some(&kwargs))
        .map_err(InvokeError::Handler)?;
    convert_result(py, result.bind(py), ctx).map_err(InvokeError::Internal)
}

/// Convert a Python return value into [`PyReturn`].
fn convert_result(
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
    if obj.is_instance_of::<PyDict>()
        || obj.is_instance_of::<PyList>()
        || obj.is_instance_of::<PyTuple>()
    {
        return match json::extract(obj) {
            Ok(v) => Ok(PyReturn::Json(v)),
            Err(json::ExtractError::Unsupported(what)) => Ok(PyReturn::Unsupported(what)),
            Err(json::ExtractError::Python(err)) => Err(err),
        };
    }
    Ok(PyReturn::Unsupported(type_name(obj)))
}

/// Convert a `fulmen.Response` instance from its `status`, `headers` and
/// `body` members.
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

    let body_obj = obj.getattr("body")?;
    let body = if body_obj.is_none() {
        None
    } else {
        match convert_result(py, &body_obj, ctx)? {
            ret @ (PyReturn::Text(_) | PyReturn::Octets(_) | PyReturn::Json(_)) => {
                Some(Box::new(ret))
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
        body,
    })
}

/// Build the wire [`Response`] from a converted return value. Runs without
/// the GIL: everything it reads is owned Rust data.
pub(crate) fn into_response(ret: PyReturn) -> fulmen_server::Response {
    match ret {
        PyReturn::Text(s) => typed(StatusCode::OK, CT_TEXT, Bytes::from(s)),
        PyReturn::Octets(b) => typed(StatusCode::OK, CT_OCTETS, Bytes::from(b)),
        PyReturn::Json(v) => typed(StatusCode::OK, CT_JSON, Bytes::from(json::write(&v))),
        PyReturn::Built {
            status,
            headers,
            body,
        } => {
            let mut response = fulmen_server::Response::status(status);
            for (name, value) in headers {
                response.headers.append(name, value);
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

/// A minimal `500` used wherever the dispatcher must answer an error.
pub(crate) fn internal_error() -> fulmen_server::Response {
    typed(
        StatusCode::INTERNAL_SERVER_ERROR,
        CT_TEXT,
        Bytes::from_static(b"Internal Server Error"),
    )
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
