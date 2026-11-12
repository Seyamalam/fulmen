//! The input value model.
//!
//! [`Value`] is the boundary between decoding and validation (DESIGN.md
//! §11.3–§11.4). The JSON decoder (M2.6) produces `Value`s borrowing from
//! its parse buffer where possible; lax text sources (query, path, headers,
//! cookies, forms) produce them from their string segments. Validators
//! consume `&Value` and return either a borrow of the input (unchanged) or
//! an owned, coerced/canonicalized value — never a Python object, which is
//! what lets validation run with the GIL released.
//!
//! # Value semantics
//!
//! - Strings and bytes are [`Cow`]: decoders hand in borrowed slices of the
//!   request body; validators hand back owned data only when a transform or
//!   coercion changed the content.
//! - `Object` keeps pairs in input order and may contain duplicate keys —
//!   the schema decides how to treat them (a `dict` keeps the last pair,
//!   a `Record` consults its extra policy).
//! - Python `int` is unbounded, so integers are split three ways —
//!   [`Value::Int`] (`i64`), [`Value::UInt`] (`u64` above `i64::MAX`) and
//!   [`Value::BigInt`] (decimal text beyond `u64`), mirroring
//!   `fulmen-py`'s `JsonValue` so the encoder round-trips every JSON
//!   integer exactly.
//! - `Bytes` never comes out of a JSON decoder (JSON has no bytes literal);
//!   it exists for `Raw`/file sources and for `bytes` outputs.
//!
//! Equality is *type-aware*: `Int(1) != Float(1.0)` and `Int(1) != Bool(true)`
//! (both are `!=` in JSON semantics even though Python equates them), while
//! `Int`, `UInt` and `BigInt` compare numerically as one integer domain.
//! `Object` equality ignores pair order, matching Python `dict` semantics.
//! These rules drive `Literal` matching, enum lookup, `Unique` and set
//! deduplication.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::num::int_from_text;

/// A decoded input value, borrowed from the decoder's buffer where possible.
///
/// See the module docs for semantics. `Clone` is a shallow copy: borrowed
/// `Cow` contents stay borrowed, so cloning a `Value` is cheap unless it
/// owns large containers.
#[derive(Debug, Clone)]
pub enum Value<'a> {
    /// JSON `null` / Python `None`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Integer in `i64` range.
    Int(i64),
    /// Integer in `(i64::MAX, u64::MAX]` — JSON can express it, `i64` cannot.
    UInt(u64),
    /// Integer beyond `u64`: canonical decimal text (optional `-` then ASCII
    /// digits, no leading zeros, no `+`). Python's `int` is unbounded, so
    /// the value is kept as text rather than rejected.
    BigInt(Cow<'a, str>),
    /// IEEE-754 double. JSON has no `NaN`/`Infinity` literals, but a
    /// non-JSON source may produce them; validators preserve them.
    Float(f64),
    /// UTF-8 text.
    Str(Cow<'a, str>),
    /// Raw bytes (files, `Raw`, `bytes` output). Never produced by a JSON
    /// decoder.
    Bytes(Cow<'a, [u8]>),
    /// Ordered sequence.
    Array(Vec<Value<'a>>),
    /// Ordered key→value pairs, duplicates allowed, keys in input order.
    Object(Vec<(Cow<'a, str>, Value<'a>)>),
}

impl<'a> Value<'a> {
    /// A `Str` borrowing `s`.
    pub fn str(s: &'a str) -> Self {
        Self::Str(Cow::Borrowed(s))
    }

    /// A `Str` owning `s`.
    pub fn string(s: String) -> Self {
        Self::Str(Cow::Owned(s))
    }

    /// The JSON-schema-ish name of the value's shape, for error messages
    /// (`"integer"`, `"array"`, ...). `Bytes` reports `"bytes"`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Int(_) | Self::UInt(_) | Self::BigInt(_) => "integer",
            Self::Float(_) => "number",
            Self::Str(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
        }
    }

    /// The string contents, when this is a `Str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The bytes contents, when this is a `Bytes`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// The items, when this is an `Array`.
    pub fn as_array(&self) -> Option<&[Value<'a>]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The pairs, when this is an `Object`.
    pub fn as_object(&self) -> Option<&[(Cow<'a, str>, Value<'a>)]> {
        match self {
            Self::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    /// Look up `key` in an `Object` value; the *last* pair wins, like a
    /// Python `dict` built from the pairs in order.
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self {
            Self::Object(pairs) => pairs
                .iter()
                .rev()
                .find(|(k, _)| k.as_ref() == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// The integer as `i128` when this value is one of the integer variants
    /// *and* fits; `BigInt` is parsed to `i128`.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Self::Int(i) => Some(i128::from(*i)),
            Self::UInt(u) => Some(i128::from(*u)),
            Self::BigInt(text) => int_from_text(text),
            _ => None,
        }
    }
}

/// Normalize a decimal-integer text: strip a leading `+`, strip leading
/// zeros, keep the `-`. `"-007"` → `"-7"`, `"+0"` → `"0"`. Returns `None`
/// when `text` is not valid integer syntax (`[+-]?[0-9]+`).
pub(crate) fn normalize_int_text(text: &str) -> Option<String> {
    let t = text.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some("0".to_owned());
    }
    Some(if neg {
        format!("-{digits}")
    } else {
        digits.to_owned()
    })
}

/// Three-way compare of an integer's decimal text against `v`. `text` must
/// be valid integer syntax after `normalize_int_text`; sign, length, then
/// lexicographic order decide — no bignum arithmetic.
#[allow(
    dead_code,
    reason = "reserved for the M2.6 decoder; exercised by unit tests for now"
)]
pub(crate) fn cmp_int_text_to_i128(text: &str, v: i128) -> Option<Ordering> {
    let normalized = normalize_int_text(text)?;
    let (neg, digits) = match normalized.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, normalized.as_str()),
    };
    // Compare with the decimal digits of `v` — no parsing of `text` needed
    // beyond the syntax check already done.
    let v_neg = v < 0;
    let v_digits = v.unsigned_abs().to_string();
    if neg != v_neg {
        return Some(if neg {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let ord = digits
        .len()
        .cmp(&v_digits.len())
        .then_with(|| digits.cmp(&v_digits));
    Some(if neg { ord.reverse() } else { ord })
}

impl PartialEq for Value<'_> {
    /// Type-aware equality (module docs): integers compare across
    /// `Int`/`UInt`/`BigInt`, `Float` only equals `Float` (`f64` `==`, so
    /// `NaN != NaN` like Python), `Object` ignores pair order.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (
                Self::Int(_) | Self::UInt(_) | Self::BigInt(_),
                Self::Int(_) | Self::UInt(_) | Self::BigInt(_),
            ) => int_eq(self, other),
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => object_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for Value<'_> {}

fn int_eq(a: &Value<'_>, b: &Value<'_>) -> bool {
    match (a.as_i128(), b.as_i128()) {
        (Some(x), Some(y)) => x == y,
        // Both are integers (variants matched), so a non-i128 side is a
        // BigInt beyond i128: compare normalized text instead.
        _ => match (a, b) {
            (Value::BigInt(ta), Value::BigInt(tb)) => {
                match (normalize_int_text(ta), normalize_int_text(tb)) {
                    (Some(ta), Some(tb)) => ta == tb,
                    _ => false,
                }
            }
            _ => false,
        },
    }
}

fn object_eq<'a>(a: &[(Cow<'a, str>, Value<'a>)], b: &[(Cow<'a, str>, Value<'a>)]) -> bool {
    // Dict semantics: last duplicate wins on each side, then key sets equal.
    let a = dedupe_last(a);
    let b = dedupe_last(b);
    a.len() == b.len()
        && a.iter().all(|(k, v)| {
            b.iter()
                .find(|(bk, _)| bk == k)
                .is_some_and(|(_, bv)| bv == v)
        })
}

fn dedupe_last<'a, 'b>(
    pairs: &'b [(Cow<'a, str>, Value<'a>)],
) -> Vec<(&'b Cow<'a, str>, &'b Value<'a>)> {
    let mut out: Vec<(&Cow<'a, str>, &Value<'a>)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        match out.iter_mut().find(|(ek, _)| *ek == k) {
            Some((_, ev)) => *ev = v,
            None => out.push((k, v)),
        }
    }
    out
}

/// Consistent with [`PartialEq`]: integers hash by their `i128` value (a
/// `BigInt` that fits hashes as `i128`), floats by their bits with `-0.0`
/// normalized to `0.0` (`-0.0 == 0.0`), objects order-insensitively.
impl Hash for Value<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Null => 0u8.hash(state),
            Self::Bool(b) => {
                1u8.hash(state);
                b.hash(state);
            }
            Self::Int(_) | Self::UInt(_) | Self::BigInt(_) => {
                2u8.hash(state);
                match self.as_i128() {
                    Some(v) => v.hash(state),
                    None => {
                        if let Self::BigInt(t) = self {
                            normalize_int_text(t).hash(state);
                        }
                    }
                }
            }
            Self::Float(f) => {
                3u8.hash(state);
                if *f == 0.0 {
                    // -0.0 == 0.0, so they must hash identically.
                    0.0f64.to_bits().hash(state);
                } else {
                    f.to_bits().hash(state);
                }
            }
            Self::Str(s) => {
                4u8.hash(state);
                s.hash(state);
            }
            Self::Bytes(b) => {
                5u8.hash(state);
                b.hash(state);
            }
            Self::Array(items) => {
                6u8.hash(state);
                items.hash(state);
            }
            Self::Object(pairs) => {
                7u8.hash(state);
                pairs.len().hash(state);
                // Order-insensitive: accumulate pair hashes commutatively.
                let mut acc: u64 = 0;
                for (k, v) in dedupe_last(pairs) {
                    let mut member = std::collections::hash_map::DefaultHasher::new();
                    k.hash(&mut member);
                    v.hash(&mut member);
                    acc ^= member.finish();
                }
                acc.hash(state);
            }
        }
    }
}

/// A literal constant: the payload of `Literal[...]`, enum member values,
/// `Default(...)` values and error `limit` fields (DESIGN.md §11.2, §11.7).
///
/// This is the JSON-scalar fragment of [`Value`]: no containers — Python
/// `Literal` members are scalars in practice, and `Enum` values that are
/// containers cannot round-trip through a wire format anyway. `Decimal`
/// literals are expressed as [`LiteralValue::Str`] canonical text; the
/// surrounding descriptor knows the semantic type.
#[derive(Debug, Clone)]
pub enum LiteralValue {
    /// `None` / `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Integer in `i64` range.
    Int(i64),
    /// Integer in `(i64::MAX, u64::MAX]`.
    UInt(u64),
    /// Integer beyond `u64`, as canonical decimal text (see
    /// [`Value::BigInt`]).
    BigInt(Box<str>),
    /// IEEE-754 double.
    Float(f64),
    /// UTF-8 text.
    Str(Box<str>),
    /// Raw bytes.
    Bytes(Box<[u8]>),
}

impl LiteralValue {
    /// Borrow as an owned-free [`Value`] copy (all variants clone cheaply
    /// except `Str`/`Bytes`/`BigInt`, which clone their bytes).
    pub fn to_value(&self) -> Value<'static> {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(*b),
            Self::Int(i) => Value::Int(*i),
            Self::UInt(u) => Value::UInt(*u),
            Self::BigInt(t) => Value::BigInt(Cow::Owned(t.to_string())),
            Self::Float(f) => Value::Float(*f),
            Self::Str(s) => Value::Str(Cow::Owned(s.to_string())),
            Self::Bytes(b) => Value::Bytes(Cow::Owned(b.to_vec())),
        }
    }

    /// The same names [`Value::type_name`] uses.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Int(_) | Self::UInt(_) | Self::BigInt(_) => "integer",
            Self::Float(_) => "number",
            Self::Str(_) => "string",
            Self::Bytes(_) => "bytes",
        }
    }

    /// The scalar as `i128`, when this is an integer variant and fits.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Self::Int(i) => Some(i128::from(*i)),
            Self::UInt(u) => Some(i128::from(*u)),
            Self::BigInt(t) => int_from_text(t),
            _ => None,
        }
    }

    /// Render in wire form: how the value appears in JSON and in error
    /// `limit` fields (`1`, `"abc"`, `true`, `null`, `1.5`, `Abc1==`).
    pub fn wire_text(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Self::Int(i) => i.to_string(),
            Self::UInt(u) => u.to_string(),
            Self::BigInt(t) => t.to_string(),
            Self::Float(f) => format!("{f:?}"),
            Self::Str(s) => s.to_string(),
            Self::Bytes(b) => format!("{b:?}"),
        }
    }

    /// Render as a JSON literal: `null`, `true`, `1`, `1.5`, `"abc"`.
    /// Strings are quoted and JSON-escaped; `BigInt` emits its decimal
    /// digits (a valid JSON number); a non-finite float renders as
    /// `null` since JSON has no literal for it; bytes render as
    /// `\xNN`-escaped ASCII text inside quotes. This is the `limit`
    /// field encoding of §11.7 and lets `fulmen-py` serialize entries
    /// without serde.
    pub fn to_json(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::UInt(u) => u.to_string(),
            Self::BigInt(t) => t.to_string(),
            Self::Float(f) if f.is_finite() => format!("{f:?}"),
            // Non-finite floats have no JSON literal — render `null`.
            Self::Null | Self::Float(_) => "null".to_owned(),
            Self::Str(s) => format!("\"{}\"", escape_json(s)),
            Self::Bytes(b) => format!("\"{}\"", escape_json(&b.escape_ascii().to_string())),
        }
    }
}

/// JSON-escape `s`'s content (no surrounding quotes): `\"`, `\\`,
/// `\n`, `\r`, `\t` plus `\uXXXX` for other control characters.
pub(crate) fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => {
                let _ = fmt::Write::write_fmt(&mut out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

impl PartialEq for LiteralValue {
    /// Same type-aware rules as [`Value`]: integer variants compare
    /// numerically, `Float` only equals `Float`, no cross-domain equality.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (
                Self::Int(_) | Self::UInt(_) | Self::BigInt(_),
                Self::Int(_) | Self::UInt(_) | Self::BigInt(_),
            ) => match (self.as_i128(), other.as_i128()) {
                (Some(a), Some(b)) => a == b,
                _ => match (self, other) {
                    (Self::BigInt(a), Self::BigInt(b)) => {
                        match (normalize_int_text(a), normalize_int_text(b)) {
                            (Some(a), Some(b)) => a == b,
                            _ => false,
                        }
                    }
                    _ => false,
                },
            },
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for LiteralValue {}

impl Hash for LiteralValue {
    /// Consistent with [`PartialEq`]: same normalization as [`Value`].
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Null => 0u8.hash(state),
            Self::Bool(b) => {
                1u8.hash(state);
                b.hash(state);
            }
            Self::Int(_) | Self::UInt(_) | Self::BigInt(_) => {
                2u8.hash(state);
                match self.as_i128() {
                    Some(v) => v.hash(state),
                    None => {
                        if let Self::BigInt(t) = self {
                            normalize_int_text(t).hash(state);
                        }
                    }
                }
            }
            Self::Float(f) => {
                3u8.hash(state);
                if *f == 0.0 {
                    0.0f64.to_bits().hash(state);
                } else {
                    f.to_bits().hash(state);
                }
            }
            Self::Str(s) => {
                4u8.hash(state);
                s.hash(state);
            }
            Self::Bytes(b) => {
                5u8.hash(state);
                b.hash(state);
            }
        }
    }
}

/// Display for diagnostics (wire form, strings unquoted).
impl fmt::Display for LiteralValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_text())
    }
}

/// Whether `input` equals the literal `lit`.
///
/// - **Strict** (JSON): the same type-aware equality as `Value` — an
///   `Int(1)` literal does not match a `Str("1")` or `Float(1.0)` input.
/// - **Lax** (text sources): a `Str` input is additionally interpreted in
///   the literal's domain — `"1"` matches `Int(1)`, `"true"` matches
///   `Bool(true)`, `"null"`/`"none"` match `Null`, `"1.5"` matches
///   `Float(1.5)`, and `Str` literals compare textually. A non-`Str` lax
///   input compares by strict rules.
pub(crate) fn literal_eq(lit: &LiteralValue, input: &Value<'_>, lax: bool) -> bool {
    if let (Value::Str(text), true) = (input, lax) {
        return literal_eq_str(lit, text);
    }
    literal_eq_strict(lit, input)
}

/// Strict half of [`literal_eq`].
#[expect(
    clippy::float_cmp,
    reason = "Literal equality is exact — `Literal[1.5]` matches 1.5 only"
)]
fn literal_eq_strict(lit: &LiteralValue, input: &Value<'_>) -> bool {
    match (lit, input) {
        (LiteralValue::Null, Value::Null) => true,
        (LiteralValue::Bool(a), Value::Bool(b)) => a == b,
        (
            LiteralValue::Int(_) | LiteralValue::UInt(_) | LiteralValue::BigInt(_),
            Value::Int(_) | Value::UInt(_) | Value::BigInt(_),
        ) => match (lit.as_i128(), input.as_i128()) {
            (Some(a), Some(b)) => a == b,
            _ => match (lit, input) {
                (LiteralValue::BigInt(a), Value::BigInt(b)) => {
                    match (normalize_int_text(a), normalize_int_text(b)) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    }
                }
                _ => false,
            },
        },
        (LiteralValue::Float(a), Value::Float(b)) => a == b,
        (LiteralValue::Str(a), Value::Str(b)) => a.as_ref() == b.as_ref(),
        (LiteralValue::Bytes(a), Value::Bytes(b)) => a.as_ref() == b.as_ref(),
        _ => false,
    }
}

/// Lax half of [`literal_eq`]: reinterpret the input text in the literal's
/// domain (`"1"` → `Int(1)`, `"true"` → `Bool(true)`, ...). `Str` literals
/// compare textually first, so `Literal["1"]` matches `"1"` before any
/// numeric reading.
fn literal_eq_str(lit: &LiteralValue, text: &str) -> bool {
    match lit {
        LiteralValue::Null => matches!(text.trim().to_ascii_lowercase().as_str(), "null" | "none"),
        LiteralValue::Bool(b) => crate::parse::parse_bool_text(text) == Some(*b),
        LiteralValue::Int(_) | LiteralValue::UInt(_) | LiteralValue::BigInt(_) => {
            match (lit.as_i128(), int_from_text(text.trim())) {
                (Some(l), Some(i)) => l == i,
                _ => match lit {
                    // Literal beyond i128: compare normalized text.
                    LiteralValue::BigInt(a) => {
                        normalize_int_text(text.trim()).is_some_and(|n| n == a.as_ref())
                    }
                    _ => false,
                },
            }
        }
        LiteralValue::Float(a) => crate::parse::parse_float_text(text) == Some(*a),
        LiteralValue::Str(a) => a.as_ref() == text,
        LiteralValue::Bytes(a) => a.as_ref() == text.as_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_integer_text() {
        assert_eq!(normalize_int_text("007").unwrap(), "7");
        assert_eq!(normalize_int_text("-007").unwrap(), "-7");
        assert_eq!(normalize_int_text("+9").unwrap(), "9");
        assert_eq!(normalize_int_text("0").unwrap(), "0");
        assert_eq!(normalize_int_text("-0").unwrap(), "0");
        assert_eq!(normalize_int_text(" 42 ").unwrap(), "42");
        assert!(normalize_int_text("").is_none());
        assert!(normalize_int_text("-").is_none());
        assert!(normalize_int_text("1.0").is_none());
        assert!(normalize_int_text("1e3").is_none());
    }

    #[test]
    fn cmp_int_text() {
        assert_eq!(
            cmp_int_text_to_i128("999999999999999999999999999999999999999", i128::MAX),
            Some(Ordering::Greater)
        );
        assert_eq!(cmp_int_text_to_i128("-5", 3), Some(Ordering::Less));
        assert_eq!(cmp_int_text_to_i128("007", 7), Some(Ordering::Equal));
        assert_eq!(
            cmp_int_text_to_i128("-170141183460469231731687303715884105728", i128::MIN),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn type_aware_equality() {
        assert_ne!(Value::Int(1), Value::Float(1.0));
        assert_ne!(Value::Int(1), Value::Bool(true));
        assert_eq!(Value::Int(1), Value::UInt(1));
        assert_eq!(Value::Int(-5), Value::BigInt(Cow::Borrowed("-5")));
        assert_eq!(
            Value::UInt(u64::MAX),
            Value::BigInt(Cow::Borrowed("18446744073709551615"))
        );
        assert_ne!(Value::Float(f64::NAN), Value::Float(f64::NAN));
        assert_eq!(Value::Float(-0.0), Value::Float(0.0));
    }

    #[test]
    fn object_equality_ignores_order_and_dups() {
        let a = Value::Object(vec![
            (Cow::Borrowed("x"), Value::Int(1)),
            (Cow::Borrowed("y"), Value::Int(2)),
        ]);
        let b = Value::Object(vec![
            (Cow::Borrowed("y"), Value::Int(2)),
            (Cow::Borrowed("x"), Value::Int(1)),
        ]);
        assert_eq!(a, b);
        let c = Value::Object(vec![
            (Cow::Borrowed("x"), Value::Int(0)),
            (Cow::Borrowed("x"), Value::Int(1)),
            (Cow::Borrowed("y"), Value::Int(2)),
        ]);
        assert_eq!(a, c); // last duplicate wins, like dict(a)
        assert_ne!(a, Value::Object(vec![]));
    }

    #[test]
    fn hash_consistency() {
        use std::collections::hash_map::DefaultHasher;
        let h = |v: &Value<'_>| {
            let mut s = DefaultHasher::new();
            v.hash(&mut s);
            s.finish()
        };
        assert_eq!(h(&Value::Int(7)), h(&Value::UInt(7)));
        assert_eq!(h(&Value::Int(7)), h(&Value::BigInt(Cow::Borrowed("7"))));
        assert_eq!(h(&Value::Float(0.0)), h(&Value::Float(-0.0)));
    }

    #[test]
    fn literal_matching() {
        let one = LiteralValue::Int(1);
        assert!(literal_eq(&one, &Value::Int(1), false));
        assert!(!literal_eq(&one, &Value::str("1"), false));
        assert!(literal_eq(&one, &Value::str("1"), true));
        assert!(!literal_eq(&one, &Value::str("true"), true));

        let t = LiteralValue::Bool(true);
        assert!(literal_eq(&t, &Value::str("YES"), true));
        assert!(!literal_eq(&t, &Value::Bool(false), false));

        let s = LiteralValue::Str("x".into());
        assert!(literal_eq(&s, &Value::str("x"), true));
        assert!(!literal_eq(&s, &Value::Int(1), true));

        let n = LiteralValue::Null;
        assert!(literal_eq(&n, &Value::Null, false));
        assert!(literal_eq(&n, &Value::str("none"), true));
    }

    #[test]
    fn literal_value_eq_and_hash() {
        assert_eq!(LiteralValue::Int(3), LiteralValue::UInt(3));
        assert_eq!(LiteralValue::Int(3), LiteralValue::BigInt("3".into()));
        assert_ne!(LiteralValue::Int(3), LiteralValue::Float(3.0));
        assert_ne!(LiteralValue::Str("1".into()), LiteralValue::Int(1));
    }
}
