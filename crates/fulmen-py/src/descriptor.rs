//! The descriptor bridge — the single pyo3 boundary between the Python
//! annotation converter (`fulmen._typing`) and `fulmen-schema`
//! (DESIGN.md §11.3).
//!
//! `fulmen._typing` walks type annotations once — at handler registration
//! and at `Model` class creation — and produces a plain "spec tree": nested
//! dicts/lists/strings/numbers, no Python objects (docs/SPEC-FORMAT.md).
//! [`compile_descriptor`] turns a spec tree into a [`TypeDesc`], compiles it
//! into a [`Validator`], and returns a [`Descriptor`] pyclass holding both.
//! The descriptor can then:
//!
//! - `validate(obj, source, lax)` — Python object → `Value` → validator →
//!   materialized Python object (`None`/defaults applied, transforms run,
//!   `AfterValidator` hooks called). Raises `fulmen.errors.ValidationError`.
//! - `construct(instance, data)` — validate a mapping against a record
//!   descriptor and `object.__setattr__` the emitted fields onto
//!   `instance` — the native half of `Model.__init__`.
//! - `validate_field(name, value)` — validate one record field — the native
//!   half of `Model.__setattr__` (D19, mutable-with-validated-setattr).
//! - `.spec` — the spec dict back, for the OpenAPI/extraction tasks.
//!
//! `PyObject → Value` extraction and `Value → PyObject` materialization are
//! `pub(crate)`: the param-extraction and encoder tasks reuse them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fulmen_schema::desc::{
    Bound as SchemaBound, BytesDesc, ClassRef, DecimalDesc, DictDesc, EnumDesc, EnumKind,
    EnumMember, ExtraPolicy, FieldDesc, FloatDesc, GenericDesc, IntDesc, Kind, Len, ListDesc,
    LiteralDesc, Missing, NewTypeDesc, OptionalDesc, Pattern, RecordDesc, RecordKind, RenameRule,
    SetDesc, StrDesc, TupleFixedDesc, TupleVarDesc, TypeDesc, TypeMeta, UnionDesc,
};
use fulmen_schema::errors::{ErrorEntry, Source, ValidationErrors};
use fulmen_schema::parse::DurationParts;
use fulmen_schema::value::{LiteralValue, Value};
use fulmen_schema::{Mode, Validator, compile};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyDelta, PyDict, PyFloat, PyFrozenSet, PyInt,
    PyList, PySet, PyString, PyTime, PyTuple,
};

/// Depth cap for `extract_value` — Python-side recursion is bounded by the
/// same limit the JSON decoder uses for request bodies.
pub(crate) const MAX_EXTRACT_DEPTH: usize = 200;

// ---- lazily imported Python classes ----------------------------------------

/// One lazily-resolved Python class (`module.attr`, possibly a dotted
/// qualname). Resolved once per process under the GIL. `pub(crate)` —
/// the response encoder checks the same classes.
pub(crate) static DECIMAL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) static UUID_CLS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) static IPV4: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) static IPV6: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) static ENUM: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static DATETIME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static DATE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TIME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TIMEDELTA: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static EMAIL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static URL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static OBJECT_SETATTR: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static BUILTINS_INT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static VALIDATION_ERROR: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

/// Resolve `module.attr` once per process under the GIL.
pub(crate) fn cached(
    py: Python<'_>,
    cell: &PyOnceLock<Py<PyAny>>,
    module: &str,
    attr: &str,
) -> PyResult<Py<PyAny>> {
    if let Some(c) = cell.get(py) {
        return Ok(c.clone_ref(py));
    }
    let obj = py.import(module)?.getattr(attr)?.unbind();
    Ok(cell.get_or_init(py, || obj).clone_ref(py))
}

fn object_setattr(py: Python<'_>) -> PyResult<Py<PyAny>> {
    cached(py, &OBJECT_SETATTR, "builtins", "object")
        .and_then(|object| Ok(object.bind(py).getattr("__setattr__")?.unbind()))
}

/// `fulmen.errors.ValidationError`, resolved once (also used by the
/// dispatch path to recognise raised validation failures).
pub(crate) fn validation_error_class(py: Python<'_>) -> PyResult<Py<PyAny>> {
    if let Some(c) = VALIDATION_ERROR.get(py) {
        return Ok(c.clone_ref(py));
    }
    let cls = py
        .import("fulmen.errors")?
        .getattr("ValidationError")?
        .unbind();
    Ok(VALIDATION_ERROR.get_or_init(py, || cls).clone_ref(py))
}

/// `fulmen.errors.HTTPError`, resolved once for the problem-response path.
static HTTP_ERROR: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) fn http_error_class(py: Python<'_>) -> PyResult<Py<PyAny>> {
    cached(py, &HTTP_ERROR, "fulmen.errors", "HTTPError")
}

/// `fulmen.responses.Stream`, resolved once for the buffered-stream body
/// path (a `Stream` body always joins its chunks, even when the iterable
/// itself would be JSON-serializable).
static STREAM: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
pub(crate) fn stream_class(py: Python<'_>) -> PyResult<Py<PyAny>> {
    cached(py, &STREAM, "fulmen.responses", "Stream")
}

/// `timedelta` → its canonical ISO 8601 text (shared with `json::extract`).
pub(crate) fn delta_text(obj: &Bound<'_, PyAny>) -> PyResult<String> {
    let days = obj.getattr("days")?.extract::<i128>()?;
    let secs = obj.getattr("seconds")?.extract::<i128>()?;
    let micros = obj.getattr("microseconds")?.extract::<i128>()?;
    let total = days * 86_400_000_000 + secs * 1_000_000 + micros;
    let neg = total < 0;
    let mag = total.unsigned_abs();
    let d = DurationParts {
        negative: neg,
        days: u64::try_from(mag / 86_400_000_000).unwrap_or(u64::MAX),
        seconds: u64::try_from((mag / 1_000_000) % 86_400).unwrap_or(u64::MAX),
        micros: u64::try_from(mag % 1_000_000).unwrap_or(u64::MAX),
    };
    Ok(d.to_canonical())
}

// ---- spec tree → TypeDesc ---------------------------------------------------

/// A `{"name": qualname, "module": mod}` class reference.
fn class_ref(obj: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> ClassRef {
    let (mut name, mut module) = (String::new(), String::new());
    if let Ok(d) = obj.cast::<PyDict>() {
        match get_str(d, "name") {
            Some(s) => name = s,
            None => errs.push(format!("{path}: class.name must be a string")),
        }
        match get_str(d, "module") {
            Some(s) => module = s,
            None => errs.push(format!("{path}: class.module must be a string")),
        }
    } else {
        errs.push(format!("{path}: class must be a dict"));
    }
    ClassRef {
        name: name.into_boxed_str(),
        module: module.into_boxed_str(),
    }
}

fn get<'d>(d: &'d Bound<'d, PyDict>, key: &str) -> Option<Bound<'d, PyAny>> {
    d.get_item(key).ok().flatten()
}

fn get_str(d: &Bound<'_, PyDict>, key: &str) -> Option<String> {
    get(d, key).and_then(|v| v.extract::<String>().ok())
}

fn get_bool(d: &Bound<'_, PyDict>, key: &str) -> Option<bool> {
    get(d, key).and_then(|v| v.extract::<bool>().ok())
}

/// A `Literal` member / `missing` literal / bound limit: one of
/// `None|bool|int|float|str|bytes` — anything else is a spec error.
fn literal_value(
    obj: &Bound<'_, PyAny>,
    errs: &mut Vec<String>,
    path: &str,
) -> Option<LiteralValue> {
    if obj.is_none() {
        return Some(LiteralValue::Null);
    }
    if let Ok(b) = obj.extract::<bool>() {
        return Some(LiteralValue::Bool(b));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(i) = obj.extract::<i64>() {
            return Some(LiteralValue::Int(i));
        }
        if let Ok(u) = obj.extract::<u64>() {
            return Some(LiteralValue::UInt(u));
        }
        return obj
            .str()
            .ok()
            .and_then(|s| s.to_str().ok().map(|t| LiteralValue::BigInt(t.into())));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return Some(LiteralValue::Float(f));
    }
    if let Ok(s) = obj.extract::<String>() {
        return Some(LiteralValue::Str(s.into_boxed_str()));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Some(LiteralValue::Bytes(b.as_bytes().into()));
    }
    errs.push(format!(
        "{path}: literal must be None, bool, int, float, str or bytes"
    ));
    None
}

/// A `Range`/`MultipleOf` operand: `int|float` or `{"decimal": text}`.
fn bound(obj: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> Option<SchemaBound> {
    if obj.is_instance_of::<PyBool>() {
        errs.push(format!("{path}: bound must be a number, not a bool"));
        return None;
    }
    if let Ok(i) = obj.extract::<i128>() {
        return Some(SchemaBound::Int(i));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return Some(SchemaBound::Float(f));
    }
    if let Ok(d) = obj.cast::<PyDict>()
        && let Some(s) = get_str(d, "decimal")
    {
        return Some(SchemaBound::Decimal(s.into_boxed_str()));
    }
    errs.push(format!(
        "{path}: bound must be a number or {{\"decimal\": text}}"
    ));
    None
}

fn range_desc(
    obj: &Bound<'_, PyAny>,
    errs: &mut Vec<String>,
    path: &str,
) -> Option<fulmen_schema::desc::Range> {
    let d = obj.cast::<PyDict>().ok()?;
    let mut r = fulmen_schema::desc::Range::default();
    for (key, slot) in [
        ("ge", &mut r.ge),
        ("gt", &mut r.gt),
        ("le", &mut r.le),
        ("lt", &mut r.lt),
    ] {
        if let Some(v) = get(d, key) {
            *slot = bound(&v, errs, &format!("{path}.range.{key}"));
        }
    }
    Some(r)
}

fn len_desc(obj: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> Option<Len> {
    let d = obj.cast::<PyDict>().ok()?;
    let (mut min, mut max) = (None, None);
    if let Some(v) = get(d, "min") {
        match v.extract::<u64>() {
            Ok(n) => min = Some(n),
            Err(_) => errs.push(format!("{path}.len.min must be a non-negative int")),
        }
    }
    if let Some(v) = get(d, "max") {
        match v.extract::<u64>() {
            Ok(n) => max = Some(n),
            Err(_) => errs.push(format!("{path}.len.max must be a non-negative int")),
        }
    }
    Some(Len::new(min, max))
}

fn meta_desc(obj: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> Option<Box<TypeMeta>> {
    let d = obj.cast::<PyDict>().ok()?;
    let mut m = TypeMeta {
        alias: get_str(d, "alias").map(Into::into),
        description: get_str(d, "description").map(Into::into),
        deprecated: get_bool(d, "deprecated").unwrap_or(false),
        ..TypeMeta::default()
    };
    if let Some(v) = get(d, "default") {
        m.default = literal_value(&v, errs, &format!("{path}.meta.default"));
    }
    if let Some(v) = get(d, "example") {
        m.example = literal_value(&v, errs, &format!("{path}.meta.example"));
    }
    if let Some(t) = get(d, "transforms") {
        if let Ok(td) = t.cast::<PyDict>() {
            m.transforms.strip = get_bool(td, "strip").unwrap_or(false);
            m.transforms.lower = get_bool(td, "lower").unwrap_or(false);
            m.transforms.upper = get_bool(td, "upper").unwrap_or(false);
        } else {
            errs.push(format!("{path}.meta.transforms must be a dict"));
        }
    }
    if let Some(a) = get(d, "after") {
        if let Ok(l) = a.cast::<PyList>() {
            for item in l.iter() {
                match item.extract::<u32>() {
                    Ok(id) => {
                        let mut ids = m.after.into_vec();
                        ids.push(id);
                        m.after = ids.into_boxed_slice();
                    }
                    Err(_) => errs.push(format!("{path}.meta.after must hold ints")),
                }
            }
        } else {
            errs.push(format!("{path}.meta.after must be a list"));
        }
    }
    if m == TypeMeta::default() {
        return None;
    }
    Some(Box::new(m))
}

fn missing_desc(obj: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> Missing {
    if let Ok(s) = obj.extract::<String>() {
        return match s.as_str() {
            "required" => Missing::Required,
            "omit" => Missing::Omit,
            _ => {
                errs.push(format!("{path}: missing must be 'required', 'omit', {{'literal': …}} or {{'factory': id}}"));
                Missing::Required
            }
        };
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        if let Some(v) = get(d, "literal") {
            match literal_value(&v, errs, path) {
                Some(l) => return Missing::Literal(l),
                None => return Missing::Required,
            }
        }
        if let Some(v) = get(d, "factory") {
            match v.extract::<u32>() {
                Ok(id) => return Missing::Bridge(id),
                Err(_) => errs.push(format!("{path}: factory must be a callback id (int)")),
            }
        }
        return Missing::Required;
    }
    errs.push(format!("{path}: missing must be a string or dict"));
    Missing::Required
}

/// Build one `TypeDesc` node from a spec dict. Errors are collected (not
/// raised) so `compile_descriptor` reports every bad node at once.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per spec 'kind' — the flat match IS the SPEC-FORMAT table; splitting it would scatter the contract"
)]
fn build_desc(spec: &Bound<'_, PyAny>, errs: &mut Vec<String>, path: &str) -> Option<TypeDesc> {
    let d = if let Ok(d) = spec.cast::<PyDict>() {
        d.clone()
    } else {
        errs.push(format!("{path}: spec node must be a dict"));
        return None;
    };
    let Some(kind) = get_str(&d, "kind") else {
        errs.push(format!("{path}: missing 'kind'"));
        return None;
    };
    let sub = |key: &str, errs: &mut Vec<String>| -> Option<TypeDesc> {
        if let Some(v) = get(&d, key) {
            build_desc(&v, errs, &format!("{path}.{key}"))
        } else {
            errs.push(format!("{path}.{key}: required"));
            None
        }
    };
    let int_desc = |errs: &mut Vec<String>| IntDesc {
        range: get(&d, "range").and_then(|v| range_desc(&v, errs, path)),
        multiple_of: get(&d, "multiple_of").and_then(|v| bound(&v, errs, path)),
    };
    let len_of = |errs: &mut Vec<String>| get(&d, "len").and_then(|v| len_desc(&v, errs, path));

    let kind = match kind.as_str() {
        "bool" => Kind::Bool,
        "none" => Kind::Null,
        "any" => Kind::Any,
        "raw" => Kind::Raw,
        "int" => Kind::Int(int_desc(errs)),
        "float" => Kind::Float(FloatDesc {
            range: get(&d, "range").and_then(|v| range_desc(&v, errs, path)),
            multiple_of: get(&d, "multiple_of").and_then(|v| bound(&v, errs, path)),
        }),
        "decimal" => Kind::Decimal(DecimalDesc {
            range: get(&d, "range").and_then(|v| range_desc(&v, errs, path)),
            multiple_of: get(&d, "multiple_of").and_then(|v| bound(&v, errs, path)),
        }),
        "str" => Kind::Str(StrDesc {
            len: len_of(errs),
            pattern: get(&d, "pattern").and_then(|v| {
                v.extract::<String>()
                    .ok()
                    .and_then(|s| Pattern::new(&s).ok())
            }),
        }),
        "bytes" => Kind::Bytes(BytesDesc { len: len_of(errs) }),
        "datetime" => Kind::DateTime,
        "date" => Kind::Date,
        "time" => Kind::Time,
        "timedelta" => Kind::TimeDelta,
        "uuid" => Kind::Uuid,
        "email" => Kind::Email,
        "url" => Kind::Url,
        "ipv4" => Kind::Ipv4,
        "ipv6" => Kind::Ipv6,
        "list" => Kind::List(ListDesc {
            item: Box::new(sub("item", errs)?),
            len: len_of(errs),
            unique: get_bool(&d, "unique").unwrap_or(false),
        }),
        "set" => Kind::Set(SetDesc {
            item: Box::new(sub("item", errs)?),
            len: len_of(errs),
        }),
        "frozenset" => Kind::FrozenSet(SetDesc {
            item: Box::new(sub("item", errs)?),
            len: len_of(errs),
        }),
        "tuple" => Kind::TupleVar(TupleVarDesc {
            item: Box::new(sub("item", errs)?),
            len: len_of(errs),
        }),
        "tuple_fixed" => {
            let items = if let Some(v) = get(&d, "items") {
                if let Ok(l) = v.cast::<PyList>() {
                    l.iter()
                        .enumerate()
                        .filter_map(|(i, s)| build_desc(&s, errs, &format!("{path}.items[{i}]")))
                        .collect::<Vec<_>>()
                } else {
                    errs.push(format!("{path}.items: must be a list"));
                    Vec::new()
                }
            } else {
                errs.push(format!("{path}.items: required"));
                Vec::new()
            };
            Kind::TupleFixed(TupleFixedDesc {
                items: items.into_boxed_slice(),
            })
        }
        "dict" => Kind::Dict(DictDesc {
            key: Box::new(sub("key", errs)?),
            value: Box::new(sub("value", errs)?),
            len: len_of(errs),
        }),
        "literal" => {
            let members = if let Some(v) = get(&d, "members") {
                if let Ok(l) = v.cast::<PyList>() {
                    l.iter()
                        .enumerate()
                        .filter_map(|(i, m)| {
                            literal_value(&m, errs, &format!("{path}.members[{i}]"))
                        })
                        .collect::<Vec<_>>()
                } else {
                    errs.push(format!("{path}.members: must be a list"));
                    Vec::new()
                }
            } else {
                errs.push(format!("{path}.members: required"));
                Vec::new()
            };
            Kind::Literal(LiteralDesc {
                members: members.into_boxed_slice(),
            })
        }
        "enum" => {
            let cls = if let Some(c) = get(&d, "class") {
                class_ref(&c, errs, path)
            } else {
                errs.push(format!("{path}.class: required"));
                ClassRef {
                    name: "Enum".into(),
                    module: "enum".into(),
                }
            };
            let flavor = match get_str(&d, "flavor").as_deref() {
                Some("int") => EnumKind::Int,
                Some("str") => EnumKind::Str,
                _ => EnumKind::Plain,
            };
            let members = if let Some(v) = get(&d, "members") {
                if let Ok(l) = v.cast::<PyList>() {
                    l.iter()
                        .enumerate()
                        .filter_map(|(i, m)| {
                            let md = m.cast::<PyDict>().ok()?;
                            let name = get_str(md, "name")?;
                            let value = get(md, "value").and_then(|v| {
                                literal_value(&v, errs, &format!("{path}.members[{i}]"))
                            })?;
                            Some(EnumMember {
                                name: name.into_boxed_str(),
                                value,
                            })
                        })
                        .collect::<Vec<_>>()
                } else {
                    errs.push(format!("{path}.members: must be a list"));
                    Vec::new()
                }
            } else {
                errs.push(format!("{path}.members: required"));
                Vec::new()
            };
            Kind::Enum(EnumDesc {
                class: cls,
                kind: flavor,
                members: members.into_boxed_slice(),
            })
        }
        "optional" => Kind::Optional(OptionalDesc {
            inner: Box::new(sub("inner", errs)?),
        }),
        "union" => {
            let variants = if let Some(v) = get(&d, "variants") {
                if let Ok(l) = v.cast::<PyList>() {
                    l.iter()
                        .enumerate()
                        .filter_map(|(i, s)| build_desc(&s, errs, &format!("{path}.variants[{i}]")))
                        .collect::<Vec<_>>()
                } else {
                    errs.push(format!("{path}.variants: must be a list"));
                    Vec::new()
                }
            } else {
                errs.push(format!("{path}.variants: required"));
                Vec::new()
            };
            Kind::Union(UnionDesc {
                variants: variants.into_boxed_slice(),
                tag: get_str(&d, "tag").map(Into::into),
            })
        }
        "newtype" => Kind::NewType(NewTypeDesc {
            name: get_str(&d, "name").unwrap_or_default().into_boxed_str(),
            inner: Box::new(sub("inner", errs)?),
        }),
        "generic" => Kind::Generic(GenericDesc {
            origin: Box::new(sub("origin", errs)?),
            args: match get(&d, "args") {
                Some(v) => {
                    if let Ok(l) = v.cast::<PyList>() {
                        l.iter()
                            .enumerate()
                            .filter_map(|(i, s)| build_desc(&s, errs, &format!("{path}.args[{i}]")))
                            .collect::<Vec<_>>()
                            .into_boxed_slice()
                    } else {
                        errs.push(format!("{path}.args: must be a list"));
                        Vec::new().into_boxed_slice()
                    }
                }
                None => Vec::new().into_boxed_slice(),
            },
        }),
        "typevar" => Kind::TypeVar(get_str(&d, "name").unwrap_or_default().into_boxed_str()),
        "record" => {
            let cls = if let Some(c) = get(&d, "class") {
                class_ref(&c, errs, path)
            } else {
                errs.push(format!("{path}.class: required"));
                ClassRef {
                    name: "?".into(),
                    module: "?".into(),
                }
            };
            let flavor = match get_str(&d, "flavor").as_deref() {
                Some("dataclass") => RecordKind::Dataclass,
                Some("typed_dict") => RecordKind::TypedDict,
                _ => RecordKind::Model,
            };
            let extra = match get_str(&d, "extra").as_deref() {
                Some("forbid") => ExtraPolicy::Forbid,
                Some("keep") => ExtraPolicy::Keep,
                _ => ExtraPolicy::Ignore,
            };
            let rename = match get_str(&d, "rename").as_deref() {
                Some("camel") => Some(RenameRule::Camel),
                Some("pascal") => Some(RenameRule::Pascal),
                Some("kebab") => Some(RenameRule::Kebab),
                Some(_) | None => None,
            };
            let fields = match get(&d, "fields") {
                Some(v) => {
                    if let Ok(l) = v.cast::<PyList>() {
                        l.iter()
                            .enumerate()
                            .filter_map(|(i, f)| {
                                let fd = f.cast::<PyDict>().ok()?;
                                let name = get_str(fd, "name")?;
                                let ty = get(fd, "type").and_then(|t| {
                                    build_desc(&t, errs, &format!("{path}.fields[{i}].type"))
                                })?;
                                Some(FieldDesc {
                                    name: name.into_boxed_str(),
                                    alias: get_str(fd, "alias").map(Into::into),
                                    missing: get(fd, "missing").map_or(Missing::Required, |m| {
                                        missing_desc(
                                            &m,
                                            errs,
                                            &format!("{path}.fields[{i}].missing"),
                                        )
                                    }),
                                    ty,
                                    meta: get(fd, "meta").and_then(|m| meta_desc(&m, errs, path)),
                                })
                            })
                            .collect::<Vec<_>>()
                    } else {
                        errs.push(format!("{path}.fields: must be a list"));
                        Vec::new()
                    }
                }
                None => Vec::new(),
            };
            let type_params = get(&d, "type_params")
                .and_then(|v| v.cast::<PyList>().ok().cloned())
                .map(|l| {
                    l.iter()
                        .filter_map(|s| s.extract::<String>().ok())
                        .map(Into::into)
                        .collect::<Vec<Box<str>>>()
                })
                .unwrap_or_default();
            Kind::Record(RecordDesc {
                class: cls,
                kind: flavor,
                fields: fields.into_boxed_slice(),
                extra,
                rename,
                type_params: type_params.into_boxed_slice(),
            })
        }
        other => {
            errs.push(format!("{path}: unknown kind {other:?}"));
            return None;
        }
    };
    let meta = get(&d, "meta").and_then(|m| meta_desc(&m, errs, path));
    Some(TypeDesc { kind, meta })
}

// ---- Python object → Value ---------------------------------------------------

/// Convert a Python object into an owned [`Value`] — the input side of
/// `Descriptor.validate`/`construct`. `pub(crate)` for the extraction task.
#[expect(
    clippy::too_many_lines,
    reason = "one branch per Python input shape — the type-dispatch table reads top to bottom"
)]
pub(crate) fn extract_value(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    depth: usize,
) -> PyResult<Value<'static>> {
    if depth > MAX_EXTRACT_DEPTH {
        return Err(PyTypeError::new_err(format!(
            "object nesting exceeds {MAX_EXTRACT_DEPTH} levels"
        )));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    // bool before int — `True` is an `int` subclass.
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(i) = obj.extract::<i64>() {
            return Ok(Value::Int(i));
        }
        if let Ok(u) = obj.extract::<u64>() {
            return Ok(Value::UInt(u));
        }
        let text = obj.str()?.to_str()?.to_owned();
        return Ok(Value::BigInt(Cow::Owned(text)));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(obj.extract::<f64>()?));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::string(s.to_str()?.to_owned()));
    }
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(Value::Bytes(Cow::Owned(b.as_bytes().to_vec())));
    }
    if let Ok(b) = obj.cast::<PyByteArray>() {
        return Ok(Value::Bytes(Cow::Owned(b.to_vec())));
    }
    // datetime before date — `datetime` is a `date` subclass.
    if obj.is_instance_of::<PyDateTime>() {
        return Ok(Value::string(
            obj.call_method0("isoformat")?.extract::<String>()?,
        ));
    }
    if obj.is_instance_of::<PyDate>() {
        return Ok(Value::string(
            obj.call_method0("isoformat")?.extract::<String>()?,
        ));
    }
    if obj.is_instance_of::<PyTime>() {
        return Ok(Value::string(
            obj.call_method0("isoformat")?.extract::<String>()?,
        ));
    }
    if obj.is_instance_of::<PyDelta>() {
        return Ok(Value::string(delta_text(obj)?));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut pairs = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let key = match k.cast::<PyString>() {
                Ok(s) => s.to_str()?.to_owned(),
                Err(_) => k.str()?.to_str()?.to_owned(),
            };
            pairs.push((Cow::Owned(key), extract_value(py, &v, depth + 1)?));
        }
        return Ok(Value::Object(pairs));
    }
    if let Ok(l) = obj.cast::<PyList>() {
        return collect_array(py, l.iter(), depth);
    }
    if let Ok(t) = obj.cast::<PyTuple>() {
        return collect_array(py, t.iter(), depth);
    }
    if let Ok(s) = obj.cast::<PySet>() {
        return collect_array(py, s.iter(), depth);
    }
    if let Ok(s) = obj.cast::<PyFrozenSet>() {
        return collect_array(py, s.iter(), depth);
    }
    // Non-builtin scalar types via lazy class checks.
    if obj.is_instance(cached(py, &DECIMAL, "decimal", "Decimal")?.bind(py))? {
        return Ok(Value::string(obj.str()?.to_str()?.to_owned()));
    }
    if obj.is_instance(cached(py, &UUID_CLS, "uuid", "UUID")?.bind(py))? {
        return Ok(Value::string(obj.str()?.to_str()?.to_owned()));
    }
    if obj.is_instance(cached(py, &IPV4, "ipaddress", "IPv4Address")?.bind(py))?
        || obj.is_instance(cached(py, &IPV6, "ipaddress", "IPv6Address")?.bind(py))?
    {
        return Ok(Value::string(obj.str()?.to_str()?.to_owned()));
    }
    if obj.is_instance(cached(py, &ENUM, "enum", "Enum")?.bind(py))? {
        return extract_value(py, &obj.getattr("value")?, depth + 1);
    }
    // Model and dataclass instances expose their declared state.
    let ty = obj.get_type();
    if (ty.hasattr("__fulmen_model__")? || ty.hasattr("__dataclass_fields__")?)
        && let Ok(state) = obj.getattr("__dict__")
    {
        return extract_value(py, &state, depth + 1);
    }
    if ty.hasattr("__dataclass_fields__")? {
        // Slotted dataclass — no __dict__; read each declared field.
        let fields = ty.getattr("__dataclass_fields__")?;
        let mut pairs = Vec::new();
        for item in fields.call_method0("keys")?.try_iter()? {
            let name = item?.extract::<String>()?;
            pairs.push((
                Cow::Owned(name.clone()),
                extract_value(py, &obj.getattr(&name)?, depth + 1)?,
            ));
        }
        return Ok(Value::Object(pairs));
    }
    Err(PyTypeError::new_err(format!(
        "cannot use a {} as validation input",
        ty.name()?
    )))
}

fn collect_array<'py>(
    py: Python<'py>,
    it: impl Iterator<Item = Bound<'py, PyAny>>,
    depth: usize,
) -> PyResult<Value<'static>> {
    let mut items = Vec::new();
    for item in it {
        items.push(extract_value(py, &item, depth + 1)?);
    }
    Ok(Value::Array(items))
}

// ---- Value → Python object ---------------------------------------------------

/// A borrowed view of the materialization context.
struct MatCtx<'a> {
    callbacks: &'a [Py<PyAny>],
    classes: &'a Mutex<HashMap<Box<str>, Py<PyAny>>>,
}

type Scope<'a> = HashMap<&'a str, &'a TypeDesc>;

/// Import `module` and walk a dotted qualname to the class. Cached per
/// descriptor — class identity is stable for a process.
fn resolve_class<'py>(
    py: Python<'py>,
    ctx: &MatCtx<'_>,
    cls: &ClassRef,
) -> PyResult<Bound<'py, PyAny>> {
    let key = format!("{}.{}", cls.module, cls.name);
    let mut cache = match ctx.classes.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(c) = cache.get(key.as_str()) {
        return Ok(c.bind(py).clone());
    }
    let mut obj: Bound<'_, PyAny> = py
        .import("importlib")?
        .call_method1("import_module", (&*cls.module,))?;
    for part in cls.name.split('.') {
        obj = obj.getattr(part)?;
    }
    let owned = obj.clone().unbind();
    cache.insert(key.into_boxed_str(), owned);
    Ok(obj)
}

/// `desc`/`value`-agnostic materialization for `Any`/`Raw` and record extras.
/// `pub(crate)` for the extraction path's descriptor-free values.
pub(crate) fn value_to_py(py: Python<'_>, value: &Value<'_>) -> PyResult<Py<PyAny>> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().unbind().into_any()),
        Value::Int(i) => Ok(i.into_pyobject(py)?.unbind().into_any()),
        Value::UInt(u) => Ok(u.into_pyobject(py)?.unbind().into_any()),
        Value::BigInt(t) => {
            let int_cls = cached(py, &BUILTINS_INT, "builtins", "int")?;
            Ok(int_cls.bind(py).call1((&**t,))?.unbind())
        }
        Value::Float(f) => Ok(f.into_pyobject(py)?.unbind().into_any()),
        Value::Str(s) => Ok(PyString::new(py, s).unbind().into_any()),
        Value::Bytes(b) => Ok(PyBytes::new(py, b).unbind().into_any()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(value_to_py(py, v)?);
            }
            Ok(PyList::new(py, out)?.unbind().into_any())
        }
        Value::Object(pairs) => {
            let d = PyDict::new(py);
            for (k, v) in pairs {
                d.set_item(&**k, value_to_py(py, v)?)?;
            }
            Ok(d.unbind().into_any())
        }
    }
}

fn call1<'py>(
    py: Python<'py>,
    cell: &PyOnceLock<Py<PyAny>>,
    module: &str,
    name: &str,
    arg: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let cls = cached(py, cell, module, name)?;
    cls.bind(py).call1((arg,))
}

/// Materialize a validated [`Value`] into the Python object `desc` declares.
/// After-validator hooks (`meta.after`) run on the materialized object.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per `Kind` — the materialization table mirrors `desc::Kind` 1:1"
)]
#[expect(
    clippy::cast_precision_loss,
    reason = "int→f64 coercion is the defined `float` materialization (desc.rs documents the >2^53 rounding)"
)]
fn materialize<'a>(
    py: Python<'a>,
    ctx: &MatCtx<'_>,
    desc: &'a TypeDesc,
    scope: &Scope<'a>,
    value: &Value<'_>,
) -> PyResult<Py<PyAny>> {
    let obj = match &desc.kind {
        Kind::Bool => match value {
            Value::Bool(b) => PyBool::new(py, *b).to_owned().unbind().into_any(),
            other => return Err(internal_shape("bool", other)),
        },
        Kind::Int(_) => match value {
            Value::Int(i) => i.into_pyobject(py)?.unbind().into_any(),
            Value::UInt(u) => u.into_pyobject(py)?.unbind().into_any(),
            Value::BigInt(t) => cached(py, &BUILTINS_INT, "builtins", "int")?
                .bind(py)
                .call1((&**t,))?
                .unbind(),
            other => return Err(internal_shape("int", other)),
        },
        Kind::Float(_) => match value {
            Value::Float(f) => f.into_pyobject(py)?.unbind().into_any(),
            Value::Int(i) => (*i as f64).into_pyobject(py)?.unbind().into_any(),
            Value::UInt(u) => (*u as f64).into_pyobject(py)?.unbind().into_any(),
            Value::BigInt(t) => t
                .parse::<f64>()
                .unwrap_or(f64::NAN)
                .into_pyobject(py)?
                .unbind()
                .into_any(),
            other => return Err(internal_shape("float", other)),
        },
        Kind::Str(_) | Kind::Any | Kind::Raw | Kind::Literal(_) | Kind::NewType(_) => {
            value_to_py(py, value)?
        }
        Kind::Bytes(_) => match value {
            Value::Bytes(b) => PyBytes::new(py, b).unbind().into_any(),
            other => return Err(internal_shape("bytes", other)),
        },
        Kind::Null => py.None(),
        Kind::Decimal(_) => match value {
            Value::Str(s) => call1(py, &DECIMAL, "decimal", "Decimal", s)?.unbind(),
            other => return Err(internal_shape("decimal text", other)),
        },
        Kind::DateTime => match value {
            Value::Str(s) => {
                let cls = cached(py, &DATETIME, "datetime", "datetime")?;
                cls.bind(py)
                    .call_method1("fromisoformat", (&**s,))?
                    .unbind()
            }
            other => return Err(internal_shape("datetime text", other)),
        },
        Kind::Date => match value {
            Value::Str(s) => {
                let cls = cached(py, &DATE, "datetime", "date")?;
                cls.bind(py)
                    .call_method1("fromisoformat", (&**s,))?
                    .unbind()
            }
            other => return Err(internal_shape("date text", other)),
        },
        Kind::Time => match value {
            Value::Str(s) => {
                let cls = cached(py, &TIME, "datetime", "time")?;
                cls.bind(py)
                    .call_method1("fromisoformat", (&**s,))?
                    .unbind()
            }
            other => return Err(internal_shape("time text", other)),
        },
        Kind::TimeDelta => match value {
            Value::Str(s) => {
                let td = cached(py, &TIMEDELTA, "datetime", "timedelta")?;
                match fulmen_schema::parse::parse_iso8601_duration(s) {
                    Some(p) => {
                        // `timedelta` range-checks its arguments itself;
                        // clamping only silences the u64→i64 wrap lint.
                        let (d, s_, m) = (
                            i64::try_from(p.days).unwrap_or(i64::MAX),
                            i64::try_from(p.seconds).unwrap_or(i64::MAX),
                            i64::try_from(p.micros).unwrap_or(i64::MAX),
                        );
                        let (d, s_, m) = if p.negative {
                            (-d, -s_, -m)
                        } else {
                            (d, s_, m)
                        };
                        let kwargs = PyDict::new(py);
                        kwargs.set_item("days", d)?;
                        kwargs.set_item("seconds", s_)?;
                        kwargs.set_item("microseconds", m)?;
                        td.bind(py).call((), Some(&kwargs))?.unbind()
                    }
                    None => return Err(PyValueError::new_err(format!("bad duration text {s:?}"))),
                }
            }
            other => return Err(internal_shape("timedelta text", other)),
        },
        Kind::Uuid => match value {
            Value::Str(s) => call1(py, &UUID_CLS, "uuid", "UUID", s)?.unbind(),
            other => return Err(internal_shape("uuid text", other)),
        },
        Kind::Email => match value {
            Value::Str(s) => call1(py, &EMAIL, "fulmen.types", "Email", s)?.unbind(),
            other => return Err(internal_shape("email text", other)),
        },
        Kind::Url => match value {
            Value::Str(s) => call1(py, &URL, "fulmen.types", "Url", s)?.unbind(),
            other => return Err(internal_shape("url text", other)),
        },
        Kind::Ipv4 => match value {
            Value::Str(s) => call1(py, &IPV4, "ipaddress", "IPv4Address", s)?.unbind(),
            other => return Err(internal_shape("ipv4 text", other)),
        },
        Kind::Ipv6 => match value {
            Value::Str(s) => call1(py, &IPV6, "ipaddress", "IPv6Address", s)?.unbind(),
            other => return Err(internal_shape("ipv6 text", other)),
        },
        Kind::List(d) => match value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items {
                    out.push(materialize(py, ctx, &d.item, scope, v)?);
                }
                PyList::new(py, out)?.unbind().into_any()
            }
            other => return Err(internal_shape("array", other)),
        },
        Kind::Set(d) | Kind::FrozenSet(d) => match value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items {
                    out.push(materialize(py, ctx, &d.item, scope, v)?);
                }
                let set = PySet::new(py, out.iter())?;
                if matches!(desc.kind, Kind::FrozenSet(_)) {
                    PyFrozenSet::new(py, out.iter())?.unbind().into_any()
                } else {
                    set.unbind().into_any()
                }
            }
            other => return Err(internal_shape("array", other)),
        },
        Kind::TupleVar(d) => match value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items {
                    out.push(materialize(py, ctx, &d.item, scope, v)?);
                }
                PyTuple::new(py, out)?.unbind().into_any()
            }
            other => return Err(internal_shape("array", other)),
        },
        Kind::TupleFixed(d) => match value {
            Value::Array(items) if items.len() == d.items.len() => {
                let mut out = Vec::with_capacity(items.len());
                for (v, item_desc) in items.iter().zip(d.items.iter()) {
                    out.push(materialize(py, ctx, item_desc, scope, v)?);
                }
                PyTuple::new(py, out)?.unbind().into_any()
            }
            other => return Err(internal_shape("fixed tuple", other)),
        },
        Kind::Dict(d) => match value {
            Value::Object(pairs) => {
                let out = PyDict::new(py);
                for (k, v) in pairs {
                    let key = materialize_key(py, ctx, &d.key, scope, k)?;
                    out.set_item(key, materialize(py, ctx, &d.value, scope, v)?)?;
                }
                out.unbind().into_any()
            }
            other => return Err(internal_shape("object", other)),
        },
        Kind::Enum(d) => {
            let cls = resolve_class(py, ctx, &d.class)?;
            cls.call1((value_to_py(py, value)?,))?.unbind()
        }
        Kind::Optional(d) => match value {
            Value::Null => py.None(),
            other => materialize(py, ctx, &d.inner, scope, other)?,
        },
        Kind::Union(d) => materialize_union(py, ctx, d, scope, value)?,
        Kind::Record(d) => materialize_record(py, ctx, desc, d, scope, value)?,
        Kind::Generic(g) => {
            // Bind the origin's type params to this node's args and recurse
            // — `TypeVar` lookups below resolve through `scope`.
            let mut child: Scope<'_> = scope.clone();
            if let Kind::Record(rec) = &g.origin.kind {
                for (name, arg) in rec.type_params.iter().zip(g.args.iter()) {
                    child.insert(&**name, arg);
                }
            }
            materialize(py, ctx, &g.origin, &child, value)?
        }
        Kind::TypeVar(name) => match scope.get(&**name) {
            Some(target) => materialize(py, ctx, target, scope, value)?,
            None => value_to_py(py, value)?,
        },
    };
    run_after_hooks(py, ctx, desc.meta.as_deref(), obj)
}

fn internal_shape(expected: &str, got: &Value<'_>) -> PyErr {
    PyTypeError::new_err(format!(
        "validator emitted a {expected}-shaped node but produced {} — internal error",
        got.type_name()
    ))
}

fn run_after_hooks(
    py: Python<'_>,
    ctx: &MatCtx<'_>,
    meta: Option<&TypeMeta>,
    mut obj: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let Some(meta) = meta else { return Ok(obj) };
    for id in &meta.after {
        let Some(cb) = ctx.callbacks.get(*id as usize) else {
            return Err(PyTypeError::new_err(format!(
                "after-validator callback id {id} out of range"
            )));
        };
        obj = cb.bind(py).call1((obj,))?.unbind();
    }
    Ok(obj)
}

/// `Union` materialization: tagged unions resolve through the tag field;
/// untagged pick the first variant whose shape fits (records: the best
/// field-name coverage — the same "smart union" rule pydantic documents).
fn materialize_union<'a>(
    py: Python<'a>,
    ctx: &MatCtx<'_>,
    d: &'a UnionDesc,
    scope: &Scope<'a>,
    value: &Value<'_>,
) -> PyResult<Py<PyAny>> {
    if let Some(tag) = &d.tag
        && let Value::Object(pairs) = value
    {
        let tag_val = pairs
            .iter()
            .find(|(k, _)| **k == **tag)
            .and_then(|(_, v)| v.as_str().map(str::to_owned));
        if let Some(tag_val) = tag_val {
            for v in &d.variants {
                let Some(rec) = record_of(v) else { continue };
                let matches = rec.fields.iter().any(|f| {
                    *f.name == **tag
                        && matches!(&f.ty.kind, Kind::Literal(l)
                                if l.members.iter().any(|m| literal_text(m) == tag_val))
                });
                if matches {
                    return materialize(py, ctx, v, scope, value);
                }
            }
        }
    }
    // Untagged: for objects, score records by field-name coverage; for
    // scalars pick the first shape-compatible variant.
    let mut best: Option<(&TypeDesc, usize)> = None;
    for v in &d.variants {
        let fit = variant_fit(v, value);
        match (best, fit) {
            (_, Some(s)) if best.is_none() || s > best.map_or(0, |(_, bs)| bs) => {
                best = Some((v, s));
            }
            _ => {}
        }
        if matches!(fit, Some(usize::MAX)) {
            break;
        }
    }
    match best {
        Some((v, _)) => materialize(py, ctx, v, scope, value),
        None => value_to_py(py, value),
    }
}

/// The [`RecordDesc`] inside a `Record` (or `Generic` origin) node.
/// `pub(crate)` — the response encoder resolves generic bindings and
/// union tags through it.
pub(crate) fn record_of(desc: &TypeDesc) -> Option<&RecordDesc> {
    match &desc.kind {
        Kind::Record(r) => Some(r),
        Kind::Generic(g) => record_of(&g.origin),
        _ => None,
    }
}

fn literal_text(l: &LiteralValue) -> String {
    match l {
        LiteralValue::Bool(b) => b.to_string(),
        LiteralValue::Int(i) => i.to_string(),
        LiteralValue::UInt(u) => u.to_string(),
        LiteralValue::BigInt(t) => t.to_string(),
        LiteralValue::Float(f) => format!("{f:?}"),
        LiteralValue::Str(s) => s.to_string(),
        LiteralValue::Null | LiteralValue::Bytes(_) => String::new(),
    }
}

/// Shallow "could this variant have produced this validated value" check.
/// `usize::MAX` = exact-shape record/scalar match.
fn variant_fit(desc: &TypeDesc, value: &Value<'_>) -> Option<usize> {
    let fits = match (&desc.kind, value) {
        (Kind::Bool, Value::Bool(_))
        | (Kind::Int(_), Value::Int(_) | Value::UInt(_) | Value::BigInt(_))
        | (Kind::Str(_), Value::Str(_))
        | (Kind::Bytes(_), Value::Bytes(_))
        | (Kind::Null | Kind::Optional(_), Value::Null) => usize::MAX,
        (Kind::Float(_), Value::Float(_) | Value::Int(_) | Value::UInt(_) | Value::BigInt(_))
        | (
            Kind::Decimal(_)
            | Kind::DateTime
            | Kind::Date
            | Kind::Time
            | Kind::TimeDelta
            | Kind::Uuid
            | Kind::Email
            | Kind::Url
            | Kind::Ipv4
            | Kind::Ipv6
            | Kind::Literal(_)
            | Kind::Enum(_),
            Value::Str(_),
        )
        | (Kind::Enum(_) | Kind::Literal(_), Value::Int(_) | Value::Bool(_))
        | (
            Kind::List(_)
            | Kind::Set(_)
            | Kind::FrozenSet(_)
            | Kind::TupleVar(_)
            | Kind::TupleFixed(_),
            Value::Array(_),
        )
        | (Kind::Dict(_), Value::Object(_)) => 2,
        (Kind::Optional(inner), v) => variant_fit(&inner.inner, v).map(|s| s + 1)?,
        (Kind::NewType(n), v) => variant_fit(&n.inner, v).map(|s| s + 1)?,
        (Kind::Record(rec), Value::Object(pairs)) => {
            let mut cover = 0usize;
            for (k, _) in pairs {
                if rec.fields.iter().any(|f| *f.name == **k) {
                    cover += 1;
                }
            }
            Some(cover).filter(|_| {
                rec.fields.iter().all(|f| {
                    f.missing != Missing::Required || pairs.iter().any(|(k, _)| **k == *f.name)
                })
            })?;
            cover
        }
        (Kind::Generic(g), v) => variant_fit(&g.origin, v).map(|s| s + 1)?,
        (Kind::TypeVar(_) | Kind::Any | Kind::Raw, _) => 1,
        _ => return None,
    };
    Some(fits)
}

/// Canonical dict-key text → the key type `desc` declares (the output of
/// dict validation stores canonical text keys).
fn materialize_key<'a>(
    py: Python<'a>,
    ctx: &MatCtx<'_>,
    desc: &'a TypeDesc,
    scope: &Scope<'a>,
    text: &str,
) -> PyResult<Py<PyAny>> {
    let obj = match &desc.kind {
        Kind::Int(_) => match text.parse::<i64>() {
            Ok(i) => i.into_pyobject(py)?.unbind().into_any(),
            Err(_) => match text.parse::<u64>() {
                Ok(u) => u.into_pyobject(py)?.unbind().into_any(),
                Err(_) => cached(py, &BUILTINS_INT, "builtins", "int")?
                    .bind(py)
                    .call1((text,))?
                    .unbind(),
            },
        },
        Kind::Float(_) => text
            .parse::<f64>()
            .unwrap_or(f64::NAN)
            .into_pyobject(py)?
            .unbind()
            .into_any(),
        Kind::Bool => match text {
            "true" => PyBool::new(py, true).to_owned().unbind().into_any(),
            _ => PyBool::new(py, false).to_owned().unbind().into_any(),
        },
        Kind::Decimal(_) => call1(py, &DECIMAL, "decimal", "Decimal", text)?.unbind(),
        Kind::DateTime => cached(py, &DATETIME, "datetime", "datetime")?
            .bind(py)
            .call_method1("fromisoformat", (text,))?
            .unbind(),
        Kind::Date => cached(py, &DATE, "datetime", "date")?
            .bind(py)
            .call_method1("fromisoformat", (text,))?
            .unbind(),
        Kind::Time => cached(py, &TIME, "datetime", "time")?
            .bind(py)
            .call_method1("fromisoformat", (text,))?
            .unbind(),
        Kind::Uuid => call1(py, &UUID_CLS, "uuid", "UUID", text)?.unbind(),
        Kind::Email => call1(py, &EMAIL, "fulmen.types", "Email", text)?.unbind(),
        Kind::Url => call1(py, &URL, "fulmen.types", "Url", text)?.unbind(),
        Kind::Ipv4 => call1(py, &IPV4, "ipaddress", "IPv4Address", text)?.unbind(),
        Kind::Ipv6 => call1(py, &IPV6, "ipaddress", "IPv6Address", text)?.unbind(),
        Kind::Enum(d) => {
            let cls = resolve_class(py, ctx, &d.class)?;
            let member = d
                .members
                .iter()
                .find(|m| literal_text(&m.value) == text)
                .map(|m| m.value.clone());
            match member {
                Some(v) => cls.call1((literal_to_py(py, &v)?,))?.unbind(),
                None => PyString::new(py, text).unbind().into_any(),
            }
        }
        Kind::Literal(l) => {
            let member = l.members.iter().find(|m| literal_text(m) == text).cloned();
            match member {
                Some(v) => literal_to_py(py, &v)?,
                None => PyString::new(py, text).unbind().into_any(),
            }
        }
        Kind::NewType(n) => materialize_key(py, ctx, &n.inner, scope, text)?,
        Kind::Optional(d) if text.is_empty() => {
            let _ = d;
            py.None()
        }
        Kind::Optional(d) => materialize_key(py, ctx, &d.inner, scope, text)?,
        Kind::Union(d) => {
            let mut out = None;
            for v in &d.variants {
                if let Ok(o) = materialize_key(py, ctx, v, scope, text) {
                    out = Some(o);
                    break;
                }
            }
            out.unwrap_or_else(|| PyString::new(py, text).unbind().into_any())
        }
        Kind::Generic(g) => materialize_key(py, ctx, &g.origin, scope, text)?,
        Kind::TypeVar(name) => match scope.get(&**name) {
            Some(target) => materialize_key(py, ctx, target, scope, text)?,
            None => PyString::new(py, text).unbind().into_any(),
        },
        _ => PyString::new(py, text).unbind().into_any(),
    };
    Ok(obj)
}

fn literal_to_py(py: Python<'_>, lit: &LiteralValue) -> PyResult<Py<PyAny>> {
    match lit {
        LiteralValue::Null => Ok(py.None()),
        LiteralValue::Bool(b) => Ok(PyBool::new(py, *b).to_owned().unbind().into_any()),
        LiteralValue::Int(i) => Ok(i.into_pyobject(py)?.unbind().into_any()),
        LiteralValue::UInt(u) => Ok(u.into_pyobject(py)?.unbind().into_any()),
        LiteralValue::BigInt(t) => Ok(cached(py, &BUILTINS_INT, "builtins", "int")?
            .bind(py)
            .call1((&**t,))?
            .unbind()),
        LiteralValue::Float(f) => Ok(f.into_pyobject(py)?.unbind().into_any()),
        LiteralValue::Str(s) => Ok(PyString::new(py, s).unbind().into_any()),
        LiteralValue::Bytes(b) => Ok(PyBytes::new(py, b).unbind().into_any()),
    }
}

/// `Record` materialization — `Model`/`dataclass` → class instance,
/// `TypedDict` → dict. Class construction uses `__new__` +
/// `object.__setattr__` for `Model` (validated values, no re-check) and a
/// plain `cls(**fields)` for dataclasses (lets `__post_init__` run).
fn materialize_record<'a>(
    py: Python<'a>,
    ctx: &MatCtx<'_>,
    desc: &'a TypeDesc,
    rec: &'a RecordDesc,
    scope: &Scope<'a>,
    value: &Value<'_>,
) -> PyResult<Py<PyAny>> {
    let Value::Object(pairs) = value else {
        return Err(internal_shape("object", value));
    };
    let field_for = |name: &str| rec.fields.iter().find(|f| &*f.name == name);
    match rec.kind {
        RecordKind::TypedDict => {
            let out = PyDict::new(py);
            for (k, v) in pairs {
                let val = match field_for(k) {
                    Some(f) => run_after_hooks(
                        py,
                        ctx,
                        f.meta.as_deref(),
                        materialize(py, ctx, &f.ty, scope, v)?,
                    )?,
                    None => value_to_py(py, v)?,
                };
                out.set_item(&**k, val)?;
            }
            Ok(out.unbind().into_any())
        }
        RecordKind::Dataclass => {
            let cls = resolve_class(py, ctx, &rec.class)?;
            let kwargs = PyDict::new(py);
            let mut extras: Vec<(Bound<'_, PyAny>, Bound<'_, PyAny>)> = Vec::new();
            for (k, v) in pairs {
                match field_for(k) {
                    Some(f) => {
                        let val = run_after_hooks(
                            py,
                            ctx,
                            f.meta.as_deref(),
                            materialize(py, ctx, &f.ty, scope, v)?,
                        )?;
                        kwargs.set_item(&**k, val)?;
                    }
                    None => extras.push((
                        PyString::new(py, k).into_any(),
                        value_to_py(py, v)?.bind(py).clone(),
                    )),
                }
            }
            let obj = cls.call((), Some(&kwargs))?;
            let set_ = object_setattr(py)?;
            for (k, v) in extras {
                set_.bind(py).call1((&obj, k, v))?;
            }
            run_after_hooks(py, ctx, desc.meta.as_deref(), obj.unbind())
        }
        RecordKind::Model => {
            let cls = resolve_class(py, ctx, &rec.class)?;
            let obj = cls.getattr("__new__")?.call1((cls.clone(),))?;
            let set_ = object_setattr(py)?;
            for (k, v) in pairs {
                let val = match field_for(k) {
                    Some(f) => run_after_hooks(
                        py,
                        ctx,
                        f.meta.as_deref(),
                        materialize(py, ctx, &f.ty, scope, v)?,
                    )?,
                    None => value_to_py(py, v)?,
                };
                set_.bind(py).call1((&obj, &**k, val))?;
            }
            // Bridge defaults (default_factory) for keys the validator left out.
            for f in &rec.fields {
                if let Missing::Bridge(id) = f.missing
                    && pairs.iter().all(|(k, _)| **k != *f.name)
                {
                    let Some(cb) = ctx.callbacks.get(id as usize) else {
                        return Err(PyTypeError::new_err(format!(
                            "default-factory callback id {id} out of range"
                        )));
                    };
                    let produced = cb.bind(py).call0()?;
                    set_.bind(py).call1((&obj, &*f.name, produced))?;
                }
            }
            run_after_hooks(py, ctx, desc.meta.as_deref(), obj.unbind())
        }
    }
}

// ---- ValidationError plumbing --------------------------------------------------

fn entry_dict<'a>(py: Python<'a>, e: &ErrorEntry) -> PyResult<Bound<'a, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("loc", e.pointer())?;
    d.set_item("code", e.code.as_str())?;
    d.set_item("message", &e.message)?;
    d.set_item("source", e.source.as_str())?;
    if let Some(l) = &e.limit {
        d.set_item("limit", literal_to_py(py, l)?)?;
    }
    Ok(d)
}

/// Build and raise `fulmen.errors.ValidationError(entries, truncated)`.
pub(crate) fn validation_error(py: Python<'_>, errors: &ValidationErrors) -> PyErr {
    let build = || -> PyResult<Py<PyAny>> {
        let cls = validation_error_class(py)?;
        let entries = PyList::empty(py);
        for e in errors.entries() {
            entries.append(entry_dict(py, e)?)?;
        }
        cls.bind(py)
            .call1((entries, errors.truncated()))
            .map(Bound::unbind)
    };
    match build() {
        Ok(inst) => PyErr::from_value(inst.into_bound(py)),
        Err(e) => e,
    }
}

fn source_from(text: &str) -> Option<Source> {
    match text {
        "body" => Some(Source::Body),
        "query" => Some(Source::Query),
        "path" => Some(Source::Path),
        "header" => Some(Source::Header),
        "cookie" => Some(Source::Cookie),
        "form" => Some(Source::Form),
        "response" => Some(Source::Response),
        _ => None,
    }
}

// ---- the Descriptor pyclass ---------------------------------------------------

/// Per-field validator for `validate_field` (Model `__setattr__`).
struct FieldSlot {
    validator: Validator,
    desc: TypeDesc,
}

/// A compiled spec tree: the `Validator` plus the `TypeDesc` it came from
/// (`OpenAPI` and the extraction task read descriptor metadata through it).
#[pyclass(frozen)]
pub(crate) struct Descriptor {
    /// `Arc` so the dispatch path can validate request parameters without
    /// touching the GIL (`fulmen-py::extract`); `Validator` is `Send`/`Sync`.
    validator: Arc<Validator>,
    desc: TypeDesc,
    spec: Py<PyAny>,
    callbacks: Vec<Py<PyAny>>,
    fields: HashMap<Box<str>, FieldSlot>,
    record: Option<RecordDesc>,
    classes: Mutex<HashMap<Box<str>, Py<PyAny>>>,
}

impl Descriptor {
    fn mat_ctx(&self) -> MatCtx<'_> {
        MatCtx {
            callbacks: &self.callbacks,
            classes: &self.classes,
        }
    }

    /// Validate `obj` through this descriptor's compiled validator and
    /// materialize the result. `pub(crate)` for the deferred call-time
    /// validation of extracted parameters.
    pub(crate) fn do_validate(
        &self,
        py: Python<'_>,
        obj: &Bound<'_, PyAny>,
        source: Source,
        lax: bool,
    ) -> PyResult<Py<PyAny>> {
        let value = extract_value(py, obj, 0)?;
        let mode = if lax { Mode::Lax } else { Mode::Strict };
        let out = py
            .detach(|| self.validator.validate(&value, source, mode))
            .map_err(|e| validation_error(py, &e))?;
        let scope = Scope::new();
        materialize(py, &self.mat_ctx(), &self.desc, &scope, &out)
    }

    /// The compiled validator, shared with the GIL-free extraction path.
    pub(crate) fn validator_arc(&self) -> Arc<Validator> {
        Arc::clone(&self.validator)
    }

    /// The descriptor's `TypeDesc` (for kind checks at extraction).
    pub(crate) fn type_desc(&self) -> &TypeDesc {
        &self.desc
    }

    /// The `default_factory` callback table (`Missing::Bridge` handles
    /// index into it — output encoding and materialization share it).
    pub(crate) fn callbacks(&self) -> &[Py<PyAny>] {
        &self.callbacks
    }

    /// Materialize an *already validated* `Value` — the extraction path
    /// validates at dispatch time and only materialization (rich types,
    /// `AfterValidator` hooks) needs the GIL.
    pub(crate) fn do_materialize(&self, py: Python<'_>, value: &Value<'_>) -> PyResult<Py<PyAny>> {
        let scope = Scope::new();
        materialize(py, &self.mat_ctx(), &self.desc, &scope, value)
    }
}

#[pymethods]
impl Descriptor {
    /// The spec dict this descriptor was built from — the contract the
    /// `OpenAPI` and param-extraction tasks read (docs/SPEC-FORMAT.md).
    #[getter]
    fn spec(&self, py: Python<'_>) -> Py<PyAny> {
        self.spec.clone_ref(py)
    }

    /// `describe()` — same as `.spec`, kept as a method for consumers that
    /// prefer a call.
    fn describe(&self, py: Python<'_>) -> Py<PyAny> {
        self.spec.clone_ref(py)
    }

    /// `validate(obj, *, source="body", lax=False)` — strict-mode default.
    /// Returns the materialized Python object; raises
    /// `fulmen.errors.ValidationError` carrying `entries` + `truncated`.
    #[pyo3(signature = (obj, *, source = "body", lax = false))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pymethod arguments are extracted by value — `Bound` is the pyo3 convention"
    )]
    fn validate(
        &self,
        py: Python<'_>,
        obj: Bound<'_, PyAny>,
        source: &str,
        lax: bool,
    ) -> PyResult<Py<PyAny>> {
        let Some(src) = source_from(source) else {
            return Err(PyValueError::new_err(format!(
                "unknown source {source:?} — expected body/query/path/header/cookie/form/response"
            )));
        };
        self.do_validate(py, &obj, src, lax)
    }

    /// `construct(instance, data, *, lax=False)` — validate `data` (a
    /// mapping) against this record descriptor and `object.__setattr__`
    /// every emitted field onto `instance`. The native half of
    /// `Model.__init__`.
    #[pyo3(signature = (instance, data, *, lax = false))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pymethod arguments are extracted by value — `Bound` is the pyo3 convention"
    )]
    fn construct(
        &self,
        py: Python<'_>,
        instance: Bound<'_, PyAny>,
        data: Bound<'_, PyAny>,
        lax: bool,
    ) -> PyResult<()> {
        let Some(rec) = &self.record else {
            return Err(PyTypeError::new_err(
                "construct() requires a record descriptor",
            ));
        };
        let value = extract_value(py, &data, 0)?;
        if !matches!(value, Value::Object(_)) {
            return Err(PyTypeError::new_err("construct() data must be a mapping"));
        }
        let mode = if lax { Mode::Lax } else { Mode::Strict };
        let out = py
            .detach(|| self.validator.validate(&value, Source::Body, mode))
            .map_err(|e| validation_error(py, &e))?;
        let Value::Object(pairs) = &*out else {
            return Err(PyTypeError::new_err(
                "record validator emitted a non-object — internal error",
            ));
        };
        let set_ = object_setattr(py)?;
        let ctx = self.mat_ctx();
        let scope = Scope::new();
        for (k, v) in pairs {
            let val = match rec.fields.iter().find(|f| *f.name == **k) {
                Some(f) => run_after_hooks(
                    py,
                    &ctx,
                    f.meta.as_deref(),
                    materialize(py, &ctx, &f.ty, &scope, v)?,
                )?,
                None => value_to_py(py, v)?,
            };
            set_.bind(py).call1((&instance, &**k, val))?;
        }
        for f in &rec.fields {
            if let Missing::Bridge(id) = f.missing
                && pairs.iter().all(|(k, _)| **k != *f.name)
            {
                let Some(cb) = self.callbacks.get(id as usize) else {
                    return Err(PyTypeError::new_err(format!(
                        "default-factory callback id {id} out of range"
                    )));
                };
                let produced = cb.bind(py).call0()?;
                set_.bind(py).call1((&instance, &*f.name, produced))?;
            }
        }
        Ok(())
    }

    /// `validate_field(name, value, *, lax=False)` — validate one declared
    /// record field; the native half of `Model.__setattr__`.
    #[pyo3(signature = (name, value, *, lax = false))]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pymethod arguments are extracted by value — `Bound` is the pyo3 convention"
    )]
    fn validate_field(
        &self,
        py: Python<'_>,
        name: &str,
        value: Bound<'_, PyAny>,
        lax: bool,
    ) -> PyResult<Py<PyAny>> {
        let Some(slot) = self.fields.get(name) else {
            return Err(PyKeyError::new_err(name.to_owned()));
        };
        let input = extract_value(py, &value, 0)?;
        let mode = if lax { Mode::Lax } else { Mode::Strict };
        let out = py
            .detach(|| slot.validator.validate(&input, Source::Body, mode))
            .map_err(|e| validation_error(py, &e))?;
        let scope = Scope::new();
        materialize(py, &self.mat_ctx(), &slot.desc, &scope, &out)
    }

    /// The declared field names, for `Model.__fields__` consumers.
    #[getter]
    fn field_names(&self, py: Python<'_>) -> Py<PyAny> {
        let names: Vec<&str> = self
            .record
            .as_ref()
            .map(|r| r.fields.iter().map(|f| &*f.name).collect())
            .unwrap_or_default();
        PyList::new(py, names).map_or_else(|_| py.None(), |l| l.unbind().into_any())
    }

    /// The declared record class, resolved lazily (None for non-records).
    #[getter]
    fn record_class(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.record {
            Some(rec) => Ok(resolve_class(py, &self.mat_ctx(), &rec.class)?.unbind()),
            None => Ok(py.None()),
        }
    }

    fn __repr__(&self) -> String {
        let what = match &self.desc.kind {
            Kind::Record(r) => format!("{}.{}", r.class.module, r.class.name),
            other => other.name(),
        };
        format!("Descriptor({what})")
    }
}

/// `compile_descriptor(spec, callbacks=None)` — build the `TypeDesc`, run
/// `fulmen_schema::compile`, and return the `Descriptor`. Spec-structure
/// problems and compile errors are reported together as `ValueError` lines.
#[pyfunction]
#[pyo3(signature = (spec, callbacks = None))]
pub(crate) fn compile_descriptor(
    spec: Bound<'_, PyDict>,
    callbacks: Option<Vec<Py<PyAny>>>,
) -> PyResult<Descriptor> {
    let callbacks = callbacks.unwrap_or_default();
    let spec_clone: Py<PyAny> = spec.clone().unbind().into_any();
    let mut errs = Vec::new();
    let Some(desc) = build_desc(&spec.into_any(), &mut errs, "$") else {
        errs.insert(0, "invalid descriptor spec:".to_owned());
        return Err(PyValueError::new_err(errs.join("\n  ")));
    };
    if !errs.is_empty() {
        errs.insert(0, "invalid descriptor spec:".to_owned());
        return Err(PyValueError::new_err(errs.join("\n  ")));
    }
    let validator = match compile(&desc) {
        Ok(v) => Arc::new(v),
        Err(ce) => {
            let mut msg = vec!["descriptor compile errors:".to_owned()];
            for e in ce.entries() {
                msg.push(format!("  {}: {}", e.path, e.message));
            }
            return Err(PyValueError::new_err(msg.join("\n")));
        }
    };
    // Per-field validators power `validate_field` (Model.__setattr__).
    let mut fields = HashMap::new();
    let mut record = None;
    if let Kind::Record(rec) = &desc.kind {
        for f in &rec.fields {
            if let Ok(v) = compile(&f.ty) {
                fields.insert(
                    f.name.clone(),
                    FieldSlot {
                        validator: v,
                        desc: f.ty.clone(),
                    },
                );
            }
        }
        record = Some(rec.clone());
    }
    Ok(Descriptor {
        validator,
        desc,
        spec: spec_clone,
        callbacks,
        fields,
        record,
        classes: Mutex::new(HashMap::new()),
    })
}
