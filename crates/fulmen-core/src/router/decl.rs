//! Declaration-side types: the route descriptions a caller hands to
//! [`CompiledApp::compile`](crate::router::CompiledApp::compile).
//!
//! The Python bridge (`fulmen-py`, M1.13+) builds these from decorator
//! declarations; they can equally be built by other Rust code.

use http::Method;

use super::pattern::join_paths;
use super::types::{ScalarType, TrailingSlash};

/// The type declared for a `{name}` or `{*name}` path parameter.
///
/// For M1 only `int` and `str` are valid path parameter types (ROADMAP.md).
/// Any other type is carried as [`PathParamType::Unsupported`] so the
/// compiler can report it as a startup error — an invalid declaration must
/// still be representable for the error to be listed with all the others
/// (DESIGN.md §14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathParamType {
    /// A signed 64-bit integer segment: an optional `-`/`+` sign followed by
    /// digits, within `i64` range.
    Int,
    /// Any (decoded) segment string. The only sensible type for a
    /// `{*catch-all}`, though an `int` catch-all is not rejected.
    Str,
    /// Any other type name, kept for the startup error message.
    Unsupported(Box<str>),
}

impl PathParamType {
    /// The scalar form of this type, when it is one (`int` or `str`).
    pub(crate) fn as_scalar(&self) -> Option<ScalarType> {
        match self {
            Self::Int => Some(ScalarType::Int),
            Self::Str => Some(ScalarType::Str),
            Self::Unsupported(_) => None,
        }
    }
}

impl From<&str> for PathParamType {
    /// Map a type name: `"int"` and `"str"` are scalars, anything else is
    /// [`PathParamType::Unsupported`].
    fn from(name: &str) -> Self {
        match name {
            "int" => Self::Int,
            "str" => Self::Str,
            other => Self::Unsupported(other.into()),
        }
    }
}

impl From<String> for PathParamType {
    fn from(name: String) -> Self {
        Self::from(name.as_str())
    }
}

/// A single route declaration.
///
/// `param_types` maps every `{name}` and `{*name}` parameter in `path` to its
/// declared type, in handler-argument order. A match returns the extracted
/// values in this same order, so the dispatcher can bind them positionally.
/// Parameters present in the pattern but missing here — or declared here but
/// absent from the pattern — are startup errors (DESIGN.md §9.3).
#[derive(Debug)]
pub struct RouteDecl {
    /// The HTTP method this route answers.
    pub method: Method,
    /// The route pattern: `/users/{user_id}` matches one segment,
    /// `/files/{*path}` catches the rest of the path (last segment only).
    pub path: String,
    /// Index of the handler in the application's handler table.
    pub handler_index: u32,
    /// `(name, type)` for each path parameter, in declaration order.
    pub param_types: Vec<(String, PathParamType)>,
}

impl RouteDecl {
    /// A route declaration without path parameters.
    pub fn new(method: Method, path: impl Into<String>, handler_index: u32) -> Self {
        Self {
            method,
            path: path.into(),
            handler_index,
            param_types: Vec::new(),
        }
    }

    /// Declare the type of a `{name}` path parameter (builder style).
    #[must_use]
    pub fn param(mut self, name: impl Into<String>, ty: impl Into<PathParamType>) -> Self {
        self.param_types.push((name.into(), ty.into()));
        self
    }
}

/// A group of route declarations sharing an optional mount prefix, plus the
/// application's trailing-slash policy.
///
/// The prefix is applied when the group is compiled or mounted into a
/// parent, so nested groups compose: `api` (`/api`) mounting `v1` (`/v1`)
/// yields `/api/v1/...` once `api` is itself mounted or compiled.
#[derive(Debug)]
pub struct AppDecl {
    /// Mount prefix for every route in the group (static text only).
    pub(crate) prefix: Option<String>,
    /// Trailing-slash policy applied to the whole compiled app.
    pub(crate) trailing_slash: TrailingSlash,
    /// The group's route declarations, unprefixed.
    pub(crate) routes: Vec<RouteDecl>,
}

impl AppDecl {
    /// An empty group without a prefix.
    pub fn new() -> Self {
        Self {
            prefix: None,
            trailing_slash: TrailingSlash::default(),
            routes: Vec::new(),
        }
    }

    /// An empty group whose routes are mounted under `prefix`.
    ///
    /// Prefixes must start with `/` and must be static text: parameters are
    /// not allowed in group prefixes. `"/"` and `""` are rejected; a group
    /// without a prefix is simply [`AppDecl::new`].
    ///
    /// # Errors
    /// [`PrefixError`] when the prefix does not start with `/` or contains
    /// `{`/`}`.
    pub fn with_prefix(prefix: &str) -> Result<Self, PrefixError> {
        validate_prefix(prefix)?;
        Ok(Self {
            prefix: Some(prefix.to_owned()),
            ..Self::new()
        })
    }

    /// Set the trailing-slash policy for the compiled app (builder style).
    /// The default is [`TrailingSlash::Strict`] (DESIGN.md §9.2).
    #[must_use]
    pub fn trailing_slash_policy(mut self, policy: TrailingSlash) -> Self {
        self.trailing_slash = policy;
        self
    }

    /// Add a route declaration to the group.
    pub fn route(&mut self, decl: RouteDecl) -> &mut Self {
        self.routes.push(decl);
        self
    }

    /// Mount another group's routes into this one, under the child's prefix
    /// (`app.mount(api)` in the Python API; DESIGN.md §5.5).
    ///
    /// A child without a prefix mounts its routes unchanged. The child's
    /// trailing-slash policy is ignored; the policy of the group that is
    /// eventually compiled wins.
    pub fn mount(&mut self, child: AppDecl) -> &mut Self {
        let prefix = child.prefix.as_deref().unwrap_or("");
        self.routes
            .extend(child.routes.into_iter().map(|mut route| {
                route.path = join_paths(prefix, &route.path);
                route
            }));
        self
    }

    /// The group's mount prefix, if any.
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The group's route declarations (unprefixed, as added).
    pub fn declarations(&self) -> &[RouteDecl] {
        &self.routes
    }
}

impl Default for AppDecl {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a mount prefix was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixError {
    /// The prefix does not start with `/`.
    MissingLeadingSlash(String),
    /// The prefix contains `{` or `}`; group prefixes must be static text.
    ContainsParams(String),
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingLeadingSlash(prefix) => {
                write!(f, "mount prefix `{prefix}` must start with `/`")
            }
            Self::ContainsParams(prefix) => {
                write!(f, "mount prefix `{prefix}` must not contain `{{` or `}}`")
            }
        }
    }
}

impl std::error::Error for PrefixError {}

fn validate_prefix(prefix: &str) -> Result<(), PrefixError> {
    if !prefix.starts_with('/') {
        return Err(PrefixError::MissingLeadingSlash(prefix.to_owned()));
    }
    if prefix.contains(['{', '}']) {
        return Err(PrefixError::ContainsParams(prefix.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_validation() {
        assert!(AppDecl::with_prefix("/api").is_ok());
        assert!(AppDecl::with_prefix("/api/").is_ok());
        assert!(matches!(
            AppDecl::with_prefix("api"),
            Err(PrefixError::MissingLeadingSlash(_))
        ));
        assert!(matches!(
            AppDecl::with_prefix(""),
            Err(PrefixError::MissingLeadingSlash(_))
        ));
        assert!(matches!(
            AppDecl::with_prefix("/orgs/{org}"),
            Err(PrefixError::ContainsParams(_))
        ));
    }

    #[test]
    fn mount_joins_under_child_prefix() {
        let mut api = AppDecl::with_prefix("/api/v1").unwrap();
        api.route(RouteDecl::new(Method::GET, "/me", 0));
        api.route(RouteDecl::new(Method::GET, "/", 1));

        let mut app = AppDecl::new();
        app.route(RouteDecl::new(Method::GET, "/health", 2));
        app.mount(api);

        let paths: Vec<&str> = app.declarations().iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/health", "/api/v1/me", "/api/v1/"]);
    }

    #[test]
    fn nested_mounts_compose() {
        let mut v1 = AppDecl::with_prefix("/v1").unwrap();
        v1.route(RouteDecl::new(Method::GET, "/users", 0));

        let mut api = AppDecl::with_prefix("/api").unwrap();
        api.route(RouteDecl::new(Method::GET, "/status", 1));
        api.mount(v1);

        let mut app = AppDecl::new();
        app.mount(api);

        let paths: Vec<&str> = app.declarations().iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/api/status", "/api/v1/users"]);
    }

    #[test]
    fn mount_without_prefix_joins_unchanged() {
        let mut child = AppDecl::new();
        child.route(RouteDecl::new(Method::GET, "/x", 0));
        let mut app = AppDecl::new();
        app.mount(child);
        assert_eq!(app.declarations()[0].path, "/x");
    }
}
