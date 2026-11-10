//! Fulmen core: router, request/response model, lifecycle hooks and the
//! plugin/adapter registry. No I/O, no Python: this crate is pure Rust.

pub mod router;

pub use router::{
    AllowList, AppDecl, CompileError, CompileErrors, CompiledApp, Method, ParamSpec, PathParamType,
    PathValue, PrefixError, RouteDecl, RouteHit, RouteId, RouteInfo, RouteLabel, RouteLookup,
    ScalarType, TrailingSlash, TypedParam,
};

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "core";
