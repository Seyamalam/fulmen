//! Lax-mode source adapters (DESIGN.md §11.4): turn the request's text
//! sources into [`Value`]s the validator consumes in [`Mode::Lax`].
//!
//! ## Produced shapes
//!
//! - **Query** (`?a=1&a=2&b=x`) and **`application/x-www-form-urlencoded`
//!   bodies** → [`Value::Object`] where a repeated key collects its
//!   values into an `Array`: `{"a": ["1", "2"], "b": "x"}`. `?a` and
//!   `?a=` both give `{"a": ""}`, and keys decode percent-escapes before
//!   comparison (`?%61=1&a=2` is one repeated key). Param extraction can
//!   therefore ask "is the key present" via [`Value::get`], while
//!   `list[T]` parameters get their `Array` directly and a scalar
//!   parameter sees a lone `Str` (which lax promotion handles).
//! - **`Cookie` header** (`a=1; b=2`) → the same object shape on
//!   `;`-separated `name=value` pairs. Cookie names/values are *not*
//!   percent-decoded (RFC 6265 has no such rule); one pair of surrounding
//!   double quotes on a value is stripped. `a` without `=` maps to `""`.
//! - **Path/header/cookie single values** stay `str` —
//!   [`validate_text`] wraps one value directly for lax validation;
//!   header values are opaque and need no decoding at all.
//!
//! ## Errors
//!
//! - Malformed percent-encoding or decoded bytes that aren't UTF-8 →
//!   [`TextError`] (`position` + machine `code`) — an HTTP 400.
//! - Well-formed input past a [`Limits`] bound →
//!   [`SourceError::Limit`] carrying [`ValidationErrors`] in §11.7 shape
//!   (`/query`, `/form`, `/cookie` locs) — an HTTP 422.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::compile::Limits;
use crate::errors::{ErrorCode, Source, ValidationErrors};
use crate::validator::{Checked, Mode, Validator};
use crate::value::{LiteralValue, Value};

/// Machine-readable `code` of a [`TextError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextErrorCode {
    /// A `%` that is not followed by two hex digits.
    PercentEscape,
    /// Percent-decoded bytes are not valid UTF-8.
    InvalidUtf8,
}

impl TextErrorCode {
    /// The wire code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PercentEscape => "percent_escape",
            Self::InvalidUtf8 => "invalid_utf8",
        }
    }
}

impl std::fmt::Display for TextErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a text source is malformed — maps to an HTTP 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextError {
    /// Machine-readable code.
    pub code: TextErrorCode,
    /// Byte offset in the input where the error was detected. For
    /// [`TextErrorCode::PercentEscape`] it is the offending `%`; for
    /// [`TextErrorCode::InvalidUtf8`] it is the start of the segment
    /// whose decoded bytes failed (the raw and decoded offsets diverge).
    pub position: usize,
    /// Human-readable detail; never echoes input content.
    pub message: String,
}

impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (byte {})", self.message, self.position)
    }
}

impl std::error::Error for TextError {}

/// Why a source adapter failed — the same 400/422 split as
/// [`crate::DecodeError`].
#[derive(Debug)]
pub enum SourceError {
    /// The text source is malformed (HTTP 400).
    Malformed(TextError),
    /// Well-formed but past a [`Limits`] bound (HTTP 422).
    Limit(ValidationErrors),
}

impl SourceError {
    /// The HTTP status this maps to downstream.
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 400,
            Self::Limit(_) => 422,
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed text source: {e}"),
            Self::Limit(_) => write!(f, "text source exceeds structural limits"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Decode a `?`-less query string into the repeated-key object shape
/// described in the module docs.
///
/// # Errors
///
/// [`SourceError::Malformed`] for bad percent-escapes or non-UTF-8
/// decoded text; [`SourceError::Limit`] for bound violations.
pub fn decode_query(input: &str) -> Result<Value<'_>, SourceError> {
    decode_query_with_limits(input, &Limits::default())
}

/// [`decode_query`] with explicit [`Limits`]: distinct keys and values
/// per repeated key each count against `max_items`, decoded names and
/// values against `max_str_bytes`.
///
/// # Errors
///
/// Same contract as [`decode_query`].
pub fn decode_query_with_limits<'a>(
    input: &'a str,
    limits: &Limits,
) -> Result<Value<'a>, SourceError> {
    urlencoded(input, limits, Source::Query)
}

/// Decode an `application/x-www-form-urlencoded` body — the same grammar
/// and shape as [`decode_query`], with `/form` locs.
///
/// # Errors
///
/// Same contract as [`decode_query`].
pub fn decode_form(input: &str) -> Result<Value<'_>, SourceError> {
    decode_form_with_limits(input, &Limits::default())
}

/// [`decode_form`] with explicit [`Limits`].
///
/// # Errors
///
/// Same contract as [`decode_query`].
pub fn decode_form_with_limits<'a>(
    input: &'a str,
    limits: &Limits,
) -> Result<Value<'a>, SourceError> {
    urlencoded(input, limits, Source::Form)
}

/// Decode a `Cookie` header (`a=1; b=2`) into the repeated-key object
/// shape — no percent-decoding, one pair of value quotes stripped.
///
/// # Errors
///
/// [`SourceError::Limit`] for bound violations. The grammar is lenient
/// (segments without `=` map to `""`, empty segments and empty names are
/// skipped), so [`SourceError::Malformed`] is currently never produced.
pub fn decode_cookie(header: &str) -> Result<Value<'_>, SourceError> {
    decode_cookie_with_limits(header, &Limits::default())
}

/// [`decode_cookie`] with explicit [`Limits`], counted as in
/// [`decode_query_with_limits`].
///
/// # Errors
///
/// Same contract as [`decode_cookie`].
pub fn decode_cookie_with_limits<'a>(
    header: &'a str,
    limits: &Limits,
) -> Result<Value<'a>, SourceError> {
    let mut fold = PairFold::new();
    let mut at = 0usize;
    for seg in header.split(';') {
        let seg_at = at;
        at += seg.len() + 1;
        let seg = seg.trim_matches([' ', '\t']);
        if seg.is_empty() {
            continue;
        }
        let (name, value) = match seg.find('=') {
            Some(eq) => {
                let name = seg[..eq].trim_matches([' ', '\t']);
                let value = unquote(seg[eq + 1..].trim_matches([' ', '\t']));
                (name, value)
            }
            None => (seg, ""),
        };
        if name.is_empty() {
            continue;
        }
        fold.push(
            Cow::Borrowed(name),
            Cow::Borrowed(value),
            seg_at,
            limits,
            Source::Cookie,
        )?;
    }
    Ok(fold.finish())
}

/// Validate one text value (path segment, header or cookie value, single
/// query value) through `validator` in [`Mode::Lax`], wrapped as
/// [`Value::Str`].
///
/// `Limits` apply as usual: an over-long `text` fails `max_str_len`
/// inside the validator. The returned `Cow` is always `Owned` — the
/// wrapper value is created inside the call.
///
/// # Errors
///
/// [`ValidationErrors`] for any failed check (`"42x"` for `int`,
/// `"nope"` for `datetime`, ...).
pub fn validate_text<'a>(
    text: &'a str,
    validator: &Validator,
    source: Source,
) -> Result<Cow<'a, Value<'a>>, ValidationErrors> {
    let input = Value::str(text);
    match validator.validate_out(&input, source, Mode::Lax) {
        Ok(Checked::Same(_)) => Ok(Cow::Owned(input)),
        Ok(Checked::New(v)) => Ok(Cow::Owned(v)),
        Err(errs) => Err(errs),
    }
}

/// `key=value&…` shared by query and form bodies.
fn urlencoded<'a>(
    input: &'a str,
    limits: &Limits,
    source: Source,
) -> Result<Value<'a>, SourceError> {
    let mut fold = PairFold::new();
    let mut at = 0usize;
    for seg in input.split('&') {
        let seg_at = at;
        at += seg.len() + 1;
        if seg.is_empty() {
            continue;
        }
        let (name_raw, value_raw, value_at) = match seg.find('=') {
            Some(eq) => (&seg[..eq], &seg[eq + 1..], seg_at + eq + 1),
            None => (seg, "", seg_at + seg.len()),
        };
        let name = percent_decode(name_raw, seg_at)?;
        let value = percent_decode(value_raw, value_at)?;
        fold.push(name, value, seg_at, limits, source)?;
    }
    Ok(fold.finish())
}

/// `%XX` → byte, `+` → space; borrows when neither appears. The decoded
/// byte buffer becomes the `String` directly, so decoding allocates once.
fn percent_decode(seg: &str, at: usize) -> Result<Cow<'_, str>, SourceError> {
    if !seg.bytes().any(|b| b == b'%' || b == b'+') {
        return Ok(Cow::Borrowed(seg));
    }
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).copied().and_then(hex_val);
                let lo = bytes.get(i + 2).copied().and_then(hex_val);
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(SourceError::Malformed(TextError {
                        code: TextErrorCode::PercentEscape,
                        position: at + i,
                        message: "invalid percent escape".to_owned(),
                    }));
                };
                out.extend_from_slice(&bytes[plain_start..i]);
                out.push(hi * 16 + lo);
                i += 3;
                plain_start = i;
            }
            b'+' => {
                out.extend_from_slice(&bytes[plain_start..i]);
                out.push(b' ');
                i += 1;
                plain_start = i;
            }
            _ => i += 1,
        }
    }
    out.extend_from_slice(&bytes[plain_start..]);
    match String::from_utf8(out) {
        Ok(s) => Ok(Cow::Owned(s)),
        Err(_) => Err(SourceError::Malformed(TextError {
            code: TextErrorCode::InvalidUtf8,
            position: at,
            message: "percent-decoded text is not valid UTF-8".to_owned(),
        })),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip one pair of surrounding double quotes (the RFC 6265
/// `DQUOTE *cookie-octet DQUOTE` value form).
fn unquote(value: &str) -> &str {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// A [`SourceError::Limit`] with one §11.7 entry at `loc` `[]`.
fn limit_err<T>(
    source: Source,
    code: ErrorCode,
    limit: usize,
    message: String,
) -> Result<T, SourceError> {
    let mut errs = ValidationErrors::new();
    errs.fail(
        source,
        Vec::new(),
        code,
        message,
        Some(LiteralValue::UInt(limit as u64)),
    );
    Err(SourceError::Limit(errs))
}

/// Fold `name=value` pairs into the repeated-key object shape:
/// first-seen order is kept, a second occurrence promotes the `Str` to
/// an `Array` that later values append to.
struct PairFold<'a> {
    pairs: Vec<(Cow<'a, str>, Vec<Cow<'a, str>>)>,
    index: HashMap<Cow<'a, str>, usize>,
}

impl<'a> PairFold<'a> {
    fn new() -> Self {
        Self {
            pairs: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn push(
        &mut self,
        name: Cow<'a, str>,
        value: Cow<'a, str>,
        at: usize,
        limits: &Limits,
        source: Source,
    ) -> Result<(), SourceError> {
        if name.len() > limits.max_str_bytes || value.len() > limits.max_str_bytes {
            return limit_err(
                source,
                ErrorCode::MaxStrLen,
                limits.max_str_bytes,
                format!(
                    "string longer than {} bytes at byte {}",
                    limits.max_str_bytes, at
                ),
            );
        }
        if let Some(&i) = self.index.get(&name) {
            let vals = &mut self.pairs[i].1;
            if vals.len() >= limits.max_items {
                return limit_err(
                    source,
                    ErrorCode::MaxItems,
                    limits.max_items,
                    format!(
                        "array longer than {} items at byte {}",
                        limits.max_items, at
                    ),
                );
            }
            vals.push(value);
        } else {
            if self.pairs.len() >= limits.max_items {
                return limit_err(
                    source,
                    ErrorCode::MaxItems,
                    limits.max_items,
                    format!(
                        "object has more than {} entries at byte {}",
                        limits.max_items, at
                    ),
                );
            }
            self.index.insert(name.clone(), self.pairs.len());
            self.pairs.push((name, vec![value]));
        }
        Ok(())
    }

    /// Single-occurrence keys emit `Value::Str`, repeated keys an
    /// `Array` of them.
    fn finish(self) -> Value<'a> {
        Value::Object(
            self.pairs
                .into_iter()
                .map(|(k, vals)| {
                    let v = if vals.len() == 1 {
                        Value::Str(vals.into_iter().next().unwrap_or(Cow::Borrowed("")))
                    } else {
                        Value::Array(vals.into_iter().map(Value::Str).collect())
                    };
                    (k, v)
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec_q(text: &str) -> Value<'_> {
        decode_query(text).expect("decode should succeed")
    }

    #[test]
    fn query_shapes() {
        let v = dec_q("a=1&a=2&b=x");
        assert_eq!(
            v.get("a"),
            Some(&Value::Array(vec![Value::str("1"), Value::str("2")]))
        );
        assert_eq!(v.get("b"), Some(&Value::str("x")));
        // Bare and empty values; empty segments skipped.
        let v = dec_q("a&&b=&");
        assert_eq!(v.get("a"), Some(&Value::str("")));
        assert_eq!(v.get("b"), Some(&Value::str("")));
        assert_eq!(v.as_object().map(<[_]>::len), Some(2));
        assert_eq!(dec_q(""), Value::Object(vec![]));
        // '=' inside a value.
        assert_eq!(dec_q("a=b=c").get("a"), Some(&Value::str("b=c")));
        // Key present check: absent vs present-empty.
        assert!(dec_q("a").get("missing").is_none());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(dec_q("a=%41").get("a"), Some(&Value::str("A")));
        assert_eq!(dec_q("a=%C3%A9").get("a"), Some(&Value::str("é")));
        assert_eq!(dec_q("a=1+2").get("a"), Some(&Value::str("1 2")));
        // Keys decode before dedupe.
        let v = dec_q("%61=1&a=2");
        assert_eq!(
            v.get("a"),
            Some(&Value::Array(vec![Value::str("1"), Value::str("2")]))
        );
        assert_eq!(v.as_object().map(<[_]>::len), Some(1));
    }

    #[test]
    fn percent_errors() {
        for (input, at) in [("a=%zz", 2), ("a=%4", 2), ("a=%", 2), ("ab%zz=1", 2)] {
            let Err(SourceError::Malformed(e)) = decode_query(input) else {
                panic!("expected malformed for {input:?}")
            };
            assert_eq!(e.code, TextErrorCode::PercentEscape, "{input:?}");
            assert_eq!(e.position, at, "{input:?}");
        }
        for input in ["a=%FF", "a=%C3", "a=%C3%28"] {
            let Err(SourceError::Malformed(e)) = decode_query(input) else {
                panic!("expected malformed for {input:?}")
            };
            assert_eq!(e.code, TextErrorCode::InvalidUtf8, "{input:?}");
        }
    }

    #[test]
    fn cookies() {
        let v = decode_cookie("a=1; b=2").expect("decode should succeed");
        assert_eq!(v.get("a"), Some(&Value::str("1")));
        assert_eq!(v.get("b"), Some(&Value::str("2")));
        // Bare name, quoted value, no percent decoding, repeats, skips.
        let v = decode_cookie("a=1;a=2; flag; q=\"v v\"; p=%41; ; =x").expect("decode");
        assert_eq!(
            v.get("a"),
            Some(&Value::Array(vec![Value::str("1"), Value::str("2")]))
        );
        assert_eq!(v.get("flag"), Some(&Value::str("")));
        assert_eq!(v.get("q"), Some(&Value::str("v v")));
        assert_eq!(v.get("p"), Some(&Value::str("%41")));
        assert!(v.get("").is_none());
        assert_eq!(decode_cookie("").expect("decode"), Value::Object(vec![]));
    }

    #[test]
    fn adapter_limits() {
        let limits = Limits {
            max_items: 3,
            max_str_bytes: 4,
            ..Limits::default()
        };
        // 4 distinct keys > max_items.
        match decode_query_with_limits("a=1&b=2&c=3&d=4", &limits) {
            Err(SourceError::Limit(e)) => {
                assert_eq!(e.entries()[0].code, ErrorCode::MaxItems);
                assert_eq!(e.entries()[0].pointer(), "/query");
            }
            r => panic!("expected limit, got {r:?}"),
        }
        // 4 values on one key > max_items.
        assert!(matches!(
            decode_query_with_limits("a=1&a=2&a=3&a=4", &limits),
            Err(SourceError::Limit(_))
        ));
        // Decoded string too long (name or value).
        assert!(matches!(
            decode_query_with_limits("abcde=1", &limits),
            Err(SourceError::Limit(_))
        ));
        assert!(matches!(
            decode_query_with_limits("a=12345", &limits),
            Err(SourceError::Limit(_))
        ));
        assert!(decode_query_with_limits("a=1&a=2&b=x", &limits).is_ok());
        // Form shares the grammar, cookies the fold.
        assert!(matches!(
            decode_form_with_limits("a=1&b=2&c=3&d=4", &limits),
            Err(SourceError::Limit(_))
        ));
        assert!(matches!(
            decode_cookie_with_limits("a=1;b=2;c=3;d=4", &limits),
            Err(SourceError::Limit(_))
        ));
    }
}
