//! Types produced by compiling and matching routes: the frozen [`RouteInfo`]
//! table, the [`RouteHit`] returned on a match, and the [`AllowList`] used
//! for `405` responses and automatic `OPTIONS` handling.

use std::borrow::Cow;
use std::fmt;

use http::Method;

/// Identifier of a compiled route: its position in
/// [`CompiledApp::routes`](crate::router::CompiledApp::routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RouteId(pub(crate) usize);

impl RouteId {
    /// The route's index in the compiled route table.
    pub fn index(self) -> usize {
        self.0
    }
}

impl fmt::Display for RouteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The scalar type of a path parameter in a compiled route.
///
/// Declarations may carry arbitrary type names
/// ([`PathParamType`](crate::router::PathParamType)), but a compiled route is
/// guaranteed to contain only `int` and `str` parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    /// `int`: the decoded segment must parse as an `i64`.
    Int,
    /// `str`: any decoded segment.
    Str,
}

/// A path parameter of a compiled route (a `{name}` or `{*name}` segment).
#[derive(Debug)]
pub struct ParamSpec {
    name: Box<str>,
    ty: ScalarType,
    catch_all: bool,
}

impl ParamSpec {
    pub(crate) fn new(name: impl Into<Box<str>>, ty: ScalarType, catch_all: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            catch_all,
        }
    }

    /// The parameter name, without braces or a leading `*`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The parameter's scalar type.
    pub fn param_type(&self) -> ScalarType {
        self.ty
    }

    /// Whether this parameter is a `{*name}` catch-all.
    pub fn is_catch_all(&self) -> bool {
        self.catch_all
    }
}

/// One entry of the compiled route table.
#[derive(Debug)]
pub struct RouteInfo {
    method: Method,
    path: Box<str>,
    handler_index: u32,
    params: Box<[ParamSpec]>,
    hooks: crate::pipeline::RouteHooks,
    errors: u32,
}

impl RouteInfo {
    pub(crate) fn new(
        method: Method,
        path: impl Into<Box<str>>,
        handler_index: u32,
        params: Box<[ParamSpec]>,
        hooks: crate::pipeline::RouteHooks,
        errors: u32,
    ) -> Self {
        Self {
            method,
            path: path.into(),
            handler_index,
            params,
            hooks,
            errors,
        }
    }

    /// The HTTP method this route answers.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The route's full pattern, mount prefixes applied.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Index of the handler in the application's handler table.
    pub fn handler_index(&self) -> u32 {
        self.handler_index
    }

    /// The route's path parameters, in declaration order.
    pub fn params(&self) -> &[ParamSpec] {
        &self.params
    }

    /// The route's resolved lifecycle hook chains (DESIGN.md §8).
    pub fn hooks(&self) -> &crate::pipeline::RouteHooks {
        &self.hooks
    }

    /// Index of the route's `on_error` handler table (`0` = app-wide).
    pub fn error_table(&self) -> u32 {
        self.errors
    }
}

/// A typed, percent-decoded path parameter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathValue<'a> {
    /// `int` parameter, parsed from the decoded segment.
    Int(i64),
    /// `str` parameter. Borrows from the request path when the raw segment
    /// contained no `%` escapes; owned after decoding otherwise.
    Str(Cow<'a, str>),
}

impl PathValue<'_> {
    /// The integer value, when this is an `int` parameter.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            Self::Str(_) => None,
        }
    }

    /// The string value, when this is a `str` parameter.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            Self::Int(_) => None,
        }
    }
}

/// One extracted path parameter of a [`RouteHit`], in declaration order.
#[derive(Debug)]
pub struct TypedParam<'a> {
    /// The parameter name, without braces or a leading `*`.
    pub name: &'a str,
    /// The typed, percent-decoded value.
    pub value: PathValue<'a>,
}

/// A successful route lookup.
#[derive(Debug)]
pub struct RouteHit<'a> {
    /// The matched route; look it up via
    /// [`CompiledApp::route_info`](crate::router::CompiledApp::route_info).
    pub route: RouteId,
    /// The extracted path parameters, in declaration order
    /// ([`RouteDecl::param_types`](crate::router::RouteDecl::param_types)).
    pub params: Vec<TypedParam<'a>>,
    /// `true` when a `HEAD` request was matched by a `GET` route: the
    /// dispatcher must send headers only and drop the response body
    /// (DESIGN.md §9.2).
    pub implicit_head: bool,
}

impl<'a> RouteHit<'a> {
    /// The value of the parameter `name`, if present.
    pub fn param(&self, name: &str) -> Option<&PathValue<'a>> {
        self.params
            .iter()
            .find(|p| p.name == name)
            .map(|p| &p.value)
    }

    /// Mark this hit as a `GET` route answering a `HEAD` request.
    pub(crate) fn implicit_head(mut self) -> Self {
        self.implicit_head = true;
        self
    }
}

/// The outcome of [`CompiledApp::route`](crate::router::CompiledApp::route).
#[derive(Debug)]
pub enum RouteLookup<'a> {
    /// The request matched a route.
    Hit(RouteHit<'a>),
    /// No exact match, but the path with its trailing slash toggled would
    /// match — only produced under [`TrailingSlash::Redirect`]. Carries the
    /// corrected path (the dispatcher turns it into a `308` redirect).
    Redirect(String),
    /// The path exists under other methods → `405 Method Not Allowed`.
    /// Carries the methods that do match, for the `Allow` header.
    MethodNotAllowed(AllowList),
    /// No route matched the path at all → `404`.
    NotFound,
}

impl RouteLookup<'_> {
    /// Whether the lookup matched a route.
    pub fn is_hit(&self) -> bool {
        matches!(self, Self::Hit(_))
    }

    /// The hit, when the lookup matched.
    pub fn hit(&self) -> Option<&RouteHit<'_>> {
        match self {
            Self::Hit(hit) => Some(hit),
            _ => None,
        }
    }
}

/// The set of methods whose routes match a path: the `Allow` header value
/// for `405` responses, and the metadata the dispatcher needs for automatic
/// `OPTIONS` handling (DESIGN.md §9.2, §16.1).
#[derive(Debug)]
pub struct AllowList {
    methods: Vec<Method>,
}

impl AllowList {
    pub(crate) fn new(methods: Vec<Method>) -> Self {
        Self { methods }
    }

    /// The matching methods, in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = &Method> {
        self.methods.iter()
    }

    /// Whether `method` matches the path.
    pub fn contains(&self, method: &Method) -> bool {
        self.methods.contains(method)
    }

    /// Render the `Allow` header value. `OPTIONS` is appended when absent,
    /// because the dispatcher answers it automatically.
    pub fn header(&self) -> String {
        let mut value = self
            .methods
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !self.contains(&Method::OPTIONS) {
            if !value.is_empty() {
                value.push_str(", ");
            }
            value.push_str("OPTIONS");
        }
        value
    }
}

/// What to do when a request path differs from a route only by a trailing
/// slash (DESIGN.md §9.2). The default is [`TrailingSlash::Strict`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TrailingSlash {
    /// `/a` and `/a/` are different resources (the default).
    #[default]
    Strict,
    /// The mismatching form gets a `308` redirect to the matching one.
    Redirect,
    /// Both forms match the same route.
    Ignore,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_header_appends_options() {
        let allow = AllowList::new(vec![Method::GET, Method::POST]);
        assert_eq!(allow.header(), "GET, POST, OPTIONS");
        let allow = AllowList::new(vec![Method::OPTIONS]);
        assert_eq!(allow.header(), "OPTIONS");
    }

    #[test]
    fn path_value_accessors() {
        assert_eq!(PathValue::Int(7).as_int(), Some(7));
        assert_eq!(PathValue::Int(7).as_str(), None);
        let v = PathValue::Str(Cow::Borrowed("x"));
        assert_eq!(v.as_str(), Some("x"));
        assert_eq!(v.as_int(), None);
    }
}
