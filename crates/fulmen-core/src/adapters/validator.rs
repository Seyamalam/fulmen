//! `Validator` (DESIGN.md §15.1, §11.8): the pluggable type → validator
//! compiler.
//!
//! A `Validator` decides whether it handles a type (`supports`), then
//! turns it into a spec tree (`compile`) — the `fulmen-schema` IR
//! documented in `SPEC-FORMAT.md` — and produces its JSON Schema for
//! `OpenAPI` (`json_schema`). All three run once per type at registration,
//! never on the request path, so the methods are synchronous.
//!
//! `compile` returns the spec *tree*, not the compiled descriptor: the
//! descriptor type lives in `fulmen-schema`, which `fulmen-core` cannot
//! depend on. The caller — `fulmen-py`'s route registration — feeds the
//! spec to `compile_descriptor`, exactly as `PydanticValidator.compile`
//! does on the Python side (§15.2: third-party validators "translate to
//! native specs"). An adapter that cannot express its validation as a
//! spec cannot serve this slot.
//!
//! A native `Validator` is invoked without a Python crossing; a Python
//! `Validator` implementation is called through the executor bridge at
//! registration — under the GIL, which is safe precisely because no
//! dispatch is in flight yet.
//!
//! There is deliberately no built-in Rust `Validator` implementation:
//! introspecting a Python `type` requires the interpreter, so the
//! default ("native") validator lives in `fulmen._typing` on the Python
//! side — `NativeValidator` in `fulmen.adapters.native` — with
//! `PydanticValidator` as the second built-in (§15.2).

use std::collections::BTreeMap;
use std::fmt;

use super::StoreResult;

/// A cross-language reference to a Python `type` — the `tp` argument of
/// the `Validator` methods as a Rust implementation sees it.
///
/// `fulmen-py` builds one from the annotation object at registration:
/// `module`/`qualname` come from `tp.__module__`/`tp.__qualname__` and
/// `args` carries specialization arguments for generics (`list[User]` →
/// `list` with `args = [User]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeRef {
    /// `tp.__module__` — e.g. `"myapp.models"` or `"builtins"`.
    pub module: String,
    /// `tp.__qualname__` — e.g. `"User"` or `"list"`.
    pub qualname: String,
    /// Specialization arguments for generic types.
    pub args: Vec<TypeRef>,
}

impl TypeRef {
    /// A plain (non-generic) type reference.
    pub fn new(module: impl Into<String>, qualname: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            qualname: qualname.into(),
            args: Vec::new(),
        }
    }

    /// A generic specialization — `list[User]` and friends.
    pub fn generic(
        module: impl Into<String>,
        qualname: impl Into<String>,
        args: Vec<TypeRef>,
    ) -> Self {
        Self {
            module: module.into(),
            qualname: qualname.into(),
            args,
        }
    }

    /// The `module.qualname` dotted path — `"myapp.models.User"`.
    pub fn dotted(&self) -> String {
        format!("{}.{}", self.module, self.qualname)
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module, self.qualname)?;
        if !self.args.is_empty() {
            write!(f, "[")?;
            for (index, arg) in self.args.iter().enumerate() {
                if index > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{arg}")?;
            }
            write!(f, "]")?;
        }
        Ok(())
    }
}

/// A node of the spec IR and of JSON Schema documents — the JSON-shaped
/// value SPEC-FORMAT.md calls "a JSON-shaped dict", plus `Bytes` for the
/// scalar `bytes` literals `missing` policies may carry (§3).
///
/// Every spec is a `Map` whose `"kind"` key selects the node type
/// (`"record"`, `"str"`, `"list"`, …); nested nodes are `Map`s inside.
#[derive(Debug, Clone, PartialEq)]
pub enum SpecValue {
    /// JSON `null` — also `None`-typed spec leaves.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON integer.
    Int(i64),
    /// JSON number.
    Float(f64),
    /// JSON string.
    Str(String),
    /// A scalar `bytes` literal (SPEC-FORMAT §3 `missing` policies).
    Bytes(Vec<u8>),
    /// JSON array — `fields`, `variants`, `items`, `meta.after`, …
    List(Vec<SpecValue>),
    /// JSON object — every spec node is one.
    Map(BTreeMap<String, SpecValue>),
}

impl SpecValue {
    /// The `key` member of a `Map` value — `None` for other kinds.
    pub fn get(&self, key: &str) -> Option<&SpecValue> {
        match self {
            Self::Map(map) => map.get(key),
            _ => None,
        }
    }

    /// The string inside a `Str` — `None` for other kinds.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    /// The integer inside an `Int` — `None` for other kinds.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// The boolean inside a `Bool` — `None` for other kinds.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The entries inside a `Map` — `None` for other kinds.
    pub fn as_map(&self) -> Option<&BTreeMap<String, SpecValue>> {
        match self {
            Self::Map(map) => Some(map),
            _ => None,
        }
    }

    /// The elements inside a `List` — `None` for other kinds.
    pub fn as_list(&self) -> Option<&[SpecValue]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

impl From<&str> for SpecValue {
    fn from(value: &str) -> Self {
        Self::Str(value.into())
    }
}

impl From<String> for SpecValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<i64> for SpecValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<usize> for SpecValue {
    fn from(value: usize) -> Self {
        Self::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<bool> for SpecValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<f64> for SpecValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<Vec<u8>> for SpecValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<Vec<SpecValue>> for SpecValue {
    fn from(value: Vec<SpecValue>) -> Self {
        Self::List(value)
    }
}

impl From<BTreeMap<String, SpecValue>> for SpecValue {
    fn from(value: BTreeMap<String, SpecValue>) -> Self {
        Self::Map(value)
    }
}

/// The type → validator compiler.
///
/// Registration-time only: a native implementation is invoked without a
/// Python crossing, a Python `Validator` protocol implementation through
/// the executor bridge — under the GIL, while no dispatch is in flight.
pub trait Validator: Send + Sync + 'static {
    /// Whether this adapter can compile `tp`. A `compile` call for a
    /// type `supports` rejects is an [`AdapterError::Unsupported`]
    /// contract violation.
    ///
    /// [`AdapterError::Unsupported`]: super::AdapterError::Unsupported
    fn supports(&self, tp: &TypeRef) -> bool;

    /// `tp` → its spec tree (`SPEC-FORMAT.md`). The caller hands the
    /// returned `Map` to `fulmen-schema`'s compiler along with the
    /// adapter's callback table; `meta.after` and `factory` ids refer to
    /// that table, exactly as on the Python side.
    ///
    /// # Errors
    /// [`AdapterError::Unsupported`] when `supports` would answer `false`;
    /// [`AdapterError::Invalid`] when `tp` is supported but cannot be
    /// expressed as a spec.
    ///
    /// [`AdapterError::Unsupported`]: super::AdapterError::Unsupported
    /// [`AdapterError::Invalid`]: super::AdapterError::Invalid
    fn compile(&self, tp: &TypeRef) -> StoreResult<SpecValue>;

    /// The JSON Schema document for `tp` — what `OpenAPI` emits.
    ///
    /// # Errors
    /// Same contract as [`Validator::compile`].
    fn json_schema(&self, tp: &TypeRef) -> StoreResult<SpecValue>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn typeref_display_formats_generics() {
        let user = TypeRef::new("myapp.models", "User");
        let list = TypeRef::generic("builtins", "list", vec![user]);
        assert_eq!(list.to_string(), "builtins.list[myapp.models.User]");
        assert_eq!(list.dotted(), "builtins.list");
    }

    #[test]
    fn spec_value_accessors() {
        let spec = SpecValue::Map(BTreeMap::from([
            ("kind".to_string(), SpecValue::from("str")),
            ("len".to_string(), SpecValue::from(4_i64)),
        ]));
        assert_eq!(spec.get("kind").and_then(SpecValue::as_str), Some("str"));
        assert_eq!(spec.get("len").and_then(SpecValue::as_int), Some(4));
        assert!(spec.get("missing").is_none());
        assert!(SpecValue::Null.as_str().is_none());
    }

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn trait_is_object_safe() {
        struct Noop;
        impl Validator for Noop {
            fn supports(&self, _tp: &TypeRef) -> bool {
                false
            }
            fn compile(&self, tp: &TypeRef) -> StoreResult<SpecValue> {
                Err(super::super::AdapterError::Unsupported(format!("{tp}")))
            }
            fn json_schema(&self, tp: &TypeRef) -> StoreResult<SpecValue> {
                self.compile(tp)
            }
        }
        let v: Arc<dyn Validator> = Arc::new(Noop);
        assert_send_sync(&v);
    }
}
