//! Compiling a [`TypeDesc`] into a [`Validator`].
//!
//! `compile` is the startup path: it walks the descriptor once, collects
//! *every* structural problem into a [`CompileErrors`] (bad `Range`,
//! `MultipleOf(0)`, colliding wire keys, unbound `TypeVar`s, malformed
//! discriminated unions, ...) and — only when nothing failed — hands back
//! a `Validator` whose node tree is specialized for validation:
//!
//! - `Pattern`s were already compiled when the `TypeDesc` was built
//!   (invalid regexes fail at IR construction — even earlier).
//! - `Range`/`MultipleOf` bounds are parsed into `LitNum`s once.
//! - Records get a `wire_key → field` map; discriminated unions get a
//!   `tag value → variant` map; `Literal`/`Enum` members get a value map.
//! - `Generic` applications are *substituted* away — no `Generic` or
//!   `TypeVar` node survives into the validator.
//!
//! The request path then runs [`Validator::validate`] with no further
//! allocation of structure.

use std::collections::{HashMap, HashSet};

use crate::desc::{
    Bound, EnumDesc, FieldDesc, GenericDesc, Kind, Len, Range, RecordDesc, TypeDesc, UnionDesc,
};
use crate::num::{DecLit, LitNum, MAX_MOD_MANTISSA_DIGITS};
use crate::validator::{
    BytesV, DictV, EnumV, FieldV, IntV, ListV, LiteralV, RangeV, RecordV, SetV, StrV, UnionV,
    VKind, VNode, Validator,
};
use crate::value::LiteralValue;

/// Limits enforced by validators — both at compile time (descriptor
/// nesting) and per request (input shape). Defaults follow DESIGN.md §14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum input nesting depth a validator descends before emitting
    /// `max_depth` (default 64, §14 "maxDepth").
    pub max_depth: u32,
    /// Maximum items/entries in any `Array`/`Object` input before
    /// `max_items` (default 100 000, §14 "maxItems").
    pub max_items: usize,
    /// Maximum byte length of any `Str`/`Bytes` input before `max_str_len`
    /// (default unbounded — callers set this from the request body limit).
    pub max_str_bytes: usize,
    /// Maximum `TypeDesc` nesting `compile` walks before failing the
    /// descriptor (default 64 — a deeper descriptor is rejected, so the
    /// validator can never be pushed past `max_depth` by structure alone).
    pub max_desc_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_items: 100_000,
            max_str_bytes: usize::MAX,
            max_desc_depth: 64,
        }
    }
}

impl Limits {
    /// The element-wise tighter bound of `self` and `other` — §14.8's
    /// "plugins can only make limits stricter" merge: the result is
    /// never looser than either input.
    #[must_use]
    pub fn tightened(&self, other: &Limits) -> Limits {
        Limits {
            max_depth: self.max_depth.min(other.max_depth),
            max_items: self.max_items.min(other.max_items),
            max_str_bytes: self.max_str_bytes.min(other.max_str_bytes),
            max_desc_depth: self.max_desc_depth.min(other.max_desc_depth),
        }
    }
}

/// One problem found while compiling a descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// Where in the descriptor — a `/`-joined path of kind names and
    /// field names, e.g. `"User/fields/email/str"`.
    pub path: String,
    /// What is wrong (`"Range: ge > le"`, `"MultipleOf must be > 0"`, ...).
    pub message: String,
}

/// Every problem found during one [`compile`] pass.
#[derive(Debug, Default)]
pub struct CompileErrors {
    entries: Vec<CompileError>,
}

impl CompileErrors {
    /// The collected problems.
    pub fn entries(&self) -> &[CompileError] {
        &self.entries
    }
}

impl std::fmt::Display for CompileErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for e in &self.entries {
            writeln!(f, "{}: {}", e.path, e.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for CompileErrors {}

/// Compile `desc` with default [`Limits`].
///
/// # Errors
///
/// [`CompileErrors`] lists every structural/semantic problem found in the
/// descriptor (empty `Literal`/`union`, bad `Range`/`MultipleOf`, invalid
/// regex pattern, unbound `TypeVar`, arity mismatch, depth limit, …).
pub fn compile(desc: &TypeDesc) -> Result<Validator, CompileErrors> {
    compile_with_limits(desc, Limits::default())
}

/// Compile `desc`, enforcing `limits` both now (descriptor depth) and in
/// every [`Validator::validate`] call the result performs.
///
/// # Errors
///
/// [`CompileErrors`] lists every structural/semantic problem found in the
/// descriptor — same contract as [`compile`].
pub fn compile_with_limits(desc: &TypeDesc, limits: Limits) -> Result<Validator, CompileErrors> {
    let mut errs = Vec::new();
    let mut path = Vec::new();
    let scope = Scope::new();
    let root = compile_node(desc, &mut path, 0, &scope, &limits, &mut errs);
    match root {
        Some(root) if errs.is_empty() => Ok(Validator::new(root, limits)),
        _ => Err(CompileErrors { entries: errs }),
    }
}

/// The `TypeVar → TypeDesc` bindings in scope at a point in the tree —
/// populated by [`Kind::Generic`] substitution.
type Scope = HashMap<Box<str>, TypeDesc>;

fn compile_node(
    desc: &TypeDesc,
    path: &mut Vec<String>,
    depth: u32,
    scope: &Scope,
    limits: &Limits,
    errs: &mut Vec<CompileError>,
) -> Option<VNode> {
    if depth > limits.max_desc_depth {
        errs.push(CompileError {
            path: format!("{}/{}", path.join("/"), desc.kind.name()),
            message: format!(
                "descriptor nesting exceeds max_desc_depth ({})",
                limits.max_desc_depth
            ),
        });
        return None;
    }
    match &desc.kind {
        // A `TypeVar` resolves through scope — the argument replaces the
        // whole node (its own metadata wins; the TypeVar node's `meta`
        // is dropped).
        Kind::TypeVar(name) => {
            if let Some(d) = scope.get(name) {
                let d = d.clone();
                return compile_node(&d, path, depth, scope, limits, errs);
            }
            errs.push(CompileError {
                path: format!("{}/{}", path.join("/"), name),
                message: format!("unbound type variable {name:?}"),
            });
            return None;
        }
        // `Origin[args]` — substitute args for the origin's type_params
        // over a clone, then compile the substituted tree. The Generic
        // node's own `meta` (if any) overrides the origin's root meta.
        Kind::Generic(g) => {
            let applied = apply_generic(g, path, scope, errs)?;
            let mut applied = applied;
            if desc.meta.is_some() {
                applied.meta.clone_from(&desc.meta);
            }
            return compile_node(&applied, path, depth, scope, limits, errs);
        }
        _ => {}
    }
    let transforms = desc.meta().transforms();
    path.push(desc.kind.name());
    let kind = compile_kind(&desc.kind, path, depth, scope, limits, errs);
    path.pop();
    kind.map(|kind| VNode { kind, transforms })
}

/// Substitute `args` into a `Generic` origin; validates the origin is a
/// `Record` with matching arity. Returns the substituted descriptor.
fn apply_generic(
    g: &GenericDesc,
    path: &[String],
    scope: &Scope,
    errs: &mut Vec<CompileError>,
) -> Option<TypeDesc> {
    let Kind::Record(origin) = &g.origin.kind else {
        errs.push(CompileError {
            path: path.join("/"),
            message: format!("generic origin {} is not a record", g.origin.kind.name()),
        });
        return None;
    };
    if origin.type_params.len() != g.args.len() {
        errs.push(CompileError {
            path: path.join("/"),
            message: format!(
                "{} takes {} type arguments, got {}",
                origin.class.qualified(),
                origin.type_params.len(),
                g.args.len()
            ),
        });
        return None;
    }
    // Args may themselves mention TypeVars from an *outer* scope.
    let subst: Scope = origin
        .type_params
        .iter()
        .cloned()
        .zip(g.args.iter().map(|a| substitute(a, scope)))
        .collect();
    // Substitute the origin's fields directly — going through
    // `substitute` on the record itself would unbind its params.
    let mut applied = (*g.origin).clone();
    if let Kind::Record(r) = &mut applied.kind {
        r.fields = r
            .fields
            .iter()
            .map(|f| FieldDesc {
                ty: substitute(&f.ty, &subst),
                ..f.clone()
            })
            .collect();
        r.type_params = Box::new([]);
    }
    Some(applied)
}

/// `Len` bounds sanity: `min ≤ max` when both are set.
fn check_len(len: Option<Len>, path: &[String], errs: &mut Vec<CompileError>) {
    if let Some(l) = len
        && let (Some(min), Some(max)) = (l.min, l.max)
        && min > max
    {
        errs.push(CompileError {
            path: path.join("/"),
            message: format!("Len: min ({min}) > max ({max})"),
        });
    }
}

/// Parse a `Bound` into its runtime numeric form; invalid `Decimal` text
/// is a compile error here, never a request-time surprise.
fn bound_num(b: &Bound, path: &[String], errs: &mut Vec<CompileError>) -> Option<LitNum> {
    match b {
        Bound::Int(i) => Some(LitNum::Int(*i)),
        Bound::Float(f) => {
            if f.is_nan() {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: "bound is NaN".to_owned(),
                });
                None
            } else {
                Some(LitNum::Float(*f))
            }
        }
        Bound::Decimal(text) => {
            if let Some(d) = DecLit::parse(text) {
                Some(LitNum::Dec(d))
            } else {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!("invalid Decimal literal {text:?}"),
                });
                None
            }
        }
    }
}

/// A `Range`: parse bounds, reject `NaN`, reject empty ranges.
fn range_v(range: Option<&Range>, path: &[String], errs: &mut Vec<CompileError>) -> Option<RangeV> {
    let r = range?;
    let mut opt = |b: &Option<Bound>| b.as_ref().and_then(|b| bound_num(b, path, errs));
    let v = RangeV {
        ge: opt(&r.ge),
        gt: opt(&r.gt),
        le: opt(&r.le),
        lt: opt(&r.lt),
    };
    // Empty range checks — every comparable (low, high) pair.
    for (lo, hi, strict) in [
        (v.ge.as_ref(), v.le.as_ref(), false),
        (v.ge.as_ref(), v.lt.as_ref(), true),
        (v.gt.as_ref(), v.le.as_ref(), true),
        (v.gt.as_ref(), v.lt.as_ref(), true),
    ] {
        if let (Some(lo), Some(hi)) = (lo, hi) {
            let empty = match lo.cmp(hi) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => strict,
                std::cmp::Ordering::Less => false,
            };
            if empty {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: "Range is empty (lower bound excludes upper bound)".to_owned(),
                });
            }
        }
    }
    Some(v)
}

/// A `MultipleOf` operand: must parse and be strictly positive, and a
/// `Decimal` operand's mantissa must fit `u128` (38 digits — the
/// divisibility algorithm's documented bound).
fn multiple_num(
    m: Option<&Bound>,
    path: &[String],
    errs: &mut Vec<CompileError>,
) -> Option<LitNum> {
    let b = m?;
    let n = bound_num(b, path, errs)?;
    let ok = match &n {
        LitNum::Int(i) => *i > 0,
        LitNum::Float(f) => *f > 0.0,
        LitNum::Dec(d) => {
            if d.digits.len() > MAX_MOD_MANTISSA_DIGITS {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!(
                        "MultipleOf mantissa exceeds {MAX_MOD_MANTISSA_DIGITS} digits"
                    ),
                });
                return None;
            }
            !d.is_zero()
        }
    };
    if !ok {
        errs.push(CompileError {
            path: path.join("/"),
            message: "MultipleOf must be > 0".to_owned(),
        });
        return None;
    }
    Some(n)
}

/// Whether `kind` may sit behind a `dict[K, _]` key — the key must be a
/// string-coercible scalar (everything the wire can spell in an object
/// key position).
fn key_coercible(kind: &Kind) -> bool {
    match kind {
        Kind::Bool
        | Kind::Int(_)
        | Kind::Float(_)
        | Kind::Str(_)
        | Kind::Decimal(_)
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
        | Kind::Enum(_)
        | Kind::NewType(_)
        | Kind::TypeVar(_)
        | Kind::Optional(_)
        | Kind::Generic(_) => true,
        Kind::Union(d) => d.variants.iter().all(|v| key_coercible(&v.kind)),
        _ => false,
    }
}

/// The declared wire keys a `Literal` field contributes to a
/// discriminated-union tag map — every member must be a `Str`.
fn literal_tag_values(field: &FieldDesc) -> Vec<Box<str>> {
    let Kind::Literal(lit) = &field.ty.kind else {
        return Vec::new();
    };
    lit.members
        .iter()
        .filter_map(|m| match m {
            LiteralValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn compile_kind(
    kind: &Kind,
    path: &mut Vec<String>,
    depth: u32,
    scope: &Scope,
    limits: &Limits,
    errs: &mut Vec<CompileError>,
) -> Option<VKind> {
    match kind {
        Kind::Bool => Some(VKind::Bool),
        Kind::Null => Some(VKind::Null),
        Kind::DateTime => Some(VKind::DateTime),
        Kind::Date => Some(VKind::Date),
        Kind::Time => Some(VKind::Time),
        Kind::TimeDelta => Some(VKind::TimeDelta),
        Kind::Uuid => Some(VKind::Uuid),
        Kind::Email => Some(VKind::Email),
        Kind::Url => Some(VKind::Url),
        Kind::Ipv4 => Some(VKind::Ipv4),
        Kind::Ipv6 => Some(VKind::Ipv6),
        Kind::Any => Some(VKind::Any),
        Kind::Raw => Some(VKind::Raw),
        Kind::Int(d) => Some(VKind::Int(IntV {
            range: range_v(d.range.as_ref(), path, errs),
            multiple: multiple_num(d.multiple_of.as_ref(), path, errs),
        })),
        Kind::Float(d) => Some(VKind::Float(IntV {
            range: range_v(d.range.as_ref(), path, errs),
            multiple: multiple_num(d.multiple_of.as_ref(), path, errs),
        })),
        Kind::Decimal(d) => Some(VKind::Decimal(IntV {
            range: range_v(d.range.as_ref(), path, errs),
            multiple: multiple_num(d.multiple_of.as_ref(), path, errs),
        })),
        Kind::Str(d) => {
            check_len(d.len, path, errs);
            Some(VKind::Str(StrV {
                len: d.len,
                pattern: d.pattern.clone(),
            }))
        }
        Kind::Bytes(d) => {
            check_len(d.len, path, errs);
            Some(VKind::Bytes(BytesV { len: d.len }))
        }
        _ => compile_composite(kind, path, depth, scope, limits, errs),
    }
}

/// The `Kind`s that contain nested descriptors.
fn compile_composite(
    kind: &Kind,
    path: &mut Vec<String>,
    depth: u32,
    scope: &Scope,
    limits: &Limits,
    errs: &mut Vec<CompileError>,
) -> Option<VKind> {
    match kind {
        Kind::List(d) => {
            check_len(d.len, path, errs);
            compile_node(&d.item, path, depth + 1, scope, limits, errs).map(|item| {
                VKind::List(ListV {
                    item: Box::new(item),
                    len: d.len,
                    unique: d.unique,
                })
            })
        }
        Kind::Set(d) => {
            check_len(d.len, path, errs);
            compile_node(&d.item, path, depth + 1, scope, limits, errs).map(|item| {
                VKind::Set(SetV {
                    item: Box::new(item),
                    len: d.len,
                })
            })
        }
        Kind::FrozenSet(d) => {
            check_len(d.len, path, errs);
            compile_node(&d.item, path, depth + 1, scope, limits, errs).map(|item| {
                VKind::FrozenSet(SetV {
                    item: Box::new(item),
                    len: d.len,
                })
            })
        }
        Kind::TupleVar(d) => {
            check_len(d.len, path, errs);
            compile_node(&d.item, path, depth + 1, scope, limits, errs).map(|item| {
                VKind::TupleVar(ListV {
                    item: Box::new(item),
                    len: d.len,
                    unique: false,
                })
            })
        }
        Kind::TupleFixed(d) => {
            let items: Option<Vec<VNode>> = d
                .items
                .iter()
                .map(|t| compile_node(t, path, depth + 1, scope, limits, errs))
                .collect();
            items.map(|items| VKind::TupleFixed(items.into_boxed_slice()))
        }
        Kind::Dict(d) => {
            check_len(d.len, path, errs);
            if !key_coercible(&d.key.kind) {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!(
                        "dict key type {} is not string-coercible",
                        d.key.kind.name()
                    ),
                });
            }
            let key = compile_node(&d.key, path, depth + 1, scope, limits, errs);
            let value = compile_node(&d.value, path, depth + 1, scope, limits, errs);
            match (key, value) {
                (Some(key), Some(value)) => Some(VKind::Dict(DictV {
                    key: Box::new(key),
                    value: Box::new(value),
                    len: d.len,
                })),
                _ => None,
            }
        }
        Kind::Literal(d) => compile_literal(&d.members, path, errs),
        Kind::Enum(d) => compile_enum(d, path, errs),
        Kind::Optional(d) => compile_node(&d.inner, path, depth + 1, scope, limits, errs)
            .map(|n| VKind::Optional(Box::new(n))),
        Kind::Union(d) => compile_union(d, path, depth, scope, limits, errs),
        Kind::Record(d) => compile_record(d, path, depth, scope, limits, errs),
        Kind::NewType(d) => compile_node(&d.inner, path, depth + 1, scope, limits, errs)
            .map(|n| VKind::NewType(Box::new(n))),
        // Reached only when a `TypeVar`/`Generic` node slips through
        // `compile_node`'s early match — e.g. inside a substituted tree.
        Kind::TypeVar(name) => {
            if let Some(d) = scope.get(name) {
                let d = d.clone();
                let kind = compile_kind(&d.kind, path, depth, scope, limits, errs)?;
                Some(kind)
            } else {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!("unbound type variable {name:?}"),
                });
                None
            }
        }
        Kind::Generic(g) => {
            let applied = apply_generic(g, path, scope, errs)?;
            compile_kind(&applied.kind, path, depth, scope, limits, errs)
        }
        // The leaf arms live in `compile_kind` — unreachable here.
        _ => None,
    }
}

fn compile_literal(
    members: &[LiteralValue],
    path: &[String],
    errs: &mut Vec<CompileError>,
) -> Option<VKind> {
    if members.is_empty() {
        errs.push(CompileError {
            path: path.join("/"),
            message: "Literal needs at least one member".to_owned(),
        });
        return None;
    }
    // First-wins dedupe — `Literal[1, 1]` behaves as `Literal[1]`.
    let mut seen = HashSet::new();
    let members: Box<[LiteralValue]> = members
        .iter()
        .filter(|m| seen.insert((*m).clone()))
        .cloned()
        .collect();
    let index = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.clone(), i))
        .collect();
    Some(VKind::Literal(LiteralV { members, index }))
}

fn compile_enum(d: &EnumDesc, path: &[String], errs: &mut Vec<CompileError>) -> Option<VKind> {
    if d.members.is_empty() {
        errs.push(CompileError {
            path: path.join("/"),
            message: format!("enum {} has no members", d.class.qualified()),
        });
        return None;
    }
    let mut by_value: HashMap<LiteralValue, usize> = HashMap::new();
    for (i, m) in d.members.iter().enumerate() {
        // Enum aliases share a value; the first declared member wins.
        by_value.entry(m.value.clone()).or_insert(i);
    }
    Some(VKind::Enum(EnumV {
        class: d.class.clone(),
        kind: d.kind,
        members: d.members.clone(),
        by_value,
    }))
}

fn compile_union(
    d: &UnionDesc,
    path: &mut Vec<String>,
    depth: u32,
    scope: &Scope,
    limits: &Limits,
    errs: &mut Vec<CompileError>,
) -> Option<VKind> {
    if d.variants.is_empty() {
        errs.push(CompileError {
            path: path.join("/"),
            message: "union needs at least one variant".to_owned(),
        });
        return None;
    }
    let mut tag_map: HashMap<Box<str>, usize> = HashMap::new();
    if let Some(tag) = &d.tag {
        for (i, v) in d.variants.iter().enumerate() {
            let Kind::Record(r) = &v.kind else {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!(
                        "discriminated union variant {} is not a record",
                        v.kind.name()
                    ),
                });
                continue;
            };
            let Some(field) = r.fields.iter().find(|f| f.name.as_ref() == tag.as_ref()) else {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!("record {} has no tag field {tag:?}", r.class.qualified()),
                });
                continue;
            };
            let tags = literal_tag_values(field);
            if tags.is_empty() {
                errs.push(CompileError {
                    path: path.join("/"),
                    message: format!(
                        "tag field {tag:?} on {} is not a Literal of strings",
                        r.class.qualified()
                    ),
                });
                continue;
            }
            for tv in tags {
                if map_dup(&mut tag_map, tv, i) {
                    errs.push(CompileError {
                        path: path.join("/"),
                        message: "tag value maps to two variants".to_owned(),
                    });
                }
            }
        }
    }
    let variants: Option<Vec<VNode>> = d
        .variants
        .iter()
        .map(|v| compile_node(v, path, depth + 1, scope, limits, errs))
        .collect();
    variants.map(|variants| {
        VKind::Union(UnionV {
            variants: variants.into_boxed_slice(),
            tag: d.tag.clone(),
            tag_map,
        })
    })
}

fn map_dup(map: &mut HashMap<Box<str>, usize>, key: Box<str>, i: usize) -> bool {
    map.insert(key, i).is_some()
}

fn compile_record(
    d: &RecordDesc,
    path: &mut Vec<String>,
    depth: u32,
    scope: &Scope,
    limits: &Limits,
    errs: &mut Vec<CompileError>,
) -> Option<VKind> {
    // The record's own type params scope its fields (`Page[T]`'s `T`) —
    // unapplied they behave as `Any`; they also shadow any outer binding.
    let mut inner_scope = scope.clone();
    for p in &d.type_params {
        inner_scope.insert(p.clone(), TypeDesc::any());
    }
    let mut fields = Vec::with_capacity(d.fields.len());
    let mut wire_index: HashMap<Box<str>, usize> = HashMap::new();
    let mut ok = true;
    for (i, f) in d.fields.iter().enumerate() {
        let wire_key = f.wire_key(d.rename);
        if wire_index.insert(wire_key.clone(), i).is_some() {
            errs.push(CompileError {
                path: path.join("/"),
                message: format!("fields share wire key {wire_key:?}"),
            });
            ok = false;
        }
        path.push(f.name.to_string());
        let node = compile_node(&f.ty, path, depth + 1, &inner_scope, limits, errs);
        path.pop();
        match node {
            Some(node) => fields.push(FieldV {
                name: f.name.clone(),
                wire_key,
                missing: f.missing.clone(),
                node,
            }),
            None => ok = false,
        }
    }
    if !ok {
        return None;
    }
    Some(VKind::Record(RecordV {
        class: d.class.clone(),
        kind: d.kind,
        extra: d.extra,
        fields: fields.into_boxed_slice(),
        wire_index,
    }))
}

/// Clone `desc` replacing every `TypeVar` bound in `subst`. Hygienic:
/// inside a nested `Record`/`Generic` that re-declares a param name, the
/// param is *removed* from the substitution map (it binds locally).
fn substitute(desc: &TypeDesc, subst: &Scope) -> TypeDesc {
    if let Kind::TypeVar(name) = &desc.kind {
        if let Some(d) = subst.get(name) {
            return d.clone();
        }
        return desc.clone();
    }
    let kind = match &desc.kind {
        Kind::List(d) => Kind::List(crate::desc::ListDesc {
            item: Box::new(substitute(&d.item, subst)),
            ..d.clone()
        }),
        Kind::Set(d) => Kind::Set(crate::desc::SetDesc {
            item: Box::new(substitute(&d.item, subst)),
            ..d.clone()
        }),
        Kind::FrozenSet(d) => Kind::FrozenSet(crate::desc::SetDesc {
            item: Box::new(substitute(&d.item, subst)),
            ..d.clone()
        }),
        Kind::TupleVar(d) => Kind::TupleVar(crate::desc::TupleVarDesc {
            item: Box::new(substitute(&d.item, subst)),
            ..d.clone()
        }),
        Kind::TupleFixed(d) => Kind::TupleFixed(crate::desc::TupleFixedDesc {
            items: d.items.iter().map(|t| substitute(t, subst)).collect(),
        }),
        Kind::Dict(d) => Kind::Dict(crate::desc::DictDesc {
            key: Box::new(substitute(&d.key, subst)),
            value: Box::new(substitute(&d.value, subst)),
            ..d.clone()
        }),
        Kind::Optional(d) => Kind::Optional(crate::desc::OptionalDesc {
            inner: Box::new(substitute(&d.inner, subst)),
        }),
        Kind::Union(d) => Kind::Union(UnionDesc {
            variants: d.variants.iter().map(|v| substitute(v, subst)).collect(),
            tag: d.tag.clone(),
        }),
        Kind::Record(d) => {
            // The record's own params bind inside it — unbind them here.
            let mut inner = subst.clone();
            for p in &d.type_params {
                inner.remove(p);
            }
            let mut d = d.clone();
            d.fields = d
                .fields
                .iter()
                .map(|f| FieldDesc {
                    ty: substitute(&f.ty, &inner),
                    ..f.clone()
                })
                .collect();
            Kind::Record(d)
        }
        Kind::NewType(d) => Kind::NewType(crate::desc::NewTypeDesc {
            name: d.name.clone(),
            inner: Box::new(substitute(&d.inner, subst)),
        }),
        Kind::Generic(d) => {
            // Args substitute in the *outer* scope; the origin's params
            // unbind inside the origin.
            let mut inner = subst.clone();
            if let Kind::Record(o) = &d.origin.kind {
                for p in &o.type_params {
                    inner.remove(p);
                }
            }
            Kind::Generic(GenericDesc {
                origin: Box::new(substitute(&d.origin, &inner)),
                args: d.args.iter().map(|a| substitute(a, subst)).collect(),
            })
        }
        k => k.clone(),
    };
    TypeDesc {
        kind,
        meta: desc.meta.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::*;

    fn record(class: &str, fields: Vec<FieldDesc>) -> TypeDesc {
        TypeDesc::record(RecordDesc {
            class: ClassRef {
                name: class.into(),
                module: "m".into(),
            },
            kind: RecordKind::Model,
            fields: fields.into(),
            extra: ExtraPolicy::default(),
            rename: None,
            type_params: Box::new([]),
        })
    }

    #[test]
    fn bad_descriptors_are_compile_errors() {
        assert!(compile(&TypeDesc::union(vec![])).is_err());
        let mut int = IntDesc::new();
        int.multiple_of = Some(Bound::Int(0));
        assert!(compile(&TypeDesc::new(Kind::Int(int))).is_err());
        let mut int = IntDesc::new();
        int.range = Some(Range {
            ge: Some(Bound::Int(5)),
            le: Some(Bound::Int(3)),
            ..Range::default()
        });
        assert!(compile(&TypeDesc::new(Kind::Int(int))).is_err());
        // Empty via strict bounds: ge == lt.
        let mut int = IntDesc::new();
        int.range = Some(Range {
            ge: Some(Bound::Int(3)),
            lt: Some(Bound::Int(3)),
            ..Range::default()
        });
        assert!(compile(&TypeDesc::new(Kind::Int(int))).is_err());
        let mut s = StrDesc::new();
        s.len = Some(Len::new(Some(5), Some(2)));
        assert!(compile(&TypeDesc::new(Kind::Str(s))).is_err());
        assert!(compile(&TypeDesc::new(Kind::TypeVar("T".into()))).is_err());
        // dict[list[int], str] — key not coercible.
        let d = TypeDesc::dict(TypeDesc::list(TypeDesc::int()), TypeDesc::str());
        assert!(compile(&d).is_err());
    }

    #[test]
    fn discriminator_checks() {
        let variant = |tag: &str| {
            record(
                tag,
                vec![FieldDesc::required(
                    "kind",
                    TypeDesc::literal(vec![LiteralValue::Str(tag.into())]),
                )],
            )
        };
        assert!(
            compile(&TypeDesc::union_tagged(
                vec![variant("a"), variant("b")],
                "kind"
            ))
            .is_ok()
        );
        assert!(
            compile(&TypeDesc::union_tagged(
                vec![variant("a"), TypeDesc::int()],
                "kind"
            ))
            .is_err()
        );
        let missing = record("c", vec![FieldDesc::required("x", TypeDesc::int())]);
        assert!(compile(&TypeDesc::union_tagged(vec![variant("a"), missing], "kind")).is_err());
        assert!(
            compile(&TypeDesc::union_tagged(
                vec![variant("a"), variant("a")],
                "kind"
            ))
            .is_err()
        );
    }

    #[test]
    fn generic_substitution() {
        let origin = TypeDesc::record(RecordDesc {
            class: ClassRef {
                name: "Page".into(),
                module: "m".into(),
            },
            kind: RecordKind::Model,
            fields: vec![
                FieldDesc::required(
                    "items",
                    TypeDesc::list(TypeDesc::new(Kind::TypeVar("T".into()))),
                ),
                FieldDesc::required("total", TypeDesc::int()),
            ]
            .into(),
            extra: ExtraPolicy::default(),
            rename: None,
            type_params: vec!["T".into()].into(),
        });
        let applied = TypeDesc::new(Kind::Generic(GenericDesc {
            origin: Box::new(origin.clone()),
            args: vec![TypeDesc::int()].into(),
        }));
        assert!(compile(&applied).is_ok());
        // Arity mismatch.
        let mut two = origin.clone();
        if let Kind::Record(r) = &mut two.kind {
            r.type_params = vec!["T".into(), "U".into()].into();
        }
        let bad = TypeDesc::new(Kind::Generic(GenericDesc {
            origin: Box::new(two),
            args: vec![TypeDesc::int()].into(),
        }));
        assert!(compile(&bad).is_err());
        // An unapplied generic record treats params as Any.
        assert!(compile(&origin).is_ok());
    }

    #[test]
    fn colliding_wire_keys() {
        let mut f = FieldDesc::required("a", TypeDesc::int());
        f.alias = Some("x".into());
        let mut g = FieldDesc::required("b", TypeDesc::int());
        g.alias = Some("x".into());
        assert!(compile(&record("R", vec![f, g])).is_err());
    }

    #[test]
    fn desc_depth_guard() {
        let mut d = TypeDesc::int();
        for _ in 0..80 {
            d = TypeDesc::list(d);
        }
        assert!(compile(&d).is_err()); // exceeds default max_desc_depth 64
        assert!(compile(&TypeDesc::list(TypeDesc::int())).is_ok());
    }

    #[test]
    fn limits_tightened() {
        let a = Limits {
            max_depth: 8,
            max_items: 5,
            ..Limits::default()
        };
        let b = Limits {
            max_depth: 64,
            max_items: 3,
            max_str_bytes: 7,
            ..Limits::default()
        };
        let merged = a.tightened(&b);
        assert_eq!(merged.max_depth, 8);
        assert_eq!(merged.max_items, 3);
        assert_eq!(merged.max_str_bytes, 7);
        // Commutative, and never looser than either input (§14.8).
        assert_eq!(merged, b.tightened(&a));
        assert_eq!(a.tightened(&Limits::default()), a);
        assert_eq!(Limits::default().tightened(&a), a);
    }
}
