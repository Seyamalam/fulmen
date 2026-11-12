//! Property tests for the schema engine (ROADMAP M2.20).
//!
//! Three families of properties, all through the public API:
//!
//! - **JSON round-trips are a fixpoint** — a generated `Value` survives
//!   `to_json → decode_json` unchanged, and `decode ∘ encode` re-decode
//!   is idempotent.
//! - **Values generated *for* a descriptor always validate** — a random
//!   `TypeDesc` compiles, and the `Value` its matching generator produces
//!   passes `validate` (and `decode_and_validate` when JSON-encodable).
//! - **Arbitrary bytes never panic** — the JSON decoder and the lax
//!   query/form/cookie adapters return `Err`, never unwind, on any input
//!   (the same property `just fuzz` hammers, M2.19).

#![expect(
    clippy::panic,
    clippy::expect_used,
    reason = "test file — panics in helpers express generator/test failures, \
        which is the mechanism under test"
)]

use std::borrow::Cow;
use std::fmt::Write;
use std::net::Ipv6Addr;
use std::ops::RangeInclusive;

use fulmen_schema::{
    Bound, BytesDesc, Checked, ClassRef, DecodeError, DictDesc, EnumDesc, EnumKind, EnumMember,
    ExtraPolicy, FieldDesc, FloatDesc, IntDesc, Kind, Len, Limits, ListDesc, LiteralDesc,
    LiteralValue, Missing, Mode, Pattern, Range, RecordDesc, RecordKind, RenameRule, SetDesc,
    Source, StrDesc, TupleVarDesc, TypeDesc, Value, compile, decode_and_validate, decode_cookie,
    decode_form, decode_json, decode_json_with_limits, decode_query, validate_text,
};
use proptest::prelude::*;

const FIELD_NAMES: &[&str] = &[
    "id",
    "name",
    "user_name",
    "email",
    "age",
    "zip_code",
    "tag_list",
    "value_x",
    "page_size",
    "q",
];

/// Every generated pattern has a matching generator below.
const PATTERNS: &[&str] = &["^[a-z]+$", "^[0-9]{2,4}$", "^[A-Z][a-z]{2,8}$"];

// ---- scalar atoms -----------------------------------------------------------

/// Decimal text with ≥21 digits — always past `u64`, so always `BigInt`.
fn arb_bigint() -> impl Strategy<Value = String> {
    (proptest::bool::ANY, "[1-9][0-9]{20,38}").prop_map(|(neg, digits)| {
        if neg { format!("-{digits}") } else { digits }
    })
}

fn finite_f64() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("finite", |f| f.is_finite())
}

// ---- JSON values ------------------------------------------------------------

/// A JSON-representable `Value`: no `Bytes` (no JSON literal) and only
/// finite floats (the encoder's `{:?}` form must stay valid JSON).
fn arb_json_value() -> impl Strategy<Value = Value<'static>> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        (9_223_372_036_854_775_808u64..=u64::MAX).prop_map(Value::UInt),
        arb_bigint().prop_map(|t| Value::BigInt(t.into())),
        finite_f64().prop_map(Value::Float),
        ".*".prop_map(|s| Value::Str(s.into())),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
            proptest::collection::vec((".*", inner), 0..8).prop_map(|pairs| {
                Value::Object(pairs.into_iter().map(|(k, v)| (Cow::Owned(k), v)).collect())
            }),
        ]
    })
}

/// Escape `s` as JSON string content (no surrounding quotes).
fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// Serialize a `Value` to strict JSON. Test-local: the crate decodes but
/// does not encode `Value`s (materialization lives in `fulmen-py`).
/// `Bytes` has no JSON form — callers keep it out (see `contains_bytes`).
fn to_json(v: &Value<'_>) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::BigInt(t) => t.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Str(s) => format!("\"{}\"", escape_json_str(s)),
        Value::Bytes(_) => panic!("test encoder: Bytes is not JSON-encodable"),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(to_json).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(pairs) => {
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", escape_json_str(k), to_json(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

fn contains_bytes(v: &Value<'_>) -> bool {
    match v {
        Value::Bytes(_) => true,
        Value::Array(items) => items.iter().any(contains_bytes),
        Value::Object(pairs) => pairs.iter().any(|(_, i)| contains_bytes(i)),
        _ => false,
    }
}

// ---- descriptor generation --------------------------------------------------

/// `Len` with `min <= max` and at least one bound set.
fn arb_len() -> impl Strategy<Value = Len> {
    (
        proptest::option::of(0u64..6),
        proptest::option::of(0u64..12),
    )
        .prop_filter_map("at least one bound", |(min, max)| match (min, max) {
            (None, None) => None,
            (Some(a), Some(b)) if a > b => Some(Len::new(Some(b), Some(a))),
            _ => Some(Len::new(min, max)),
        })
}

fn arb_len_opt() -> impl Strategy<Value = Option<Len>> {
    proptest::option::of(arb_len())
}

fn arb_literal() -> impl Strategy<Value = LiteralValue> {
    prop_oneof![
        Just(LiteralValue::Null),
        any::<bool>().prop_map(LiteralValue::Bool),
        any::<i64>().prop_map(LiteralValue::Int),
        (9_223_372_036_854_775_808u64..=u64::MAX).prop_map(LiteralValue::UInt),
        arb_bigint().prop_map(|t| LiteralValue::BigInt(t.into())),
        finite_f64().prop_map(LiteralValue::Float),
        ".*".prop_map(|s| LiteralValue::Str(s.into())),
    ]
}

fn arb_int_desc() -> impl Strategy<Value = TypeDesc> {
    prop_oneof![
        3 => Just(TypeDesc::int()),
        1 => (any::<i64>(), any::<i64>()).prop_map(|(a, b)| {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            TypeDesc::new(Kind::Int(IntDesc {
                range: Some(Range {
                    ge: Some(Bound::Int(i128::from(lo))),
                    gt: None,
                    le: Some(Bound::Int(i128::from(hi))),
                    lt: None,
                }),
                multiple_of: None,
            }))
        }),
        1 => (1i128..=1000).prop_map(|k| {
            TypeDesc::new(Kind::Int(IntDesc {
                range: None,
                multiple_of: Some(Bound::Int(k)),
            }))
        }),
    ]
}

fn arb_float_desc() -> impl Strategy<Value = TypeDesc> {
    prop_oneof![
        3 => Just(TypeDesc::float()),
        1 => (finite_f64(), finite_f64()).prop_map(|(a, b)| {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            TypeDesc::new(Kind::Float(FloatDesc {
                range: Some(Range {
                    ge: Some(Bound::Float(lo)),
                    gt: None,
                    le: Some(Bound::Float(hi)),
                    lt: None,
                }),
                multiple_of: None,
            }))
        }),
    ]
}

fn arb_str_desc() -> impl Strategy<Value = TypeDesc> {
    prop_oneof![
        3 => Just(TypeDesc::str()),
        1 => arb_len().prop_map(|len| {
            TypeDesc::new(Kind::Str(StrDesc {
                len: Some(len),
                pattern: None,
            }))
        }),
        1 => proptest::sample::select(PATTERNS.to_vec()).prop_map(|src| {
            TypeDesc::new(Kind::Str(StrDesc {
                len: None,
                pattern: Some(Pattern::new(src).expect("whitelisted pattern compiles")),
            }))
        }),
    ]
}

fn arb_enum_desc() -> BoxedStrategy<TypeDesc> {
    proptest::collection::vec(("[A-Z]{1,8}", arb_literal()), 1..4)
        .prop_map(|members| {
            TypeDesc::new(Kind::Enum(EnumDesc {
                class: ClassRef {
                    name: "E".into(),
                    module: "m".into(),
                },
                kind: EnumKind::Plain,
                members: members
                    .into_iter()
                    .map(|(name, value)| EnumMember {
                        name: name.into(),
                        value,
                    })
                    .collect(),
            }))
        })
        .boxed()
}

/// Scalar leaves + `Literal`/`Enum`/`Any`/`Raw` — the depth-0 case.
fn arb_leaf_desc() -> BoxedStrategy<TypeDesc> {
    prop_oneof![
        2 => Just(TypeDesc::bool()),
        3 => arb_int_desc(),
        2 => arb_float_desc(),
        3 => arb_str_desc(),
        1 => arb_len_opt().prop_map(|len| TypeDesc::new(Kind::Bytes(BytesDesc { len }))),
        1 => Just(TypeDesc::decimal()),
        1 => Just(TypeDesc::none()),
        1 => Just(TypeDesc::datetime()),
        1 => Just(TypeDesc::date()),
        1 => Just(TypeDesc::time()),
        1 => Just(TypeDesc::timedelta()),
        1 => Just(TypeDesc::uuid()),
        1 => Just(TypeDesc::email()),
        1 => Just(TypeDesc::url()),
        1 => Just(TypeDesc::ipv4()),
        1 => Just(TypeDesc::ipv6()),
        2 => proptest::collection::vec(arb_literal(), 1..4).prop_map(TypeDesc::literal),
        1 => arb_enum_desc(),
        1 => Just(TypeDesc::any()),
        1 => Just(TypeDesc::raw()),
    ]
    .boxed()
}

/// `dict` key kinds kept to types whose text form is always generatable
/// below (`bool`/`int`/`float`/`str` are all `key_coercible`).
fn arb_key_desc() -> BoxedStrategy<TypeDesc> {
    prop_oneof![
        4 => Just(TypeDesc::str()),
        1 => Just(TypeDesc::int()),
        1 => Just(TypeDesc::float()),
        1 => Just(TypeDesc::bool()),
    ]
    .boxed()
}

fn arb_missing() -> impl Strategy<Value = Missing> {
    prop_oneof![
        3 => Just(Missing::Required),
        1 => Just(Missing::Omit),
        1 => arb_literal().prop_map(Missing::Literal),
    ]
}

fn arb_record_desc(depth: usize) -> BoxedStrategy<TypeDesc> {
    (0usize..=5)
        .prop_flat_map(move |n| {
            (
                proptest::collection::hash_set(
                    proptest::sample::select(FIELD_NAMES.to_vec()),
                    n..=n,
                ),
                proptest::collection::vec((arb_desc(depth - 1), arb_missing()), n..=n),
                proptest::sample::select(vec![
                    RecordKind::Model,
                    RecordKind::Dataclass,
                    RecordKind::TypedDict,
                ]),
                proptest::sample::select(vec![
                    ExtraPolicy::Ignore,
                    ExtraPolicy::Forbid,
                    ExtraPolicy::Keep,
                ]),
                proptest::option::of(proptest::sample::select(vec![
                    RenameRule::Camel,
                    RenameRule::Pascal,
                    RenameRule::Kebab,
                ])),
            )
                .prop_map(|(names, fields, kind, extra, rename)| {
                    TypeDesc::record(RecordDesc {
                        class: ClassRef {
                            name: "Rec".into(),
                            module: "m".into(),
                        },
                        kind,
                        fields: names
                            .into_iter()
                            .zip(fields)
                            .map(|(name, (ty, missing))| FieldDesc {
                                name: name.into(),
                                alias: None,
                                missing,
                                ty,
                                meta: None,
                            })
                            .collect(),
                        extra,
                        rename,
                        type_params: Box::new([]),
                    })
                })
        })
        .boxed()
}

/// Descriptor tree: scalars/containers/records/unions/literals, depth ≤
/// `depth` of nesting. `Generic`/`TypeVar` are compile-level machinery
/// (unit-tested in `compile.rs`) and deliberately not generated.
fn arb_desc(depth: usize) -> BoxedStrategy<TypeDesc> {
    if depth == 0 {
        return arb_leaf_desc();
    }
    prop_oneof![
        6 => arb_leaf_desc(),
        1 => arb_desc(depth - 1).prop_map(TypeDesc::optional),
        1 => arb_desc(depth - 1).prop_map(|d| TypeDesc::newtype("NT", d)),
        2 => (arb_desc(depth - 1), arb_len_opt(), any::<bool>()).prop_map(
            |(item, len, unique)| {
                TypeDesc::new(Kind::List(ListDesc {
                    item: Box::new(item),
                    len,
                    unique,
                }))
            }
        ),
        1 => (arb_desc(depth - 1), arb_len_opt(), any::<bool>()).prop_map(
            |(item, len, frozen)| {
                let set = SetDesc {
                    item: Box::new(item),
                    len,
                };
                TypeDesc::new(if frozen {
                    Kind::FrozenSet(set)
                } else {
                    Kind::Set(set)
                })
            }
        ),
        1 => (arb_desc(depth - 1), arb_len_opt()).prop_map(|(item, len)| {
            TypeDesc::new(Kind::TupleVar(TupleVarDesc {
                item: Box::new(item),
                len,
            }))
        }),
        1 => proptest::collection::vec(arb_desc(depth - 1), 0..4)
            .prop_map(TypeDesc::tuple_fixed),
        2 => (arb_key_desc(), arb_desc(depth - 1), arb_len_opt()).prop_map(
            |(key, value, len)| {
                TypeDesc::new(Kind::Dict(DictDesc {
                    key: Box::new(key),
                    value: Box::new(value),
                    len,
                }))
            }
        ),
        1 => proptest::collection::vec(arb_desc(depth - 1), 1..4).prop_map(TypeDesc::union),
        2 => arb_record_desc(depth),
    ]
    .boxed()
}

// ---- values that satisfy a descriptor --------------------------------------

/// Item-count range honoring a `Len` bound (`None` → `0..=default_max`).
fn len_range(len: Option<Len>, default_max: usize) -> RangeInclusive<usize> {
    match len {
        None => 0..=default_max,
        Some(l) => {
            let lo = usize::try_from(l.min.unwrap_or(0)).unwrap_or(0);
            let hi = match l.max {
                Some(m) => usize::try_from(m).unwrap_or(usize::MAX).min(16).max(lo),
                None => lo + default_max,
            };
            lo..=hi
        }
    }
}

/// Domain sizes are capped at `DOM_CAP` — anything bigger is "large
/// enough" for generation purposes.
const DOM_CAP: usize = 64;

/// `base^exp`, capped at `DOM_CAP`.
fn pow_capped(base: usize, exp: usize) -> usize {
    let mut r = 1usize;
    for _ in 0..exp.min(64) {
        r = r.saturating_mul(base);
        if r >= DOM_CAP {
            return DOM_CAP;
        }
    }
    r.min(DOM_CAP)
}

/// `Σ_{n=lo}^{hi} baseⁿ`, capped at `DOM_CAP`.
fn pow_sum_range(base: usize, lo: usize, hi: usize) -> usize {
    let mut sum = 0usize;
    let mut term = pow_capped(base, lo);
    for _ in lo..=hi {
        sum = sum.saturating_add(term);
        term = term.saturating_mul(base);
        if sum >= DOM_CAP {
            return DOM_CAP;
        }
    }
    sum.min(DOM_CAP)
}

/// `C(dom, k)` as `u128` — callers keep `dom` small (< 64).
fn choose(dom: usize, k: usize) -> u128 {
    let mut c = 1u128;
    for i in 0..k.min(dom) {
        c = c * u128::try_from(dom - i).unwrap_or(u128::MAX)
            / u128::try_from(i + 1).unwrap_or(u128::MAX);
        if c >= u128::from(u64::MAX) {
            return u128::from(u64::MAX);
        }
    }
    c
}

/// `Σ_{n=lo}^{hi} C(dom, n)`, capped — subsets of sizes in `[lo, hi]`.
fn choose_sum_range(dom: usize, lo: usize, hi: usize) -> usize {
    let mut sum = 0usize;
    for n in lo..=hi.min(dom) {
        sum = sum.saturating_add(usize::try_from(choose(dom, n)).unwrap_or(usize::MAX));
        if sum >= DOM_CAP {
            return DOM_CAP;
        }
    }
    sum.min(DOM_CAP)
}

/// Whether `Null` is already in the node's domain — determines the
/// `Optional` count (`{Null} ∪ inner`, deduped).
fn admits_null(d: &TypeDesc) -> bool {
    match &d.kind {
        Kind::Null | Kind::Any | Kind::Raw | Kind::Optional(_) => true,
        Kind::NewType(d) => admits_null(&d.inner),
        Kind::Union(d) => d.variants.iter().any(admits_null),
        Kind::Literal(d) => d.members.contains(&LiteralValue::Null),
        Kind::Enum(d) => d.members.iter().any(|m| m.value == LiteralValue::Null),
        _ => false,
    }
}

/// How many distinct values `arb_value_for` can emit for `desc` — the
/// *generatable* domain, bounded at `DOM_CAP`. Dedupe-sensitive contexts
/// (`set`, `unique` lists, `dict` keys) must keep requested sizes inside
/// this or generation cannot finish. Under-estimates are safe (they only
/// tighten sizes); the formulas mirror the generators' bounds — e.g. a
/// 1-member `tuple[Literal, ...]` can only emit lengths `0..=5` (6
/// distinct arrays), which is its domain here, not the mathematical ∞.
fn domain_size(desc: &TypeDesc) -> usize {
    match &desc.kind {
        Kind::Null => 1,
        Kind::Bool => 2,
        // Compile dedupes members first-wins — count distinct values.
        Kind::Literal(d) => distinct_literal_count(d),
        Kind::Enum(d) => distinct_enum_count(d),
        Kind::Optional(d) => domain_size(&d.inner)
            .saturating_add(usize::from(!admits_null(&d.inner)))
            .min(DOM_CAP),
        // Variants may overlap — max is a safe under-estimate of the union.
        Kind::Union(d) => d.variants.iter().map(domain_size).max().unwrap_or(0),
        Kind::NewType(d) => domain_size(&d.inner),
        Kind::TupleFixed(d) => d
            .items
            .iter()
            .map(domain_size)
            .fold(1usize, usize::saturating_mul)
            .min(DOM_CAP),
        Kind::List(d) => container_domain(&d.item, d.len, d.unique),
        Kind::TupleVar(d) => container_domain(&d.item, d.len, false),
        Kind::Set(d) | Kind::FrozenSet(d) => container_domain(&d.item, d.len, true),
        Kind::Dict(d) => dict_domain(d),
        Kind::Record(d) => record_domain(d),
        // Scalar domains are bounded only by tight `Len`/`Range` bounds:
        // `str{max=0}` admits exactly `""`, `int[0..2]` exactly 3 values.
        // The counts mirror `arb_str_value`/`arb_bytes_value`/`arb_int_value`.
        Kind::Str(d) if d.pattern.is_none() => {
            let r = len_range(d.len, 8);
            pow_sum_range(26, *r.start(), (*r.end()).min(4))
        }
        Kind::Bytes(d) => match d.len.and_then(|l| l.max) {
            Some(0) => 1,
            _ => DOM_CAP,
        },
        Kind::Int(d) => int_domain(d),
        Kind::Float(d) => float_domain(d),
        Kind::Generic(_) | Kind::TypeVar(_) => {
            unreachable!("the generator emits neither Generic nor TypeVar")
        }
        _ => DOM_CAP, // scalars, temporal, ids, Any, Raw — unbounded
    }
}

fn distinct_literal_count(d: &LiteralDesc) -> usize {
    let mut seen = std::collections::HashSet::new();
    d.members
        .iter()
        .filter(|m| seen.insert((*m).clone()))
        .count()
        .clamp(1, DOM_CAP)
}

fn distinct_enum_count(d: &EnumDesc) -> usize {
    let mut seen = std::collections::HashSet::new();
    d.members
        .iter()
        .filter(|m| seen.insert(m.value.clone()))
        .count()
        .clamp(1, DOM_CAP)
}

/// Objects a `dict` node can take: key-subsets in the emit-size range ×
/// value assignments — `Σₙ C(k,n)·vⁿ` over `n ∈ len_range`.
fn dict_domain(d: &DictDesc) -> usize {
    let r = domain_len_range(d.len, 5, key_domain_size(&d.key));
    if *r.end() == 0 {
        return 1; // `len.max = 0` — exactly `{}` is generatable
    }
    let (k, v) = (key_domain_size(&d.key), domain_size(&d.value));
    if k >= 8 || v >= 8 {
        return DOM_CAP;
    }
    let mut sum = 0usize;
    for n in *r.start()..=(*r.end()).min(k) {
        sum = sum.saturating_add(
            usize::try_from(choose(k, n))
                .unwrap_or(usize::MAX)
                .saturating_mul(pow_capped(v, n)),
        );
        if sum >= DOM_CAP {
            return DOM_CAP;
        }
    }
    sum.min(DOM_CAP)
}

/// Record outputs: required fields always present, non-required add an
/// "absent" option. `Keep` extras make the domain unbounded.
fn record_domain(d: &RecordDesc) -> usize {
    if matches!(d.extra, ExtraPolicy::Keep) {
        return DOM_CAP;
    }
    d.fields
        .iter()
        .map(|f| {
            let base = domain_size(&f.ty);
            match f.missing {
                Missing::Required => base,
                _ => base.saturating_add(1),
            }
        })
        .fold(1usize, usize::saturating_mul)
        .min(DOM_CAP)
}

fn int_domain(d: &IntDesc) -> usize {
    match &d.range {
        Some(r) => {
            let (lo, hi) = int_bounds(r);
            usize::try_from(i128::from(hi).saturating_sub(i128::from(lo)) + 1)
                .unwrap_or(DOM_CAP)
                .min(DOM_CAP)
        }
        None => DOM_CAP,
    }
}

/// A degenerate float range `[x, x]` admits exactly one value; every
/// other range is effectively unbounded for generation. Exact equality
/// is intentional: `[-0.0, 0.0]` compares equal and yields ≤2 values,
/// which the caller under-estimates safely as 1.
#[expect(
    clippy::float_cmp,
    reason = "degenerate-range detection requires exact bound equality"
)]
fn float_domain(d: &FloatDesc) -> usize {
    let Some(r) = &d.range else { return DOM_CAP };
    let as_f64 = |b: &Bound| match b {
        Bound::Float(f) => *f,
        _ => 0.0,
    };
    let lo = r.ge.as_ref().map_or(f64::NEG_INFINITY, &as_f64);
    let hi = r.le.as_ref().map_or(f64::INFINITY, &as_f64);
    if lo == hi { 1 } else { DOM_CAP }
}

/// Generatable domain of a container node (`list`, `tuple[T, ...]`,
/// `set`, `frozenset`): `Σ sizes` of arrays (ordered, dups allowed) or
/// subsets (deduped) over the sizes `arb_value_for` actually emits
/// (`len_range` with the same `default_max`).
fn container_domain(item: &TypeDesc, len: Option<Len>, dedupes: bool) -> usize {
    let r = len_range(len, 5);
    let (lo, hi) = (*r.start(), *r.end());
    if hi == 0 {
        return 1; // `len.max = 0` admits exactly the empty container
    }
    let item_dom = domain_size(item);
    if item_dom >= DOM_CAP {
        return DOM_CAP;
    }
    if dedupes {
        if item_dom < 8 {
            choose_sum_range(item_dom, lo, hi)
        } else {
            DOM_CAP
        }
    } else {
        pow_sum_range(item_dom, lo, hi)
    }
}

/// Distinct *coerced* keys a `dict` key descriptor admits — dict Len is
/// checked after key-text dedupe, so `dict[bool]` can hold at most 2
/// entries (`"true"`/`"false"` are the canonical key texts); every other
/// generated key kind is unbounded.
fn key_domain_size(desc: &TypeDesc) -> usize {
    match &desc.kind {
        Kind::Bool => 2,
        _ => DOM_CAP,
    }
}

/// [`len_range`] additionally capped at `domain` — the result is always
/// a valid non-empty range.
fn domain_len_range(len: Option<Len>, default_max: usize, domain: usize) -> RangeInclusive<usize> {
    let r = len_range(len, default_max);
    let lo = (*r.start()).min(domain);
    let hi = (*r.end()).min(domain).max(lo);
    lo..=hi
}

/// Whether a generated descriptor admits at least one value. Dedupe-
/// sensitive nodes (`set`, `unique` lists, `dict` keys) fail this when a
/// `Len.min` exceeds the item/key domain — such descriptors compile fine
/// but no input can validate, so the typed property filters them out.
fn satisfiable(desc: &TypeDesc) -> bool {
    fn fits(len: Option<Len>, domain: usize) -> bool {
        len.is_none_or(|l| usize::try_from(l.min.unwrap_or(0)).unwrap_or(usize::MAX) <= domain)
    }
    match &desc.kind {
        Kind::List(d) => (!d.unique || fits(d.len, domain_size(&d.item))) && satisfiable(&d.item),
        Kind::Set(d) | Kind::FrozenSet(d) => {
            fits(d.len, domain_size(&d.item)) && satisfiable(&d.item)
        }
        Kind::TupleVar(d) => satisfiable(&d.item),
        Kind::TupleFixed(d) => d.items.iter().all(satisfiable),
        Kind::Dict(d) => fits(d.len, key_domain_size(&d.key)) && satisfiable(&d.value),
        Kind::Optional(d) => satisfiable(&d.inner),
        Kind::NewType(d) => satisfiable(&d.inner),
        // The generator picks a variant uniformly — every variant must
        // admit a value for the property to hold.
        Kind::Union(d) => d.variants.iter().all(satisfiable),
        Kind::Record(d) => d.fields.iter().all(|f| satisfiable(&f.ty)),
        _ => true,
    }
}

/// `ge`/`le` of a generated `Range` as `i64` — the generator only emits
/// `Bound::Int` in those slots (and never `gt`/`lt`).
fn int_bounds(range: &Range) -> (i64, i64) {
    let as_i64 = |b: &Bound| match b {
        Bound::Int(i) => i64::try_from(*i).unwrap_or(0),
        _ => 0,
    };
    let lo = range.ge.as_ref().map_or(i64::MIN, &as_i64);
    let hi = range.le.as_ref().map_or(i64::MAX, &as_i64);
    (lo.min(hi), hi.max(lo))
}

fn arb_int_value(d: &IntDesc) -> BoxedStrategy<Value<'static>> {
    if let Some(range) = &d.range {
        let (lo, hi) = int_bounds(range);
        return (lo..=hi).prop_map(Value::Int).boxed();
    }
    if let Some(Bound::Int(k)) = &d.multiple_of {
        let k = i64::try_from(*k).unwrap_or(1).max(1);
        return any::<i32>()
            .prop_map(move |m| Value::Int(i64::from(m) * k))
            .boxed();
    }
    prop_oneof![
        6 => any::<i64>().prop_map(Value::Int),
        1 => (9_223_372_036_854_775_808u64..=u64::MAX).prop_map(Value::UInt),
        1 => arb_bigint().prop_map(|t| Value::BigInt(t.into())),
    ]
    .boxed()
}

fn arb_float_value(d: &FloatDesc) -> BoxedStrategy<Value<'static>> {
    if let Some(range) = &d.range {
        let as_f64 = |b: &Bound| match b {
            Bound::Float(f) => *f,
            _ => 0.0,
        };
        let lo = range.ge.as_ref().map_or(f64::NEG_INFINITY, &as_f64);
        let hi = range.le.as_ref().map_or(f64::INFINITY, &as_f64);
        return (lo.min(hi)..=hi.max(lo)).prop_map(Value::Float).boxed();
    }
    finite_f64().prop_map(Value::Float).boxed()
}

/// A string matching one of the whitelisted `PATTERNS` sources.
fn arb_pattern_string(source: &str) -> BoxedStrategy<String> {
    match source {
        "^[a-z]+$" => "[a-z]{1,8}".boxed(),
        "^[0-9]{2,4}$" => "[0-9]{2,4}".boxed(),
        "^[A-Z][a-z]{2,8}$" => "[A-Z][a-z]{2,8}".boxed(),
        _ => unreachable!("the generator only emits PATTERNS sources"),
    }
}

fn arb_str_value(d: &StrDesc) -> BoxedStrategy<Value<'static>> {
    if let Some(pattern) = &d.pattern {
        return arb_pattern_string(pattern.source())
            .prop_map(|s| Value::Str(s.into()))
            .boxed();
    }
    match d.len {
        Some(l) => {
            proptest::collection::vec(proptest::char::range('a', 'z'), len_range(Some(l), 8))
                .prop_map(|cs| Value::Str(cs.into_iter().collect::<String>().into()))
                .boxed()
        }
        None => ".*".prop_map(|s| Value::Str(s.into())).boxed(),
    }
}

fn arb_bytes_value(d: &BytesDesc) -> BoxedStrategy<Value<'static>> {
    proptest::collection::vec(any::<u8>(), len_range(d.len, 16))
        .prop_map(|b| Value::Bytes(b.into()))
        .boxed()
}

/// A valid `Decimal` input: `"<int>.<frac>"` text, a plain integer, or
/// big-integer text — all accepted strict inputs.
fn arb_decimal_value() -> BoxedStrategy<Value<'static>> {
    prop_oneof![
        3 => (any::<i64>(), 0u32..1_000_000)
            .prop_map(|(i, f)| Value::Str(format!("{i}.{f}").into())),
        1 => any::<i64>().prop_map(Value::Int),
        1 => arb_bigint().prop_map(|t| Value::Str(format!("{t}.0").into())),
    ]
    .boxed()
}

fn arb_iso_date() -> impl Strategy<Value = String> {
    (1u64..=9999, 1u64..=12, 1u64..=28).prop_map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}"))
}

fn arb_iso_time() -> impl Strategy<Value = String> {
    (0u64..24, 0u64..60, 0u64..60).prop_map(|(h, m, s)| format!("{h:02}:{m:02}:{s:02}"))
}

fn arb_iso_duration() -> impl Strategy<Value = String> {
    (0u64..10_000, 0u64..24, 0u64..60, 0u64..60)
        .prop_map(|(d, h, m, s)| format!("P{d}DT{h}H{m}M{s}S"))
}

/// Text that always satisfies a string-carried scalar kind.
fn arb_text_value(desc: &TypeDesc) -> BoxedStrategy<Value<'static>> {
    let s: BoxedStrategy<String> = match &desc.kind {
        Kind::Date => arb_iso_date().boxed(),
        Kind::Time => arb_iso_time().boxed(),
        Kind::DateTime => (arb_iso_date(), arb_iso_time())
            .prop_map(|(d, t)| format!("{d}T{t}Z"))
            .boxed(),
        Kind::TimeDelta => arb_iso_duration().boxed(),
        Kind::Uuid => "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}".boxed(),
        Kind::Email => "[a-z0-9]{1,8}@[a-z]{1,8}\\.[a-z]{2,4}".boxed(),
        Kind::Url => "[a-z]{2,6}://[a-z]{2,10}".boxed(),
        Kind::Ipv4 => proptest::collection::vec(any::<u8>(), 4..=4)
            .prop_map(|o| format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]))
            .boxed(),
        Kind::Ipv6 => proptest::collection::vec(any::<u16>(), 8..=8)
            .prop_map(|g| Ipv6Addr::new(g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]).to_string())
            .boxed(),
        _ => unreachable!("not a string-carried kind"),
    };
    s.prop_map(|t| Value::Str(t.into())).boxed()
}

/// Object-key text honoring a `dict` key descriptor (`arb_key_desc`
/// restricts the space to `str`/`int`/`float`/`bool`). Keys are emitted
/// in canonical `key_text` form — the validator dedupes on the coerced
/// text (`"off"` and `"false"` both land on `"false"`), so distinct
/// wire texts are not enough; canonical texts are.
fn arb_dict_key(key: &TypeDesc) -> BoxedStrategy<String> {
    match &key.kind {
        Kind::Bool => proptest::sample::select(vec!["true", "false"])
            .prop_map(str::to_owned)
            .boxed(),
        Kind::Int(_) => any::<i64>().prop_map(|i| i.to_string()).boxed(),
        Kind::Float(_) => finite_f64().prop_map(|f| format!("{f:?}")).boxed(),
        Kind::Str(_) => ".*".boxed(),
        _ => unreachable!("dict keys are restricted to str/int/float/bool"),
    }
}

/// `set`/`frozenset`/`unique`-list items, deduplicated in *output space*:
/// the validator dedupes on validated values — two distinct inputs can
/// normalize to the same output (`{"a":1,"zzz_x":2}` and `{"a":1}` are
/// the same record under `ExtraPolicy::Ignore`; `"5.50"` and `"5.5"`
/// the same Decimal), so input-side uniqueness is not enough.
///
/// The flat map picks a size `n` in `Len` bounds first — emitting `n`
/// distinct items *must* vary (a `list max=1` over a 1-member Literal
/// yields only `[]`/`[lit]`; filling to the max always emits `[lit]`).
/// Inputs are kept whose validated outputs are distinct (emitting
/// outputs would be wrong — record outputs carry field `name`s, not
/// wire keys, so an output value is not necessarily a valid input).
/// `None` when the oversample cannot supply `n` distinct outputs.
fn arb_deduped_items(item: &TypeDesc, len: Option<Len>) -> BoxedStrategy<Vec<Value<'static>>> {
    let item = item.clone();
    domain_len_range(len, 5, domain_size(&item))
        .prop_flat_map(move |n| {
            // Exactly `n` distinct outputs — oversample inputs, dedupe on
            // validated values (emitting inputs — outputs aren't always
            // valid inputs: record outputs carry `name`, not wire keys).
            let item_v = compile(&item).expect("generated item desc must compile");
            let reason: &'static str =
                Box::leak(format!("dedupe<{item:?}> n={n}").into_boxed_str());
            proptest::collection::vec(arb_value_for(&item), n.saturating_mul(2) + 2)
                .prop_filter_map(reason, move |items| {
                    let mut seen = std::collections::HashSet::new();
                    let mut out = Vec::new();
                    for v in &items {
                        if out.len() == n {
                            break;
                        }
                        let out_v = match item_v.validate_out(v, Source::Body, Mode::Strict) {
                            Ok(Checked::Same(v)) => (*v).clone(),
                            Ok(Checked::New(v)) => v,
                            Err(e) => panic!("generated item must validate: {v:?} → {e:?}"),
                        };
                        if seen.insert(out_v) {
                            out.push(v.clone());
                        }
                    }
                    (out.len() == n).then_some(out)
                })
                .boxed()
        })
        .boxed()
}

fn arb_list_value(
    item: &TypeDesc,
    len: Option<Len>,
    unique: bool,
) -> BoxedStrategy<Value<'static>> {
    if unique {
        arb_deduped_items(item, len).prop_map(Value::Array).boxed()
    } else {
        proptest::collection::vec(arb_value_for(item), len_range(len, 5))
            .prop_map(Value::Array)
            .boxed()
    }
}

fn arb_set_value(item: &TypeDesc, len: Option<Len>) -> BoxedStrategy<Value<'static>> {
    arb_deduped_items(item, len).prop_map(Value::Array).boxed()
}

fn arb_dict_value(d: &DictDesc) -> BoxedStrategy<Value<'static>> {
    proptest::collection::hash_map(
        arb_dict_key(&d.key),
        arb_value_for(&d.value),
        domain_len_range(d.len, 5, key_domain_size(&d.key)),
    )
    .prop_map(|m| Value::Object(m.into_iter().map(|(k, v)| (Cow::Owned(k), v)).collect()))
    .boxed()
}

/// A `Value` per position of a `TupleFixed`/`Record` field list.
fn arb_value_list(items: &[TypeDesc]) -> BoxedStrategy<Vec<Value<'static>>> {
    let mut strategy = Just(Vec::new()).boxed();
    for t in items {
        strategy = (strategy, arb_value_for(t))
            .prop_map(|(mut acc, v)| {
                acc.push(v);
                acc
            })
            .boxed();
    }
    strategy
}

/// An `Object` satisfying a `RecordDesc`: required fields always present,
/// `Omit`/`Literal`-missing fields present half the time, extras only
/// where the policy tolerates them (named `zzz_*` — never a wire key).
fn arb_record_value(desc: &RecordDesc) -> BoxedStrategy<Value<'static>> {
    type Pairs = Vec<(Cow<'static, str>, Value<'static>)>;
    let base: BoxedStrategy<Pairs> = match desc.extra {
        ExtraPolicy::Forbid => Just(Vec::new()).boxed(),
        ExtraPolicy::Ignore | ExtraPolicy::Keep => {
            proptest::collection::vec(("zzz_[a-z]{1,4}", arb_json_value()), 0..2)
                .prop_map(|extras| {
                    extras
                        .into_iter()
                        .map(|(k, v)| (Cow::Owned(k), v))
                        .collect()
                })
                .boxed()
        }
    };
    let mut strategy = base;
    for field in &desc.fields {
        let wire_key = field.wire_key(desc.rename).to_string();
        let value = match field.missing {
            Missing::Required => arb_value_for(&field.ty).prop_map(Some).boxed(),
            _ => proptest::option::of(arb_value_for(&field.ty)).boxed(),
        };
        strategy = (strategy, value)
            .prop_map(move |(mut pairs, v)| {
                if let Some(v) = v {
                    pairs.push((Cow::Owned(wire_key.clone()), v));
                }
                pairs
            })
            .boxed();
    }
    strategy.prop_map(Value::Object).boxed()
}

/// A `Value` that must validate against `desc` in strict mode.
fn arb_value_for(desc: &TypeDesc) -> BoxedStrategy<Value<'static>> {
    match &desc.kind {
        Kind::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        Kind::Int(d) => arb_int_value(d),
        Kind::Float(d) => arb_float_value(d),
        Kind::Str(d) => arb_str_value(d),
        Kind::Bytes(d) => arb_bytes_value(d),
        Kind::Decimal(_) => arb_decimal_value(),
        Kind::Null => Just(Value::Null).boxed(),
        Kind::DateTime
        | Kind::Date
        | Kind::Time
        | Kind::TimeDelta
        | Kind::Uuid
        | Kind::Email
        | Kind::Url
        | Kind::Ipv4
        | Kind::Ipv6 => arb_text_value(desc),
        Kind::List(d) => arb_list_value(&d.item, d.len, d.unique),
        Kind::Set(d) | Kind::FrozenSet(d) => arb_set_value(&d.item, d.len),
        Kind::TupleVar(d) => proptest::collection::vec(arb_value_for(&d.item), len_range(d.len, 5))
            .prop_map(Value::Array)
            .boxed(),
        Kind::TupleFixed(d) => arb_value_list(&d.items).prop_map(Value::Array).boxed(),
        Kind::Dict(d) => arb_dict_value(d),
        Kind::Literal(d) => proptest::sample::select(d.members.to_vec())
            .prop_map(|m| m.to_value())
            .boxed(),
        Kind::Enum(d) => proptest::sample::select(d.members.to_vec())
            .prop_map(|m| m.value.to_value())
            .boxed(),
        Kind::Optional(d) => prop_oneof![
            1 => Just(Value::Null),
            3 => arb_value_for(&d.inner),
        ]
        .boxed(),
        Kind::Union(d) => {
            let variants = d.variants.to_vec();
            proptest::sample::select((0..variants.len()).collect::<Vec<_>>())
                .prop_flat_map(move |i| arb_value_for(&variants[i]))
                .boxed()
        }
        Kind::Record(d) => arb_record_value(d),
        Kind::NewType(d) => arb_value_for(&d.inner),
        Kind::Any | Kind::Raw => arb_json_value().boxed(),
        Kind::TypeVar(_) | Kind::Generic(_) => {
            unreachable!("the generator emits neither Generic nor TypeVar")
        }
    }
}

/// `(TypeDesc, value-that-validates-for-it)` pairs, depth ≤ 3.
/// Unsatisfiable descriptors (`set[bool] Len(min=5)` compiles but no
/// value can validate) are filtered out before pairing.
fn arb_typed() -> impl Strategy<Value = (TypeDesc, Value<'static>)> {
    arb_desc(3)
        .prop_filter("satisfiable descriptor", satisfiable)
        .prop_flat_map(|d| (Just(d.clone()), arb_value_for(&d)))
}

// ---- properties -------------------------------------------------------------

proptest! {
    /// `decode(encode(v)) == v`, and the decoder is a fixpoint under its
    /// own encode/decode loop (M2.20a).
    #[test]
    fn json_encode_decode_roundtrip(v in arb_json_value()) {
        let text = to_json(&v);
        let decoded =
            decode_json(text.as_bytes()).expect("self-encoded JSON must decode");
        prop_assert_eq!(&decoded, &v);
        let text2 = to_json(&decoded);
        let re = decode_json(text2.as_bytes()).expect("re-encoded JSON must decode");
        prop_assert_eq!(re, decoded);
    }

    /// A generated descriptor compiles and its generated value validates
    /// (M2.20b) — strict `validate`, plus `decode_and_validate` whenever
    /// the value is JSON-encodable.
    #[test]
    fn generated_values_validate((desc, value) in arb_typed()) {
        let validator = compile(&desc)
            .unwrap_or_else(|e| panic!("generated desc must compile: {desc:?}\n{e}"));
        let out = validator.validate(&value, Source::Body, Mode::Strict);
        prop_assert!(
            out.is_ok(),
            "generated value must validate\ndesc: {desc:?}\nvalue: {value:?}\nerrors: {out:?}"
        );
        if !contains_bytes(&value) {
            let text = to_json(&value);
            let res = decode_and_validate(text.as_bytes(), &validator);
            prop_assert!(
                res.is_ok(),
                "decode_and_validate must pass\ntext: {text}\ndesc: {desc:?}"
            );
        }
    }

    /// Arbitrary bytes never panic (M2.20c) — the fuzz targets' property,
    /// inside `cargo test` too.
    #[test]
    fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode_json(&bytes);
        let limits = Limits {
            max_depth: 7,
            max_items: 64,
            max_str_bytes: 128,
            ..Limits::default()
        };
        let _ = decode_json_with_limits(&bytes, &limits);
    }

    /// Well-formed generated documents decode under generous limits, and
    /// depth-1 limits reject every non-scalar as `Limit` (never Malformed,
    /// never a panic).
    #[test]
    fn decode_respects_limits(v in arb_json_value()) {
        let text = to_json(&v);
        let generous = Limits {
            max_depth: 8,
            ..Limits::default()
        };
        prop_assert!(decode_json_with_limits(text.as_bytes(), &generous).is_ok());
        let shallow = Limits {
            max_depth: 1,
            ..Limits::default()
        };
        // A container nests (and hits the depth limit) iff it is non-empty;
        // scalars and empty `{}`/`[]` decode at depth 1.
        let nests = v.as_array().is_some_and(|i| !i.is_empty())
            || v.as_object().is_some_and(|p| !p.is_empty());
        match decode_json_with_limits(text.as_bytes(), &shallow) {
            Ok(_) => prop_assert!(!nests),
            Err(DecodeError::Limit(_)) => prop_assert!(nests),
            Err(DecodeError::Malformed(e)) => {
                panic!("self-encoded JSON malformed: {e}")
            }
        }
    }

    /// The lax adapters take arbitrary text and return — never panic
    /// (M2.20c, same surface as the `decode_query`/`decode_form`/
    /// `decode_cookie` fuzz targets).
    #[test]
    fn lax_sources_never_panic(s in ".*") {
        let _ = decode_query(&s);
        let _ = decode_form(&s);
        let _ = decode_cookie(&s);
    }

    /// `validate_text` coerces or rejects arbitrary text — never panics.
    #[test]
    fn validate_text_never_panics(s in ".*") {
        let validators = [
            compile(&TypeDesc::int()).expect("int compiles"),
            compile(&TypeDesc::datetime()).expect("datetime compiles"),
            compile(&TypeDesc::bool()).expect("bool compiles"),
            compile(&TypeDesc::list(TypeDesc::int())).expect("list compiles"),
        ];
        for v in &validators {
            let _ = validate_text(&s, v, Source::Query);
        }
    }
}
