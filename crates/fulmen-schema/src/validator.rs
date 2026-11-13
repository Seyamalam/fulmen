//! The compiled validator tree and its `validate` implementations.
//!
//! [`crate::compile`] produces a [`Validator`] holding a [`VNode`] tree —
//! the same shape as [`crate::desc::TypeDesc`] but specialized: regexes and
//! numeric bounds are parsed, record wire keys and union tags are indexed,
//! `Generic`/`TypeVar` nodes are gone. The types in this module are
//! `pub(crate)`; the public surface is [`Validator::validate`].
//!
//! # Output convention
//!
//! Internally each node reports [`Outcome::Same`] (*the input passed
//! unchanged*) or `Outcome::New` (a coerced/canonicalized value —
//! `Int(1)` for lax `"1"`, canonical RFC 3339 text for datetimes, an
//! `Object` with field names for records). [`Validator::validate`] maps
//! that to `Cow<'v, Value<'v>>`: `Borrowed` = unchanged input (the strict
//! happy path is zero-copy), `Owned` = the new value. The materializer in
//! `fulmen-py` consumes the output per-kind, independent of the variant.
//!
//! # Error convention
//!
//! Every failure pushes into [`ValidationErrors`] — records keep going
//! after a bad field, unions try every variant (each against a scratch
//! buffer, so failed-variant noise never reaches the caller), containers
//! visit every item. A node that pushed an error returns `None`; a node
//! returning `Some` pushed nothing.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use crate::compile::Limits;
use crate::desc::{
    ClassRef, EnumKind, EnumMember, ExtraPolicy, Len, Missing, Pattern, RecordKind, Transforms,
};
use crate::errors::{ErrorCode, ErrorEntry, LocSeg, Source, ValidationErrors};
use crate::num::{DecLit, LitNum};
use crate::parse;
use crate::value::{LiteralValue, Value, literal_eq, normalize_int_text};

/// Strict vs lax validation — see DESIGN.md §11.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// JSON body: no coercion — `"1"` is not an int.
    Strict,
    /// Query/path/header/cookie/form: scalars arrive as strings and are
    /// coerced — `"1"` → `1`, `"true"` → `true`. `list`/`set`/`tuple[...]`
    /// also promote a lone scalar to a one-element container
    /// (`?tags=a` → `["a"]`).
    Lax,
}

/// A compiled descriptor — validates `Value`s against its `TypeDesc`.
///
/// The tree is immutable after compile, so `Validator` is `Send`/`Sync`.
pub struct Validator {
    root: VNode,
    limits: Limits,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Validator {
    pub(crate) fn new(root: VNode, limits: Limits) -> Self {
        Self { root, limits }
    }

    /// The limits this validator was compiled with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Validate `input` from `source` under `mode`.
    ///
    /// `Ok(cow)` — the value passed all checks; `Err(errors)` — every
    /// problem found, in [`ValidationErrors`] order. See the module docs
    /// for the `Cow` convention.
    ///
    /// # Errors
    ///
    /// [`ValidationErrors`] collects every violation found — field and
    /// item errors keep validating their siblings, so one bad call
    /// reports all of them (capped at [`crate::MAX_ERRORS`]).
    pub fn validate<'v>(
        &self,
        input: &'v Value<'v>,
        source: Source,
        mode: Mode,
    ) -> Result<Cow<'v, Value<'v>>, ValidationErrors> {
        match self.validate_out(input, source, mode) {
            Ok(Checked::Same(input)) => Ok(Cow::Borrowed(input)),
            Ok(Checked::New(v)) => Ok(Cow::Owned(v)),
            Err(errs) => Err(errs),
        }
    }

    /// Validate `input` like [`Validator::validate`], but keep the
    /// output's buffer lifetime (`'i`) independent of the input borrow
    /// (`'v`): a [`Checked::New`] result is a `Value<'i>` that outlives
    /// the borrow of `input`.
    ///
    /// This is the form decode-and-validate paths need: the decoded
    /// `Value` is typically a local, so `Cow::Borrowed(&local)` and a
    /// rewritten `Value<'borrow-of-local>` could never leave the call.
    /// With `Checked`, `Same` means "the input passed" and callers move
    /// or drop their own `Value`, while `New` is returned with the full
    /// buffer lifetime.
    ///
    /// # Errors
    ///
    /// Same contract as [`Validator::validate`].
    pub fn validate_out<'v, 'i>(
        &self,
        input: &'v Value<'i>,
        source: Source,
        mode: Mode,
    ) -> Result<Checked<'v, 'i>, ValidationErrors> {
        self.validate_out_with_limits(input, source, mode, &self.limits)
    }

    /// `validate_out` under `limits`, never looser than the compiled-in
    /// set: the walk enforces `self.limits.tightened(limits)` (DESIGN.md
    /// §14.8 — a caller can only make validation stricter, never looser).
    ///
    /// # Errors
    ///
    /// Same contract as [`Validator::validate`].
    pub fn validate_out_with_limits<'v, 'i>(
        &self,
        input: &'v Value<'i>,
        source: Source,
        mode: Mode,
        limits: &Limits,
    ) -> Result<Checked<'v, 'i>, ValidationErrors> {
        let effective = self.limits.tightened(limits);
        let mut ctx = Ctx {
            source,
            mode,
            limits: &effective,
            depth: 0,
            loc: Vec::new(),
            errs: ValidationErrors::new(),
        };
        let out = self.root.check_value(input, &mut ctx);
        if ctx.errs.is_empty() {
            Ok(match out {
                Some(Outcome::Same) | None => Checked::Same(input),
                Some(Outcome::New(v)) => Checked::New(v),
            })
        } else {
            Err(ctx.errs)
        }
    }
}

/// The outcome of [`Validator::validate_out`]: the `Same`/`New` split of
/// the internal `Outcome`, but `New` carries the input's buffer lifetime
/// `'i` rather than the borrow `'v`, so rewritten values escape the call
/// site (see the method's docs).
#[derive(Debug)]
pub enum Checked<'v, 'i> {
    /// Every check passed on the input unchanged — the strict happy
    /// path. The input is handed back for convenience; callers that own
    /// it can equally keep using their own value.
    Same(&'v Value<'i>),
    /// Validation rewrote the value — lax coercions, canonicalized
    /// datetimes/IPs/decimals, canonical record keys, injected defaults,
    /// deduplicated sets.
    New(Value<'i>),
}

// ---- compiled node tree ----------------------------------------------------

/// A compiled node: kind + the transforms `Annotated` declared on it.
pub(crate) struct VNode {
    pub(crate) kind: VKind,
    pub(crate) transforms: Transforms,
}

/// Parsed numeric bounds for `int`/`float`/`Decimal`.
pub(crate) struct IntV {
    pub(crate) range: Option<RangeV>,
    pub(crate) multiple: Option<LitNum>,
}

/// A `Range` as parsed numerics.
pub(crate) struct RangeV {
    pub(crate) ge: Option<LitNum>,
    pub(crate) gt: Option<LitNum>,
    pub(crate) le: Option<LitNum>,
    pub(crate) lt: Option<LitNum>,
}

/// `str` with compiled checks.
pub(crate) struct StrV {
    pub(crate) len: Option<Len>,
    pub(crate) pattern: Option<Pattern>,
}

/// `bytes` with checks.
pub(crate) struct BytesV {
    pub(crate) len: Option<Len>,
}

/// `list`/`tuple[T, ...]`: item node + bounds.
pub(crate) struct ListV {
    pub(crate) item: Box<VNode>,
    pub(crate) len: Option<Len>,
    pub(crate) unique: bool,
}

/// `set`/`frozenset`.
pub(crate) struct SetV {
    pub(crate) item: Box<VNode>,
    pub(crate) len: Option<Len>,
}

/// `dict`.
pub(crate) struct DictV {
    pub(crate) key: Box<VNode>,
    pub(crate) value: Box<VNode>,
    pub(crate) len: Option<Len>,
}

/// `Literal` with a value → member index map for the strict fast path.
pub(crate) struct LiteralV {
    pub(crate) members: Box<[LiteralValue]>,
    pub(crate) index: HashMap<LiteralValue, usize>,
}

/// `Enum` with a value → member index map.
pub(crate) struct EnumV {
    /// Used in error messages ("not a member of X").
    pub(crate) class: ClassRef,
    /// Enum flavor — for the materializer, not validation.
    #[expect(
        dead_code,
        reason = "used by the materializer-side API, not validation"
    )]
    pub(crate) kind: EnumKind,
    pub(crate) members: Box<[EnumMember]>,
    pub(crate) by_value: HashMap<LiteralValue, usize>,
}

/// `Union` with the optional discriminator map.
pub(crate) struct UnionV {
    pub(crate) variants: Box<[VNode]>,
    /// The tag *field name* (for `union_tag_*` errors) when discriminated.
    pub(crate) tag: Option<Box<str>>,
    /// Tag value → variant index.
    pub(crate) tag_map: HashMap<Box<str>, usize>,
}

/// A record field, resolved: `wire_key` is the only accepted input key.
pub(crate) struct FieldV {
    pub(crate) name: Box<str>,
    pub(crate) wire_key: Box<str>,
    pub(crate) missing: Missing,
    pub(crate) node: VNode,
}

/// `Model`/`dataclass`/`TypedDict`.
pub(crate) struct RecordV {
    /// Retained for diagnostics; materialization reads the descriptor.
    #[expect(dead_code, reason = "class identity is diagnostics/OpenAPI data")]
    pub(crate) class: ClassRef,
    /// Materialization flavor — for `fulmen-py`, not validation.
    #[expect(
        dead_code,
        reason = "used by the materializer-side API, not validation"
    )]
    pub(crate) kind: RecordKind,
    pub(crate) extra: ExtraPolicy,
    pub(crate) fields: Box<[FieldV]>,
    /// `wire_key → fields` index.
    pub(crate) wire_index: HashMap<Box<str>, usize>,
}

/// The compiled node kinds — one per [`crate::desc::Kind`], minus
/// `Generic`/`TypeVar` (resolved at compile).
pub(crate) enum VKind {
    Bool,
    Int(IntV),
    Float(IntV),
    Str(StrV),
    Bytes(BytesV),
    Decimal(IntV),
    Null,
    DateTime,
    Date,
    Time,
    TimeDelta,
    Uuid,
    Email,
    Url,
    Ipv4,
    Ipv6,
    List(ListV),
    Set(SetV),
    FrozenSet(SetV),
    TupleVar(ListV),
    TupleFixed(Box<[VNode]>),
    Dict(DictV),
    Literal(LiteralV),
    Enum(EnumV),
    Optional(Box<VNode>),
    Union(UnionV),
    Record(RecordV),
    NewType(Box<VNode>),
    Any,
    Raw,
}

/// What a node returned: `Same` = the input passed through (the caller
/// emits `Cow::Borrowed`); `New` = a coerced/canonicalized value.
enum Outcome<'v> {
    Same,
    New(Value<'v>),
}

/// Per-call validation state.
struct Ctx<'a> {
    source: Source,
    mode: Mode,
    limits: &'a Limits,
    /// Current recursion depth — every node visit is one level.
    depth: u32,
    /// The `loc` path to the value under validation — pushed/popped by
    /// composite nodes around child visits; cloned into each error.
    loc: Vec<LocSeg>,
    errs: ValidationErrors,
}

impl Ctx<'_> {
    fn err(&mut self, code: ErrorCode, message: impl Into<String>, limit: Option<LiteralValue>) {
        self.errs.push(ErrorEntry {
            source: self.source,
            loc: self.loc.clone(),
            code,
            message: message.into(),
            limit,
        });
    }

    fn type_err(&mut self, expected: &str, input: &Value<'_>) {
        self.err(
            ErrorCode::Type,
            format!("expected {expected}, got {}", input.type_name()),
            None,
        );
    }
}

impl VNode {
    /// Validate `input`; `Some` → clean (`Same`/`New`), `None` → errors
    /// were pushed. Applies: depth guard → input guardrails → transforms →
    /// kind checks (the §11.2 ordering).
    fn check_value<'v>(&self, input: &Value<'v>, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
        ctx.depth += 1;
        let out = if ctx.depth > ctx.limits.max_depth {
            ctx.err(
                ErrorCode::MaxDepth,
                format!("nesting deeper than max_depth ({})", ctx.limits.max_depth),
                Some(LiteralValue::UInt(u64::from(ctx.limits.max_depth))),
            );
            None
        } else {
            self.check_shallow(input, ctx)
        };
        ctx.depth -= 1;
        out
    }

    fn check_shallow<'v>(&self, input: &Value<'v>, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
        // Input-shape guardrails (§14).
        match input {
            Value::Str(s) if s.len() > ctx.limits.max_str_bytes => {
                ctx.err(
                    ErrorCode::MaxStrLen,
                    format!("string longer than {} bytes", ctx.limits.max_str_bytes),
                    Some(LiteralValue::UInt(ctx.limits.max_str_bytes as u64)),
                );
                return None;
            }
            Value::Bytes(b) if b.len() > ctx.limits.max_str_bytes => {
                ctx.err(
                    ErrorCode::MaxStrLen,
                    format!("bytes longer than {} bytes", ctx.limits.max_str_bytes),
                    Some(LiteralValue::UInt(ctx.limits.max_str_bytes as u64)),
                );
                return None;
            }
            Value::Array(a) if a.len() > ctx.limits.max_items => {
                ctx.err(
                    ErrorCode::MaxItems,
                    format!("array longer than {} items", ctx.limits.max_items),
                    Some(LiteralValue::UInt(ctx.limits.max_items as u64)),
                );
                return None;
            }
            Value::Object(o) if o.len() > ctx.limits.max_items => {
                ctx.err(
                    ErrorCode::MaxItems,
                    format!("object has more than {} entries", ctx.limits.max_items),
                    Some(LiteralValue::UInt(ctx.limits.max_items as u64)),
                );
                return None;
            }
            _ => {}
        }
        // Transforms on a `Str` input run before any type/constraint check.
        if self.transforms.any()
            && let Value::Str(s) = input
        {
            let local = Value::Str(Cow::Owned(self.transforms.apply(s).into_owned()));
            return self.kind.check(&local, ctx).map(|o| match o {
                Outcome::Same => Outcome::New(local),
                Outcome::New(v) => Outcome::New(v),
            });
        }
        self.kind.check(input, ctx)
    }
}

/// `len` checks shared by strings (chars), bytes and containers (items).
/// Returns `false` when any bound failed (errors pushed).
fn check_len(len: Option<Len>, n: usize, unit: &str, ctx: &mut Ctx<'_>) -> bool {
    let Some(l) = len else { return true };
    let n = n as u64;
    let mut ok = true;
    if let Some(min) = l.min
        && n < min
    {
        ctx.err(
            ErrorCode::MinLen,
            format!("must have at least {min} {unit}"),
            Some(LiteralValue::UInt(min)),
        );
        ok = false;
    }
    if let Some(max) = l.max
        && n > max
    {
        ctx.err(
            ErrorCode::MaxLen,
            format!("must have at most {max} {unit}"),
            Some(LiteralValue::UInt(max)),
        );
        ok = false;
    }
    ok
}

/// `Range` + `MultipleOf` checks shared by the numeric kinds. Returns
/// `false` when a bound failed.
fn check_numeric(n: &LitNum, d: &IntV, ctx: &mut Ctx<'_>) -> bool {
    let mut ok = true;
    if let Some(r) = &d.range {
        use std::cmp::Ordering;
        let mut bound =
            |bound: &Option<LitNum>, fails: fn(Ordering) -> bool, code: ErrorCode, sym: &str| {
                if let Some(b) = bound
                    && fails(n.cmp(b))
                {
                    ctx.err(
                        code,
                        format!("must be {sym} {}", b.wire_text()),
                        Some(b.wire_literal()),
                    );
                    ok = false;
                }
            };
        bound(&r.ge, |o| o == Ordering::Less, ErrorCode::Ge, ">=");
        bound(&r.gt, |o| o != Ordering::Greater, ErrorCode::Gt, ">");
        bound(&r.le, |o| o == Ordering::Greater, ErrorCode::Le, "<=");
        bound(&r.lt, |o| o != Ordering::Less, ErrorCode::Lt, "<");
    }
    if let Some(m) = &d.multiple
        && !n.is_multiple_of(m)
    {
        ctx.err(
            ErrorCode::MultipleOf,
            format!("must be a multiple of {}", m.wire_text()),
            Some(m.wire_literal()),
        );
        ok = false;
    }
    ok
}

/// Lax int coercion: `[+-]?D+` text → narrowest of `Int`/`UInt`/`BigInt`.
/// Returns the `LitNum` (for checks) and the output `Value`.
fn parse_lax_int(text: &str) -> Option<(LitNum, Value<'static>)> {
    let n = normalize_int_text(text)?;
    match crate::num::int_from_text(&n) {
        Some(i) => {
            let v = if let Ok(v) = i64::try_from(i) {
                Value::Int(v)
            } else if let Ok(v) = u64::try_from(i) {
                Value::UInt(v)
            } else {
                Value::BigInt(Cow::Owned(n))
            };
            Some((LitNum::Int(i), v))
        }
        None => Some((
            DecLit::parse(&n).map(LitNum::Dec)?,
            Value::BigInt(Cow::Owned(n)),
        )),
    }
}

/// Shared `Str → parse → canonical Str` path for the string-carried kinds.
/// A parse result equal to the input reports `Same` (no allocation churn).
fn parse_str_kind<'v>(
    input: &Value<'v>,
    ctx: &mut Ctx<'_>,
    what: &str,
    parse: impl Fn(&str) -> Option<String>,
    code: ErrorCode,
    message: &str,
) -> Option<Outcome<'v>> {
    if let Value::Str(s) = input {
        match parse(s) {
            Some(c) if c == s.as_ref() => Some(Outcome::Same),
            Some(c) => Some(Outcome::New(Value::Str(Cow::Owned(c)))),
            None => {
                ctx.err(code, message, None);
                None
            }
        }
    } else {
        ctx.type_err(what, input);
        None
    }
}

fn check_bool<'v>(input: &Value<'v>, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    match input {
        Value::Bool(_) => Some(Outcome::Same),
        Value::Str(s) if ctx.mode == Mode::Lax => {
            if let Some(b) = parse::parse_bool_text(s) {
                Some(Outcome::New(Value::Bool(b)))
            } else {
                ctx.err(ErrorCode::Bool, "not a valid boolean", None);
                None
            }
        }
        _ => {
            ctx.type_err("bool", input);
            None
        }
    }
}

fn check_int<'v>(input: &Value<'v>, d: &IntV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let (n, out) = match input {
        Value::Int(_) | Value::UInt(_) | Value::BigInt(_) => {
            if let Some(n) = LitNum::of_value(input) {
                (n, Outcome::Same)
            } else {
                ctx.err(ErrorCode::Int, "not a valid integer", None);
                return None;
            }
        }
        Value::Str(s) if ctx.mode == Mode::Lax => {
            if let Some((n, v)) = parse_lax_int(s) {
                (n, Outcome::New(v))
            } else {
                ctx.err(ErrorCode::Int, "not a valid integer", None);
                return None;
            }
        }
        _ => {
            ctx.type_err("int", input);
            return None;
        }
    };
    if check_numeric(&n, d, ctx) {
        Some(out)
    } else {
        None
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "int → float coercion is an intended, documented precision limit"
)]
fn check_float<'v>(input: &Value<'v>, d: &IntV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let (n, out) = match input {
        Value::Float(f) => (LitNum::Float(*f), Outcome::Same),
        Value::Int(i) => (
            LitNum::Int(i128::from(*i)),
            Outcome::New(Value::Float(*i as f64)),
        ),
        Value::UInt(u) => (
            LitNum::Int(i128::from(*u)),
            Outcome::New(Value::Float(*u as f64)),
        ),
        Value::BigInt(_) => {
            if let Some(n) = LitNum::of_value(input) {
                let f = n.to_f64();
                (LitNum::Float(f), Outcome::New(Value::Float(f)))
            } else {
                ctx.err(ErrorCode::Float, "not a valid float", None);
                return None;
            }
        }
        Value::Str(s) if ctx.mode == Mode::Lax => {
            if let Some(f) = parse::parse_float_text(s) {
                (LitNum::Float(f), Outcome::New(Value::Float(f)))
            } else {
                ctx.err(ErrorCode::Float, "not a valid float", None);
                return None;
            }
        }
        _ => {
            ctx.type_err("float", input);
            return None;
        }
    };
    if check_numeric(&n, d, ctx) {
        Some(out)
    } else {
        None
    }
}

fn check_str<'v>(input: &Value<'v>, d: &StrV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    if let Value::Str(s) = input {
        let mut ok = check_len(d.len, s.chars().count(), "characters", ctx);
        if let Some(p) = &d.pattern
            && !p.is_match(s)
        {
            ctx.err(
                ErrorCode::Pattern,
                format!("does not match pattern {:?}", p.source()),
                None,
            );
            ok = false;
        }
        if ok { Some(Outcome::Same) } else { None }
    } else {
        ctx.type_err("string", input);
        None
    }
}

fn check_bytes<'v>(input: &Value<'v>, d: &BytesV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let bytes: Option<Outcome<'v>> = match input {
        Value::Bytes(_) => Some(Outcome::Same),
        Value::Str(s) if ctx.mode == Mode::Lax => Some(Outcome::New(Value::Bytes(Cow::Owned(
            s.as_bytes().to_vec(),
        )))),
        _ => {
            ctx.type_err("bytes", input);
            return None;
        }
    };
    bytes.and_then(|o| {
        let len = match &o {
            Outcome::Same => input.as_bytes().map_or(0, <[u8]>::len),
            Outcome::New(Value::Bytes(b)) => b.len(),
            Outcome::New(_) => 0,
        };
        if check_len(d.len, len, "bytes", ctx) {
            Some(o)
        } else {
            None
        }
    })
}

fn check_decimal<'v>(input: &Value<'v>, d: &IntV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let parsed: Option<LitNum> = match input {
        Value::Str(s) => DecLit::parse(s).map(LitNum::Dec),
        Value::Int(i) => Some(LitNum::Int(i128::from(*i))),
        Value::UInt(u) => Some(LitNum::Int(i128::from(*u))),
        Value::BigInt(_) => LitNum::of_value(input),
        Value::Float(f) if f.is_finite() => DecLit::parse(&format!("{f:?}")).map(LitNum::Dec),
        Value::Float(_) => None, // NaN/inf are not decimals
        _ => {
            ctx.type_err("Decimal", input);
            return None;
        }
    };
    if let Some(n) = parsed {
        if !check_numeric(&n, d, ctx) {
            return None;
        }
        let canonical = match &n {
            LitNum::Dec(d) => d.to_canonical_string(),
            LitNum::Int(i) => i.to_string(),
            LitNum::Float(f) => format!("{f:?}"),
        };
        Some(Outcome::New(Value::Str(Cow::Owned(canonical))))
    } else {
        ctx.err(ErrorCode::Decimal, "not a valid decimal", None);
        None
    }
}

impl VKind {
    /// `None` → at least one error was pushed for this input.
    fn check<'v>(&self, input: &Value<'v>, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
        match self {
            Self::Bool => check_bool(input, ctx),
            Self::Null => {
                if let Value::Null = input {
                    Some(Outcome::Same)
                } else {
                    ctx.type_err("None", input);
                    None
                }
            }
            Self::Int(d) => check_int(input, d, ctx),
            Self::Float(d) => check_float(input, d, ctx),
            Self::Str(d) => check_str(input, d, ctx),
            Self::Bytes(d) => check_bytes(input, d, ctx),
            Self::Decimal(d) => check_decimal(input, d, ctx),
            Self::DateTime | Self::Date | Self::Time | Self::TimeDelta | Self::Uuid => {
                check_temporal(self, input, ctx)
            }
            Self::Email => check_flag(
                input,
                parse::check_email,
                ErrorCode::Email,
                "email address",
                "not a valid email address",
                ctx,
            ),
            Self::Url => check_flag(
                input,
                parse::check_url,
                ErrorCode::Url,
                "URL",
                "not a valid URL (scheme://host required)",
                ctx,
            ),
            Self::Ipv4 => check_ip(
                input,
                ErrorCode::Ipv4,
                "IPv4 address",
                |s| Ipv4Addr::from_str(s).ok().map(|a| a.to_string()),
                ctx,
            ),
            Self::Ipv6 => check_ip(
                input,
                ErrorCode::Ipv6,
                "IPv6 address",
                |s| Ipv6Addr::from_str(s).ok().map(|a| a.to_string()),
                ctx,
            ),
            Self::List(d) => validate_items(input, &d.item, d.len, false, d.unique, ctx),
            Self::TupleVar(d) => validate_items(input, &d.item, d.len, false, false, ctx),
            Self::Set(d) | Self::FrozenSet(d) => {
                validate_items(input, &d.item, d.len, true, false, ctx)
            }
            Self::TupleFixed(items) => validate_tuple_fixed(input, items, ctx),
            Self::Dict(d) => validate_dict(input, d, ctx),
            Self::Literal(d) => validate_literal(input, d, ctx),
            Self::Enum(d) => validate_enum(input, d, ctx),
            Self::Optional(inner) => match input {
                Value::Null => Some(Outcome::Same),
                _ => inner.check_value(input, ctx),
            },
            Self::Union(d) => validate_union(input, d, ctx),
            Self::Record(d) => validate_record(input, d, ctx),
            Self::NewType(inner) => inner.check_value(input, ctx),
            Self::Any | Self::Raw => Some(Outcome::Same),
        }
    }
}

/// Temporal/`Uuid` kinds — one dispatch so `check` stays a thin match.
fn check_temporal<'v>(kind: &VKind, input: &Value<'v>, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    type Spec = (
        &'static str,
        fn(&str) -> Option<String>,
        ErrorCode,
        &'static str,
    );
    let (what, parse, code, msg): Spec = match kind {
        VKind::DateTime => (
            "datetime",
            parse::parse_rfc3339,
            ErrorCode::DateTime,
            "not a valid RFC 3339 datetime",
        ),
        VKind::Date => (
            "date",
            parse::parse_iso_date,
            ErrorCode::Date,
            "not a valid ISO 8601 date",
        ),
        VKind::Time => (
            "time",
            parse::parse_iso_time,
            ErrorCode::Time,
            "not a valid ISO 8601 time",
        ),
        VKind::TimeDelta => (
            "timedelta",
            |s| parse::parse_iso8601_duration(s).map(|d| d.to_canonical()),
            ErrorCode::TimeDelta,
            "not a valid ISO 8601 duration",
        ),
        VKind::Uuid => (
            "UUID",
            parse::parse_uuid,
            ErrorCode::Uuid,
            "not a valid UUID",
        ),
        _ => return None,
    };
    parse_str_kind(input, ctx, what, parse, code, msg)
}

/// `Email`/`Url`-style checks: a `Str` input the flag-check accepts passes
/// unchanged; other `Str`s get the kind's error code, other types the
/// generic type error.
fn check_flag<'v>(
    input: &Value<'v>,
    check: fn(&str) -> bool,
    code: ErrorCode,
    what: &str,
    msg: &str,
    ctx: &mut Ctx<'_>,
) -> Option<Outcome<'v>> {
    match input {
        Value::Str(s) if check(s) => Some(Outcome::Same),
        Value::Str(_) => {
            ctx.err(code, msg, None);
            None
        }
        _ => {
            ctx.type_err(what, input);
            None
        }
    }
}

/// IP addresses: parse to an address, emit the canonical `to_string`
/// form — input already canonical borrows (`Outcome::Same`).
fn check_ip<'v>(
    input: &Value<'v>,
    code: ErrorCode,
    what: &str,
    canon: fn(&str) -> Option<String>,
    ctx: &mut Ctx<'_>,
) -> Option<Outcome<'v>> {
    if let Value::Str(s) = input {
        match canon(s) {
            Some(c) if c == s.as_ref() => Some(Outcome::Same),
            Some(c) => Some(Outcome::New(Value::Str(Cow::Owned(c)))),
            None => {
                ctx.err(code, "not a valid IP address", None);
                None
            }
        }
    } else {
        ctx.type_err(what, input);
        None
    }
}

/// `list`/`tuple[T, ...]`/`set`/`frozenset`: validate every item, collect
/// all errors, dedupe when `dedupe` (sets), `Unique` check when `unique`.
fn validate_items<'v>(
    input: &Value<'v>,
    item: &VNode,
    len: Option<Len>,
    dedupe: bool,
    unique: bool,
    ctx: &mut Ctx<'_>,
) -> Option<Outcome<'v>> {
    let (items, scalar_wrapped): (&[Value<'v>], bool) = match input {
        Value::Array(items) => (items, false),
        // Lax promotion: a lone scalar is a one-element container.
        _ if ctx.mode == Mode::Lax => (std::slice::from_ref(input), true),
        _ => {
            ctx.type_err("array", input);
            return None;
        }
    };
    let mut out: Vec<Value<'v>> = Vec::with_capacity(items.len());
    let mut seen: HashSet<Value<'v>> = HashSet::new();
    let mut all_same = true;
    let mut failed = false;
    for (i, it) in items.iter().enumerate() {
        ctx.loc
            .push(LocSeg::Index(u32::try_from(i).unwrap_or(u32::MAX)));
        match item.check_value(it, ctx) {
            Some(Outcome::Same) => {
                if !dedupe || seen.insert(it.clone()) {
                    out.push(it.clone());
                }
            }
            Some(Outcome::New(v)) => {
                all_same = false;
                if !dedupe || seen.insert(v.clone()) {
                    out.push(v);
                }
            }
            None => failed = true,
        }
        ctx.loc.pop();
    }
    if failed {
        return None;
    }
    if !check_len(len, out.len(), "items", ctx) {
        return None;
    }
    // `Unique` on a list: every item distinct (type-aware equality).
    if unique {
        let mut seen: HashSet<&Value<'v>> = HashSet::new();
        for (i, v) in out.iter().enumerate() {
            if !seen.insert(v) {
                ctx.loc
                    .push(LocSeg::Index(u32::try_from(i).unwrap_or(u32::MAX)));
                ctx.err(ErrorCode::Unique, "items must be unique", None);
                ctx.loc.pop();
                return None;
            }
        }
    }
    if all_same && !scalar_wrapped && out.len() == items.len() {
        Some(Outcome::Same)
    } else {
        Some(Outcome::New(Value::Array(out)))
    }
}

/// `tuple[A, B, ...]` — exact arity, per-position types.
fn validate_tuple_fixed<'v>(
    input: &Value<'v>,
    items: &[VNode],
    ctx: &mut Ctx<'_>,
) -> Option<Outcome<'v>> {
    let Value::Array(arr) = input else {
        ctx.type_err("array", input);
        return None;
    };
    if arr.len() != items.len() {
        ctx.err(
            ErrorCode::TupleLen,
            format!("tuple must have exactly {} items", items.len()),
            Some(LiteralValue::UInt(items.len() as u64)),
        );
        return None;
    }
    let mut out: Vec<Value<'v>> = Vec::with_capacity(arr.len());
    let mut all_same = true;
    let mut failed = false;
    for (i, (vnode, it)) in items.iter().zip(arr.iter()).enumerate() {
        ctx.loc
            .push(LocSeg::Index(u32::try_from(i).unwrap_or(u32::MAX)));
        match vnode.check_value(it, ctx) {
            Some(Outcome::Same) => out.push(it.clone()),
            Some(Outcome::New(v)) => {
                all_same = false;
                out.push(v);
            }
            None => failed = true,
        }
        ctx.loc.pop();
    }
    if failed {
        return None;
    }
    if all_same {
        Some(Outcome::Same)
    } else {
        Some(Outcome::New(Value::Array(out)))
    }
}

/// `dict[K, V]` — keys always validated lax; canonical keys emitted;
/// last duplicate canonical key wins.
fn validate_dict<'v>(input: &Value<'v>, d: &DictV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let Value::Object(pairs) = input else {
        ctx.type_err("object", input);
        return None;
    };
    let saved = ctx.mode;
    let mut out: Vec<(Cow<'v, str>, Value<'v>)> = Vec::with_capacity(pairs.len());
    let mut all_same = true;
    let mut failed = false;
    for (k, v) in pairs {
        ctx.loc.push(LocSeg::Key(k.as_ref().into()));
        // Key: local `Value::Str` validated lax (wire keys are text).
        ctx.mode = Mode::Lax;
        let key_input = Value::Str(k.clone());
        let key_ok = d.key.check_value(&key_input, ctx);
        ctx.mode = saved;
        let key_text = key_ok.and_then(|o| match o {
            Outcome::Same => Some(k.clone()),
            Outcome::New(v) => key_text(&v),
        });
        let val_ok = d.value.check_value(v, ctx);
        ctx.loc.pop();
        match (key_text, val_ok) {
            (Some(kt), Some(Outcome::Same)) => {
                if kt.as_ref() != k.as_ref() {
                    all_same = false;
                }
                match out.iter_mut().find(|(ek, _)| ek.as_ref() == kt.as_ref()) {
                    Some((_, ev)) => {
                        *ev = v.clone();
                        all_same = false;
                    }
                    None => out.push((kt, v.clone())),
                }
            }
            (Some(kt), Some(Outcome::New(vv))) => {
                all_same = false;
                match out.iter_mut().find(|(ek, _)| ek.as_ref() == kt.as_ref()) {
                    Some((_, ev)) => *ev = vv,
                    None => out.push((kt, vv)),
                }
            }
            _ => failed = true,
        }
    }
    if failed {
        return None;
    }
    if !check_len(d.len, out.len(), "entries", ctx) {
        return None;
    }
    if all_same && out.len() == pairs.len() {
        Some(Outcome::Same)
    } else {
        Some(Outcome::New(Value::Object(out)))
    }
}

/// Canonical wire text for a validated dict key value.
fn key_text<'v>(v: &Value<'v>) -> Option<Cow<'v, str>> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(Cow::Owned(i.to_string())),
        Value::UInt(u) => Some(Cow::Owned(u.to_string())),
        Value::BigInt(t) => Some(t.clone()),
        Value::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        Value::Float(f) => Some(Cow::Owned(format!("{f:?}"))),
        Value::Null => Some(Cow::Borrowed("null")),
        _ => None,
    }
}

/// `Literal[...]` — strict: value-map equality; lax `Str`: reinterpreting
/// match per `literal_eq` (declaration order).
fn validate_literal<'v>(input: &Value<'v>, d: &LiteralV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let lax_str = ctx.mode == Mode::Lax && matches!(input, Value::Str(_));
    let member = if lax_str {
        d.members.iter().find(|m| literal_eq(m, input, true))
    } else if let Value::Str(s) = input {
        // Textual match only — no allocation for the common strict case.
        d.members.iter().find(|m| match m {
            LiteralValue::Str(ms) => ms.as_ref() == s.as_ref(),
            _ => false,
        })
    } else {
        value_to_literal(input)
            .and_then(|k| d.index.get(&k))
            .map(|&i| &d.members[i])
    };
    if let Some(m) = member {
        if literal_eq(m, input, false) {
            Some(Outcome::Same)
        } else {
            Some(Outcome::New(m.to_value()))
        }
    } else {
        let options: Vec<String> = d.members.iter().map(LiteralValue::wire_text).collect();
        ctx.err(
            ErrorCode::Literal,
            format!("expected one of: {}", options.join(", ")),
            None,
        );
        None
    }
}

/// `Enum` — validated by member value; emits the member's literal value.
fn validate_enum<'v>(input: &Value<'v>, d: &EnumV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let lax_str = ctx.mode == Mode::Lax && matches!(input, Value::Str(_));
    let member = if lax_str {
        d.members.iter().find(|m| literal_eq(&m.value, input, true))
    } else {
        value_to_literal(input)
            .and_then(|k| d.by_value.get(&k))
            .map(|&i| &d.members[i])
    };
    if let Some(m) = member {
        if literal_eq(&m.value, input, false) {
            Some(Outcome::Same)
        } else {
            Some(Outcome::New(m.value.to_value()))
        }
    } else {
        ctx.err(
            ErrorCode::Enum,
            format!("not a member of {}", d.class.name),
            None,
        );
        None
    }
}

/// The `Value` → `LiteralValue` canonicalization for map lookup —
/// returns `None` for containers (never literal members).
fn value_to_literal(v: &Value<'_>) -> Option<LiteralValue> {
    match v {
        Value::Null => Some(LiteralValue::Null),
        Value::Bool(b) => Some(LiteralValue::Bool(*b)),
        Value::Int(i) => Some(LiteralValue::Int(*i)),
        Value::UInt(u) => Some(LiteralValue::UInt(*u)),
        Value::BigInt(t) => normalize_int_text(t).map(|n| LiteralValue::BigInt(n.into())),
        Value::Float(f) => Some(LiteralValue::Float(*f)),
        Value::Str(s) => Some(LiteralValue::Str(s.to_string().into())),
        Value::Bytes(b) => Some(LiteralValue::Bytes(b.to_vec().into())),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// `Union` — discriminated fast path, else variants tried in order.
fn validate_union<'v>(input: &Value<'v>, d: &UnionV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    if let Some(tag_name) = &d.tag {
        if input.as_object().is_none() {
            ctx.type_err("object (discriminated union)", input);
            return None;
        }
        let tag = tag_name.as_ref();
        return match input.get(tag) {
            Some(Value::Str(s)) => {
                if let Some(&i) = d.tag_map.get(s.as_ref()) {
                    d.variants[i].check_value(input, ctx)
                } else {
                    let mut valid: Vec<String> =
                        d.tag_map.keys().map(ToString::to_string).collect();
                    valid.sort();
                    ctx.err(
                        ErrorCode::UnionTagInvalid,
                        format!(
                            "invalid tag for {tag:?}; expected one of: {}",
                            valid.join(", ")
                        ),
                        None,
                    );
                    None
                }
            }
            Some(_) => {
                ctx.err(
                    ErrorCode::UnionTagInvalid,
                    format!("tag {tag:?} must be a string"),
                    None,
                );
                None
            }
            None => {
                ctx.err(
                    ErrorCode::UnionTagMissing,
                    format!("missing discriminator field {tag:?}"),
                    None,
                );
                None
            }
        };
    }
    // Untagged: variants in declaration order, each against a scratch
    // error buffer so failed attempts don't pollute the report.
    for v in &d.variants {
        let mut scratch = ValidationErrors::new();
        std::mem::swap(&mut ctx.errs, &mut scratch);
        let out = v.check_value(input, ctx);
        std::mem::swap(&mut ctx.errs, &mut scratch);
        if scratch.is_empty() {
            return out;
        }
    }
    ctx.err(
        ErrorCode::Union,
        "input does not match any union variant",
        None,
    );
    None
}

/// `Record` — field-by-field validation; collects every error.
fn validate_record<'v>(input: &Value<'v>, d: &RecordV, ctx: &mut Ctx<'_>) -> Option<Outcome<'v>> {
    let Value::Object(pairs) = input else {
        ctx.type_err("object", input);
        return None;
    };
    let mut slots: Vec<Option<Value<'v>>> = d.fields.iter().map(|_| None).collect();
    let mut slot_borrowed = vec![false; d.fields.len()];
    let mut extras: Vec<(Cow<'v, str>, Value<'v>)> = Vec::new();
    let mut seen = vec![false; d.fields.len()];
    let mut failed = false;
    for (k, v) in pairs {
        match d.wire_index.get(k.as_ref()) {
            Some(&i) => {
                let f = &d.fields[i];
                seen[i] = true; // present — invalid is not "missing"
                ctx.loc.push(LocSeg::Key(k.as_ref().into()));
                match f.node.check_value(v, ctx) {
                    Some(Outcome::Same) => {
                        slots[i] = Some(v.clone());
                        slot_borrowed[i] = true;
                    }
                    Some(Outcome::New(vv)) => slots[i] = Some(vv),
                    None => failed = true,
                }
                ctx.loc.pop();
            }
            None => match d.extra {
                ExtraPolicy::Ignore => {}
                ExtraPolicy::Keep => extras.push((k.clone(), v.clone())),
                ExtraPolicy::Forbid => {
                    ctx.loc.push(LocSeg::Key(k.as_ref().into()));
                    ctx.err(ErrorCode::ExtraForbidden, "unexpected field", None);
                    ctx.loc.pop();
                    failed = true;
                }
            },
        }
    }
    for (i, f) in d.fields.iter().enumerate() {
        if seen[i] {
            continue;
        }
        match &f.missing {
            Missing::Required => {
                ctx.loc.push(LocSeg::Key(f.wire_key.clone()));
                ctx.err(ErrorCode::Missing, "required field missing", None);
                ctx.loc.pop();
                failed = true;
            }
            Missing::Literal(l) => slots[i] = Some(l.to_value()),
            Missing::Omit | Missing::Bridge(_) => {}
        }
    }
    if failed {
        return None;
    }
    // Borrowed shortcut: input returned unchanged iff every pair maps to
    // a field in declaration order, no extras, no injected defaults, all
    // values borrowed, and every emitted key equals the input key.
    let identical = extras.is_empty() && {
        let mut pi = 0; // index of the next present field into `pairs`
        let mut ok = true;
        for (i, f) in d.fields.iter().enumerate() {
            if slots[i].is_none() {
                continue;
            }
            ok &= slot_borrowed[i]
                && f.wire_key == f.name
                && pairs
                    .get(pi)
                    .is_some_and(|(k, _)| k.as_ref() == f.name.as_ref());
            pi += 1;
        }
        ok && pi == pairs.len()
    };
    if identical {
        return Some(Outcome::Same);
    }
    let mut out: Vec<(Cow<'v, str>, Value<'v>)> = Vec::with_capacity(d.fields.len() + extras.len());
    for (i, f) in d.fields.iter().enumerate() {
        if let Some(v) = slots[i].take() {
            out.push((Cow::Owned(f.name.to_string()), v));
        }
    }
    out.extend(extras);
    Some(Outcome::New(Value::Object(out)))
}
