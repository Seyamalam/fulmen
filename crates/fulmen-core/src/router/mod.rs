//! The router (DESIGN.md §9).
//!
//! Declarations ([`AppDecl`] / [`RouteDecl`]) are compiled once at startup
//! into the frozen, `Send + Sync` [`CompiledApp`]: one `matchit` radix tree
//! per HTTP method, no locks on the request path.
//!
//! Matching semantics (§9.2):
//!
//! - static segments beat parameters, parameters beat `{*catch-all}`
//!   (matchit precedence);
//! - `int` parameters only match segments that parse as an integer —
//!   a failed type check means *no match* (404; Q4 is decided in M2);
//! - `HEAD` is served by `GET` routes when no `HEAD` route matches; the hit
//!   is flagged so the dispatcher drops the body;
//! - a path that exists only under other methods yields the set of allowed
//!   methods for `405` / `Allow` and for automatic `OPTIONS`;
//! - matching runs on the raw, still-encoded path; percent-decoding happens
//!   once, when parameter values are extracted, so an encoded `/` never
//!   splits a segment.

mod decl;
mod error;
mod pattern;
mod types;

use std::collections::{HashMap, HashSet};
use std::fmt;

pub use http::Method;

pub use decl::{AppDecl, PathParamType, PrefixError, RouteDecl};
pub use error::{CompileError, CompileErrors, RouteLabel};
pub use types::{
    AllowList, ParamSpec, PathValue, RouteHit, RouteId, RouteInfo, RouteLookup, ScalarType,
    TrailingSlash, TypedParam,
};

use pattern::{join_paths, percent_decode, scan_params, toggle_trailing_slash};

/// The standard HTTP methods, in `Allow`-header order.
const STANDARD_METHODS: [Method; 9] = [
    Method::GET,
    Method::HEAD,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::OPTIONS,
    Method::TRACE,
    Method::CONNECT,
];

const STANDARD_COUNT: usize = STANDARD_METHODS.len();
const GET_INDEX: usize = 0;
const HEAD_INDEX: usize = 1;

/// Index of a standard method, or `None` for extension methods.
fn standard_index(method: &Method) -> Option<usize> {
    STANDARD_METHODS.iter().position(|m| m == method)
}

/// Values stored per HTTP method: standard methods live in fixed slots,
/// extension methods in a short list.
#[derive(Debug)]
struct MethodMap<T> {
    standard: [Option<T>; STANDARD_COUNT],
    extended: Vec<(Method, T)>,
}

impl<T> MethodMap<T> {
    fn new() -> Self {
        Self {
            standard: std::array::from_fn(|_| None),
            extended: Vec::new(),
        }
    }

    fn get(&self, method: &Method) -> Option<&T> {
        match standard_index(method) {
            Some(index) => self.standard[index].as_ref(),
            None => self
                .extended
                .iter()
                .find(|(m, _)| m == method)
                .map(|(_, t)| t),
        }
    }

    fn get_or_insert_with(&mut self, method: &Method, make: impl FnOnce() -> T) -> &mut T {
        match standard_index(method) {
            Some(index) => self.standard[index].get_or_insert_with(make),
            None => {
                if let Some(pos) = self.extended.iter().position(|(m, _)| m == method) {
                    &mut self.extended[pos].1
                } else {
                    self.extended.push((method.clone(), make()));
                    let last = self.extended.len() - 1;
                    &mut self.extended[last].1
                }
            }
        }
    }

    /// All populated entries: standard methods in `STANDARD_METHODS` order,
    /// then extension methods in insertion order.
    fn iter(&self) -> impl Iterator<Item = (&Method, &T)> {
        self.standard
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.as_ref().map(|t| (&STANDARD_METHODS[i], t)))
            .chain(self.extended.iter().map(|(m, t)| (m, t)))
    }
}

/// The two radix trees of one method: `primary` holds every route,
/// `str_only` only routes whose parameters can never fail a type check.
///
/// When the highest-priority match in `primary` fails an `int` check, any
/// route that could still match the path is necessarily an all-`str` one —
/// that is what `str_only` provides, with no extra cost on the happy path.
struct MethodRouters {
    primary: matchit::Router<usize>,
    str_only: matchit::Router<usize>,
}

impl MethodRouters {
    fn new() -> Self {
        Self {
            primary: matchit::Router::new(),
            str_only: matchit::Router::new(),
        }
    }
}

impl fmt::Debug for MethodRouters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MethodRouters { .. }")
    }
}

/// The frozen application, compiled once at startup and shared by all
/// worker threads (DESIGN.md §6.4, §25.2).
///
/// `CompiledApp` is immutable, `Send + Sync`, and serves lookups without
/// locks; the only request-path allocation is the extracted parameter list.
#[derive(Debug)]
pub struct CompiledApp {
    routers: MethodMap<MethodRouters>,
    routes: Box<[RouteInfo]>,
    trailing_slash: TrailingSlash,
}

impl CompiledApp {
    /// Compile route declarations into the frozen app.
    ///
    /// Every declaration's pattern is validated, its `{param}`/`{*param}`
    /// names cross-checked against `param_types`, duplicates and router-tree
    /// conflicts reported, and the group's mount prefix applied — all
    /// problems are collected and returned together (DESIGN.md §14.1).
    ///
    /// # Errors
    /// [`CompileErrors`] listing every invalid, duplicate, conflicting or
    /// mistyped declaration.
    pub fn compile(decls: AppDecl) -> Result<Self, CompileErrors> {
        let AppDecl {
            prefix,
            trailing_slash,
            routes,
        } = decls;
        let prefix = prefix.as_deref().unwrap_or("");
        let mut errors = Vec::new();
        let mut routers = MethodMap::new();
        let mut infos = Vec::with_capacity(routes.len());
        let mut seen: HashMap<(Method, String), usize> = HashMap::new();

        for (index, decl) in routes.into_iter().enumerate() {
            let spec = match check_decl(&decl, prefix, index, &mut seen) {
                Ok(spec) => spec,
                Err(decl_errors) => {
                    errors.extend(decl_errors);
                    continue;
                }
            };
            let mr = routers.get_or_insert_with(&decl.method, MethodRouters::new);
            match mr.primary.insert(spec.path.clone(), infos.len()) {
                Ok(()) => {
                    if spec.all_str {
                        if let Err(err) = mr.str_only.insert(spec.path.clone(), infos.len()) {
                            errors.push(map_insert_error(spec.label(&decl), err));
                        }
                    }
                    infos.push(spec.into_info(decl));
                }
                Err(err) => errors.push(map_insert_error(spec.label(&decl), err)),
            }
        }

        if errors.is_empty() {
            Ok(Self {
                routers,
                routes: infos.into_boxed_slice(),
                trailing_slash,
            })
        } else {
            Err(CompileErrors::new(errors))
        }
    }

    /// Look up `method` + `path` (the raw, still-encoded request path).
    ///
    /// Returns the matching route and its typed, percent-decoded parameters,
    /// a redirect target under [`TrailingSlash::Redirect`], the `Allow` set
    /// for `405`s, or `NotFound` for `404`.
    pub fn route<'a>(&'a self, method: &Method, path: &'a str) -> RouteLookup<'a> {
        if let Some(hit) = self.try_method(method, path) {
            return RouteLookup::Hit(hit);
        }
        if method == Method::HEAD {
            if let Some(hit) = self.try_method(&Method::GET, path) {
                return RouteLookup::Hit(hit.implicit_head());
            }
        }
        match self.trailing_slash {
            TrailingSlash::Strict => {}
            TrailingSlash::Redirect => {
                if let Some(alt) = toggle_trailing_slash(path) {
                    let found = self.try_method(method, &alt).is_some()
                        || (method == Method::HEAD
                            && self.try_method(&Method::GET, &alt).is_some());
                    if found {
                        return RouteLookup::Redirect(alt);
                    }
                }
            }
            TrailingSlash::Ignore => {
                if let Some(alt) = toggle_trailing_slash(path) {
                    let mut hit = self.try_method_owned(method, &alt);
                    if hit.is_none() && method == Method::HEAD {
                        hit = self
                            .try_method_owned(&Method::GET, &alt)
                            .map(RouteHit::implicit_head);
                    }
                    if let Some(hit) = hit {
                        return RouteLookup::Hit(hit);
                    }
                }
            }
        }
        let allowed = self.allowed_methods(path);
        if allowed.is_empty() {
            RouteLookup::NotFound
        } else {
            RouteLookup::MethodNotAllowed(AllowList::new(allowed))
        }
    }

    /// The set of methods whose routes match `path` — the `Allow` set for a
    /// `405` and the data the dispatcher needs to answer `OPTIONS`
    /// automatically. `HEAD` is included whenever `GET` matches; the
    /// trailing-slash policy applies to every method.
    pub fn allowed_methods(&self, path: &str) -> Vec<Method> {
        let alt = if self.trailing_slash == TrailingSlash::Strict {
            None
        } else {
            toggle_trailing_slash(path)
        };
        let mut hits = [false; STANDARD_COUNT];
        let mut extra = Vec::new();
        for (method, mr) in self.routers.iter() {
            let mut found = self.hit_in(mr, path).is_some();
            if !found {
                if let Some(alt) = &alt {
                    found = self.hit_in(mr, alt).is_some();
                }
            }
            if !found {
                continue;
            }
            match standard_index(method) {
                Some(i) => hits[i] = true,
                None => extra.push(method.clone()),
            }
        }
        if hits[GET_INDEX] {
            hits[HEAD_INDEX] = true;
        }
        STANDARD_METHODS
            .iter()
            .zip(hits.iter())
            .filter(|(_, hit)| **hit)
            .map(|(m, _)| m.clone())
            .chain(extra)
            .collect()
    }

    /// Metadata for a route returned by a hit.
    pub fn route_info(&self, id: RouteId) -> &RouteInfo {
        &self.routes[id.0]
    }

    /// All routes, in declaration order (for `fulmen routes` and similar).
    pub fn routes(&self) -> &[RouteInfo] {
        &self.routes
    }

    /// The number of compiled routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the app has no routes.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Try `method`'s own trees (or `None` when it has no routes at all).
    fn try_method<'a>(&'a self, method: &Method, path: &'a str) -> Option<RouteHit<'a>> {
        let mr = self.routers.get(method)?;
        self.hit_in(mr, path)
    }

    /// Like [`CompiledApp::try_method`], but the hit's parameters are decoded
    /// into owned strings so they may borrow from a path that does not live
    /// as long as `self` — used by the [`TrailingSlash::Ignore`] retry.
    fn try_method_owned<'a>(&'a self, method: &Method, path: &str) -> Option<RouteHit<'a>> {
        let mr = self.routers.get(method)?;
        self.hit_in_owned(mr, path)
    }

    /// Match `path` inside one method's trees.
    fn hit_in<'a>(&'a self, mr: &'a MethodRouters, path: &'a str) -> Option<RouteHit<'a>> {
        if let Ok(m) = mr.primary.at(path) {
            if let Some(hit) = self.build_hit(*m.value, &m.params) {
                return Some(hit);
            }
            // The best match failed a parameter type check; only an
            // all-`str` route could still match, so consult the small tree.
            let m = mr.str_only.at(path).ok()?;
            return self.build_hit(*m.value, &m.params);
        }
        None
    }

    /// [`CompiledApp::hit_in`] variant producing owned parameters.
    fn hit_in_owned<'a>(&'a self, mr: &'a MethodRouters, path: &str) -> Option<RouteHit<'a>> {
        if let Ok(m) = mr.primary.at(path) {
            if let Some(hit) = self.build_hit_owned(*m.value, &m.params) {
                return Some(hit);
            }
            let m = mr.str_only.at(path).ok()?;
            return self.build_hit_owned(*m.value, &m.params);
        }
        None
    }

    /// Extract and type-check the parameters of a match. Returns `None`
    /// when a parameter is absent or fails its type check — the route then
    /// counts as no match.
    fn build_hit<'a>(
        &'a self,
        route: usize,
        params: &matchit::Params<'a, 'a>,
    ) -> Option<RouteHit<'a>> {
        let info = self.routes.get(route)?;
        let mut out = Vec::with_capacity(info.params().len());
        for spec in info.params() {
            let raw = params.get(spec.name())?;
            out.push(TypedParam {
                name: spec.name(),
                value: convert_param(raw, spec.param_type())?,
            });
        }
        Some(RouteHit {
            route: RouteId(route),
            params: out,
            implicit_head: false,
        })
    }

    /// [`CompiledApp::build_hit`] variant whose `str` values are owned.
    fn build_hit_owned<'a>(
        &'a self,
        route: usize,
        params: &matchit::Params<'a, '_>,
    ) -> Option<RouteHit<'a>> {
        let info = self.routes.get(route)?;
        let mut out = Vec::with_capacity(info.params().len());
        for spec in info.params() {
            let raw = params.get(spec.name())?;
            let decoded = percent_decode(raw)?;
            let value = match spec.param_type() {
                ScalarType::Str => PathValue::Str(std::borrow::Cow::Owned(decoded.into_owned())),
                ScalarType::Int => PathValue::Int(decoded.parse::<i64>().ok()?),
            };
            out.push(TypedParam {
                name: spec.name(),
                value,
            });
        }
        Some(RouteHit {
            route: RouteId(route),
            params: out,
            implicit_head: false,
        })
    }
}

/// Convert a raw (still-encoded) segment into a typed value: decode once,
/// then enforce the type.
fn convert_param(raw: &str, ty: ScalarType) -> Option<PathValue<'_>> {
    match ty {
        ScalarType::Str => Some(PathValue::Str(percent_decode(raw)?)),
        ScalarType::Int => percent_decode(raw)?.parse::<i64>().ok().map(PathValue::Int),
    }
}

/// The outcome of [`check_decl`]: a validated route ready for insertion.
struct CheckedDecl {
    /// The joined pattern (mount prefix applied).
    path: String,
    /// Parameter specs in declaration order.
    params: Vec<ParamSpec>,
    /// Whether every parameter is `str` (the route can never type-fail).
    all_str: bool,
}

impl CheckedDecl {
    /// The label identifying this route in error messages.
    fn label(&self, decl: &RouteDecl) -> RouteLabel {
        RouteLabel {
            method: decl.method.clone(),
            path: self.path.clone(),
        }
    }

    fn into_info(self, decl: RouteDecl) -> RouteInfo {
        RouteInfo::new(
            decl.method,
            self.path.into_boxed_str(),
            decl.handler_index,
            self.params.into_boxed_slice(),
        )
    }
}

/// Validate one declaration without touching the router trees: pattern
/// syntax, `param_types` ↔ pattern cross-checks, scalar types and exact
/// duplicates. `prefix` is only used to produce the joined path.
fn check_decl(
    decl: &RouteDecl,
    prefix: &str,
    index: usize,
    seen: &mut HashMap<(Method, String), usize>,
) -> Result<CheckedDecl, Vec<CompileError>> {
    let mut errors = Vec::new();
    let label = |path: &str| RouteLabel {
        method: decl.method.clone(),
        path: path.to_owned(),
    };

    if !decl.path.starts_with('/') {
        errors.push(CompileError::InvalidPattern {
            route: label(&decl.path),
            reason: "route paths must start with `/`".to_owned(),
        });
        return Err(errors);
    }

    let path = join_paths(prefix, &decl.path);
    let pattern_params = match scan_params(&path) {
        Ok(params) => params,
        Err(err) => {
            errors.push(CompileError::InvalidPattern {
                route: label(&path),
                reason: err.to_string(),
            });
            return Err(errors);
        }
    };

    // A `{param}` may appear only once in the pattern.
    let mut names = HashSet::new();
    for p in &pattern_params {
        if !names.insert(p.name.as_str()) {
            errors.push(CompileError::InvalidPattern {
                route: label(&path),
                reason: format!("parameter `{}` appears more than once", p.name),
            });
        }
    }

    // Every declared type must name a pattern parameter and be scalar.
    let mut declared = HashSet::new();
    for (name, ty) in &decl.param_types {
        if !declared.insert(name.as_str()) {
            errors.push(CompileError::DuplicateParamType {
                route: label(&path),
                param: name.clone(),
            });
            continue;
        }
        if !names.contains(name.as_str()) {
            errors.push(CompileError::TypeWithoutParam {
                route: label(&path),
                param: name.clone(),
            });
        }
        if let PathParamType::Unsupported(ty) = ty {
            errors.push(CompileError::NonScalarParam {
                route: label(&path),
                param: name.clone(),
                ty: ty.to_string(),
            });
        }
    }

    // Every pattern parameter must have a declared type.
    for p in &pattern_params {
        if !declared.contains(p.name.as_str()) {
            errors.push(CompileError::ParamWithoutType {
                route: label(&path),
                param: p.name.clone(),
            });
        }
    }

    // Exact duplicates: same method, same joined pattern.
    let key = (decl.method.clone(), path.clone());
    if let Some(first) = seen.get(&key) {
        errors.push(CompileError::DuplicateRoute {
            route: label(&path),
            first: *first,
        });
    } else {
        seen.insert(key, index);
    }

    if errors.is_empty() {
        let mut all_str = true;
        let params = decl
            .param_types
            .iter()
            .filter_map(|(name, ty)| {
                ty.as_scalar().map(|scalar| {
                    if scalar == ScalarType::Int {
                        all_str = false;
                    }
                    ParamSpec::new(
                        name.as_str(),
                        scalar,
                        pattern_params
                            .iter()
                            .any(|p| p.name == *name && p.catch_all),
                    )
                })
            })
            .collect();
        Ok(CheckedDecl {
            path,
            params,
            all_str,
        })
    } else {
        Err(errors)
    }
}

/// Map a `matchit` insertion error onto our error type.
fn map_insert_error(route: RouteLabel, err: matchit::InsertError) -> CompileError {
    match err {
        matchit::InsertError::Conflict { with } => CompileError::RouteConflict { route, with },
        matchit::InsertError::InvalidCatchAll => CompileError::InvalidPattern {
            route,
            reason: "catch-all `{*name}` is only allowed at the end of a pattern".to_owned(),
        },
        matchit::InsertError::InvalidParamSegment => CompileError::InvalidPattern {
            route,
            reason: "only one `{param}` is allowed per path segment".to_owned(),
        },
        matchit::InsertError::InvalidParam => CompileError::InvalidPattern {
            route,
            reason: "malformed `{param}` (unbalanced or empty braces)".to_owned(),
        },
        other => CompileError::InvalidPattern {
            route,
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(decls: Vec<RouteDecl>) -> CompiledApp {
        let mut app = AppDecl::new();
        for d in decls {
            app.route(d);
        }
        CompiledApp::compile(app).unwrap()
    }

    fn hit_route(app: &CompiledApp, method: &Method, path: &str) -> RouteId {
        match app.route(method, path) {
            RouteLookup::Hit(hit) => hit.route,
            other => panic!("expected a hit for {method} {path}, got {other:?}"),
        }
    }

    #[test]
    fn static_beats_param_beats_catch_all() {
        // `/users/{id}` and `/users/{*rest}` cannot coexist: a standalone
        // parameter and a catch-all in the same position are a matchit
        // conflict (a startup error). The catch-all below sits at the root,
        // which exercises the same precedence: static > param > catch-all.
        let app = app(vec![
            RouteDecl::new(Method::GET, "/users/me", 0),
            RouteDecl::new(Method::GET, "/users/{id}", 1).param("id", "str"),
            RouteDecl::new(Method::GET, "/{*rest}", 2).param("rest", "str"),
        ]);
        assert_eq!(hit_route(&app, &Method::GET, "/users/me").index(), 0);
        assert_eq!(hit_route(&app, &Method::GET, "/users/42").index(), 1);
        assert_eq!(hit_route(&app, &Method::GET, "/other/a/b").index(), 2);
    }

    #[test]
    fn same_position_param_and_catch_all_conflict() {
        let mut app = AppDecl::new();
        app.route(RouteDecl::new(Method::GET, "/users/{id}", 0).param("id", "str"));
        app.route(RouteDecl::new(Method::GET, "/users/{*rest}", 1).param("rest", "str"));
        let err = CompiledApp::compile(app).unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, CompileError::RouteConflict { .. }))
        );
    }

    #[test]
    fn params_in_declaration_order() {
        let app = app(vec![
            RouteDecl::new(Method::GET, "/{a}/x/{b}", 0)
                .param("b", "str")
                .param("a", "str"),
        ]);
        let RouteLookup::Hit(hit) = app.route(&Method::GET, "/first/x/second") else {
            panic!("expected a hit");
        };
        let names: Vec<&str> = hit.params.iter().map(|p| p.name).collect();
        assert_eq!(names, ["b", "a"]);
        assert_eq!(hit.param("a").and_then(PathValue::as_str), Some("first"));
        assert_eq!(hit.param("b").and_then(PathValue::as_str), Some("second"));
    }

    #[test]
    fn int_params_are_typed() {
        let app = app(vec![
            RouteDecl::new(Method::GET, "/users/{id}", 0).param("id", "int"),
        ]);
        let RouteLookup::Hit(hit) = app.route(&Method::GET, "/users/-42") else {
            panic!("expected a hit");
        };
        assert_eq!(hit.param("id").and_then(PathValue::as_int), Some(-42));
        assert!(matches!(
            app.route(&Method::GET, "/users/abc"),
            RouteLookup::NotFound
        ));
        assert!(matches!(
            app.route(&Method::GET, "/users/99999999999999999999"),
            RouteLookup::NotFound
        ));
    }

    #[test]
    fn failed_int_falls_back_to_catch_all() {
        // `/users/abc` matches `/users/{id}` structurally but fails the
        // `int` check, so the all-`str` catch-all gets its chance.
        let app = app(vec![
            RouteDecl::new(Method::GET, "/users/{id}", 0).param("id", "int"),
            RouteDecl::new(Method::GET, "/{*rest}", 1).param("rest", "str"),
        ]);
        assert_eq!(hit_route(&app, &Method::GET, "/users/42").index(), 0);
        let RouteLookup::Hit(hit) = app.route(&Method::GET, "/users/abc") else {
            panic!("expected a hit");
        };
        assert_eq!(hit.route.index(), 1);
        assert_eq!(
            hit.param("rest").and_then(PathValue::as_str),
            Some("users/abc")
        );
    }

    #[test]
    fn compiled_app_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CompiledApp>();
    }
}
