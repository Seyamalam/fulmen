//! The output half of DESIGN.md §11.6: walk a handler's return value
//! under the compiled [`TypeDesc`] and produce the wire-shaped
//! [`Value`] — record keys already aliased/renamed, scalars already in
//! their canonical text/number form. `fulmen_schema::encode_json` then
//! writes the bytes in one pass; nothing re-inspects the object.
//!
//! A return value that does not fit the declared shape is a contract
//! violation — a [`ShapeError`] — which the caller logs and answers
//! `500` (a `problem+json` carrying the error entry under debug mode,
//! §14.2). A Python access that *raises* (`getattr`, `isoformat`, a
//! `default_factory`) is a [`EncodeError::Python`] and rides the
//! ordinary internal-error path like the generic extractor's failures.
//!
//! `Any`/`Raw` subtrees — and unannotated routes, which have no
//! descriptor — keep the generic `extract_value` walk unchanged.

use std::borrow::Cow;
use std::collections::HashMap;

use fulmen_schema::desc::{
    DictDesc, ExtraPolicy, GenericDesc, Kind, Missing, RecordDesc, RecordKind, TypeDesc,
    UnionDesc,
};
use fulmen_schema::errors::{ErrorCode, LocSeg};
use fulmen_schema::value::Value;
use pyo3::exceptions::PyAttributeError;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyDelta, PyDict, PyFrozenSet, PyInt,
    PyList, PySet, PyString, PyTime, PyTuple, PyType,
};

use crate::descriptor::{
    self, DECIMAL, ENUM, IPV4, IPV6, MAX_EXTRACT_DEPTH, UUID_CLS, cached, extract_value,
};

/// A Python object that does not fit its declared descriptor — a
/// response-contract violation. `loc` names the output position where
/// the shape broke, outermost segment first.
#[derive(Debug)]
pub(crate) struct ShapeError {
    /// The output position (`/` loc path segments).
    pub(crate) loc: Vec<LocSeg>,
    /// The §11.7-style code for a debug `errors` entry.
    pub(crate) code: ErrorCode,
    /// What was expected and what was found.
    pub(crate) message: String,
}

impl ShapeError {
    /// `expected <want>, got <got>` — the standalone form of the walker's
    /// [`expected`] helper, for channel-side checks in `convert`.
    pub(crate) fn expected(want: &str, got: &Bound<'_, PyAny>) -> Self {
        Self {
            loc: Vec::new(),
            code: ErrorCode::Type,
            message: format!("expected {want}, got {}", py_type_name(got)),
        }
    }

    /// Prepend `seg` to the location while unwinding through a container.
    fn at(mut self, seg: LocSeg) -> Self {
        self.loc.insert(0, seg);
        self
    }

    /// The loc as a `/a/0/b` pointer for log lines.
    pub(crate) fn pointer(&self) -> String {
        let mut out = String::new();
        for seg in &self.loc {
            match seg {
                LocSeg::Key(k) => {
                    out.push('/');
                    out.push_str(k);
                }
                LocSeg::Index(i) => {
                    out.push('/');
                    out.push_str(&i.to_string());
                }
            }
        }
        if out.is_empty() {
            "/".to_owned()
        } else {
            out
        }
    }
}

/// Why a returned object could not be encoded under its descriptor.
#[derive(Debug)]
pub(crate) enum EncodeError {
    /// Structural contract violation.
    Shape(ShapeError),
    /// A Python access or call raised.
    Python(PyErr),
}

impl EncodeError {
    /// Prepend `seg` to a shape error's location (Python errors pass).
    fn at(self, seg: LocSeg) -> Self {
        match self {
            Self::Shape(s) => Self::Shape(s.at(seg)),
            other => other,
        }
    }
}

impl From<PyErr> for EncodeError {
    fn from(err: PyErr) -> Self {
        Self::Python(err)
    }
}

/// The descriptor's `default_factory` callback table — the same handle
/// materialization uses for `Missing::Bridge`.
pub(crate) struct EncCtx<'a> {
    /// `Descriptor.callbacks` (`Missing::Bridge` handles index into it).
    pub(crate) callbacks: &'a [Py<PyAny>],
}

/// `TypeVar` name → concrete `TypeDesc`, populated by `Kind::Generic`.
type Scope<'a> = HashMap<&'a str, &'a TypeDesc>;

/// Encode `obj` under `desc` into the wire-shaped [`Value`].
///
/// # Errors
/// [`EncodeError::Shape`] when `obj` does not fit the declared shape;
/// [`EncodeError::Python`] when a Python access raises.
pub(crate) fn encode<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    desc: &'a TypeDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    if depth > MAX_EXTRACT_DEPTH {
        return Err(shape(
            ErrorCode::Type,
            format!("response nesting exceeds {MAX_EXTRACT_DEPTH} levels"),
        ));
    }
    match &desc.kind {
        Kind::Null => {
            if obj.is_none() {
                Ok(Value::Null)
            } else {
                Err(expected("None", obj))
            }
        }
        Kind::Bool => match obj.cast::<PyBool>() {
            Ok(b) => Ok(Value::Bool(b.is_true())),
            Err(_) => Err(expected("bool", obj)),
        },
        Kind::Int(_) => encode_int(obj),
        Kind::Float(_) => encode_float(obj),
        Kind::Str(_) | Kind::Email | Kind::Url => match obj.cast::<PyString>() {
            Ok(s) => Ok(Value::string(s.to_str()?.to_owned())),
            Err(_) => Err(expected(&desc.kind.name(), obj)),
        },
        Kind::Bytes(_) => encode_bytes(obj),
        Kind::Decimal(_) => encode_named_str(
            py,
            obj,
            &cached(py, &DECIMAL, "decimal", "Decimal")?,
            "Decimal",
        ),
        Kind::DateTime => {
            if obj.is_instance_of::<PyDateTime>() {
                isoformat(obj)
            } else if let Ok(s) = obj.cast::<PyString>() {
                Ok(Value::string(s.to_str()?.to_owned()))
            } else {
                Err(expected("datetime", obj))
            }
        }
        Kind::Date => {
            if obj.is_instance_of::<PyDateTime>() {
                // A `datetime` is a `date` subclass but its isoformat is
                // not a valid `date` wire value — contract violation.
                Err(expected("date (got datetime)", obj))
            } else if obj.is_instance_of::<PyDate>() {
                isoformat(obj)
            } else if let Ok(s) = obj.cast::<PyString>() {
                Ok(Value::string(s.to_str()?.to_owned()))
            } else {
                Err(expected("date", obj))
            }
        }
        Kind::Time => {
            if obj.is_instance_of::<PyTime>() {
                isoformat(obj)
            } else if let Ok(s) = obj.cast::<PyString>() {
                Ok(Value::string(s.to_str()?.to_owned()))
            } else {
                Err(expected("time", obj))
            }
        }
        Kind::TimeDelta => {
            if obj.is_instance_of::<PyDelta>() {
                Ok(Value::string(descriptor::delta_text(obj)?))
            } else if let Ok(s) = obj.cast::<PyString>() {
                Ok(Value::string(s.to_str()?.to_owned()))
            } else {
                Err(expected("timedelta", obj))
            }
        }
        Kind::Uuid => encode_named_str(
            py,
            obj,
            &cached(py, &UUID_CLS, "uuid", "UUID")?,
            "UUID",
        ),
        Kind::Ipv4 => encode_named_str(
            py,
            obj,
            &cached(py, &IPV4, "ipaddress", "IPv4Address")?,
            "IPv4Address",
        ),
        Kind::Ipv6 => encode_named_str(
            py,
            obj,
            &cached(py, &IPV6, "ipaddress", "IPv6Address")?,
            "IPv6Address",
        ),
        Kind::Enum(_) => encode_enum(py, obj, depth),
        Kind::Literal(_) => {
            // The member value is what travels; membership is the
            // validator's call (debug) — encode emits whatever scalar
            // the handler produced.
            extract_value(py, obj, depth + 1).map_err(|e| extract_failed(e, obj))
        }
        Kind::Any | Kind::Raw => extract_value(py, obj, depth + 1).map_err(EncodeError::Python),
        Kind::TypeVar(name) => match scope.get(&**name) {
            Some(inner) => encode(py, obj, inner, ctx, scope, depth),
            // A leftover unbound TypeVar is a compile-time bug; encode
            // generically rather than failing the response.
            None => extract_value(py, obj, depth + 1).map_err(EncodeError::Python),
        },
        Kind::NewType(d) => encode(py, obj, &d.inner, ctx, scope, depth),
        Kind::Optional(d) => {
            if obj.is_none() {
                Ok(Value::Null)
            } else {
                encode(py, obj, &d.inner, ctx, scope, depth)
            }
        }
        Kind::Union(d) => encode_union(py, obj, d, ctx, scope, depth),
        Kind::Generic(d) => encode_generic(py, obj, d, ctx, scope, depth),
        Kind::List(d) => encode_seq(py, obj, &d.item, ctx, scope, depth, "a sequence"),
        Kind::TupleVar(d) => encode_seq(py, obj, &d.item, ctx, scope, depth, "a sequence"),
        Kind::Set(d) | Kind::FrozenSet(d) => {
            encode_seq(py, obj, &d.item, ctx, scope, depth, "a set/frozenset")
        }
        Kind::TupleFixed(d) => {
            let items = sequence(obj).ok_or_else(|| expected("a tuple", obj))?;
            if items.len() != d.items.len() {
                let code = if items.len() < d.items.len() {
                    ErrorCode::MinLen
                } else {
                    ErrorCode::MaxLen
                };
                return Err(shape(
                    code,
                    format!("expected {} items, got {}", d.items.len(), items.len()),
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for (i, (item, item_desc)) in items.iter().zip(d.items.iter()).enumerate() {
                out.push(
                    encode(py, item, item_desc, ctx, scope, depth + 1)
                        .map_err(|e| e.at(index(i)))?,
                );
            }
            Ok(Value::Array(out))
        }
        Kind::Dict(d) => encode_dict(py, obj, d, ctx, scope, depth),
        Kind::Record(rec) => encode_record(py, obj, rec, ctx, scope, depth),
    }
}

// ---- scalars -----------------------------------------------------------------

fn encode_int(obj: &Bound<'_, PyAny>) -> Result<Value<'static>, EncodeError> {
    // `bool` is an `int` subclass — reject it like strict validation does.
    if obj.is_instance_of::<PyBool>() || !obj.is_instance_of::<PyInt>() {
        return Err(expected("int", obj));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(Value::Int(i));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(Value::UInt(u));
    }
    let text = obj.str()?.to_str()?.to_owned();
    Ok(Value::BigInt(Cow::Owned(text)))
}

fn encode_float(obj: &Bound<'_, PyAny>) -> Result<Value<'static>, EncodeError> {
    if obj.is_instance_of::<PyBool>() {
        return Err(expected("float", obj));
    }
    // `PyFloat_AsDouble` coerces ints — matching the strict validator's
    // `int → f64` coercion — without accepting `str`.
    let Ok(f) = obj.extract::<f64>() else {
        return Err(expected("float", obj));
    };
    if !f.is_finite() {
        return Err(shape(
            ErrorCode::Float,
            "non-finite float is not JSON-representable".to_owned(),
        ));
    }
    Ok(Value::Float(f))
}

fn encode_bytes(obj: &Bound<'_, PyAny>) -> Result<Value<'static>, EncodeError> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(Value::Bytes(Cow::Owned(b.as_bytes().to_vec())));
    }
    if let Ok(b) = obj.cast::<PyByteArray>() {
        return Ok(Value::Bytes(Cow::Owned(b.to_vec())));
    }
    Err(expected("bytes", obj))
}

/// `str(obj)` for the named scalar classes (`Decimal`, `UUID`, the
/// `ipaddress` types); a plain `str` also passes — canonical form is
/// the validator's job when debug is on.
fn encode_named_str(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    class: &Py<PyAny>,
    name: &str,
) -> Result<Value<'static>, EncodeError> {
    if obj.is_instance(class.bind(obj.py()))? {
        return Ok(Value::string(obj.str()?.to_str()?.to_owned()));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::string(s.to_str()?.to_owned()));
    }
    Err(expected(name, obj))
}

/// `obj.isoformat()` — the canonical RFC 3339/ISO text the class itself
/// produces.
fn isoformat(obj: &Bound<'_, PyAny>) -> Result<Value<'static>, EncodeError> {
    Ok(Value::string(
        obj.call_method0("isoformat")?.extract::<String>()?,
    ))
}

fn encode_enum(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    if obj.is_instance(cached(py, &ENUM, "enum", "Enum")?.bind(py))? {
        // The member's `.value` is what travels on the wire.
        let value = obj.getattr("value")?;
        return extract_value(py, &value, depth + 1).map_err(|e| extract_failed(e, obj));
    }
    // A bare scalar naming the member value — the validator decides.
    extract_value(py, obj, depth + 1).map_err(|e| extract_failed(e, obj))
}

// ---- containers ----------------------------------------------------------------

/// `list`/`tuple`/`set`/`frozenset` → the member objects, or `None` when
/// `obj` is none of the four.
fn sequence<'py>(obj: &Bound<'py, PyAny>) -> Option<Vec<Bound<'py, PyAny>>> {
    if let Ok(l) = obj.cast::<PyList>() {
        Some(l.iter().collect())
    } else if let Ok(t) = obj.cast::<PyTuple>() {
        Some(t.iter().collect())
    } else if let Ok(s) = obj.cast::<PySet>() {
        Some(s.iter().collect())
    } else {
        obj.cast::<PyFrozenSet>().ok().map(|s| s.iter().collect())
    }
}

fn encode_seq<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    item: &'a TypeDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
    what: &str,
) -> Result<Value<'static>, EncodeError> {
    let items = sequence(obj).ok_or_else(|| expected(what, obj))?;
    let mut out = Vec::with_capacity(items.len());
    for (i, member) in items.iter().enumerate() {
        out.push(encode(py, member, item, ctx, scope, depth + 1).map_err(|e| e.at(index(i)))?);
    }
    Ok(Value::Array(out))
}

fn encode_dict<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    d: &'a DictDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    let dict = obj.cast::<PyDict>().map_err(|_| expected("dict", obj))?;
    let mut pairs = Vec::with_capacity(dict.len());
    for (k, v) in dict.iter() {
        let key = encode_key(py, &k, &d.key, scope).map_err(|e| e.at(key_seg(&k)))?;
        let value = encode(py, &v, &d.value, ctx, scope, depth + 1)
            .map_err(|e| e.at(LocSeg::Key(key.clone().into_boxed_str())))?;
        pairs.push((Cow::Owned(key), value));
    }
    Ok(Value::Object(pairs))
}

/// Canonical wire text for a dict key — `dict[K, V]` keys are always
/// string-coercible scalars (checked at compile).
fn encode_key<'a>(
    py: Python<'_>,
    k: &Bound<'_, PyAny>,
    desc: &'a TypeDesc,
    scope: &Scope<'a>,
) -> Result<String, EncodeError> {
    match &desc.kind {
        Kind::Str(_) | Kind::Email | Kind::Url => match k.cast::<PyString>() {
            Ok(s) => Ok(s.to_str()?.to_owned()),
            Err(_) => Err(expected("str key", k)),
        },
        Kind::Int(_) => {
            if k.is_instance_of::<PyBool>() || !k.is_instance_of::<PyInt>() {
                return Err(expected("int key", k));
            }
            Ok(k.str()?.to_str()?.to_owned())
        }
        Kind::Float(_) => {
            if k.is_instance_of::<PyBool>() || k.extract::<f64>().is_err() {
                return Err(expected("float key", k));
            }
            Ok(k.str()?.to_str()?.to_owned())
        }
        Kind::Bool => match k.cast::<PyBool>() {
            Ok(b) => Ok(if b.is_true() {
                "true".to_owned()
            } else {
                "false".to_owned()
            }),
            Err(_) => Err(expected("bool key", k)),
        },
        Kind::Enum(_) => {
            if k.is_instance(cached(py, &ENUM, "enum", "Enum")?.bind(py))? {
                let value = k.getattr("value")?;
                scalar_key(&value)
            } else {
                scalar_key(k)
            }
        }
        Kind::Literal(_) | Kind::Any | Kind::Raw => scalar_key(k),
        Kind::Optional(d) => {
            if k.is_none() {
                // `None` keys travel as the empty member — the lax side
                // reads "" back to `None`.
                Ok(String::new())
            } else {
                encode_key(py, k, &d.inner, scope)
            }
        }
        Kind::Union(d) => {
            for v in &d.variants {
                match encode_key(py, k, v, scope) {
                    Ok(key) => return Ok(key),
                    Err(EncodeError::Shape(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(expected("a union member key", k))
        }
        Kind::NewType(d) => encode_key(py, k, &d.inner, scope),
        Kind::TypeVar(name) => match scope.get(&**name) {
            Some(inner) => encode_key(py, k, inner, scope),
            None => scalar_key(k),
        },
        Kind::Generic(d) => {
            let mut child = scope.clone();
            bind_generic(&mut child, d);
            encode_key(py, k, &d.origin, &child)
        }
        Kind::Decimal(_) | Kind::Uuid | Kind::Ipv4 | Kind::Ipv6 => scalar_key(k),
        Kind::DateTime | Kind::Date | Kind::Time => {
            if let Ok(s) = k.cast::<PyString>() {
                Ok(s.to_str()?.to_owned())
            } else if k.is_instance_of::<PyDateTime>()
                || k.is_instance_of::<PyDate>()
                || k.is_instance_of::<PyTime>()
            {
                Ok(k.call_method0("isoformat")?.extract::<String>()?)
            } else {
                Err(expected("a temporal key", k))
            }
        }
        Kind::TimeDelta => {
            if let Ok(s) = k.cast::<PyString>() {
                Ok(s.to_str()?.to_owned())
            } else if k.is_instance_of::<PyDelta>() {
                Ok(descriptor::delta_text(k)?)
            } else {
                Err(expected("a timedelta key", k))
            }
        }
        // Compile rejects non-coercible key kinds; anything left cannot
        // name a JSON member.
        Kind::Null
        | Kind::Bytes(_)
        | Kind::List(_)
        | Kind::Set(_)
        | Kind::FrozenSet(_)
        | Kind::TupleVar(_)
        | Kind::TupleFixed(_)
        | Kind::Dict(_)
        | Kind::Record(_) => Err(expected("a string-coercible key", k)),
    }
}

/// `str()`-equivalent text for a scalar key (`int` member values, `Any`).
fn scalar_key(k: &Bound<'_, PyAny>) -> Result<String, EncodeError> {
    if let Ok(b) = k.cast::<PyBool>() {
        return Ok(if b.is_true() {
            "true".to_owned()
        } else {
            "false".to_owned()
        });
    }
    Ok(k.str()?.to_str()?.to_owned())
}

/// The `LocSeg` naming a dict member by its Python key object.
fn key_seg(k: &Bound<'_, PyAny>) -> LocSeg {
    let text = k
        .cast::<PyString>()
        .ok()
        .and_then(|s| s.to_str().ok().map(ToOwned::to_owned))
        .or_else(|| {
            k.str()
                .and_then(|s| s.to_str().map(ToOwned::to_owned))
                .ok()
        });
    LocSeg::Key(text.unwrap_or_else(|| "?".to_owned()).into())
}

// ---- unions / generics -----------------------------------------------------------

fn encode_union<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    u: &'a UnionDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    // Discriminated unions pick by the tag member when it reads cleanly.
    if let Some(tag) = &u.tag
        && let Some(tag_obj) = member(obj, tag)?
    {
        let tag_value = extract_value(py, &tag_obj, depth + 1)?;
        if let Some(variant) = u
            .variants
            .iter()
            .find(|v| tag_names_variant(v, tag, &tag_value))
        {
            return encode(py, obj, variant, ctx, scope, depth);
        }
    }
    for variant in &u.variants {
        match encode(py, obj, variant, ctx, scope, depth) {
            Ok(out) => return Ok(out),
            Err(EncodeError::Shape(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let names: Vec<String> = u.variants.iter().map(|v| v.kind.name()).collect();
    Err(shape(
        ErrorCode::Union,
        format!("no variant of `{}` matches", names.join(" | ")),
    ))
}

/// `getattr(obj, tag)` or `obj[tag]` — the discriminator member, if present.
fn member<'py>(
    obj: &Bound<'py, PyAny>,
    name: &str,
) -> Result<Option<Bound<'py, PyAny>>, EncodeError> {
    if let Ok(d) = obj.cast::<PyDict>() {
        return Ok(d.get_item(name)?);
    }
    match obj.getattr(name) {
        Ok(v) => Ok(Some(v)),
        Err(e) if is_attribute_error(obj.py(), &e) => Ok(None),
        Err(e) => Err(EncodeError::Python(e)),
    }
}

/// `true` when `variant` is a record whose `tag` field is a `Literal`
/// containing `value`.
fn tag_names_variant(variant: &TypeDesc, tag: &str, value: &Value<'_>) -> bool {
    let Some(rec) = descriptor::record_of(variant) else {
        return false;
    };
    rec.fields.iter().any(|f| {
        &*f.name == tag
            && matches!(&f.ty.kind, Kind::Literal(l)
                if l.members.iter().any(|m| m.to_value() == *value))
    })
}

fn encode_generic<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    g: &'a GenericDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    let mut child = scope.clone();
    bind_generic(&mut child, g);
    encode(py, obj, &g.origin, ctx, &child, depth)
}

/// Zip the origin record's `type_params` to `g.args` inside `scope`.
fn bind_generic<'a>(scope: &mut Scope<'a>, g: &'a GenericDesc) {
    let Some(rec) = descriptor::record_of(&g.origin) else {
        return;
    };
    for (name, arg) in rec.type_params.iter().zip(g.args.iter()) {
        scope.insert(&**name, arg);
    }
}

// ---- records -----------------------------------------------------------------

/// Walk `obj` by `rec`'s declared fields: `getattr` for `Model`/
/// `dataclass` instances (never `__dict__` assumptions — the descriptor
/// is authoritative), `dict[wire_key]`/`dict[name]` for a `dict` input
/// (a `TypedDict`, or a dict standing in for the record). Output keys
/// are the wire keys — alias wins, then `rename`, then the field name.
fn encode_record<'a>(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    rec: &'a RecordDesc,
    ctx: &EncCtx<'_>,
    scope: &Scope<'a>,
    depth: usize,
) -> Result<Value<'static>, EncodeError> {
    let mapping = obj.cast::<PyDict>().ok();
    if rec.kind == RecordKind::TypedDict && mapping.is_none() {
        return Err(expected("a dict (TypedDict)", obj));
    }
    let mut pairs: Vec<(Cow<'static, str>, Value<'static>)> = Vec::with_capacity(rec.fields.len());
    for field in &rec.fields {
        let wire = field.wire_key(rec.rename);
        let member = match &mapping {
            Some(d) => match d.get_item(&*wire)? {
                Some(v) => Some(v),
                None => d.get_item(&*field.name)?,
            },
            None => getattr_opt(obj, &field.name)?,
        };
        match (member, &field.missing) {
            (Some(v), _) => {
                let value = encode(py, &v, &field.ty, ctx, scope, depth + 1)
                    .map_err(|e| e.at(LocSeg::Key(wire.clone())))?;
                pairs.push((Cow::Owned(wire.into_string()), value));
            }
            (None, Missing::Omit) => {}
            (None, Missing::Literal(l)) => {
                pairs.push((Cow::Owned(wire.into_string()), l.to_value()));
            }
            (None, Missing::Bridge(id)) => {
                let Some(cb) = ctx.callbacks.get(*id as usize) else {
                    return Err(shape(
                        ErrorCode::Type,
                        format!("default-factory callback id {id} out of range"),
                    ));
                };
                let produced = cb.bind(py).call0()?;
                let value = encode(py, &produced, &field.ty, ctx, scope, depth + 1)
                    .map_err(|e| e.at(LocSeg::Key(wire.clone())))?;
                pairs.push((Cow::Owned(wire.into_string()), value));
            }
            (None, Missing::Required) => {
                return Err(EncodeError::Shape(ShapeError {
                    loc: vec![LocSeg::Key(wire)],
                    code: ErrorCode::Missing,
                    message: format!("required field `{}` missing", field.name),
                }));
            }
        }
    }
    encode_extras(py, obj, rec, &mapping, depth, &mut pairs)?;
    Ok(Value::Object(pairs))
}

/// Apply the record's `extra` policy: `forbid` flags undeclared keys,
/// `keep` emits them generically after the declared fields, `ignore`
/// drops them.
fn encode_extras(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    rec: &RecordDesc,
    mapping: &Option<&Bound<'_, PyDict>>,
    depth: usize,
    pairs: &mut Vec<(Cow<'static, str>, Value<'static>)>,
) -> Result<(), EncodeError> {
    if rec.extra == ExtraPolicy::Ignore {
        return Ok(());
    }
    // The candidate extra keys: the input mapping's keys, or the
    // instance `__dict__` members when the value is an object.
    let mut extras: Vec<(String, Bound<'_, PyAny>)> = Vec::new();
    match mapping {
        Some(d) => {
            for (k, v) in d.iter() {
                let Ok(name) = k.str() else { continue };
                let Ok(name) = name.to_str() else { continue };
                if !declared(rec, name) {
                    extras.push((name.to_owned(), v));
                }
            }
        }
        None => {
            let state = match obj.getattr("__dict__") {
                Ok(s) => s,
                Err(e) if is_attribute_error(obj.py(), &e) => return Ok(()),
                Err(e) => return Err(EncodeError::Python(e)),
            };
            if let Ok(d) = state.cast::<PyDict>() {
                for (k, v) in d.iter() {
                    let Ok(name) = k.str() else { continue };
                    let Ok(name) = name.to_str() else { continue };
                    // `_`-prefixed members are object internals, not data.
                    if !name.starts_with('_') && !declared_name(rec, name) {
                        extras.push((name.to_owned(), v));
                    }
                }
            }
        }
    }
    match rec.extra {
        ExtraPolicy::Forbid => {
            if let Some((name, _)) = extras.into_iter().next() {
                return Err(EncodeError::Shape(ShapeError {
                    loc: vec![LocSeg::Key(name.into_boxed_str())],
                    code: ErrorCode::ExtraForbidden,
                    message: "unexpected extra member".to_owned(),
                }));
            }
        }
        ExtraPolicy::Keep => {
            for (name, v) in extras {
                let value = extract_value(py, &v, depth + 1).map_err(EncodeError::Python)?;
                pairs.push((Cow::Owned(name), value));
            }
        }
        ExtraPolicy::Ignore => {}
    }
    Ok(())
}

/// `name` is a declared member when it is a field's wire key or Python
/// name (a `model_dump`-shaped dict keys by `name`; a wire dict by
/// `wire_key`).
fn declared(rec: &RecordDesc, name: &str) -> bool {
    rec.fields
        .iter()
        .any(|f| &*f.name == name || &*f.wire_key(rec.rename) == name)
}

/// `name` is a declared Python field name (for `__dict__` extras).
fn declared_name(rec: &RecordDesc, name: &str) -> bool {
    rec.fields.iter().any(|f| &*f.name == name)
}

/// `getattr(obj, name)`, with `AttributeError` mapped to `None`.
fn getattr_opt<'py>(
    obj: &Bound<'py, PyAny>,
    name: &str,
) -> Result<Option<Bound<'py, PyAny>>, EncodeError> {
    match obj.getattr(name) {
        Ok(v) => Ok(Some(v)),
        Err(e) if is_attribute_error(obj.py(), &e) => Ok(None),
        Err(e) => Err(EncodeError::Python(e)),
    }
}

fn is_attribute_error(py: Python<'_>, err: &PyErr) -> bool {
    err.matches(py, PyType::new::<PyAttributeError>(py).as_any())
        .unwrap_or(false)
}

// ---- errors -------------------------------------------------------------------

/// An index `LocSeg`, saturating like the validator's own locus builder.
fn index(i: usize) -> LocSeg {
    LocSeg::Index(u32::try_from(i).unwrap_or(u32::MAX))
}

fn shape(code: ErrorCode, message: String) -> EncodeError {
    EncodeError::Shape(ShapeError {
        loc: Vec::new(),
        code,
        message,
    })
}

fn expected(want: &str, got: &Bound<'_, PyAny>) -> EncodeError {
    shape(
        ErrorCode::Type,
        format!("expected {want}, got {}", py_type_name(got)),
    )
}

/// `extract_value` failed while encoding a member — reworded as a shape
/// error (the object cannot be carried for the declared type).
fn extract_failed(_err: PyErr, obj: &Bound<'_, PyAny>) -> EncodeError {
    shape(
        ErrorCode::Type,
        format!("cannot encode {} for the declared type", py_type_name(obj)),
    )
}

/// `type(obj).__qualname__` for messages — infallible.
fn py_type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .qualname()
        .map_or_else(|_| "<unknown>".to_owned(), |q| q.to_string())
}
