//! Validation error collection (DESIGN.md §11.7).
//!
//! Every problem found during one validation pass is collected — a 422
//! lists *all* of them, not the first. Each [`ErrorEntry`] carries:
//!
//! - `source`: which part of the request the bad value came from
//!   (`body`, `query`, `path`, `header`, `cookie`, `form`, `response`);
//! - `loc`: the path inside that part as [`LocSeg`]s — `["address",
//!   "city"]`, `["items", 3]` — rendered by [`ErrorEntry::pointer`] as a
//!   JSON-pointer-ish string: `"/body/address/city"`, `"/query/page"`;
//! - `code`: a machine-readable [`ErrorCode`] (`"ge"`, `"email"`,
//!   `"missing"`, ...);
//! - `message`: the human sentence (`"must be >= 1"`);
//! - `limit`: the violated bound as a [`LiteralValue`], when one applies
//!   (`"limit": 1`).
//!
//! Input values are never embedded in errors — they may contain secrets
//! (§11.7).

use std::fmt;

use crate::value::{LiteralValue, escape_json};

/// Which part of the request a validation error came from — the first
/// segment of the rendered `loc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// JSON body (`/body/…`).
    Body,
    /// Query string (`/query/…`).
    Query,
    /// Path parameters (`/path/…`).
    Path,
    /// Headers (`/header/…`).
    Header,
    /// Cookies (`/cookie/…`).
    Cookie,
    /// Form fields (`/form/…`).
    Form,
    /// The handler's return value, when debug-mode output checking fails
    /// (§23.3).
    Response,
}

impl Source {
    /// The `loc` prefix string (`"body"`, `"query"`, ...).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Query => "query",
            Self::Path => "path",
            Self::Header => "header",
            Self::Cookie => "cookie",
            Self::Form => "form",
            Self::Response => "response",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One segment of an error's `loc` path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocSeg {
    /// An object key / field name (`/body/address/city`).
    Key(Box<str>),
    /// An array index (`/body/items/3`).
    Index(u32),
}

impl LocSeg {
    /// The JSON-pointer escaped form (`~`→`~0`, `/`→`~1`).
    fn write_escaped(&self, out: &mut String) {
        out.push('/');
        match self {
            Self::Key(k) => {
                for c in k.chars() {
                    match c {
                        '~' => out.push_str("~0"),
                        '/' => out.push_str("~1"),
                        _ => out.push(c),
                    }
                }
            }
            Self::Index(i) => out.push_str(&i.to_string()),
        }
    }
}

/// The machine-readable `code` of an [`ErrorEntry`]. The string form is
/// short and lowercase, matching the §11.7 examples (`"ge"`, `"email"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The input's shape cannot fit the type at all (`"expected object,
    /// got string"`).
    Type,
    /// A required field was absent.
    Missing,
    /// `extra=forbid` rejected an unknown key.
    ExtraForbidden,
    /// No `Literal` member matched.
    Literal,
    /// No enum member matched.
    Enum,
    /// No union variant matched.
    Union,
    /// A discriminated union's tag field was absent.
    UnionTagMissing,
    /// A discriminated union's tag value named no variant.
    UnionTagInvalid,
    /// `bool` coercion failed.
    Bool,
    /// `int` coercion failed (not `[+-]?D+`).
    Int,
    /// `float` coercion failed.
    Float,
    /// `Decimal` literal syntax failed.
    Decimal,
    /// `str` expected, got another shape.
    Str,
    /// `bytes` expected, got another shape.
    Bytes,
    /// Expected `None`, got a value.
    Null,
    /// RFC 3339 `datetime` parse failed.
    DateTime,
    /// ISO 8601 `date` parse failed.
    Date,
    /// ISO 8601 `time` parse failed.
    Time,
    /// ISO 8601 `timedelta` parse failed.
    TimeDelta,
    /// `UUID` parse failed.
    Uuid,
    /// `Email` check failed.
    Email,
    /// `Url` check failed.
    Url,
    /// `IPv4Address` parse failed.
    Ipv4,
    /// `IPv6Address` parse failed.
    Ipv6,
    /// `len` minimum violated (`limit` = min).
    MinLen,
    /// `len` maximum violated (`limit` = max).
    MaxLen,
    /// `ge` bound violated (`limit` = bound).
    Ge,
    /// `gt` bound violated (`limit` = bound).
    Gt,
    /// `le` bound violated (`limit` = bound).
    Le,
    /// `lt` bound violated (`limit` = bound).
    Lt,
    /// `multiple_of` violated (`limit` = operand).
    MultipleOf,
    /// `pattern` did not match (message carries the pattern source).
    Pattern,
    /// `Unique` found a duplicate item.
    Unique,
    /// A `dict` key failed the key descriptor (`"invalid key: …"`).
    DictKey,
    /// A `tuple[A, B]` got the wrong arity (`limit` = expected arity).
    TupleLen,
    /// The input is nested deeper than `Limits::max_depth`.
    MaxDepth,
    /// A container/object exceeds `Limits::max_items`.
    MaxItems,
    /// A string exceeds `Limits::max_str_bytes`.
    MaxStrLen,
}

impl ErrorCode {
    /// The wire code string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Missing => "missing",
            Self::ExtraForbidden => "extra_forbidden",
            Self::Literal => "literal",
            Self::Enum => "enum",
            Self::Union => "union",
            Self::UnionTagMissing => "union_tag_missing",
            Self::UnionTagInvalid => "union_tag_invalid",
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::Str => "str",
            Self::Bytes => "bytes",
            Self::Null => "null",
            Self::DateTime => "datetime",
            Self::Date => "date",
            Self::Time => "time",
            Self::TimeDelta => "timedelta",
            Self::Uuid => "uuid",
            Self::Email => "email",
            Self::Url => "url",
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
            Self::MinLen => "min_len",
            Self::MaxLen => "max_len",
            Self::Ge => "ge",
            Self::Gt => "gt",
            Self::Le => "le",
            Self::Lt => "lt",
            Self::MultipleOf => "multiple_of",
            Self::Pattern => "pattern",
            Self::Unique => "unique",
            Self::DictKey => "dict_key",
            Self::TupleLen => "tuple_len",
            Self::MaxDepth => "max_depth",
            Self::MaxItems => "max_items",
            Self::MaxStrLen => "max_str_len",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One collected validation error — the §11.7 `"errors"` member shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorEntry {
    /// Which part of the request the bad value came from.
    pub source: Source,
    /// Path inside the source, outermost first — *not* including the
    /// source itself, which `pointer()` prepends.
    pub loc: Vec<LocSeg>,
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human message (`"must be >= 1"`, `"not a valid email address"`).
    pub message: String,
    /// The violated bound, for bound-shaped codes (`ge`, `le`, `min_len`,
    /// `max_len`, `multiple_of`, `tuple_len`).
    pub limit: Option<LiteralValue>,
}

impl ErrorEntry {
    /// The JSON-pointer-ish `loc` string: `"/body/address/city"`,
    /// `"/query/page"`. Keys get `~`/`/` escaped per RFC 6901.
    pub fn pointer(&self) -> String {
        let mut out = String::with_capacity(16 + self.loc.len() * 8);
        out.push('/');
        out.push_str(self.source.as_str());
        for seg in &self.loc {
            seg.write_escaped(&mut out);
        }
        out
    }

    /// Alias for [`ErrorEntry::pointer`] — the wire `loc` string.
    pub fn loc_pointer(&self) -> String {
        self.pointer()
    }

    /// A borrowed, stable view of this entry for hand-written encoders
    /// (M2.10): `fulmen-py` serializes [`ProblemEntry`]s into the §11.7
    /// `errors` array without serde — fields walk straight into the
    /// JSON writer.
    pub fn problem(&self) -> ProblemEntry<'_> {
        ProblemEntry {
            loc_pointer: self.pointer(),
            source: self.source,
            loc: &self.loc,
            code: self.code.as_str(),
            message: &self.message,
            limit: self.limit.as_ref(),
        }
    }
}

/// A stable serialization view of one [`ErrorEntry`] — the §11.7
/// `"errors"` member shape for encoders that do not use serde.
///
/// `loc` is offered twice on purpose: `loc_pointer` is the rendered
/// JSON pointer (`"/body/email"`) for the wire, while `loc` keeps the
/// typed [`LocSeg`]s (excluding the `source` prefix) for programmatic
/// walks such as `OpenAPI` or client-side error mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct ProblemEntry<'a> {
    /// `loc` rendered as a JSON pointer including the source prefix.
    pub loc_pointer: String,
    /// Which part of the request the error came from.
    pub source: Source,
    /// `loc` as typed segments (excluding `source`).
    pub loc: &'a [LocSeg],
    /// The machine code string ([`ErrorCode::as_str`]).
    pub code: &'static str,
    /// The human message — never echoes input values (§11.7).
    pub message: &'a str,
    /// The violated bound for bound-shaped codes, renderable via
    /// [`LiteralValue::to_json`].
    pub limit: Option<&'a LiteralValue>,
}

/// The collector validators push into (§11.7 — all errors, one pass).
///
/// Capped at [`MAX_ERRORS`]: once full, further failures only set
/// `truncated`, so a hostile body cannot exhaust memory through the
/// error list itself.
#[derive(Debug)]
pub struct ValidationErrors {
    entries: Vec<ErrorEntry>,
    truncated: bool,
}

/// Hard cap on collected errors.
pub const MAX_ERRORS: usize = 100;

impl Default for ValidationErrors {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationErrors {
    /// An empty collector.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            truncated: false,
        }
    }

    /// Append `entry`; once [`MAX_ERRORS`] is reached the entry is dropped
    /// and `truncated` is set instead.
    pub fn push(&mut self, entry: ErrorEntry) {
        if self.entries.len() >= MAX_ERRORS {
            self.truncated = true;
            return;
        }
        self.entries.push(entry);
    }

    /// Convenience: build-and-push in one call.
    pub fn fail(
        &mut self,
        source: Source,
        loc: Vec<LocSeg>,
        code: ErrorCode,
        message: impl Into<String>,
        limit: Option<LiteralValue>,
    ) {
        self.push(ErrorEntry {
            source,
            loc,
            code,
            message: message.into(),
            limit,
        });
    }

    /// Whether nothing was collected — `true` means the input validated.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of collected entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether entries were dropped past [`MAX_ERRORS`].
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The entries, in collection order.
    pub fn entries(&self) -> &[ErrorEntry] {
        &self.entries
    }

    /// Consume into the entry list.
    pub fn into_entries(self) -> Vec<ErrorEntry> {
        self.entries
    }

    /// The §11.7 `errors` array rendered as a JSON string — a convenience
    /// for callers that do not walk entries themselves. Strings are
    /// JSON-escaped; `limit` renders via [`LiteralValue::to_json`].
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(self.entries.len() * 64 + 2);
        out.push('[');
        for (i, e) in self.entries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"loc\":\"");
            out.push_str(&escape_json(&e.pointer()));
            out.push_str("\",\"code\":\"");
            out.push_str(e.code.as_str());
            out.push_str("\",\"message\":\"");
            out.push_str(&escape_json(&e.message));
            out.push('"');
            if let Some(l) = &e.limit {
                out.push_str(",\"limit\":");
                out.push_str(&l.to_json());
            }
            out.push('}');
        }
        out.push(']');
        out
    }

    /// The entries as [`ProblemEntry`] views — a stable, serde-free
    /// shape for `fulmen-py`'s hand-written JSON output (M2.10).
    pub fn to_problem_entries(&self) -> Vec<ProblemEntry<'_>> {
        self.entries.iter().map(ErrorEntry::problem).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_rendering() {
        let e = ErrorEntry {
            source: Source::Body,
            loc: vec![LocSeg::Key("address".into()), LocSeg::Key("city".into())],
            code: ErrorCode::Missing,
            message: "required field missing".to_owned(),
            limit: None,
        };
        assert_eq!(e.pointer(), "/body/address/city");
        let e = ErrorEntry {
            source: Source::Query,
            loc: vec![LocSeg::Key("page".into())],
            code: ErrorCode::Ge,
            message: "must be >= 1".to_owned(),
            limit: Some(LiteralValue::Int(1)),
        };
        assert_eq!(e.pointer(), "/query/page");
        // Escaping.
        let e = ErrorEntry {
            source: Source::Body,
            loc: vec![LocSeg::Key("a/b~c".into()), LocSeg::Index(3)],
            code: ErrorCode::Type,
            message: String::new(),
            limit: None,
        };
        assert_eq!(e.pointer(), "/body/a~1b~0c/3");
    }

    #[test]
    fn collect_and_cap() {
        let mut errs = ValidationErrors::new();
        assert!(errs.is_empty());
        for i in 0..MAX_ERRORS + 5 {
            errs.fail(
                Source::Body,
                vec![LocSeg::Index(u32::try_from(i).unwrap_or(u32::MAX))],
                ErrorCode::Type,
                "nope",
                None,
            );
        }
        assert_eq!(errs.len(), MAX_ERRORS);
        assert!(errs.truncated());
        assert_eq!(errs.entries()[0].pointer(), "/body/0");
    }

    #[test]
    fn json_rendering() {
        let mut errs = ValidationErrors::new();
        errs.fail(
            Source::Body,
            vec![LocSeg::Key("email".into())],
            ErrorCode::Email,
            "not a valid email address",
            None,
        );
        errs.fail(
            Source::Query,
            vec![LocSeg::Key("page".into())],
            ErrorCode::Ge,
            "must be >= 1",
            Some(LiteralValue::Int(1)),
        );
        assert_eq!(
            errs.to_json(),
            r#"[{"loc":"/body/email","code":"email","message":"not a valid email address"},{"loc":"/query/page","code":"ge","message":"must be >= 1","limit":1}]"#
        );
    }
}
