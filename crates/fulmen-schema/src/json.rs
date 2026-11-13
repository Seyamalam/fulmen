//! JSON decoding: a purpose-built byte-slice parser producing borrowed
//! [`Value`]s (DESIGN.md §11.4, decision D18).
//!
//! The decoder walks the input once, left to right, building
//! [`Value<'a>`]s whose strings and big-integer text borrow the request
//! buffer directly — zero-copy on the happy path. That is the §11.4
//! "one pass" thesis: `serde_json` would materialize its own `Value`
//! tree first (a second pass, plus `serde` dependencies this
//! dep-restricted crate doesn't want), and `simd-json`/`sonic-rs` need
//! `unsafe`, which this crate forbids.
//!
//! ## Compliance decisions
//!
//! - Strict RFC 8259 input: no `NaN`/`Infinity`, no trailing commas, no
//!   leading zeros, raw control characters in strings rejected, `\uXXXX`
//!   escapes must form valid UTF-16 (lone surrogates are an error), and
//!   unescaped bytes must be UTF-8.
//! - **Duplicate object keys are kept** in input order — the
//!   [`Value::Object`] contract — which makes the effective policy
//!   *last wins*: [`Value::get`], `dict` validation and record field
//!   matching all resolve a repeated key to its last pair (the same
//!   answer `json.loads` gives).
//! - Integer literals beyond `u64` become [`Value::BigInt`] with the raw
//!   slice as canonical text (JSON's grammar already forbids leading
//!   zeros and `+`). `-0` decodes to `Int(0)`, like `json.loads`.
//! - Float literals whose exponent overflows `f64` decode to `±inf`,
//!   like `json.loads` (`1e999` → `inf`).
//!
//! ## Errors
//!
//! - Malformed input → [`JsonError`] (byte `position` + machine `code`):
//!   downstream this is an HTTP 400.
//! - Well-formed input past a [`Limits`] bound → [`DecodeError::Limit`]
//!   carrying [`ValidationErrors`] in §11.7 shape: an HTTP 422, the same
//!   as a validator limit. Depth, per-container items and per-string
//!   bytes are enforced *during* the walk — the only bound `Any`/`Raw`
//!   payloads get, since validators never descend into them.

use std::borrow::Cow;

use crate::compile::Limits;
use crate::errors::{ErrorCode, Source, ValidationErrors};
use crate::validator::{Checked, Mode, Validator};
use crate::value::{LiteralValue, Value};

/// Machine-readable `code` of a [`JsonError`]. Short lowercase strings,
/// stable for wire use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JsonErrorCode {
    /// Input ended (or started) mid-value — truncated body, empty input.
    Eof,
    /// A byte that cannot legally appear here (bad literal, stray token).
    UnexpectedChar,
    /// `,]` or `,}` — trailing commas are not JSON.
    TrailingComma,
    /// Bytes after a complete top-level value.
    TrailingData,
    /// `\x` where `x` is not a valid escape letter.
    InvalidEscape,
    /// Bad `\uXXXX` digits or a lone UTF-16 surrogate.
    InvalidUnicode,
    /// Raw `<0x20` byte inside a string (must be escaped).
    ControlChar,
    /// Number grammar violated (`01`, `1.`, `1e`, `-` alone, ...).
    InvalidNumber,
    /// Unescaped string bytes are not valid UTF-8.
    InvalidUtf8,
}

impl JsonErrorCode {
    /// The wire code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::UnexpectedChar => "unexpected_char",
            Self::TrailingComma => "trailing_comma",
            Self::TrailingData => "trailing_data",
            Self::InvalidEscape => "invalid_escape",
            Self::InvalidUnicode => "invalid_unicode",
            Self::ControlChar => "control_char",
            Self::InvalidNumber => "invalid_number",
            Self::InvalidUtf8 => "invalid_utf8",
        }
    }
}

impl std::fmt::Display for JsonErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a byte slice is not well-formed JSON — maps to an HTTP 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    /// Machine-readable code.
    pub code: JsonErrorCode,
    /// Byte offset in the input where the error was detected.
    pub position: usize,
    /// Human-readable detail; describes what was expected, never echoes
    /// input content (§11.7's no-echo rule applied to syntax errors too).
    pub message: String,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (byte {})", self.message, self.position)
    }
}

impl std::error::Error for JsonError {}

/// Why [`decode_json`]/[`decode_json_with_limits`] failed.
#[derive(Debug)]
pub enum DecodeError {
    /// The bytes are not well-formed JSON (HTTP 400).
    Malformed(JsonError),
    /// The JSON is well-formed but deeper or larger than [`Limits`]
    /// allow — reported as §11.7 entries with `max_depth`/`max_items`/
    /// `max_str_len` codes and `loc` `[]` (`/body`), an HTTP 422.
    Limit(ValidationErrors),
}

impl DecodeError {
    /// The HTTP status this maps to downstream.
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 400,
            Self::Limit(_) => 422,
        }
    }
}

impl From<JsonError> for DecodeError {
    fn from(err: JsonError) -> Self {
        Self::Malformed(err)
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed JSON: {e}"),
            Self::Limit(_) => write!(f, "JSON input exceeds structural limits"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why [`decode_and_validate`] failed — keeps the malformed/validation
/// split a router needs for `400` vs `422`, plus the well-formed-but-
/// oversized middle case (`422` per §11.4).
#[derive(Debug)]
pub enum PayloadError {
    /// The body is not well-formed JSON (HTTP 400).
    Malformed(JsonError),
    /// The body decoded but broke a structural limit (HTTP 422).
    Limit(ValidationErrors),
    /// The decoded value failed validation (HTTP 422).
    Validation(ValidationErrors),
}

impl PayloadError {
    /// The HTTP status this maps to downstream.
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 400,
            Self::Limit(_) | Self::Validation(_) => 422,
        }
    }

    /// The collected §11.7 entries, when this error carries them —
    /// `Some` for `Limit` and `Validation`, `None` for `Malformed`
    /// (whose [`JsonError`] fields shape the 400 problem body instead).
    pub fn errors(&self) -> Option<&ValidationErrors> {
        match self {
            Self::Malformed(_) => None,
            Self::Limit(e) | Self::Validation(e) => Some(e),
        }
    }
}

impl From<DecodeError> for PayloadError {
    fn from(err: DecodeError) -> Self {
        match err {
            DecodeError::Malformed(e) => Self::Malformed(e),
            DecodeError::Limit(e) => Self::Limit(e),
        }
    }
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed JSON: {e}"),
            Self::Limit(_) => write!(f, "JSON input exceeds structural limits"),
            Self::Validation(_) => write!(f, "request validation failed"),
        }
    }
}

impl std::error::Error for PayloadError {}

/// Decode `bytes` as one JSON value under default [`Limits`]
/// (`max_depth` 64, `max_items` 100 000, unbounded strings).
///
/// The returned [`Value`] borrows `bytes` for every unescaped string —
/// keep the buffer alive as long as the value.
///
/// # Errors
///
/// [`DecodeError::Malformed`] for syntax errors, [`DecodeError::Limit`]
/// for well-formed input past a structural bound.
pub fn decode_json(bytes: &[u8]) -> Result<Value<'_>, DecodeError> {
    decode_json_with_limits(bytes, &Limits::default())
}

/// Decode `bytes` as one JSON value, enforcing `limits` during the walk.
///
/// Limit checks run as values are *produced*, so a hostile body is
/// rejected without ever materializing the oversized part.
///
/// # Errors
///
/// Same contract as [`decode_json`].
pub fn decode_json_with_limits<'a>(
    bytes: &'a [u8],
    limits: &Limits,
) -> Result<Value<'a>, DecodeError> {
    let mut d = Decoder {
        buf: bytes,
        pos: 0,
        limits: *limits,
    };
    d.skip_ws();
    let v = d.value(1)?;
    d.skip_ws();
    if d.pos != d.buf.len() {
        return d.fail(
            JsonErrorCode::TrailingData,
            "unexpected data after the top-level value",
        );
    }
    Ok(v)
}

/// Decode `bytes` and validate the result in [`Mode::Strict`] as
/// [`Source::Body`] — the JSON request-body pipeline.
///
/// Decoding uses the validator's compiled-in [`Limits`], so `Any`/`Raw`
/// payloads are bounded exactly like typed ones. The returned `Cow` is
/// always `Owned`: the decoded tree is created inside the call, so there
/// is nothing external to borrow — `validate_out` guarantees a rewritten
/// value keeps the full `'a` buffer lifetime.
///
/// # Errors
///
/// [`PayloadError::Malformed`] (400) for bad JSON, `Limit` (422) for
/// structural bounds, `Validation` (422) for schema violations.
pub fn decode_and_validate<'a>(
    bytes: &'a [u8],
    validator: &Validator,
) -> Result<Cow<'a, Value<'a>>, PayloadError> {
    decode_and_validate_with_limits(bytes, validator, &Limits::default())
}

/// `decode_and_validate` under `limits`, tightened against the
/// validator's compiled-in set — a caller-supplied bound can only make
/// the pipeline stricter (DESIGN.md §14.8).
///
/// # Errors
///
/// Same contract as [`decode_and_validate`].
pub fn decode_and_validate_with_limits<'a>(
    bytes: &'a [u8],
    validator: &Validator,
    limits: &Limits,
) -> Result<Cow<'a, Value<'a>>, PayloadError> {
    let effective = validator.limits().tightened(limits);
    let input = decode_json_with_limits(bytes, &effective)?;
    match validator.validate_out_with_limits(&input, Source::Body, Mode::Strict, limits) {
        Ok(Checked::Same(_)) => Ok(Cow::Owned(input)),
        Ok(Checked::New(v)) => Ok(Cow::Owned(v)),
        Err(errs) => Err(PayloadError::Validation(errs)),
    }
}

/// The parser state: input slice, cursor and limits.
struct Decoder<'a> {
    /// The JSON input; all borrowed `Cow`s point into it.
    buf: &'a [u8],
    /// Current byte offset — always `<= buf.len()`.
    pos: usize,
    /// Structural limits enforced mid-walk (copied — `Limits` is `Copy`,
    /// so the decoder doesn't borrow them).
    limits: Limits,
}

/// A [`DecodeError::Malformed`] at `position`.
fn malformed<T>(
    code: JsonErrorCode,
    position: usize,
    message: impl Into<String>,
) -> Result<T, DecodeError> {
    Err(DecodeError::Malformed(JsonError {
        code,
        position,
        message: message.into(),
    }))
}

/// A [`DecodeError::Limit`] with one §11.7 entry at `loc` `[]`.
fn limit_err<T>(code: ErrorCode, limit: u64, message: String) -> Result<T, DecodeError> {
    let mut errs = ValidationErrors::new();
    errs.fail(
        Source::Body,
        Vec::new(),
        code,
        message,
        Some(LiteralValue::UInt(limit)),
    );
    Err(DecodeError::Limit(errs))
}

impl<'a> Decoder<'a> {
    /// Malformed at the current position.
    fn fail<T>(&self, code: JsonErrorCode, message: impl Into<String>) -> Result<T, DecodeError> {
        malformed(code, self.pos, message)
    }

    fn skip_ws(&mut self) {
        while self
            .buf
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.pos += 1;
        }
    }

    /// Any JSON value at nesting level `depth` (the root is `1`).
    fn value(&mut self, depth: u32) -> Result<Value<'a>, DecodeError> {
        if depth > self.limits.max_depth {
            return limit_err(
                ErrorCode::MaxDepth,
                u64::from(self.limits.max_depth),
                format!(
                    "nesting deeper than max_depth ({}) at byte {}",
                    self.limits.max_depth, self.pos
                ),
            );
        }
        let Some(&b) = self.buf.get(self.pos) else {
            return self.fail(JsonErrorCode::Eof, "unexpected end of input");
        };
        match b {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Value::Str(self.string()?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            b'N' | b'I' | b'i' | b'+' => self.fail(
                JsonErrorCode::UnexpectedChar,
                "JSON does not allow NaN, Infinity or a leading '+'",
            ),
            _ => self.fail(
                JsonErrorCode::UnexpectedChar,
                format!("unexpected byte 0x{b:02x}"),
            ),
        }
    }

    /// `true`/`false`/`null` — must match exactly.
    fn literal(&mut self, word: &[u8], v: Value<'a>) -> Result<Value<'a>, DecodeError> {
        let start = self.pos;
        if self.buf[start..].starts_with(word) {
            self.pos = start + word.len();
            Ok(v)
        } else {
            malformed(JsonErrorCode::UnexpectedChar, start, "invalid literal")
        }
    }

    /// `-?(0|[1-9]D*)(.D+)?([eE][+-]?D+)?` — the scanner already advanced
    /// to the first byte (a digit or `-`).
    fn number(&mut self) -> Result<Value<'a>, DecodeError> {
        let start = self.pos;
        if self.buf.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.buf.get(self.pos) {
            Some(b'0') => {
                self.pos += 1;
                if self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                    return self.fail(
                        JsonErrorCode::InvalidNumber,
                        "leading zeros are not allowed",
                    );
                }
            }
            Some(b) if b.is_ascii_digit() => {
                while self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                    self.pos += 1;
                }
            }
            _ => {
                return self.fail(JsonErrorCode::InvalidNumber, "expected digits in number");
            }
        }
        let mut is_float = false;
        if self.buf.get(self.pos) == Some(&b'.') {
            is_float = true;
            self.pos += 1;
            if !self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                return self.fail(JsonErrorCode::InvalidNumber, "expected digits after '.'");
            }
            while self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
        }
        if matches!(self.buf.get(self.pos), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.buf.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                return self.fail(JsonErrorCode::InvalidNumber, "expected digits in exponent");
            }
            while self.buf.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
        }
        // The token is ASCII by grammar; the `from_utf8` is a formal
        // check, not a reachable failure.
        let Ok(text) = std::str::from_utf8(&self.buf[start..self.pos]) else {
            return malformed(JsonErrorCode::InvalidNumber, start, "invalid number");
        };
        if is_float {
            match text.parse::<f64>() {
                // Exponent overflow yields ±inf — same as `json.loads`.
                Ok(f) => Ok(Value::Float(f)),
                Err(_) => malformed(JsonErrorCode::InvalidNumber, start, "invalid number"),
            }
        } else {
            Ok(int_value(text))
        }
    }

    /// `[` at the current position → `Value::Array`.
    fn array(&mut self, depth: u32) -> Result<Value<'a>, DecodeError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.buf.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            if items.len() >= self.limits.max_items {
                return limit_err(
                    ErrorCode::MaxItems,
                    self.limits.max_items as u64,
                    format!(
                        "array longer than {} items at byte {}",
                        self.limits.max_items, self.pos
                    ),
                );
            }
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.buf.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    match self.buf.get(self.pos) {
                        Some(b']') => {
                            return self
                                .fail(JsonErrorCode::TrailingComma, "trailing comma in array");
                        }
                        Some(_) => {}
                        None => return self.fail(JsonErrorCode::Eof, "unterminated array"),
                    }
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                Some(_) => return self.fail(JsonErrorCode::UnexpectedChar, "expected ',' or ']'"),
                None => return self.fail(JsonErrorCode::Eof, "unterminated array"),
            }
        }
    }

    /// `{` at the current position → `Value::Object` (pairs in input
    /// order, duplicates kept — see module docs for the last-wins rule).
    fn object(&mut self, depth: u32) -> Result<Value<'a>, DecodeError> {
        self.pos += 1;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.buf.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            if pairs.len() >= self.limits.max_items {
                return limit_err(
                    ErrorCode::MaxItems,
                    self.limits.max_items as u64,
                    format!(
                        "object has more than {} entries at byte {}",
                        self.limits.max_items, self.pos
                    ),
                );
            }
            match self.buf.get(self.pos) {
                Some(b'"') => {}
                Some(_) => return self.fail(JsonErrorCode::UnexpectedChar, "expected object key"),
                None => return self.fail(JsonErrorCode::Eof, "unterminated object"),
            }
            let key = self.string()?;
            self.skip_ws();
            match self.buf.get(self.pos) {
                Some(b':') => self.pos += 1,
                Some(_) => {
                    return self.fail(
                        JsonErrorCode::UnexpectedChar,
                        "expected ':' after object key",
                    );
                }
                None => return self.fail(JsonErrorCode::Eof, "unterminated object"),
            }
            self.skip_ws();
            let v = self.value(depth + 1)?;
            pairs.push((key, v));
            self.skip_ws();
            match self.buf.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    match self.buf.get(self.pos) {
                        Some(b'}') => {
                            return self
                                .fail(JsonErrorCode::TrailingComma, "trailing comma in object");
                        }
                        Some(_) => {}
                        None => return self.fail(JsonErrorCode::Eof, "unterminated object"),
                    }
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                Some(_) => return self.fail(JsonErrorCode::UnexpectedChar, "expected ',' or '}'"),
                None => return self.fail(JsonErrorCode::Eof, "unterminated object"),
            }
        }
    }

    /// `"` at the current position → the decoded string.
    ///
    /// Fast path: scan for `"`, `\` or a raw control byte. Bytes `≥0x80`
    /// just advance the scan — UTF-8 continuation bytes never collide
    /// with `"`/`\`, so finding the terminator bytewise is sound, and one
    /// `from_utf8` check at the end validates the whole slice.
    fn string(&mut self) -> Result<Cow<'a, str>, DecodeError> {
        let start = self.pos;
        self.pos += 1;
        let content_start = self.pos;
        let mut i = content_start;
        loop {
            let Some(&b) = self.buf.get(i) else {
                return malformed(JsonErrorCode::Eof, start, "unterminated string");
            };
            match b {
                b'"' => {
                    self.pos = i + 1;
                    let s = match std::str::from_utf8(&self.buf[content_start..i]) {
                        Ok(s) => s,
                        Err(e) => {
                            return malformed(
                                JsonErrorCode::InvalidUtf8,
                                content_start + e.valid_up_to(),
                                "invalid UTF-8 in string",
                            );
                        }
                    };
                    return self.finish_str(s.len(), start).map(|()| Cow::Borrowed(s));
                }
                b'\\' => return self.string_slow(start, content_start, i),
                b if b < 0x20 => {
                    return malformed(
                        JsonErrorCode::ControlChar,
                        i,
                        "unescaped control character in string",
                    );
                }
                _ => i += 1,
            }
        }
    }

    /// The escape path: `i` sits on the first `\`. Raw runs between
    /// escapes are copied after a `from_utf8` check; decoded output is
    /// never longer than the raw form, so capacity is bounded by input.
    fn string_slow(
        &mut self,
        start: usize,
        content_start: usize,
        mut i: usize,
    ) -> Result<Cow<'a, str>, DecodeError> {
        let mut out = String::with_capacity(i - content_start + 8);
        let mut seg_start = content_start;
        loop {
            let Some(&b) = self.buf.get(i) else {
                return malformed(JsonErrorCode::Eof, start, "unterminated string");
            };
            match b {
                b'"' => {
                    self.push_raw(&mut out, seg_start, i)?;
                    self.pos = i + 1;
                    return self.finish_str(out.len(), start).map(|()| Cow::Owned(out));
                }
                b'\\' => {
                    self.push_raw(&mut out, seg_start, i)?;
                    i += 1;
                    let Some(&e) = self.buf.get(i) else {
                        return malformed(JsonErrorCode::Eof, start, "unterminated string");
                    };
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let (c, next) = self.unicode_escape(i)?;
                            out.push(c);
                            i = next;
                            seg_start = i;
                            continue;
                        }
                        _ => {
                            return malformed(
                                JsonErrorCode::InvalidEscape,
                                i,
                                "invalid escape sequence",
                            );
                        }
                    }
                    i += 1;
                    seg_start = i;
                }
                b if b < 0x20 => {
                    return malformed(
                        JsonErrorCode::ControlChar,
                        i,
                        "unescaped control character in string",
                    );
                }
                _ => i += 1,
            }
        }
    }

    /// `\\uXXXX` at `u_pos` (the `u`), including surrogate-pair joining.
    /// Returns the char and the offset after the escape.
    fn unicode_escape(&self, u_pos: usize) -> Result<(char, usize), DecodeError> {
        let hi = self.hex4(u_pos + 1)?;
        if (0xD800..0xDC00).contains(&hi) {
            if self.buf.get(u_pos + 5) == Some(&b'\\') && self.buf.get(u_pos + 6) == Some(&b'u') {
                let lo = self.hex4(u_pos + 7)?;
                if (0xDC00..0xE000).contains(&lo) {
                    let cp = 0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                    return match char::from_u32(cp) {
                        Some(c) => Ok((c, u_pos + 11)),
                        None => malformed(
                            JsonErrorCode::InvalidUnicode,
                            u_pos,
                            "invalid unicode escape",
                        ),
                    };
                }
                return malformed(
                    JsonErrorCode::InvalidUnicode,
                    u_pos + 5,
                    "expected a low surrogate escape (\\uDC00–\\uDFFF)",
                );
            }
            return malformed(
                JsonErrorCode::InvalidUnicode,
                u_pos,
                "lone high surrogate escape",
            );
        }
        if (0xDC00..0xE000).contains(&hi) {
            return malformed(
                JsonErrorCode::InvalidUnicode,
                u_pos,
                "lone low surrogate escape",
            );
        }
        match char::from_u32(u32::from(hi)) {
            Some(c) => Ok((c, u_pos + 5)),
            None => malformed(
                JsonErrorCode::InvalidUnicode,
                u_pos,
                "invalid unicode escape",
            ),
        }
    }

    /// Exactly four hex digits at `at`.
    fn hex4(&self, at: usize) -> Result<u16, DecodeError> {
        let mut v: u16 = 0;
        for k in 0..4 {
            let Some(&b) = self.buf.get(at + k) else {
                return malformed(
                    JsonErrorCode::InvalidUnicode,
                    at + k,
                    "truncated \\u escape",
                );
            };
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => {
                    return malformed(
                        JsonErrorCode::InvalidUnicode,
                        at + k,
                        "invalid hex digit in \\u escape",
                    );
                }
            };
            v = v * 16 + u16::from(d);
        }
        Ok(v)
    }

    /// Copy the raw run `buf[from..to]` into `out` after UTF-8 checking.
    fn push_raw(&self, out: &mut String, from: usize, to: usize) -> Result<(), DecodeError> {
        if to <= from {
            return Ok(());
        }
        match std::str::from_utf8(&self.buf[from..to]) {
            Ok(s) => {
                out.push_str(s);
                Ok(())
            }
            Err(e) => malformed(
                JsonErrorCode::InvalidUtf8,
                from + e.valid_up_to(),
                "invalid UTF-8 in string",
            ),
        }
    }

    /// The `max_str_bytes` bound on a finished string, decoded length.
    fn finish_str(&self, len: usize, at: usize) -> Result<(), DecodeError> {
        if len > self.limits.max_str_bytes {
            return limit_err(
                ErrorCode::MaxStrLen,
                self.limits.max_str_bytes as u64,
                format!(
                    "string longer than {} bytes at byte {}",
                    self.limits.max_str_bytes, at
                ),
            );
        }
        Ok(())
    }
}

/// The integer value of a JSON number token matching `-?(0|[1-9]D*)`.
/// Narrowest wins: `i64` → `u64` → `BigInt` (raw text — already canonical
/// since the grammar forbids leading zeros and `+`).
fn int_value(text: &str) -> Value<'_> {
    let (neg, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, text),
    };
    let mut mag: u64 = 0;
    for &d in digits.as_bytes() {
        match mag
            .checked_mul(10)
            .and_then(|m| m.checked_add(u64::from(d - b'0')))
        {
            Some(m) => mag = m,
            None => return Value::BigInt(Cow::Borrowed(text)),
        }
    }
    if neg {
        match i64::try_from(mag) {
            Ok(i) => Value::Int(-i), // "-0" → 0, like `json.loads`
            Err(_) if mag == 1u64 << 63 => Value::Int(i64::MIN),
            Err(_) => Value::BigInt(Cow::Borrowed(text)),
        }
    } else {
        match i64::try_from(mag) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::UInt(mag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Value<'_> {
        decode_json(text.as_bytes()).expect("decode should succeed")
    }

    fn dec_lim<'a>(text: &'a str, limits: &Limits) -> Result<Value<'a>, DecodeError> {
        decode_json_with_limits(text.as_bytes(), limits)
    }

    #[test]
    fn scalars() {
        assert_eq!(dec("null"), Value::Null);
        assert_eq!(dec(" true "), Value::Bool(true));
        assert_eq!(dec("false"), Value::Bool(false));
        assert_eq!(dec("0"), Value::Int(0));
        assert_eq!(dec("-0"), Value::Int(0));
        assert_eq!(dec("123"), Value::Int(123));
        assert_eq!(dec("-9223372036854775808"), Value::Int(i64::MIN));
        assert_eq!(dec("9223372036854775807"), Value::Int(i64::MAX));
        assert_eq!(
            dec("9223372036854775808"),
            Value::UInt(9_223_372_036_854_775_808)
        );
        assert_eq!(dec("18446744073709551615"), Value::UInt(u64::MAX));
        assert_eq!(
            dec("18446744073709551616"),
            Value::BigInt(Cow::Borrowed("18446744073709551616"))
        );
        assert_eq!(
            dec("-18446744073709551616"),
            Value::BigInt(Cow::Borrowed("-18446744073709551616"))
        );
        assert_eq!(dec("1.5"), Value::Float(1.5));
        assert_eq!(dec("-0.5"), Value::Float(-0.5));
        assert_eq!(dec("1e3"), Value::Float(1000.0));
        assert_eq!(dec("2.5e-1"), Value::Float(0.25));
        let Value::Float(f) = dec("1e999") else {
            panic!("expected float")
        };
        assert!(f.is_infinite() && f > 0.0);
        let Value::Float(f) = dec("-1e999") else {
            panic!("expected float")
        };
        assert!(f.is_infinite() && f < 0.0);
        assert_eq!(dec("\"abc\""), Value::str("abc"));
        assert_eq!(dec("\"\""), Value::str(""));
    }

    #[test]
    fn strings() {
        assert_eq!(
            dec(r#""a\nb\t\"c\\d\/e\bf\fg\rh""#),
            Value::str("a\nb\t\"c\\d/e\u{8}f\u{c}g\rh")
        );
        assert_eq!(dec(r#""Aé😀""#), Value::str("Aé\u{1F600}"));
        assert_eq!(dec(r#""héllo ☃""#), Value::str("héllo ☃"));
        // Borrowed when no escapes; owned when escaped.
        match dec("\"plain\"") {
            Value::Str(Cow::Borrowed(_)) => {}
            v => panic!("expected borrowed str, got {v:?}"),
        }
        match dec(r#""a\nb""#) {
            Value::Str(Cow::Owned(_)) => {}
            v => panic!("expected owned str, got {v:?}"),
        }
    }

    #[test]
    fn containers() {
        let v = dec(r#"{"a": [1, 2.5, "x", null, true], "b": {"c": []}}"#);
        let Value::Object(pairs) = &v else {
            panic!("expected object")
        };
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            v.get("a"),
            Some(&Value::Array(vec![
                Value::Int(1),
                Value::Float(2.5),
                Value::str("x"),
                Value::Null,
                Value::Bool(true),
            ]))
        );
        assert_eq!(dec("[]"), Value::Array(vec![]));
        assert_eq!(dec("{}"), Value::Object(vec![]));
        assert_eq!(dec(" [ ] "), Value::Array(vec![]));
        // Duplicate keys kept; last wins on lookup (documented policy).
        let v = dec(r#"{"a": 1, "a": 2}"#);
        assert_eq!(v.as_object().map(<[_]>::len), Some(2));
        assert_eq!(v.get("a"), Some(&Value::Int(2)));
    }

    #[test]
    fn malformed() {
        let cases: &[(&[u8], JsonErrorCode)] = &[
            (b"", JsonErrorCode::Eof),
            (b"   ", JsonErrorCode::Eof),
            (b"nul", JsonErrorCode::UnexpectedChar),
            (b"null0", JsonErrorCode::TrailingData),
            (b"NaN", JsonErrorCode::UnexpectedChar),
            (b"Infinity", JsonErrorCode::UnexpectedChar),
            (b"+1", JsonErrorCode::UnexpectedChar),
            (b"-Infinity", JsonErrorCode::InvalidNumber),
            (b"01", JsonErrorCode::InvalidNumber),
            (b"-01", JsonErrorCode::InvalidNumber),
            (b"1.", JsonErrorCode::InvalidNumber),
            (b".5", JsonErrorCode::UnexpectedChar),
            (b"1e", JsonErrorCode::InvalidNumber),
            (b"1e+", JsonErrorCode::InvalidNumber),
            (b"--1", JsonErrorCode::InvalidNumber),
            (b"-", JsonErrorCode::InvalidNumber),
            (b"[1,]", JsonErrorCode::TrailingComma),
            (b"[1,2", JsonErrorCode::Eof),
            (b"['a']", JsonErrorCode::UnexpectedChar),
            (br#"{"a":1,}"#, JsonErrorCode::TrailingComma),
            (br#"{"a":1"#, JsonErrorCode::Eof),
            (br"{a:1}", JsonErrorCode::UnexpectedChar),
            (br#"{"a"}"#, JsonErrorCode::UnexpectedChar),
            (br#"{"a":}"#, JsonErrorCode::UnexpectedChar),
            (br#"{"a":1 "b":2}"#, JsonErrorCode::UnexpectedChar),
            (b"[1 2]", JsonErrorCode::UnexpectedChar),
            (br#""abc"#, JsonErrorCode::Eof),
            (br#""a\zb""#, JsonErrorCode::InvalidEscape),
            (br#""a\u12""#, JsonErrorCode::InvalidUnicode),
            (br#""\uD800""#, JsonErrorCode::InvalidUnicode),
            (br#""\uD800\u0041""#, JsonErrorCode::InvalidUnicode),
            (br#""\uDC00""#, JsonErrorCode::InvalidUnicode),
            (b"\"a\x01b\"", JsonErrorCode::ControlChar),
            (b"\"\xFF\xFE\"", JsonErrorCode::InvalidUtf8),
            (b"1 2", JsonErrorCode::TrailingData),
            (b"[] x", JsonErrorCode::TrailingData),
            (b"[}", JsonErrorCode::UnexpectedChar),
            (b"{]", JsonErrorCode::UnexpectedChar),
        ];
        for &(input, code) in cases {
            let Err(DecodeError::Malformed(e)) = decode_json(input) else {
                panic!("expected malformed error for {input:?}")
            };
            assert_eq!(e.code, code, "{input:?}");
            assert!(e.position <= input.len(), "{input:?}");
            assert_eq!(decode_json(input).map(|_| ()).unwrap_err().status(), 400);
        }
    }

    #[test]
    fn limits() {
        let deep = "[".repeat(70) + &"]".repeat(70);
        match decode_json(deep.as_bytes()) {
            Err(DecodeError::Limit(e)) => {
                assert_eq!(e.entries()[0].code, ErrorCode::MaxDepth);
                assert_eq!(e.entries()[0].pointer(), "/body");
            }
            r => panic!("expected depth limit, got {r:?}"),
        }
        let ok = "[".repeat(63) + &"]".repeat(63);
        assert!(decode_json(ok.as_bytes()).is_ok());
        // Depth and items under custom limits.
        let limits = Limits {
            max_depth: 3,
            max_items: 3,
            max_str_bytes: 4,
            ..Limits::default()
        };
        assert!(dec_lim("[[1]]", &limits).is_ok());
        assert!(matches!(
            dec_lim("[[[1]]]", &limits),
            Err(DecodeError::Limit(_))
        ));
        assert!(dec_lim("[1,2,3]", &limits).is_ok());
        assert!(matches!(
            dec_lim("[1,2,3,4]", &limits),
            Err(DecodeError::Limit(_))
        ));
        assert!(matches!(
            dec_lim(r#"{"a":1,"b":2,"c":3,"d":4}"#, &limits),
            Err(DecodeError::Limit(_))
        ));
        assert!(dec_lim(r#""abcd""#, &limits).is_ok());
        assert!(matches!(
            dec_lim(r#""abcde""#, &limits),
            Err(DecodeError::Limit(_))
        ));
        // Escaped strings count their decoded length.
        assert!(dec_lim(r#""ab\u0063d""#, &limits).is_ok()); // "abcd" = 4
        assert!(matches!(
            dec_lim(r#""ab\u0063de""#, &limits),
            Err(DecodeError::Limit(_))
        ));
        // The default 100_000-item cap trips before the vec grows huge.
        let big = format!("[{}0]", "0,".repeat(100_001));
        match decode_json(big.as_bytes()) {
            Err(DecodeError::Limit(e)) => {
                assert_eq!(e.entries()[0].code, ErrorCode::MaxItems);
            }
            r => panic!("expected items limit, got {r:?}"),
        }
    }

    #[test]
    fn error_codes_and_status() {
        let e = JsonError {
            code: JsonErrorCode::Eof,
            position: 0,
            message: "x".to_owned(),
        };
        assert_eq!(e.code.as_str(), "eof");
        assert_eq!(format!("{e}"), "x (byte 0)");
        assert_eq!(DecodeError::from(e).status(), 400);
    }

    #[test]
    fn decode_and_validate_tightens() {
        use crate::compile::compile_with_limits;
        use crate::desc::TypeDesc;

        // §14.8: a caller-supplied bound tightens — it can never loosen
        // the validator's compiled-in set.
        let validator = crate::compile::compile(&TypeDesc::any()).expect("compile");
        let tight = Limits {
            max_items: 2,
            ..Limits::default()
        };
        match decode_and_validate_with_limits(b"[1,2,3]", &validator, &tight) {
            Err(PayloadError::Limit(e)) => {
                assert_eq!(e.entries()[0].code, ErrorCode::MaxItems);
            }
            r => panic!("expected items limit, got {r:?}"),
        }
        assert!(decode_and_validate_with_limits(b"[1,2]", &validator, &tight).is_ok());
        // The default bound leaves the payload alone.
        assert!(decode_and_validate(b"[1,2,3]", &validator).is_ok());

        // A looser caller bound never wins over the validator's own.
        let strict = compile_with_limits(
            &TypeDesc::any(),
            Limits {
                max_items: 1,
                ..Limits::default()
            },
        )
        .expect("compile");
        assert!(matches!(
            decode_and_validate_with_limits(b"[1,2]", &strict, &Limits::default()),
            Err(PayloadError::Limit(_))
        ));
        assert!(decode_and_validate_with_limits(b"[1]", &strict, &Limits::default()).is_ok());
    }
}
