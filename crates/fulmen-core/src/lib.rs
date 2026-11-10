//! Fulmen core: router, request/response model, lifecycle hooks and the
//! plugin/adapter registry. No Python dependency: this crate is pure Rust.

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "core";
