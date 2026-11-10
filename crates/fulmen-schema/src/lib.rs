//! Fulmen schema: compiles Python type hints into native validators and
//! serializers once at startup, then validates and encodes without the GIL.

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "schema";
