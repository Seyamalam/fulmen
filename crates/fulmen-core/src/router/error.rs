//! Startup errors: everything the route compiler can reject, reported all
//! at once (DESIGN.md §14.1 — the app refuses to start listing every
//! problem, not the first one).

use std::error::Error;
use std::fmt;

use http::Method;

/// Method and joined path identifying a route in an error message.
#[derive(Debug, Clone)]
pub struct RouteLabel {
    /// The declared HTTP method.
    pub method: Method,
    /// The route's pattern, with its group's mount prefix applied.
    pub path: String,
}

impl fmt::Display for RouteLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.path)
    }
}

/// One problem found while compiling an [`AppDecl`](crate::router::AppDecl).
#[derive(Debug)]
pub enum CompileError {
    /// The pattern is malformed or was rejected by the router tree.
    InvalidPattern {
        /// The offending route.
        route: RouteLabel,
        /// What is wrong with the pattern.
        reason: String,
    },
    /// The same method and pattern were declared twice (mount-prefix joins
    /// count: a mounted route may collide with a direct declaration).
    DuplicateRoute {
        /// The repeated route.
        route: RouteLabel,
        /// Index of the earlier declaration it duplicates.
        first: usize,
    },
    /// The pattern is ambiguous against an earlier route in the same method
    /// table — for example `/users/{id}` next to `/users/{name}` or a
    /// catch-all shadowing a parameter (a matchit conflict; DESIGN.md §9.3).
    RouteConflict {
        /// The route that could not be inserted.
        route: RouteLabel,
        /// The earlier pattern it conflicts with.
        with: String,
    },
    /// A `{param}` appears in the pattern but has no declared type
    /// (DESIGN.md §9.3 — every path parameter needs a handler argument).
    ParamWithoutType {
        /// The offending route.
        route: RouteLabel,
        /// The parameter missing a type.
        param: String,
    },
    /// A type was declared for a parameter the pattern does not contain.
    TypeWithoutParam {
        /// The offending route.
        route: RouteLabel,
        /// The declared parameter absent from the pattern.
        param: String,
    },
    /// A path parameter's type is not `int` or `str` — the only types M1
    /// supports (ROADMAP.md; DESIGN.md §9.3 requires scalars).
    NonScalarParam {
        /// The offending route.
        route: RouteLabel,
        /// The parameter with the bad type.
        param: String,
        /// The declared type name.
        ty: String,
    },
    /// The same parameter name was declared twice in `param_types`.
    DuplicateParamType {
        /// The offending route.
        route: RouteLabel,
        /// The repeated parameter name.
        param: String,
    },
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPattern { route, reason } => {
                write!(f, "{route}: invalid pattern: {reason}")
            }
            Self::DuplicateRoute { route, first } => {
                write!(f, "{route}: duplicate of declaration #{first}")
            }
            Self::RouteConflict { route, with } => {
                write!(f, "{route}: conflicts with earlier pattern `{with}`")
            }
            Self::ParamWithoutType { route, param } => {
                write!(f, "{route}: parameter `{param}` has no declared type")
            }
            Self::TypeWithoutParam { route, param } => {
                write!(
                    f,
                    "{route}: type declared for `{param}` but the pattern has no such parameter"
                )
            }
            Self::NonScalarParam { route, param, ty } => {
                write!(
                    f,
                    "{route}: parameter `{param}` has type `{ty}`; path parameters must be `int` or `str`"
                )
            }
            Self::DuplicateParamType { route, param } => {
                write!(f, "{route}: parameter `{param}` is declared twice")
            }
        }
    }
}

impl Error for CompileError {}

/// Every problem found while compiling an [`AppDecl`](crate::router::AppDecl).
///
/// The compiler collects errors across all declarations instead of stopping
/// at the first, so startup reports the full list (DESIGN.md §14.1).
#[derive(Debug)]
pub struct CompileErrors {
    errors: Vec<CompileError>,
}

impl CompileErrors {
    pub(crate) fn new(errors: Vec<CompileError>) -> Self {
        Self { errors }
    }

    /// The problems found, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &CompileError> {
        self.errors.iter()
    }

    /// The number of problems.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Whether the list is empty (always `false` — an empty list compiles).
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Consume into the list of problems.
    pub fn into_vec(self) -> Vec<CompileError> {
        self.errors
    }
}

impl fmt::Display for CompileErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.errors.len();
        write!(f, "route compilation failed with {n} problem")?;
        if n != 1 {
            f.write_str("s")?;
        }
        f.write_str(":")?;
        for (i, error) in self.errors.iter().enumerate() {
            write!(f, "\n  {}. {error}", i + 1)?;
        }
        Ok(())
    }
}

impl Error for CompileErrors {}
