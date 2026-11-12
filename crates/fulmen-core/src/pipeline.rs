//! The request lifecycle's hookable phases (DESIGN.md §8).
//!
//! The pipeline has sixteen phases; eight of them accept hooks. Each phase
//! carries one ordered list of hook ids — the hook *table* (id → callable)
//! lives in the embedding crate (`fulmen-py`), because what a hook calls is
//! bridge machinery, while the *structure* — phases, scoping, ordering —
//! is pure data and lives here.
//!
//! Ordering (§8, §13.2): within a phase, hooks sort by `(priority, seq)` —
//! priority is a `u16` where lower runs earlier, so security plugins
//! register fixed bands (rate limit < authentication < CSRF) that always
//! precede ordinary plugin and user hooks regardless of registration
//! order; `seq` is the global registration sequence and keeps same-priority
//! hooks deterministic. Scopes select *membership* only: a route's chain is
//! every hook whose scope is the application scope (`0`) or one of the
//! route's group scopes.

use std::fmt;

/// The hookable phases of the request lifecycle, in pipeline order.
///
/// The non-hookable phases (`accept`, `parse`, `route`, `extract`, `handle`,
/// `encode`, `compress`, `write`) run in Rust and take no hooks; `on_error`
/// is not an ordered hook list but the exception-keyed handler table of M2
/// (DESIGN.md §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Runs before the handler pipeline is entered: request-id, trusted
    /// proxy resolution. A returned response short-circuits everything.
    OnRequest,
    /// Runs once the route is known, before guards: CORS preflight.
    BeforeRoute,
    /// Allow/deny checks: rate limit, authentication, CSRF, custom guards.
    /// Denial typically answers 401/403/429.
    Guard,
    /// Dependency resolution (DI). Native-only for now.
    Resolve,
    /// Runs immediately before the handler.
    BeforeHandle,
    /// Runs after the handler; a non-`None` return replaces the response.
    AfterHandle,
    /// Runs after the response is encoded; header mutation phase.
    OnResponse,
    /// Runs after the response is produced: access log, metrics,
    /// background tasks. Fire-and-forget.
    AfterResponse,
}

impl Phase {
    /// Every phase, in pipeline order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::OnRequest,
        Self::BeforeRoute,
        Self::Guard,
        Self::Resolve,
        Self::BeforeHandle,
        Self::AfterHandle,
        Self::OnResponse,
        Self::AfterResponse,
    ];

    /// The number of hookable phases.
    pub const COUNT: usize = 8;

    /// The phase's position in [`Phase::ALL`]; indexes per-phase tables.
    pub fn index(self) -> usize {
        match self {
            Self::OnRequest => 0,
            Self::BeforeRoute => 1,
            Self::Guard => 2,
            Self::Resolve => 3,
            Self::BeforeHandle => 4,
            Self::AfterHandle => 5,
            Self::OnResponse => 6,
            Self::AfterResponse => 7,
        }
    }

    /// The phase name used by the Python API and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on_request",
            Self::BeforeRoute => "before_route",
            Self::Guard => "guard",
            Self::Resolve => "resolve",
            Self::BeforeHandle => "before_handle",
            Self::AfterHandle => "after_handle",
            Self::OnResponse => "on_response",
            Self::AfterResponse => "after_response",
        }
    }

    /// Whether hooks of this phase are invoked with the response
    /// (post-handler phases pass `(request, response)`; earlier phases
    /// receive the request only).
    pub fn takes_response(self) -> bool {
        matches!(
            self,
            Self::AfterHandle | Self::OnResponse | Self::AfterResponse
        )
    }
}

/// An unknown hook-phase name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePhaseError(pub String);

impl fmt::Display for ParsePhaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown hook phase: {}", self.0)
    }
}

impl std::error::Error for ParsePhaseError {}

impl std::str::FromStr for Phase {
    type Err = ParsePhaseError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.as_str() == name)
            .ok_or_else(|| ParsePhaseError(name.to_owned()))
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A hook registration's ordering and scope data: the fields the resolver
/// needs. What the hook *calls* (a Python callable, a native closure) lives
/// in a parallel table in the bridge; the registration's `HookDecl` index
/// is the hook id used throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookDecl {
    /// The lifecycle phase this hook runs in.
    pub phase: Phase,
    /// Scope id: `0` is the application scope (runs on every route);
    /// any other id names a route group's scope.
    pub scope: u32,
    /// Ordering band — lower runs earlier. Reserved bands for security
    /// plugins (rate limit < authentication < CSRF) sit below the user
    /// default, so security hooks precede user hooks regardless of
    /// registration order (DESIGN.md §13.2).
    pub priority: u16,
    /// Global registration sequence; breaks priority ties deterministically.
    pub seq: u32,
}

/// The application-level scope: hooks registered here run on every route.
pub const APP_SCOPE: u32 = 0;

/// A route's resolved hook chains: one ordered id list per phase.
///
/// `Box<[u32]>` never allocates for empty chains — a bare route costs a
/// `RouteHooks::default()` and nothing else (DESIGN.md §25.2).
#[derive(Debug, Default)]
pub struct RouteHooks {
    chains: [Box<[u32]>; Phase::COUNT],
}

impl RouteHooks {
    /// The resolved hook ids for `phase`, in execution order.
    pub fn chain(&self, phase: Phase) -> &[u32] {
        &self.chains[phase.index()]
    }

    /// All per-phase chains, in [`Phase::ALL`] order.
    pub fn chains(&self) -> &[Box<[u32]>; Phase::COUNT] {
        &self.chains
    }

    /// Whether every phase chain is empty.
    pub fn is_empty(&self) -> bool {
        self.chains.iter().all(|c| c.is_empty())
    }

    /// Total hook count across all phases.
    pub fn count(&self) -> usize {
        self.chains.iter().map(|c| c.len()).sum()
    }

    pub(crate) fn set(&mut self, phase: Phase, ids: Vec<u32>) {
        self.chains[phase.index()] = ids.into_boxed_slice();
    }
}

/// Resolve one route's hook chains from the registration table.
///
/// A hook applies to the route when its scope is [`APP_SCOPE`] or one of
/// `layers` (the route's group scopes). Within a phase, hooks are ordered
/// by `(priority, seq)`: security bands precede plugins which precede user
/// hooks; same-priority hooks keep registration order.
pub fn resolve_hooks(decls: &[HookDecl], layers: &[u32]) -> RouteHooks {
    let mut hooks = RouteHooks::default();
    for phase in Phase::ALL {
        let mut ids: Vec<u32> = decls
            .iter()
            .enumerate()
            .filter(|(_, d)| d.phase == phase)
            .filter(|(_, d)| d.scope == APP_SCOPE || layers.contains(&d.scope))
            .filter_map(|(id, _)| u32::try_from(id).ok())
            .collect();
        ids.sort_by_key(|id| (decls[*id as usize].priority, decls[*id as usize].seq));
        if !ids.is_empty() {
            hooks.set(phase, ids);
        }
    }
    hooks
}

/// The hook ids of `scope` alone, per phase — the chains that apply to
/// requests that match no route (app-level `on_request`/`on_response` still
/// run for `404`/`405` responses).
pub fn resolve_scope_hooks(decls: &[HookDecl], scope: u32) -> RouteHooks {
    let mut hooks = RouteHooks::default();
    for phase in Phase::ALL {
        let mut ids: Vec<u32> = decls
            .iter()
            .enumerate()
            .filter(|(_, d)| d.phase == phase && d.scope == scope)
            .filter_map(|(id, _)| u32::try_from(id).ok())
            .collect();
        ids.sort_by_key(|id| (decls[*id as usize].priority, decls[*id as usize].seq));
        if !ids.is_empty() {
            hooks.set(phase, ids);
        }
    }
    hooks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(phase: Phase, scope: u32, priority: u16, seq: u32) -> HookDecl {
        HookDecl {
            phase,
            scope,
            priority,
            seq,
        }
    }

    #[test]
    fn phase_names_round_trip() {
        for phase in Phase::ALL {
            assert_eq!(phase.as_str().parse::<Phase>(), Ok(phase));
        }
        assert!("on_error".parse::<Phase>().is_err());
        assert!("bogus".parse::<Phase>().is_err());
    }

    #[test]
    fn empty_resolution_allocates_nothing() {
        let hooks = resolve_hooks(&[], &[]);
        assert!(hooks.is_empty());
        assert_eq!(hooks.count(), 0);
        assert!(hooks.chain(Phase::BeforeHandle).is_empty());
    }

    #[test]
    fn app_scope_hooks_apply_to_every_route() {
        let decls = [
            decl(Phase::BeforeHandle, APP_SCOPE, 1000, 0),
            decl(Phase::BeforeHandle, 7, 1000, 1),
        ];
        let route_a = resolve_hooks(&decls, &[]);
        assert_eq!(route_a.chain(Phase::BeforeHandle), &[0]);
        let route_b = resolve_hooks(&decls, &[7]);
        assert_eq!(route_b.chain(Phase::BeforeHandle), &[0, 1]);
    }

    #[test]
    fn group_hooks_apply_only_inside_their_scope() {
        let decls = [
            decl(Phase::BeforeHandle, 1, 1000, 0),
            decl(Phase::BeforeHandle, 2, 1000, 1),
        ];
        let route = resolve_hooks(&decls, &[2]);
        assert_eq!(route.chain(Phase::BeforeHandle), &[1]);
    }

    #[test]
    fn priority_beats_registration_order() {
        // Registered later but with a security band priority: runs first.
        let decls = [
            decl(Phase::Guard, APP_SCOPE, 1000, 0),
            decl(Phase::Guard, APP_SCOPE, 200, 1),
            decl(Phase::Guard, APP_SCOPE, 100, 2),
        ];
        let route = resolve_hooks(&decls, &[]);
        assert_eq!(route.chain(Phase::Guard), &[2, 1, 0]);
    }

    #[test]
    fn same_priority_keeps_registration_order() {
        let decls = [
            decl(Phase::OnResponse, 3, 1000, 0),
            decl(Phase::OnResponse, APP_SCOPE, 1000, 1),
            decl(Phase::OnResponse, 3, 1000, 2),
        ];
        let route = resolve_hooks(&decls, &[3]);
        assert_eq!(route.chain(Phase::OnResponse), &[0, 1, 2]);
    }

    #[test]
    fn resolution_is_per_phase() {
        let decls = [
            decl(Phase::BeforeHandle, APP_SCOPE, 1000, 0),
            decl(Phase::AfterHandle, APP_SCOPE, 1000, 1),
        ];
        let route = resolve_hooks(&decls, &[]);
        assert_eq!(route.chain(Phase::BeforeHandle), &[0]);
        assert_eq!(route.chain(Phase::AfterHandle), &[1]);
        assert!(route.chain(Phase::Guard).is_empty());
        assert_eq!(route.count(), 2);
    }

    #[test]
    fn scope_resolution_selects_one_scope() {
        let decls = [
            decl(Phase::OnRequest, APP_SCOPE, 1000, 0),
            decl(Phase::OnRequest, 4, 1000, 1),
        ];
        let app_only = resolve_scope_hooks(&decls, APP_SCOPE);
        assert_eq!(app_only.chain(Phase::OnRequest), &[0]);
        let group_only = resolve_scope_hooks(&decls, 4);
        assert_eq!(group_only.chain(Phase::OnRequest), &[1]);
    }
}
