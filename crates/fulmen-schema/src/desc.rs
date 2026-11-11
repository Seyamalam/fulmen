//! The type descriptor IR (DESIGN.md §11.3): one node per Python type,
//! produced once at startup by `fulmen-py` from `get_type_hints` output.
//!
//! # Reading this IR
//!
//! A [`TypeDesc`] is `kind` + `meta`. `kind` selects the type from the
//! §11.1 table; `meta` carries the `Annotated` items that are not part of
//! the type itself (`Alias`, `Default`, `Description`, `Example`,
//! `Deprecated`, `AfterValidator`) plus string transforms. Per-kind
//! constraints from §11.2 live *inside* the kind payload — `Len` on `str`
//! and every container, `Range`/`MultipleOf` on the numeric kinds,
//! `Pattern` on `str` — exactly where the §11.3 sketch puts them.
//!
//! The same tree serves three consumers: [`crate::compile`] turns it into
//! a [`crate::Validator`] (the specialized node tree the request path
//! runs), `OpenAPI` generation walks it to emit JSON Schema, and client
//! codegen reads it — so field semantics are documented exhaustively on
//! each node rather than implied by any one consumer.
//!
//! # Invariants the converter must uphold
//!
//! - `Pattern` validates its regex at construction — a bad pattern is a
//!   startup error before `compile` even runs.
//! - `Kind::TypeVar` may only appear inside a `Record` (or `NewType`)
//!   origin that names it in `type_params`; `compile` substitutes
//!   `Generic` applications and rejects leftover `TypeVar`s.
//! - `Union.tag` refers to a *field name* (`FieldDesc.name`) present in
//!   every variant `Record`, typed `Literal` of strings.
//! - Defaults and aliases inside `TypeMeta` describe the *parameter or
//!   field* this node is attached to; validators ignore them mid-tree —
//!   missing-value semantics live in [`FieldDesc::missing`].

use std::borrow::Cow;

use crate::value::LiteralValue;

/// A node in the type descriptor tree.
///
/// Construct via [`TypeDesc::new`] or the per-kind constructors
/// (`TypeDesc::str()`, `TypeDesc::list(..)`, ...); attach `Annotated`
/// metadata with [`TypeDesc::with_meta`].
#[derive(Debug, Clone, PartialEq)]
pub struct TypeDesc {
    /// Which §11.1 type this node describes.
    pub kind: Kind,
    /// `Annotated`-carried metadata for this node. `None` is the common
    /// case — the box keeps small nodes small.
    pub meta: Option<Box<TypeMeta>>,
}

impl TypeDesc {
    /// A node with no metadata.
    pub const fn new(kind: Kind) -> Self {
        Self { kind, meta: None }
    }

    /// A node carrying `Annotated` metadata.
    pub fn with_meta(kind: Kind, meta: TypeMeta) -> Self {
        Self {
            kind,
            meta: Some(Box::new(meta)),
        }
    }

    /// `bool`.
    pub const fn bool() -> Self {
        Self::new(Kind::Bool)
    }

    /// `int` without constraints.
    pub const fn int() -> Self {
        Self::new(Kind::Int(IntDesc::new()))
    }

    /// `float` without constraints.
    pub const fn float() -> Self {
        Self::new(Kind::Float(FloatDesc::new()))
    }

    /// `str` without constraints.
    pub const fn str() -> Self {
        Self::new(Kind::Str(StrDesc::new()))
    }

    /// `bytes`.
    pub const fn bytes() -> Self {
        Self::new(Kind::Bytes(BytesDesc::new()))
    }

    /// `Decimal` without constraints.
    pub const fn decimal() -> Self {
        Self::new(Kind::Decimal(DecimalDesc::new()))
    }

    /// `None` (`NoneType`).
    pub const fn none() -> Self {
        Self::new(Kind::Null)
    }

    /// `datetime` (RFC 3339).
    pub const fn datetime() -> Self {
        Self::new(Kind::DateTime)
    }

    /// `date` (ISO 8601).
    pub const fn date() -> Self {
        Self::new(Kind::Date)
    }

    /// `time` (ISO 8601).
    pub const fn time() -> Self {
        Self::new(Kind::Time)
    }

    /// `timedelta` (ISO 8601 duration).
    pub const fn timedelta() -> Self {
        Self::new(Kind::TimeDelta)
    }

    /// `UUID`.
    pub const fn uuid() -> Self {
        Self::new(Kind::Uuid)
    }

    /// `Email`.
    pub const fn email() -> Self {
        Self::new(Kind::Email)
    }

    /// `Url`.
    pub const fn url() -> Self {
        Self::new(Kind::Url)
    }

    /// `IPv4Address`.
    pub const fn ipv4() -> Self {
        Self::new(Kind::Ipv4)
    }

    /// `IPv6Address`.
    pub const fn ipv6() -> Self {
        Self::new(Kind::Ipv6)
    }

    /// `list[item]`.
    pub fn list(item: TypeDesc) -> Self {
        Self::new(Kind::List(ListDesc::new(item)))
    }

    /// `set[item]`.
    pub fn set(item: TypeDesc) -> Self {
        Self::new(Kind::Set(SetDesc::new(item)))
    }

    /// `frozenset[item]`.
    pub fn frozenset(item: TypeDesc) -> Self {
        Self::new(Kind::FrozenSet(SetDesc::new(item)))
    }

    /// `tuple[item, ...]` — variable length.
    pub fn tuple_var(item: TypeDesc) -> Self {
        Self::new(Kind::TupleVar(TupleVarDesc::new(item)))
    }

    /// `tuple[A, B, ...]` — fixed arity, per-position types.
    pub fn tuple_fixed(items: Vec<TypeDesc>) -> Self {
        Self::new(Kind::TupleFixed(TupleFixedDesc {
            items: items.into(),
        }))
    }

    /// `dict[k, v]`.
    pub fn dict(key: TypeDesc, value: TypeDesc) -> Self {
        Self::new(Kind::Dict(DictDesc::new(key, value)))
    }

    /// `Literal[members...]`.
    pub fn literal(members: Vec<LiteralValue>) -> Self {
        Self::new(Kind::Literal(LiteralDesc {
            members: members.into(),
        }))
    }

    /// `T | None`.
    pub fn optional(inner: TypeDesc) -> Self {
        Self::new(Kind::Optional(OptionalDesc {
            inner: Box::new(inner),
        }))
    }

    /// `A | B | ...` — variants are tried in declaration order.
    pub fn union(variants: Vec<TypeDesc>) -> Self {
        Self::new(Kind::Union(UnionDesc {
            variants: variants.into(),
            tag: None,
        }))
    }

    /// `A | B | ...` discriminated by `Discriminator(tag)` — every variant
    /// must be a `Record` carrying a `Literal` field named `tag`.
    pub fn union_tagged(variants: Vec<TypeDesc>, tag: impl Into<Box<str>>) -> Self {
        Self::new(Kind::Union(UnionDesc {
            variants: variants.into(),
            tag: Some(tag.into()),
        }))
    }

    /// A `Model`/`dataclass`/`TypedDict` record.
    pub fn record(desc: RecordDesc) -> Self {
        Self::new(Kind::Record(desc))
    }

    /// `NewType(name, inner)` — validates as `inner`, keeps the name for
    /// OpenAPI/documentation.
    pub fn newtype(name: impl Into<Box<str>>, inner: TypeDesc) -> Self {
        Self::new(Kind::NewType(NewTypeDesc {
            name: name.into(),
            inner: Box::new(inner),
        }))
    }

    /// `Any` — accepts every value.
    pub const fn any() -> Self {
        Self::new(Kind::Any)
    }

    /// `Raw` — accepts every value and asks the materializer to hand back
    /// the undecoded input (raw body bytes, file contents).
    pub const fn raw() -> Self {
        Self::new(Kind::Raw)
    }

    /// This node's `Annotated` metadata, or a default-constructed empty.
    pub fn meta(&self) -> TypeMetaRef<'_> {
        match &self.meta {
            Some(m) => TypeMetaRef::Present(m),
            None => TypeMetaRef::Empty,
        }
    }
}

/// Read-only view over [`TypeDesc::meta`]: avoids allocating a default
/// `TypeMeta` for the common no-metadata case.
#[derive(Debug)]
pub enum TypeMetaRef<'a> {
    /// The node carries metadata.
    Present(&'a TypeMeta),
    /// The node carries none — behaves as all-defaults.
    Empty,
}

impl TypeMetaRef<'_> {
    /// The declared transforms (defaults to none).
    pub fn transforms(&self) -> Transforms {
        match self {
            Self::Present(m) => m.transforms,
            Self::Empty => Transforms::default(),
        }
    }

    /// `AfterValidator` hook ids, in declared order.
    pub fn after_hooks(&self) -> &[u32] {
        match self {
            Self::Present(m) => &m.after,
            Self::Empty => &[],
        }
    }

    /// The `Description`, when present.
    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Present(m) => m.description.as_deref(),
            Self::Empty => None,
        }
    }

    /// The `Example` literal, when present.
    pub fn example(&self) -> Option<&LiteralValue> {
        match self {
            Self::Present(m) => m.example.as_ref(),
            Self::Empty => None,
        }
    }

    /// The `Alias` name, when present.
    pub fn alias(&self) -> Option<&str> {
        match self {
            Self::Present(m) => m.alias.as_deref(),
            Self::Empty => None,
        }
    }

    /// The `Default` literal, when present.
    pub fn default(&self) -> Option<&LiteralValue> {
        match self {
            Self::Present(m) => m.default.as_ref(),
            Self::Empty => None,
        }
    }

    /// Whether `Deprecated` was attached.
    pub fn deprecated(&self) -> bool {
        match self {
            Self::Present(m) => m.deprecated,
            Self::Empty => false,
        }
    }
}

/// Metadata an `Annotated[...]` wrapper contributes to a node (DESIGN.md
/// §11.2). Fields are the full set the Python API can express; validation
/// only *uses* `transforms` — the rest document the schema (`OpenAPI`) or
/// the parameter/field boundary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypeMeta {
    /// `Alias(name)`: the wire name of the parameter/field this node is
    /// attached to. Ignored mid-tree — record fields resolve their wire
    /// key from [`FieldDesc::alias`] and the record's `rename` rule.
    pub alias: Option<Box<str>>,
    /// `Default(value)`: the fallback when the parameter/field is absent.
    /// Ignored mid-tree — record missing-value semantics live in
    /// [`FieldDesc::missing`]. Surfaced as `OpenAPI` `default`.
    pub default: Option<LiteralValue>,
    /// `Description(text)` for `OpenAPI`.
    pub description: Option<Box<str>>,
    /// `Example(value)` for `OpenAPI` — the JSON-scalar fragment expressible
    /// in the IR.
    pub example: Option<LiteralValue>,
    /// `Deprecated` marker for `OpenAPI`.
    pub deprecated: bool,
    /// `Strip`/`Lower`/`Upper`, applied to a `Str` input *before* any type
    /// or constraint checks (order: strip → lower → upper). Has an effect
    /// only on nodes that consume strings — `str` always; scalar kinds in
    /// lax mode, where the input is already a string.
    pub transforms: Transforms,
    /// `AfterValidator(fn)` hooks: opaque ids into the bridge's callable
    /// table, invoked by `fulmen-py` after a node's own checks pass —
    /// never by this crate (hooks are Python and cost a crossing).
    pub after: Box<[u32]>,
}

/// The `Strip`/`Lower`/`Upper` transforms (§11.2), applied in this fixed
/// pipeline order to a `Str` input *before* type and constraint checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transforms {
    /// `Strip`: remove leading/trailing whitespace (Python `str.strip`
    /// semantics: Unicode whitespace).
    pub strip: bool,
    /// `Lower`: `str.lower()`.
    pub lower: bool,
    /// `Upper`: `str.upper()`.
    pub upper: bool,
}

impl Transforms {
    /// Whether any transform is set.
    pub fn any(&self) -> bool {
        self.strip || self.lower || self.upper
    }

    /// Apply strip → lower → upper, borrowing when nothing applies.
    pub fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let t = if self.strip { text.trim() } else { text };
        if self.upper {
            Cow::Owned(t.to_uppercase())
        } else if self.lower {
            Cow::Owned(t.to_lowercase())
        } else if self.strip {
            Cow::Borrowed(t)
        } else {
            Cow::Borrowed(text)
        }
    }
}

/// `Len(min, max)` — length constraint on `str` (character count),
/// `bytes` (byte count) and every container kind (item count). Either
/// bound may be absent. Compile rejects `min > max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Len {
    /// Minimum length (inclusive).
    pub min: Option<u64>,
    /// Maximum length (inclusive).
    pub max: Option<u64>,
}

impl Len {
    /// `Len(min, max)` — either may be `None`.
    pub const fn new(min: Option<u64>, max: Option<u64>) -> Self {
        Self { min, max }
    }
}

/// `Range(ge, gt, le, lt)` — numeric bounds on `int`, `float`, `Decimal`.
/// Bounds from different domains compare with the input's domain exactly
/// where possible (see [`Bound`]); compile rejects empty ranges and
/// `NaN` bounds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Range {
    /// `value >= ge`.
    pub ge: Option<Bound>,
    /// `value > gt`.
    pub gt: Option<Bound>,
    /// `value <= le`.
    pub le: Option<Bound>,
    /// `value < lt`.
    pub lt: Option<Bound>,
}

impl Range {
    /// A range with only `ge` set — `Range(ge=…)` is the common case.
    pub fn ge(v: Bound) -> Self {
        Self {
            ge: Some(v),
            ..Self::default()
        }
    }
}

/// One bound of a [`Range`] or the `MultipleOf` operand.
///
/// - `Int`: exact for `int` inputs (as `i128`, covering `u64` wire ints)
///   and exact against decimal inputs.
/// - `Float`: compared as `f64`; precision beyond 53 bits is lost —
///   documented precision limit.
/// - `Decimal`: literal text, parsed at compile time — an invalid literal
///   is a startup error.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    /// Integer bound (`i128` covers both `i64` and `u64` inputs).
    Int(i128),
    /// Float bound.
    Float(f64),
    /// Decimal bound as literal text (`"1.5"`, `"1E+3"`).
    Decimal(Box<str>),
}

impl Bound {
    /// Render the bound as wire text for error `limit` fields.
    pub fn wire_text(&self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Float(f) => format!("{f:?}"),
            Self::Decimal(d) => d.to_string(),
        }
    }

    /// The bound as a [`LiteralValue`] for the `limit` field of
    /// [`crate::ErrorEntry`] (§11.7 shows `"limit": 1`).
    pub fn to_literal(&self) -> LiteralValue {
        match self {
            Self::Int(i) => {
                if let Ok(v) = i64::try_from(*i) {
                    LiteralValue::Int(v)
                } else if let Ok(v) = u64::try_from(*i) {
                    LiteralValue::UInt(v)
                } else {
                    LiteralValue::BigInt(i.to_string().into())
                }
            }
            Self::Float(f) => LiteralValue::Float(*f),
            Self::Decimal(d) => LiteralValue::Str(d.clone()),
        }
    }
}

/// `Pattern(regex)` — a `regex` crate pattern. The regex is compiled when
/// the `Pattern` is constructed, so an invalid pattern is a startup error
/// at IR-build time, never a request-time failure. `regex` is a finite
/// automaton — linear-time matching, no `ReDoS` (DESIGN.md §11.2).
#[derive(Debug, Clone)]
pub struct Pattern {
    /// The pattern text as declared (for `OpenAPI` `pattern:`).
    source: Box<str>,
    /// The compiled automaton.
    regex: regex::Regex,
}

impl Pattern {
    /// Compile `source`; errors are returned for the startup path.
    ///
    /// # Errors
    ///
    /// [`PatternError`] carries the `regex` syntax diagnostic. Callers on
    /// the descriptor path should prefer `compile()`, which reports it
    /// through [`CompileError`](crate::CompileError) instead.
    pub fn new(source: &str) -> Result<Self, PatternError> {
        regex::Regex::new(source)
            .map(|regex| Self {
                source: source.into(),
                regex,
            })
            .map_err(|e| PatternError(e.to_string()))
    }

    /// The declared pattern text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether `text` matches (`regex::Regex::is_match`, unanchored —
    /// callers anchor with `^…$` themselves, matching JSON Schema).
    pub fn is_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

/// Why a [`Pattern`] could not be compiled — wraps `regex::Error` so the
/// dependency type stays out of the public signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError(pub String);

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatternError {}

/// `MultipleOf(n)` operand — same domain as [`Bound`]. Compile rejects
/// zero and `NaN` operands.
pub type MultipleOf = Bound;

/// A qualified Python class name: `module` + qualname — everything a
/// consumer needs to import the class (materialization), name its schema
/// component (`OpenAPI`) and name it in errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRef {
    /// The class qualname (`"User"`, `"Outer.Inner"`).
    pub name: Box<str>,
    /// The importable module (`"myapp.models"`).
    pub module: Box<str>,
}

impl ClassRef {
    /// `module.name` for display.
    pub fn qualified(&self) -> String {
        if self.module.is_empty() {
            self.name.to_string()
        } else {
            format!("{}.{}", self.module, self.name)
        }
    }
}

/// Which record flavor a [`RecordDesc`] describes — same shape, different
/// materialization: `Model`/`Dataclass` become class instances,
/// `TypedDict` stays a plain `dict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// `fulmen.Model` (§11.5).
    Model,
    /// `@dataclass`.
    Dataclass,
    /// `TypedDict` — validates a plain `dict`.
    TypedDict,
}

/// The record's policy for keys that match no declared field
/// (`extra=` in §11.5; `ignore` is the default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExtraPolicy {
    /// Drop unknown keys from the output (default).
    #[default]
    Ignore,
    /// `extra_forbidden` error per unknown key.
    Forbid,
    /// Keep unknown keys, unvalidated, after the declared fields.
    Keep,
}

/// The `rename` rule (§11.5 `rename="camel"`) applied at compile time to
/// every field without an explicit `alias`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameRule {
    /// `snake_case` → `camelCase` (`user_name` → `userName`).
    Camel,
    /// `snake_case` → `PascalCase` (`user_name` → `UserName`).
    Pascal,
    /// `snake_case` → `kebab-case` (`user_name` → `user-name`).
    Kebab,
}

impl RenameRule {
    /// Apply the rule to a `snake_case` field name. Names without
    /// underscores are mostly unchanged (`name` → `name`/`Name`/`name`).
    pub fn apply(&self, name: &str) -> String {
        match self {
            Self::Camel | Self::Pascal => {
                let mut out = String::with_capacity(name.len());
                let mut up = matches!(self, Self::Pascal);
                for c in name.chars() {
                    if c == '_' {
                        up = true;
                    } else if up {
                        out.extend(c.to_uppercase());
                        up = false;
                    } else {
                        out.push(c);
                    }
                }
                out
            }
            Self::Kebab => name.replace('_', "-"),
        }
    }
}

/// What an absent input key means for a record field — resolved by the
/// converter from the declaration (`NotRequired`, `= default`,
/// `Field(default_factory=…)`), so validation needs no Python semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Missing {
    /// Absent → `missing` error.
    Required,
    /// Absent → the key is simply not emitted (`TypedDict NotRequired`,
    /// or a class whose own `__init__` supplies the value).
    Omit,
    /// Absent → emit this literal into the output.
    Literal(LiteralValue),
    /// Absent → leave the key out; the bridge supplies the value
    /// (`default_factory`, `init=False` fields, ...). `u32` is an opaque
    /// handle into the bridge's callback table; `fulmen-schema` never
    /// interprets it.
    Bridge(u32),
}

/// One record field.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc {
    /// The Python attribute/keyword name — the output key, and the name
    /// `Union.tag` discriminators refer to.
    pub name: Box<str>,
    /// Explicit wire name (`Field(alias=…)`/`Alias`). When set it is the
    /// *only* accepted input key — like pydantic without
    /// `populate_by_name` — and it wins over `rename`.
    pub alias: Option<Box<str>>,
    /// Missing-value semantics (see [`Missing`]).
    pub missing: Missing,
    /// The field's type.
    pub ty: TypeDesc,
    /// Field-level `Annotated` metadata for `OpenAPI` (`description`,
    /// `example`, `deprecated`). `alias`/`default` there are ignored —
    /// they live in `alias`/`missing` — because Python puts them in
    /// `Field(...)`/`TypedDict` forms the converter reads directly.
    pub meta: Option<Box<TypeMeta>>,
}

impl FieldDesc {
    /// A required field of type `ty`, no alias, no metadata.
    pub fn required(name: impl Into<Box<str>>, ty: TypeDesc) -> Self {
        Self {
            name: name.into(),
            alias: None,
            missing: Missing::Required,
            ty,
            meta: None,
        }
    }

    /// The input key this field accepts: `alias` when set, else
    /// `rename` applied to `name`, else `name` itself.
    pub fn wire_key(&self, rename: Option<RenameRule>) -> Box<str> {
        if let Some(a) = &self.alias {
            a.clone()
        } else {
            match rename {
                Some(rule) => rule.apply(&self.name).into(),
                None => self.name.clone(),
            }
        }
    }
}

/// A `fulmen.Model`, `@dataclass` or `TypedDict` (§11.1 Records).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordDesc {
    /// The Python class (or `TypedDict` name) — materialization target and
    /// `OpenAPI` component name.
    pub class: ClassRef,
    /// Which record flavor (materialization semantics).
    pub kind: RecordKind,
    /// Fields in declaration order — output order for `Model`/`dataclass`
    /// construction and `OpenAPI` `properties`.
    pub fields: Box<[FieldDesc]>,
    /// Unknown-key policy (`ignore` default).
    pub extra: ExtraPolicy,
    /// `rename` rule applied to field wire keys without explicit alias.
    pub rename: Option<RenameRule>,
    /// `TypeVar` names this record is generic over (`["T"]` for
    /// `Page[T]`), in binding order — [`Kind::Generic`] zips its `args`
    /// against these. Empty for non-generic records.
    pub type_params: Box<[Box<str>]>,
}

/// One enum member: `Color.RED` is `{ name: "RED", value: Str("red") }`.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumMember {
    /// The member name (for `OpenAPI` `x-enum-varnames`-style output).
    pub name: Box<str>,
    /// The member's value — what validates on the wire.
    pub value: LiteralValue,
}

/// `Enum` flavor — needed for `OpenAPI` typing and materialization:
/// `IntEnum`/`StrEnum` members coerce to `int`/`str` on the Python side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumKind {
    /// Plain `Enum`.
    Plain,
    /// `IntEnum`/`IntFlag`.
    Int,
    /// `StrEnum`/`str`-valued `Enum`.
    Str,
}

// ---- per-kind payloads ----------------------------------------------------

/// `int` — a Python unbounded integer. Wire input may exceed `u64`
/// (`BigInt`); `Range` bounds as `i128` compare against it by widening to
/// decimal compare when needed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntDesc {
    /// `Range(...)` bounds.
    pub range: Option<Range>,
    /// `MultipleOf(...)` operand.
    pub multiple_of: Option<Bound>,
}

impl IntDesc {
    /// `int` without constraints.
    pub const fn new() -> Self {
        Self {
            range: None,
            multiple_of: None,
        }
    }
}

/// `float` — IEEE-754. `NaN`/`±inf` inputs are preserved (JSON cannot
/// carry them, but non-JSON sources can).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FloatDesc {
    /// `Range(...)` bounds.
    pub range: Option<Range>,
    /// `MultipleOf(...)` operand — checked with `f64` remainder, see the
    /// documented precision limit in `num`.
    pub multiple_of: Option<Bound>,
}

impl FloatDesc {
    /// `float` without constraints.
    pub const fn new() -> Self {
        Self {
            range: None,
            multiple_of: None,
        }
    }
}

/// `str` — the only kind taking `Pattern`; `Len` counts Unicode scalar
/// values (characters), not bytes, matching `len(str)` in Python.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StrDesc {
    /// `Len(...)` on the character count.
    pub len: Option<Len>,
    /// `Pattern(...)` — already compiled when the IR was built.
    pub pattern: Option<Pattern>,
}

impl StrDesc {
    /// `str` without constraints.
    pub const fn new() -> Self {
        Self {
            len: None,
            pattern: None,
        }
    }
}

/// `bytes` — strict input is `Value::Bytes` only (JSON cannot carry raw
/// bytes); lax input additionally accepts `Str`, whose UTF-8 encoding
/// becomes the bytes (documented: *not* base64 — a source adapter that
/// receives base64 must decode it before validation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BytesDesc {
    /// `Len(...)` on the byte count.
    pub len: Option<Len>,
}

impl BytesDesc {
    /// `bytes` without constraints.
    pub const fn new() -> Self {
        Self { len: None }
    }
}

/// `Decimal` — validated as a decimal literal (`[+-]?D+[.D*][eE±D*]`),
/// emitted as canonical `Decimal`-compatible text (see
/// `num::DecLit::to_canonical_string`). `Range`/`MultipleOf` are exact
/// for decimal and integer inputs; float inputs/bounds compare via `f64`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecimalDesc {
    /// `Range(...)` bounds.
    pub range: Option<Range>,
    /// `MultipleOf(...)` operand.
    pub multiple_of: Option<Bound>,
}

impl DecimalDesc {
    /// `Decimal` without constraints.
    pub const fn new() -> Self {
        Self {
            range: None,
            multiple_of: None,
        }
    }
}

/// `list[item]` (variable length).
#[derive(Debug, Clone, PartialEq)]
pub struct ListDesc {
    /// The item descriptor.
    pub item: Box<TypeDesc>,
    /// `Len(...)` on the item count.
    pub len: Option<Len>,
    /// `Unique` — every validated item must be distinct (type-aware
    /// equality, see `value.rs`: `1` and `1.0` are *distinct*).
    pub unique: bool,
}

impl ListDesc {
    /// `list[item]` with no constraints.
    pub fn new(item: TypeDesc) -> Self {
        Self {
            item: Box::new(item),
            len: None,
            unique: false,
        }
    }
}

/// `set[item]` / `frozenset[item]` — duplicates are *dropped* silently,
/// like a Python `set` literal; `Len` bounds the *output* count.
#[derive(Debug, Clone, PartialEq)]
pub struct SetDesc {
    /// The member descriptor.
    pub item: Box<TypeDesc>,
    /// `Len(...)` on the deduplicated member count.
    pub len: Option<Len>,
}

impl SetDesc {
    /// `set[item]` with no constraints.
    pub fn new(item: TypeDesc) -> Self {
        Self {
            item: Box::new(item),
            len: None,
        }
    }
}

/// `tuple[item, ...]` (variable length).
#[derive(Debug, Clone, PartialEq)]
pub struct TupleVarDesc {
    /// The item descriptor.
    pub item: Box<TypeDesc>,
    /// `Len(...)` on the item count.
    pub len: Option<Len>,
}

impl TupleVarDesc {
    /// `tuple[item, ...]` with no constraints.
    pub fn new(item: TypeDesc) -> Self {
        Self {
            item: Box::new(item),
            len: None,
        }
    }
}

/// `tuple[A, B, ...]` — fixed length, per-position types (`tuple[()]` —
/// the empty tuple — is an empty `items`).
#[derive(Debug, Clone, PartialEq)]
pub struct TupleFixedDesc {
    /// Per-position descriptors.
    pub items: Box<[TypeDesc]>,
}

/// `dict[k, v]` — keys arrive as strings on the wire (JSON object keys
/// are always text), so the *key* descriptor is always validated in lax
/// mode regardless of the body mode: `dict[int, V]` parses `"5"`→`5` and
/// re-emits the canonical key text. `k` must be a string-coercible kind
/// (scalars, identifiers, temporal, `Literal`, `Enum`, `NewType`, unions
/// thereof) — checked at compile.
#[derive(Debug, Clone, PartialEq)]
pub struct DictDesc {
    /// The key descriptor (always validated lax — see above).
    pub key: Box<TypeDesc>,
    /// The value descriptor (validated in the current mode).
    pub value: Box<TypeDesc>,
    /// `Len(...)` on the pair count.
    pub len: Option<Len>,
}

impl DictDesc {
    /// `dict[k, v]` with no constraints.
    pub fn new(key: TypeDesc, value: TypeDesc) -> Self {
        Self {
            key: Box::new(key),
            value: Box::new(value),
            len: None,
        }
    }
}

/// `Literal[m0, m1, ...]` — at least one member. Matching is
/// [`crate::value::literal_eq`]: strict inputs compare type-aware; a lax
/// `Str` input is reinterpreted in each member's domain in declaration
/// order.
#[derive(Debug, Clone, PartialEq)]
pub struct LiteralDesc {
    /// The allowed members, in declaration order.
    pub members: Box<[LiteralValue]>,
}

/// `Enum`/`IntEnum`/`StrEnum` — validated by member *value*: the input
/// must equal some member's `value`; the member value is the output.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumDesc {
    /// The Python enum class.
    pub class: ClassRef,
    /// Enum flavor for materialization/OpenAPI.
    pub kind: EnumKind,
    /// Members in declaration order. Aliased members (same value) share
    /// the value — the first declared name wins, Python-style.
    pub members: Box<[EnumMember]>,
}

/// `T | None` — also the node the converter should emit for
/// `Union[T, None]`: faster than a general union and maps cleanly to
/// `OpenAPI` `nullable`/`anyOf` forms.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionalDesc {
    /// The non-`None` member.
    pub inner: Box<TypeDesc>,
}

/// `A | B | ...` — variants are tried in declaration order (the first
/// match wins); with `tag` set, validation is keyed on the tag field —
/// the input must be an object whose `tag` field names one variant's
/// `Literal` field value.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionDesc {
    /// The variants, in declaration order.
    pub variants: Box<[TypeDesc]>,
    /// `Discriminator(tag)`: the field *name* shared by every variant
    /// `Record`, typed `Literal[...]` of strings — checked at compile.
    pub tag: Option<Box<str>>,
}

/// `NewType(name, T)` — validates as `T`; `name` is documentation for
/// `OpenAPI` and error messages.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTypeDesc {
    /// The `NewType` name.
    pub name: Box<str>,
    /// The wrapped type.
    pub inner: Box<TypeDesc>,
}

/// `Origin[a0, a1, ...]` — a generic application such as `Page[int]`.
/// `origin` is typically a `Record` with `type_params`; `compile`
/// substitutes `args` for the params throughout the origin's tree and
/// rejects arity mismatches. The node never reaches the validator.
#[derive(Debug, Clone, PartialEq)]
pub struct GenericDesc {
    /// The generic origin (usually `Kind::Record`).
    pub origin: Box<TypeDesc>,
    /// The concrete arguments, in `type_params` order.
    pub args: Box<[TypeDesc]>,
}

/// The §11.1 type table as an IR enum. Constraints live inside the
/// payload structs; `Annotated` metadata lives in [`TypeDesc::meta`].
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `bool` — strict: `Bool` only; lax: the boolean words
    /// (`true/1/on/yes/y/t`, `false/0/off/no/n/f`, case-insensitive).
    Bool,
    /// `int` — strict: `Int`/`UInt`/`BigInt` (never `Bool`/`Float`); lax:
    /// additionally `[+-]?D+` text (surrounding whitespace tolerated,
    /// `"007"`→`7`, `"1.0"`/`"1e3"` rejected).
    Int(IntDesc),
    /// `float` — strict: `Float`, and `Int`/`UInt`/`BigInt` coerce to the
    /// nearest `f64` (precision limit: >2^53 ints round); lax: `f64` text
    /// plus `inf`/`infinity`/`nan` words.
    Float(FloatDesc),
    /// `str` — `Str` in both modes (lax differs only in that the source
    /// already produces strings; there is no `int → "1"` coercion).
    Str(StrDesc),
    /// `bytes` — see [`BytesDesc`].
    Bytes(BytesDesc),
    /// `Decimal` — strict: `Str`/`Int`/`UInt`/`BigInt`/`Float` numeric
    /// text; lax: `Str` literal text. Output is canonical
    /// `Decimal`-compatible text.
    Decimal(DecimalDesc),
    /// `None` (`NoneType`) — `Null` only in both modes. Mapping an empty
    /// query/form string to `None` is the source adapter's job, not this
    /// node's.
    Null,
    /// `datetime` — RFC 3339 text (`parse::parse_rfc3339`); output is the
    /// canonical `fromisoformat`-compatible form.
    DateTime,
    /// `date` — ISO 8601 `YYYY-MM-DD`.
    Date,
    /// `time` — ISO 8601 `HH:MM[:SS[.frac]][offset]`.
    Time,
    /// `timedelta` — ISO 8601 duration; output is the fixed-shape
    /// canonical form (`parse::DurationParts::to_canonical`).
    TimeDelta,
    /// `UUID` — the forms `uuid.UUID(text)` accepts; output is canonical
    /// lowercase hyphenated text.
    Uuid,
    /// `Email` — pragmatic check (`parse::check_email`); output unchanged.
    Email,
    /// `Url` — `scheme://host` check (`parse::check_url`); output
    /// unchanged.
    Url,
    /// `IPv4Address` — `std::net::Ipv4Addr` parse; output is canonical
    /// dotted text.
    Ipv4,
    /// `IPv6Address` — `std::net::Ipv6Addr` parse; output is canonical
    /// compressed text.
    Ipv6,
    /// `list[T]` — in lax mode a non-`Array` input is treated as a
    /// single-element list (`?tags=a` → `["a"]`).
    List(ListDesc),
    /// `set[T]` — validates like `list`, dedupes, emits an `Array` the
    /// materializer turns into `set`. Same lax single-value promotion.
    Set(SetDesc),
    /// `frozenset[T]` — same as `set`.
    FrozenSet(SetDesc),
    /// `tuple[T, ...]` — same as `list`, no `unique`. Lax promotion too.
    TupleVar(TupleVarDesc),
    /// `tuple[A, B]` — exact length, per-position types; no lax
    /// single-value promotion.
    TupleFixed(TupleFixedDesc),
    /// `dict[K, V]` — key descriptor always lax; see [`DictDesc`].
    Dict(DictDesc),
    /// `Literal[...]`.
    Literal(LiteralDesc),
    /// `Enum`/`IntEnum`/`StrEnum`.
    Enum(EnumDesc),
    /// `T | None`.
    Optional(OptionalDesc),
    /// `A | B | ...`, optionally discriminated.
    Union(UnionDesc),
    /// `Model`/`dataclass`/`TypedDict`.
    Record(RecordDesc),
    /// `NewType(name, T)`.
    NewType(NewTypeDesc),
    /// `Origin[args]` — resolved at compile; never reaches a validator.
    Generic(GenericDesc),
    /// A `TypeVar` placeholder — only legal inside a generic `origin`'s
    /// tree; `compile` substitutes it. A leftover `TypeVar` is a compile
    /// error.
    TypeVar(Box<str>),
    /// `Any` — accepts everything, emits the input unchanged.
    Any,
    /// `Raw` — accepts everything, emits the input unchanged, and signals
    /// the materializer to hand over the undecoded bytes/string rather
    /// than building a typed object (raw body, file upload).
    Raw,
}

impl Kind {
    /// A short type name for error messages and union descriptions:
    /// `"int"`, `"str"`, `"list[int]"`, `"User"`, ...
    pub fn name(&self) -> String {
        match self {
            Self::Bool => "bool".to_owned(),
            Self::Int(_) => "int".to_owned(),
            Self::Float(_) => "float".to_owned(),
            Self::Str(_) => "str".to_owned(),
            Self::Bytes(_) => "bytes".to_owned(),
            Self::Decimal(_) => "Decimal".to_owned(),
            Self::Null => "None".to_owned(),
            Self::DateTime => "datetime".to_owned(),
            Self::Date => "date".to_owned(),
            Self::Time => "time".to_owned(),
            Self::TimeDelta => "timedelta".to_owned(),
            Self::Uuid => "UUID".to_owned(),
            Self::Email => "Email".to_owned(),
            Self::Url => "Url".to_owned(),
            Self::Ipv4 => "IPv4Address".to_owned(),
            Self::Ipv6 => "IPv6Address".to_owned(),
            Self::List(d) => format!("list[{}]", d.item.kind.name()),
            Self::Set(d) => format!("set[{}]", d.item.kind.name()),
            Self::FrozenSet(d) => format!("frozenset[{}]", d.item.kind.name()),
            Self::TupleVar(d) => format!("tuple[{}, ...]", d.item.kind.name()),
            Self::TupleFixed(d) => {
                let items: Vec<String> = d.items.iter().map(|t| t.kind.name()).collect();
                format!("tuple[{}]", items.join(", "))
            }
            Self::Dict(d) => {
                format!("dict[{}, {}]", d.key.kind.name(), d.value.kind.name())
            }
            Self::Literal(d) => {
                let members: Vec<String> = d.members.iter().map(|m| format!("{m:?}")).collect();
                format!("Literal[{}]", members.join(", "))
            }
            Self::Enum(d) => d.class.name.to_string(),
            Self::Optional(d) => format!("{} | None", d.inner.kind.name()),
            Self::Union(d) => {
                let variants: Vec<String> = d.variants.iter().map(|v| v.kind.name()).collect();
                variants.join(" | ")
            }
            Self::Record(d) => d.class.name.to_string(),
            Self::NewType(d) => d.name.to_string(),
            Self::Generic(d) => {
                let args: Vec<String> = d.args.iter().map(|a| a.kind.name()).collect();
                format!("{}[{}]", d.origin.kind.name(), args.join(", "))
            }
            Self::TypeVar(n) => n.to_string(),
            Self::Any => "Any".to_owned(),
            Self::Raw => "Raw".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_order() {
        let t = Transforms {
            strip: true,
            lower: true,
            upper: false,
        };
        assert_eq!(t.apply("  Hello  "), "hello");
        let t = Transforms {
            strip: true,
            lower: false,
            upper: true,
        };
        assert_eq!(t.apply("  Hello  "), "HELLO");
        let t = Transforms::default();
        assert!(matches!(t.apply("  x  "), Cow::Borrowed("  x  ")));
    }

    #[test]
    fn rename_rules() {
        assert_eq!(RenameRule::Camel.apply("user_name"), "userName");
        assert_eq!(RenameRule::Pascal.apply("user_name"), "UserName");
        assert_eq!(RenameRule::Kebab.apply("user_name"), "user-name");
        assert_eq!(RenameRule::Camel.apply("name"), "name");
        assert_eq!(RenameRule::Camel.apply("_private"), "Private");
    }

    #[test]
    fn pattern_compiles_eagerly() {
        assert!(Pattern::new("^a+$").is_ok());
        assert!(Pattern::new("(").is_err());
        let p = Pattern::new("^a+$").unwrap();
        assert!(p.is_match("aaa"));
        assert!(!p.is_match("ab"));
    }

    #[test]
    fn kind_names() {
        assert_eq!(TypeDesc::int().kind.name(), "int");
        assert_eq!(TypeDesc::list(TypeDesc::int()).kind.name(), "list[int]");
        assert_eq!(
            TypeDesc::optional(TypeDesc::str()).kind.name(),
            "str | None"
        );
        assert_eq!(
            TypeDesc::union(vec![TypeDesc::int(), TypeDesc::str()])
                .kind
                .name(),
            "int | str"
        );
    }

    #[test]
    fn meta_accessors() {
        let d = TypeDesc::str();
        assert!(d.meta().description().is_none());
        assert!(!d.meta().deprecated());
        let d = TypeDesc::with_meta(
            Kind::Str(StrDesc::new()),
            TypeMeta {
                description: Some("x".into()),
                deprecated: true,
                ..TypeMeta::default()
            },
        );
        assert_eq!(d.meta().description(), Some("x"));
        assert!(d.meta().deprecated());
    }
}
