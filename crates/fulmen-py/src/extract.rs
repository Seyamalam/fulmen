//! Request-parameter extraction (DESIGN.md §5.2, §11.4, §11.7).
//!
//! Runs on the dispatch I/O thread with no GIL held: every parameter's
//! compiled `Validator` is shared from its `Descriptor`, so wire decoding
//! and validation happen entirely in Rust. An extraction `4xx` therefore
//! never enters Python — only materialization does, at call time inside
//! the executor (`ParamValue::Validated`).
//!
//! Failure shape (§11.7, §17): malformed input is a `400` problem,
//! validation failures collect into one `422` problem carrying every
//! entry, and an unsupported body content type is a `415` problem.

use std::borrow::Cow;
use std::sync::Arc;

use fulmen_core::{PathValue, RouteHit};
use fulmen_schema::errors::{ErrorCode, ErrorEntry, LocSeg, ValidationErrors};
use fulmen_schema::value::Value;
use fulmen_schema::{
    Checked, Mode, PayloadError, Source, decode_and_validate, decode_cookie, decode_form,
    decode_query, validate_text,
};
use fulmen_server::{Request, Response};
use http::StatusCode;
use http::header::{CONTENT_TYPE, COOKIE};

use crate::convert::{self, ArgKind, ParamArg, ParamValue};
use crate::dispatch::{CallSource, CallSpecArg, MissingArg};

/// The outcome of [`extract_args`]: either the call's arguments (an
/// extraction `4xx` already came back as `Reply`).
pub(crate) enum Extracted {
    /// Arguments for the handler call, in signature order.
    Call(Box<[ParamArg]>),
    /// Answer the client directly — `400`/`415`/`422` problem responses.
    Reply(Response),
}

/// Which wire form the request body takes for `body`/`form` parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyRoute {
    /// `application/json` (or `application/*+json`) — strict JSON decode.
    Json,
    /// `application/x-www-form-urlencoded` — the form lax adapter.
    Form,
    /// No usable body — every body/form parameter falls back to its
    /// missing semantics (default or `422` "required").
    Absent,
    /// A content type this route cannot consume — `415`.
    Unsupported,
}

/// The request's declared media type, normalized (lowercased, parameters
/// stripped); `None` when the header is missing or unreadable.
fn media_type(request: &Request) -> Option<String> {
    request
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
}

/// Decide the body interpretation once per request (§5.2 content-type
/// routing). Only `body` params that decode (not `bytes`/`Raw`) and
/// `form` params gate on the content type — a `bytes`/`Raw` param takes
/// the octets regardless.
fn body_route(spec: &[CallSpecArg], request: &Request) -> BodyRoute {
    let needs_decode = spec
        .iter()
        .any(|a| a.source == CallSource::Body && !a.takes_raw());
    let needs_form = spec.iter().any(|a| a.source == CallSource::Form);
    if !(needs_decode || needs_form) {
        return BodyRoute::Absent;
    }
    match (media_type(request).as_deref(), request.body.is_empty()) {
        (None, true) => BodyRoute::Absent,
        (Some(ct), _) if ct == "application/json" || ct.ends_with("+json") => BodyRoute::Json,
        (Some("application/x-www-form-urlencoded"), _) => BodyRoute::Form,
        _ => BodyRoute::Unsupported,
    }
}

/// Per-request extraction state: the request, the lazily-decoded wire
/// objects, and the collected `422` error set.
struct Extraction<'r> {
    request: &'r Request,
    route: BodyRoute,
    errors: ValidationErrors,
    query: Option<Value<'r>>,
    cookie: Option<Value<'r>>,
    form: Option<Value<'r>>,
}

/// Build the call arguments for a route hit: decode each wire source
/// once, validate every parameter, and collect every problem into one
/// `422`.
///
/// `hit.params` already typed the path params (`int` segments only match
/// when they parse — invalid input never reaches extraction, DESIGN.md
/// §9.2/D20); everything else is pulled from `request` here.
pub(crate) fn extract_args(
    hit: &RouteHit<'_>,
    spec: &[CallSpecArg],
    request: &Request,
) -> Extracted {
    let route = body_route(spec, request);
    if route == BodyRoute::Unsupported {
        return Extracted::Reply(convert::problem_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "about:blank",
            "Unsupported Media Type",
            media_type(request).map(|t| format!("content type `{t}` is not supported")),
            None,
        ));
    }

    let mut x = Extraction {
        request,
        route,
        errors: ValidationErrors::new(),
        query: None,
        cookie: None,
        form: None,
    };
    let mut args: Vec<ParamArg> = Vec::with_capacity(spec.len());
    for arg in spec {
        let value = match x.arg_value(hit, arg) {
            Ok(v) => v,
            Err(response) => return Extracted::Reply(*response),
        };
        if let Some(value) = value {
            args.push(ParamArg {
                name: arg.name.to_string(),
                kind: if arg.positional {
                    ArgKind::Positional
                } else {
                    ArgKind::Named
                },
                value,
            });
        }
    }

    if x.errors.is_empty() {
        Extracted::Call(args.into_boxed_slice())
    } else {
        Extracted::Reply(convert::validation_problem(&x.errors))
    }
}

impl Extraction<'_> {
    /// The value one parameter's source supplies — `Ok(None)` when the
    /// source supplied nothing and the `missing` spec produced no arg
    /// (`required` already pushed its `422` entry, `omit` passes nothing).
    /// `Err` is a `400`/`415` answer that aborts the whole extraction.
    fn arg_value(
        &mut self,
        hit: &RouteHit<'_>,
        arg: &CallSpecArg,
    ) -> Result<Option<ParamValue>, Box<Response>> {
        match arg.source {
            CallSource::Path => self.path_value(hit, arg),
            CallSource::Query => self.query_value(arg),
            CallSource::Header => Ok(self.header_value(arg)),
            CallSource::Cookie => self.cookie_value(arg),
            CallSource::Body => self.body_value(arg),
            CallSource::Form => self.form_value(arg),
        }
    }

    /// A path parameter: the router already typed it (`int`/`str`).
    fn path_value(
        &mut self,
        hit: &RouteHit<'_>,
        arg: &CallSpecArg,
    ) -> Result<Option<ParamValue>, Box<Response>> {
        let Some(pv) = hit.param(&arg.name) else {
            // check_spec verified every path param binds — unreachable
            // unless the router and the spec disagree.
            return Err(Box::new(convert::internal_error()));
        };
        let raw = match pv {
            PathValue::Int(i) => Value::Int(*i),
            PathValue::Str(s) => Value::str(s),
        };
        Ok(validate_param(arg, &raw, Mode::Lax, &mut self.errors))
    }

    /// `?name=…` through the query source adapter (lax).
    fn query_value(&mut self, arg: &CallSpecArg) -> Result<Option<ParamValue>, Box<Response>> {
        if self.query.is_none() {
            match decode_query(self.request.uri.query().unwrap_or("")) {
                Ok(v) => self.query = Some(v),
                Err(e) => return Err(malformed(&format!("malformed query string: {e}"))),
            }
        }
        let raw = self.query.as_ref().and_then(|o| o.get(&arg.wire_name));
        Ok(validate_or_missing(arg, raw, Mode::Lax, &mut self.errors))
    }

    /// A request header (case-insensitive `wire_name`), validated as text
    /// in lax mode.
    fn header_value(&mut self, arg: &CallSpecArg) -> Option<ParamValue> {
        let Some(value) = self.request.headers.get(&*arg.wire_name) else {
            return missing_value(arg, &mut self.errors);
        };
        let Ok(text) = value.to_str() else {
            self.errors.fail(
                Source::Header,
                vec![LocSeg::Key(arg.wire_name.clone())],
                ErrorCode::Str,
                "header value is not ASCII",
                None,
            );
            return None;
        };
        match &arg.schema {
            Some(schema) => match validate_text(text, &schema.validator, Source::Header) {
                Ok(out) => Some(ParamValue::Validated {
                    desc: Arc::clone(&schema.desc),
                    value: own_value(&out),
                }),
                Err(errs) => {
                    for entry in errs.entries() {
                        self.errors.push(relocated(arg, entry));
                    }
                    None
                }
            },
            None => Some(ParamValue::Wire(Value::string(text.to_owned()))),
        }
    }

    /// A `Cookie:` member through the cookie source adapter (lax).
    fn cookie_value(&mut self, arg: &CallSpecArg) -> Result<Option<ParamValue>, Box<Response>> {
        if self.cookie.is_none() {
            let header = self
                .request
                .headers
                .get(COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            match decode_cookie(header) {
                Ok(v) => self.cookie = Some(v),
                Err(e) => {
                    return Err(malformed(&format!("malformed cookie header: {e}")));
                }
            }
        }
        let raw = self.cookie.as_ref().and_then(|o| o.get(&arg.wire_name));
        Ok(validate_or_missing(arg, raw, Mode::Lax, &mut self.errors))
    }

    /// The request body — `bytes`/`Raw` takes the octets, everything else
    /// decodes JSON under strict mode (§5.2, §11.4).
    fn body_value(&mut self, arg: &CallSpecArg) -> Result<Option<ParamValue>, Box<Response>> {
        if arg.takes_raw() {
            let raw = Value::Bytes(Cow::Borrowed(&self.request.body[..]));
            return Ok(validate_param(arg, &raw, Mode::Strict, &mut self.errors));
        }
        if self.route != BodyRoute::Json || self.request.body.is_empty() {
            // Not a JSON body this route can use — the parameter was
            // simply not provided (its `missing` spec decides).
            return Ok(missing_value(arg, &mut self.errors));
        }
        match &arg.schema {
            Some(schema) => match decode_and_validate(&self.request.body, &schema.validator) {
                Ok(out) => Ok(Some(ParamValue::Validated {
                    desc: Arc::clone(&schema.desc),
                    value: own_value(&out),
                })),
                Err(PayloadError::Malformed(e)) => {
                    Err(malformed(&format!("malformed JSON body: {e}")))
                }
                Err(PayloadError::Limit(errs) | PayloadError::Validation(errs)) => {
                    for entry in errs.entries() {
                        self.errors.push(entry.clone());
                    }
                    Ok(None)
                }
            },
            None => match fulmen_schema::decode_json(&self.request.body) {
                Ok(v) => Ok(Some(ParamValue::Wire(own_value(&v)))),
                Err(e) => Err(malformed(&format!("malformed JSON body: {e}"))),
            },
        }
    }

    /// An `application/x-www-form-urlencoded` field (lax).
    fn form_value(&mut self, arg: &CallSpecArg) -> Result<Option<ParamValue>, Box<Response>> {
        if self.route != BodyRoute::Form {
            return Ok(missing_value(arg, &mut self.errors));
        }
        if self.form.is_none() {
            match std::str::from_utf8(&self.request.body) {
                Ok(text) => match decode_form(text) {
                    Ok(v) => self.form = Some(v),
                    Err(e) => {
                        return Err(malformed(&format!("malformed form body: {e}")));
                    }
                },
                Err(_) => return Err(malformed("form body is not valid UTF-8")),
            }
        }
        let raw = self.form.as_ref().and_then(|o| o.get(&arg.wire_name));
        Ok(validate_or_missing(arg, raw, Mode::Lax, &mut self.errors))
    }
}

/// A `400` for input the decoder could not even parse (§11.7).
fn malformed(detail: &str) -> Box<Response> {
    Box::new(convert::problem_response(
        StatusCode::BAD_REQUEST,
        "about:blank",
        "Bad Request",
        Some(detail.to_owned()),
        None,
    ))
}

/// `raw` is `Some` when the source supplied a value — validate it through
/// the parameter's descriptor (or pass it through descriptor-free);
/// `None` applies the missing semantics.
fn validate_or_missing(
    arg: &CallSpecArg,
    raw: Option<&Value<'_>>,
    mode: Mode,
    errors: &mut ValidationErrors,
) -> Option<ParamValue> {
    match raw {
        Some(value) => validate_param(arg, value, mode, errors),
        None => missing_value(arg, errors),
    }
}

/// Run `raw` through the parameter's compiled validator; every problem is
/// collected into `errors` (relocated under the parameter's wire name).
fn validate_param(
    arg: &CallSpecArg,
    raw: &Value<'_>,
    mode: Mode,
    errors: &mut ValidationErrors,
) -> Option<ParamValue> {
    match &arg.schema {
        Some(schema) => match schema
            .validator
            .validate_out(raw, arg.source.schema(), mode)
        {
            Ok(Checked::Same(_)) => Some(ParamValue::Validated {
                desc: Arc::clone(&schema.desc),
                value: own_value(raw),
            }),
            Ok(Checked::New(v)) => Some(ParamValue::Validated {
                desc: Arc::clone(&schema.desc),
                value: own_value(&v),
            }),
            Err(errs) => {
                for entry in errs.entries() {
                    errors.push(relocated(arg, entry));
                }
                None
            }
        },
        None => Some(ParamValue::Wire(own_value(raw))),
    }
}

/// The value the callable gets when its source supplied nothing:
/// `422` "required" when the parameter is required, else the declared
/// default — omitted (signature default), a literal, or a factory call.
fn missing_value(arg: &CallSpecArg, errors: &mut ValidationErrors) -> Option<ParamValue> {
    match &arg.missing {
        MissingArg::Required => {
            errors.fail(
                arg.source.schema(),
                vec![LocSeg::Key(arg.wire_name.clone())],
                ErrorCode::Missing,
                "required parameter missing",
                None,
            );
            None
        }
        MissingArg::Omit => None,
        MissingArg::Value(v) => Some(ParamValue::Obj(Arc::clone(v))),
        MissingArg::Factory(f) => Some(ParamValue::CallDefault(Arc::clone(f))),
    }
}

/// Re-root a validator entry under the parameter's wire name so the
/// client sees `/query/page`, `/header/x-api-key`, … — body entries keep
/// their field-level `loc` (`/body/email`).
fn relocated(arg: &CallSpecArg, entry: &ErrorEntry) -> ErrorEntry {
    let loc = if arg.source == CallSource::Body {
        entry.loc.clone()
    } else {
        std::iter::once(LocSeg::Key(arg.wire_name.clone()))
            .chain(entry.loc.iter().cloned())
            .collect()
    };
    ErrorEntry {
        source: entry.source,
        loc,
        code: entry.code,
        message: entry.message.clone(),
        limit: entry.limit.clone(),
    }
}

/// Deep-copy `value` into an owned `'static` tree — decoded values borrow
/// the request buffers, which cannot outlive this call.
fn own_value(value: &Value<'_>) -> Value<'static> {
    match value {
        Value::Null => Value::Null,
        Value::Bool(b) => Value::Bool(*b),
        Value::Int(i) => Value::Int(*i),
        Value::UInt(u) => Value::UInt(*u),
        Value::BigInt(t) => Value::BigInt(t.to_string().into()),
        Value::Float(f) => Value::Float(*f),
        Value::Str(s) => Value::string(s.to_string()),
        Value::Bytes(b) => Value::Bytes(Cow::Owned(b.to_vec())),
        Value::Array(items) => Value::Array(items.iter().map(own_value).collect()),
        Value::Object(pairs) => Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (Cow::Owned(k.to_string()), own_value(v)))
                .collect(),
        ),
    }
}
