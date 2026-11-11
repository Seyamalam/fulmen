//! Fulmen schema: compiles Python type hints into native validators once
//! at startup, then validates decoded values without the GIL.
//!
//! # The pipeline (DESIGN.md §11)
//!
//! ```text
//! Python hints ──(fulmen-py, once)──▶ TypeDesc ──compile──▶ Validator
//!                                        │                     │
//!                     JSON decoder ──▶ Value ───────────────▶ validate()
//!                     query/path/header/form adapters ──────▶ (lax mode)
//! ```
//!
//! - [`value`] — the input model: [`Value`] is what the (future) JSON
//!   decoder and the lax text adapters produce; [`LiteralValue`] carries
//!   `Literal`/enum/`Default` constants.
//! - [`desc`] — the [`TypeDesc`] IR the Python converter builds (the
//!   §11.1 type table + §11.2 constraints + `Annotated` metadata).
//! - [`compile`] — [`compile()`] checks a descriptor once at startup and
//!   produces a [`Validator`] (specialized node tree: compiled regexes,
//!   indexed fields/tags/literals, generics substituted).
//! - [`Validator::validate`] — per-request entry point: strict mode for
//!   JSON bodies, lax for text sources; returns `Cow` (borrowed input on
//!   the strict happy path) or [`ValidationErrors`] collecting *every*
//!   problem in §11.7 shape.
//!
//! This crate is plain Rust — no `PyO3`, no I/O, no unsafe; `regex` is its
//! only dependency (DESIGN.md §11.2 requires it for `Pattern`).

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "schema";

pub mod compile;
pub mod desc;
pub mod errors;
mod num;
pub mod parse;
pub mod validator;
pub mod value;

pub use compile::{CompileError, CompileErrors, Limits, compile, compile_with_limits};
pub use desc::{
    Bound, BytesDesc, ClassRef, DecimalDesc, DictDesc, EnumDesc, EnumKind, EnumMember, ExtraPolicy,
    FieldDesc, FloatDesc, GenericDesc, IntDesc, Kind, Len, ListDesc, LiteralDesc, Missing,
    NewTypeDesc, OptionalDesc, Pattern, PatternError, Range, RecordDesc, RecordKind, RenameRule,
    SetDesc, StrDesc, Transforms, TupleFixedDesc, TupleVarDesc, TypeDesc, TypeMeta, TypeMetaRef,
    UnionDesc,
};
pub use errors::{ErrorCode, ErrorEntry, LocSeg, MAX_ERRORS, Source, ValidationErrors};
pub use validator::{Mode, Validator};
pub use value::{LiteralValue, Value};
