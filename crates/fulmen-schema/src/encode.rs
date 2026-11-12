//! The output half of §11.6 — serialize a [`Value`] to JSON bytes.
//!
//! The encoder in `fulmen-py` walks a Python object under its compiled
//! `TypeDesc` and produces a wire-shaped `Value` (record keys already
//! aliased/renamed, scalars already canonical). This module turns that
//! tree into the response body in one pass — no re-inspection.
//!
//! The tree is already the declared shape, so serialization cannot fail;
//! the only judgment call is `Bytes`, which JSON carries as a base64
//! string (the same rule `json::extract` applies in `fulmen-py`).

use crate::value::Value;
use std::io::Write as _;

/// Serialize `value` to its JSON byte form (DESIGN.md §11.6).
pub fn encode_json(value: &Value<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    write_json(&mut out, value);
    out
}

/// Append `value`'s JSON form to `out`.
pub fn write_json(out: &mut Vec<u8>, value: &Value<'_>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::UInt(u) => {
            let _ = write!(out, "{u}");
        }
        // BigInt literals are digit text — JSON numbers, unquoted.
        Value::BigInt(text) => out.extend_from_slice(text.as_bytes()),
        // JSON has no non-finite float; emit `null` rather than a token
        // a strict parser would reject (mirrors LiteralValue::to_json).
        Value::Float(f) => {
            if f.is_finite() {
                let _ = write!(out, "{f:?}");
            } else {
                out.extend_from_slice(b"null");
            }
        }
        Value::Str(s) => write_str(out, s),
        Value::Bytes(b) => write_str(out, &base64(b)),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json(out, item);
            }
            out.push(b']');
        }
        Value::Object(pairs) => {
            out.push(b'{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, key);
                out.push(b':');
                write_json(out, item);
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

/// RFC 4648 base64 (standard alphabet, padded).
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn text(v: &Value<'_>) -> String {
        String::from_utf8(encode_json(v)).expect("utf8")
    }

    #[test]
    fn scalars() {
        assert_eq!(text(&Value::Null), "null");
        assert_eq!(text(&Value::Bool(true)), "true");
        assert_eq!(text(&Value::Int(-7)), "-7");
        assert_eq!(text(&Value::UInt(7)), "7");
        assert_eq!(
            text(&Value::BigInt("123456789012345678901234567890".into())),
            "123456789012345678901234567890"
        );
        assert_eq!(text(&Value::Float(1.5)), "1.5");
        assert_eq!(text(&Value::Float(f64::NAN)), "null");
        assert_eq!(text(&Value::Float(f64::INFINITY)), "null");
    }

    #[test]
    fn strings_escape() {
        assert_eq!(text(&Value::str("a\"b\\c\nd")), "\"a\\\"b\\\\c\\nd\"");
        assert_eq!(text(&Value::str("héllo")), "\"héllo\"");
        // control chars escape; nothing else does
        assert_eq!(
            text(&Value::str("\u{0}\u{1f}\u{8}")),
            "\"\\u0000\\u001f\\b\""
        );
    }

    #[test]
    fn bytes_are_base64() {
        assert_eq!(text(&Value::Bytes(Cow::Borrowed(b""))), "\"\"");
        assert_eq!(text(&Value::Bytes(Cow::Borrowed(b"f"))), "\"Zg==\"");
        assert_eq!(text(&Value::Bytes(Cow::Borrowed(b"hi"))), "\"aGk=\"");
        assert_eq!(text(&Value::Bytes(Cow::Borrowed(b"hey"))), "\"aGV5\"");
        assert_eq!(text(&Value::Bytes(Cow::Borrowed(b"hello"))), "\"aGVsbG8=\"");
    }

    #[test]
    fn containers() {
        let v = Value::Object(
            vec![
                (Cow::Borrowed("name"), Value::str("x")),
                (
                    Cow::Borrowed("tags"),
                    Value::Array(vec![Value::Int(1), Value::Null]),
                ),
            ]
            .into(),
        );
        assert_eq!(text(&v), "{\"name\":\"x\",\"tags\":[1,null]}");
        assert_eq!(text(&Value::Array(vec![].into())), "[]");
        assert_eq!(text(&Value::Object(vec![].into())), "{}");
    }
}
