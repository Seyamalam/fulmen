//! Fulmen core: router, request/response model, lifecycle hooks and the
//! plugin/adapter registry. No I/O, no Python: this crate is pure Rust.

pub mod pipeline;
pub mod plugins;
pub mod router;

pub use pipeline::{APP_SCOPE, HookDecl, Phase, RouteHooks, resolve_hooks, resolve_scope_hooks};
pub use plugins::{PluginDecl, PluginProblem, resolve_order};
pub use router::{
    AllowList, AppDecl, CompileError, CompileErrors, CompiledApp, Method, ParamSpec, PathParamType,
    PathValue, PrefixError, RouteDecl, RouteHit, RouteId, RouteInfo, RouteLabel, RouteLookup,
    ScalarType, TrailingSlash, TypedParam,
};

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "core";
