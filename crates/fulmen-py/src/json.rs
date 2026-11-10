//! JSON support for handler return values (DESIGN.md §10.2).
//!
//! `dict`/`list`/`tuple` results are extracted into a [`JsonValue`] tree
//! while the GIL is held, then serialized to bytes without it. Extraction
//! supports nested `dict`/`list`/`tuple`, `str`, `int`, `float`, `bool` and
//! `None` (M1 subset); anything else is an unsupported return type.

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList, PyString, PyTuple};

/// Maximum nesting depth accepted when extracting a result tree. Matches the
/// JSON depth request limit (DESIGN.md §14.3); it also stops extraction on
/// self-referencing containers, which would otherwise recurse forever.
const MAX_DEPTH: usize = 64;

/// A JSON value extracted from a Python object, owned by Rust.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JsonValue {
    /// `None`.
    Null,
    /// `True` / `False`.
    Bool(bool),
    /// `int` within `i64` range.
    Int(i64),
    /// `int` within `u64` range but above `i64::MAX`.
    UInt(u64),
    /// `int` beyond `u64`: kept as its decimal text, which is a valid JSON
    /// number literal.
    BigInt(Box<str>),
    /// `float` (finite only; `NaN` and infinities are rejected at extract).
    Float(f64),
    /// `str`.
    Str(String),
    /// `list` / `tuple`.
    List(Vec<JsonValue>),
    /// `dict` with `str` keys, insertion order preserved.
    Dict(Vec<(String, JsonValue)>),
}

/// Why a value could not be extracted into [`JsonValue`].
#[derive(Debug)]
pub(crate) enum ExtractError {
    /// The object (or a nested member) is not JSON-serializable; carries the
    /// offending type name for the log message.
    Unsupported(String),
    /// A Python call during extraction raised.
    Python(PyErr),
}

impl From<PyErr> for ExtractError {
    fn from(err: PyErr) -> Self {
        Self::Python(err)
    }
}

/// A displayable type name for error messages.
fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .qualname()
        .map_or_else(|_| "<unknown>".to_owned(), |q| q.to_string())
}

/// Extract `obj` into a [`JsonValue`] tree, bounded by [`MAX_DEPTH`].
///
/// # Errors
/// [`ExtractError::Unsupported`] for non-serializable members, or
/// [`ExtractError::Python`] when a Python call fails.
pub(crate) fn extract(obj: &Bound<'_, PyAny>) -> Result<JsonValue, ExtractError> {
    extract_at(obj, 0)
}

fn extract_at(obj: &Bound<'_, PyAny>, depth: usize) -> Result<JsonValue, ExtractError> {
    if depth > MAX_DEPTH {
        return Err(ExtractError::Unsupported(format!(
            "value nested deeper than {MAX_DEPTH} levels"
        )));
    }
    if obj.is_none() {
        return Ok(JsonValue::Null);
    }
    // `bool` is a subclass of `int`: it must be checked first.
    if obj.is_instance_of::<PyBool>() {
        return Ok(JsonValue::Bool(obj.is_truthy()?));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(JsonValue::Int(i));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(JsonValue::UInt(u));
    }
    if obj.is_instance_of::<pyo3::types::PyInt>() {
        let repr = obj.str()?.to_str()?.to_owned();
        return Ok(JsonValue::BigInt(repr.into_boxed_str()));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return if f.is_finite() {
            Ok(JsonValue::Float(f))
        } else {
            Err(ExtractError::Unsupported(
                "non-finite float (NaN or infinity)".to_owned(),
            ))
        };
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(JsonValue::Str(s.to_str()?.to_owned()));
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        let mut pairs = Vec::with_capacity(d.len());
        for (key, value) in d.iter() {
            let key = match key.cast::<PyString>() {
                Ok(k) => k.to_str()?.to_owned(),
                Err(_) => {
                    return Err(ExtractError::Unsupported(format!(
                        "dict key of type `{}` (JSON keys must be str)",
                        type_name(&key)
                    )));
                }
            };
            pairs.push((key, extract_at(&value, depth + 1)?));
        }
        return Ok(JsonValue::Dict(pairs));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(extract_at(&item, depth + 1)?);
        }
        return Ok(JsonValue::List(items));
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        let mut items = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            items.push(extract_at(&item, depth + 1)?);
        }
        return Ok(JsonValue::List(items));
    }
    Err(ExtractError::Unsupported(type_name(obj)))
}

/// Serialize a [`JsonValue`] tree to UTF-8 JSON bytes. Never fails: every
/// `JsonValue` is already a valid JSON value.
pub(crate) fn write(value: &JsonValue) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut Vec<u8>, value: &JsonValue) {
    match value {
        JsonValue::Null => out.extend_from_slice(b"null"),
        JsonValue::Bool(true) => out.extend_from_slice(b"true"),
        JsonValue::Bool(false) => out.extend_from_slice(b"false"),
        JsonValue::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        JsonValue::UInt(u) => out.extend_from_slice(u.to_string().as_bytes()),
        JsonValue::BigInt(text) => out.extend_from_slice(text.as_bytes()),
        // `{:?}` always emits a JSON-valid float: decimal point or exponent,
        // shortest round-trip digits (e.g. `1.0`, `0.5`, `1e300`).
        JsonValue::Float(f) => out.extend_from_slice(format!("{f:?}").as_bytes()),
        JsonValue::Str(s) => write_str(out, s),
        JsonValue::List(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, item);
            }
            out.push(b']');
        }
        JsonValue::Dict(pairs) => {
            out.push(b'{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, key);
                out.push(b':');
                write_value(out, item);
            }
            out.push(b'}');
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for ch in s.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\x08' => out.extend_from_slice(b"\\b"),
            '\x0c' => out.extend_from_slice(b"\\f"),
            c if c < '\x20' => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_scalars_and_containers() {
        let v = JsonValue::Dict(vec![
            ("a".into(), JsonValue::Int(-3)),
            ("b".into(), JsonValue::UInt(u64::MAX)),
            ("c".into(), JsonValue::Float(1.0)),
            ("d".into(), JsonValue::Str("x\n\"y".into())),
            (
                "e".into(),
                JsonValue::List(vec![JsonValue::Null, JsonValue::Bool(true)]),
            ),
        ]);
        let text = String::from_utf8(write(&v)).unwrap();
        assert_eq!(
            text,
            r#"{"a":-3,"b":18446744073709551615,"c":1.0,"d":"x\n\"y","e":[null,true]}"#
        );
    }

    #[test]
    fn escapes_control_characters() {
        let v = JsonValue::Str("\u{1}\u{1f}\"".into());
        assert_eq!(String::from_utf8(write(&v)).unwrap(), r#""\u0001\u001f\"""#);
        let v = JsonValue::Str("é☃".into());
        assert_eq!(String::from_utf8(write(&v)).unwrap(), "\"é☃\"");
    }
}
