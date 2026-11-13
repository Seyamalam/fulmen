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
//! - [`value`] — the input model: [`Value`] is what the JSON decoder and
//!   the lax text adapters produce; [`LiteralValue`] carries
//!   `Literal`/enum/`Default` constants.
//! - [`desc`] — the [`TypeDesc`] IR the Python converter builds (the
//!   §11.1 type table + §11.2 constraints + `Annotated` metadata).
//! - [`compile`] — [`compile()`] checks a descriptor once at startup and
//!   produces a [`Validator`] (specialized node tree: compiled regexes,
//!   indexed fields/tags/literals, generics substituted).
//! - [`json`] — the one-pass byte-slice JSON decoder (D18): strict
//!   RFC 8259, borrowed strings, limits enforced mid-walk.
//! - [`encode`] — the §11.6 output half: a wire-shaped `Value` back to
//!   JSON bytes in one pass (`Bytes` members base64).
//! - [`sources`] — lax source adapters: query string, form-urlencoded
//!   bodies and the `Cookie` header into `Value`s, plus
//!   [`validate_text`] for single path/header/cookie values.
//! - [`Validator::validate`] / [`Validator::validate_out`] — per-request
//!   entry points: strict mode for JSON bodies, lax for text sources;
//!   errors collect *every* problem in §11.7 shape, renderable via
//!   [`ValidationErrors::to_json`] / `to_problem_entries`.
//!
//! This crate is plain Rust — no `PyO3`, no I/O, no unsafe; `regex` is its
//! only dependency (DESIGN.md §11.2 requires it for `Pattern`).

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "schema";

pub mod compile;
pub mod desc;
pub mod encode;
pub mod errors;
pub mod json;
mod num;
pub mod parse;
pub mod sources;
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
pub use encode::encode_json;
pub use errors::{
    ErrorCode, ErrorEntry, LocSeg, MAX_ERRORS, ProblemEntry, Source, ValidationErrors,
};
pub use json::{
    DecodeError, JsonError, JsonErrorCode, PayloadError, decode_and_validate,
    decode_and_validate_with_limits, decode_json, decode_json_with_limits,
};
pub use sources::{
    SourceError, TextError, TextErrorCode, decode_cookie, decode_cookie_with_limits, decode_form,
    decode_form_with_limits, decode_query, decode_query_with_limits, validate_text,
    validate_text_with_limits,
};
pub use validator::{Checked, Mode, Validator};
pub use value::{LiteralValue, Value};
