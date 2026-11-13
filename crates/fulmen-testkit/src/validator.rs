//! `Validator` conformance: `supports`/`compile`/`json_schema`
//! agreement — a supported type compiles to a spec tree and produces a
//! schema; an unsupported one errors instead of silently accepting.
#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "conformance checks assert the adapter contract via panics — they are tests"
)]

use fulmen_core::adapters::{SpecValue, TypeRef, Validator};

/// Run every `Validator` check against `validator`.
///
/// `supported`/`unsupported` name one type the adapter claims and one
/// it does not — the caller knows its adapter; the suite checks the
/// three methods agree with those claims.
///
/// # Panics
/// On any contract violation — that is the point of a suite.
pub fn suite(validator: &dyn Validator, supported: &TypeRef, unsupported: &TypeRef) {
    supports_declares(validator, supported, unsupported);
    compile_supported_gives_spec(validator, supported);
    compile_unsupported_errors(validator, supported, unsupported);
    json_schema_is_map(validator, supported);
}

/// `supports` answers `true` for the claimed type and `false` for the
/// unclaimed one — the registry routes on it, so it must be honest.
///
/// # Panics
/// On contract violation.
pub fn supports_declares(validator: &dyn Validator, supported: &TypeRef, unsupported: &TypeRef) {
    assert!(
        validator.supports(supported),
        "supports({supported}) must be true for the claimed type"
    );
    assert!(
        !validator.supports(unsupported),
        "supports({unsupported}) must be false for the unclaimed type"
    );
}

/// `compile` of a supported type yields a spec tree: a `Map` with a
/// string `"kind"` (SPEC-FORMAT.md §1–§2).
///
/// # Panics
/// On contract violation.
pub fn compile_supported_gives_spec(validator: &dyn Validator, supported: &TypeRef) {
    let spec = validator.compile(supported).unwrap();
    let Some(kind) = spec.get("kind").and_then(SpecValue::as_str) else {
        panic!("compile({supported}) must return a spec map with a string \"kind\"")
    };
    assert!(!kind.is_empty());
}

/// `compile` of a type `supports` rejects is an error, never a bogus
/// spec — the registry trusts the pair to agree.
///
/// # Panics
/// On contract violation.
pub fn compile_unsupported_errors(
    validator: &dyn Validator,
    supported: &TypeRef,
    unsupported: &TypeRef,
) {
    // Consistency the other way: supported must compile.
    assert!(validator.compile(supported).is_ok());
    assert!(
        validator.compile(unsupported).is_err(),
        "compile({unsupported}) must err for a type supports() rejects"
    );
}

/// `json_schema` of a supported type yields a JSON Schema document — a
/// `Map` at the top.
///
/// # Panics
/// On contract violation.
pub fn json_schema_is_map(validator: &dyn Validator, supported: &TypeRef) {
    let schema = validator.json_schema(supported).unwrap();
    assert!(
        schema.as_map().is_some(),
        "json_schema({supported}) must return a schema object"
    );
}
