//! Route-pattern helpers used while compiling and matching.
//!
//! All functions operate on the *raw* (still percent-encoded) path. Matching
//! runs against the encoded path — an encoded `/` must never be treated as a
//! segment separator — and decoding happens exactly once, when parameter
//! values are extracted for a matched route (DESIGN.md §9.2).

use std::borrow::Cow;
use std::fmt;

/// One `{name}` or `{*name}` parameter occurrence inside a route pattern.
#[derive(Debug)]
pub(crate) struct PatternParam {
    /// The parameter name, without braces and without a leading `*`.
    pub(crate) name: String,
    /// Whether this is a `{*name}` catch-all parameter.
    pub(crate) catch_all: bool,
}

/// Why a pattern could not be scanned for parameters.
#[derive(Debug)]
pub(crate) enum PatternError {
    /// A `{` opened a parameter that is never closed by `}`.
    UnclosedBrace,
    /// A `}` appeared without a matching `{`.
    UnexpectedCloseBrace,
    /// A `{}` or `{*}` parameter has no name.
    EmptyName,
    /// A `{` appeared inside a parameter name.
    NestedBrace,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::UnclosedBrace => "`{` parameter is never closed by `}`",
            Self::UnexpectedCloseBrace => "`}` without a matching `{`",
            Self::EmptyName => "parameter `{}` has no name",
            Self::NestedBrace => "`{` inside a parameter name",
        };
        f.write_str(reason)
    }
}

/// Extract every `{name}` / `{*name}` parameter from a route pattern.
///
/// `{{` and `}}` are treated as escaped literal braces (matchit syntax), so
/// `/literal/{{x}}` yields no parameters. Only the syntax needed to enumerate
/// parameter names is validated here; the router tree performs the remaining
/// checks (catch-all position, one parameter per segment, ...) at insertion.
pub(crate) fn scan_params(pattern: &str) -> Result<Vec<PatternParam>, PatternError> {
    let bytes = pattern.as_bytes();
    let mut params = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'}' {
                    if bytes[end] == b'{' {
                        return Err(PatternError::NestedBrace);
                    }
                    end += 1;
                }
                if end == bytes.len() {
                    return Err(PatternError::UnclosedBrace);
                }
                let inner = &pattern[start..end];
                let (name, catch_all) = match inner.strip_prefix('*') {
                    Some(rest) => (rest, true),
                    None => (inner, false),
                };
                if name.is_empty() {
                    return Err(PatternError::EmptyName);
                }
                params.push(PatternParam {
                    name: name.to_owned(),
                    catch_all,
                });
                i = end + 1;
            }
            b'}' => {
                if bytes.get(i + 1) == Some(&b'}') {
                    i += 2;
                    continue;
                }
                return Err(PatternError::UnexpectedCloseBrace);
            }
            _ => i += 1,
        }
    }
    Ok(params)
}

/// Join a mount prefix and a route path with exactly one `/` between them.
///
/// `join_paths("/api/v1", "/me")` is `/api/v1/me`, `join_paths("/", "/me")`
/// is `/me`, and `join_paths("/api", "/")` is `/api/` (the declared trailing
/// slash is preserved; the strict-slash policy keeps `/api` and `/api/`
/// distinct). `path` must already start with `/`.
pub(crate) fn join_paths(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}{path}")
    }
}

/// The path with its trailing-slash state toggled, or `None` for `/`.
pub(crate) fn toggle_trailing_slash(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.strip_suffix('/') {
        Some(stripped) => Some(stripped.to_owned()),
        None => Some(format!("{path}/")),
    }
}

/// Percent-decode a path segment value, once (DESIGN.md §9.2).
///
/// `+` is *not* decoded: it is a literal plus inside a path, only form-style
/// query strings give it a special meaning. Returns `None` when the input
/// contains an invalid `%` escape or the decoded bytes are not UTF-8.
pub(crate) fn percent_decode(raw: &str) -> Option<Cow<'_, str>> {
    if !raw.as_bytes().contains(&b'%') {
        return Some(Cow::Borrowed(raw));
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex_value(*bytes.get(i + 1)?)?;
                let lo = hex_value(*bytes.get(i + 2)?)?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().map(Cow::Owned)
}

/// Value of a hex digit, or `None` for non-hex input.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pattern: &str) -> Vec<String> {
        scan_params(pattern)
            .into_iter()
            .flatten()
            .map(|p| p.name)
            .collect()
    }

    #[test]
    fn scan_finds_params() {
        assert_eq!(names("/users/{user_id}"), ["user_id"]);
        assert_eq!(names("/a/{x}/b/{y}"), ["x", "y"]);
        assert_eq!(names("/files/{*path}"), ["path"]);
        assert_eq!(names("/img-{id}.png"), ["id"]);
        assert!(names("/static/path").is_empty());
    }

    #[test]
    fn scan_records_catch_all() {
        let params = scan_params("/files/{*path}")
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(params.len(), 1);
        assert!(params[0].catch_all);
        let params = scan_params("/files/{path}")
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert!(!params[0].catch_all);
    }

    #[test]
    fn scan_skips_escaped_braces() {
        assert!(names("/literal/{{x}}").is_empty());
        assert_eq!(names("/{{x}}/{y}"), ["y"]);
    }

    #[test]
    fn scan_rejects_malformed() {
        assert!(matches!(
            scan_params("/{x"),
            Err(PatternError::UnclosedBrace)
        ));
        assert!(matches!(
            scan_params("/x}"),
            Err(PatternError::UnexpectedCloseBrace)
        ));
        assert!(matches!(scan_params("/{}"), Err(PatternError::EmptyName)));
        assert!(matches!(scan_params("/{*}"), Err(PatternError::EmptyName)));
        assert!(matches!(
            scan_params("/{a{b}}"),
            Err(PatternError::NestedBrace)
        ));
    }

    #[test]
    fn join_is_exact() {
        assert_eq!(join_paths("/api/v1", "/me"), "/api/v1/me");
        assert_eq!(join_paths("/api/", "/me"), "/api/me");
        assert_eq!(join_paths("/", "/me"), "/me");
        assert_eq!(join_paths("", "/me"), "/me");
        assert_eq!(join_paths("/api", "/"), "/api/");
        assert_eq!(join_paths("/api//", "/x"), "/api/x");
    }

    #[test]
    fn toggle_swaps_slash() {
        assert_eq!(toggle_trailing_slash("/a"), Some("/a/".to_owned()));
        assert_eq!(toggle_trailing_slash("/a/"), Some("/a".to_owned()));
        assert_eq!(toggle_trailing_slash("/"), None);
        assert_eq!(toggle_trailing_slash("/a//"), Some("/a/".to_owned()));
    }

    #[test]
    fn decode_plain_borrows() {
        assert_eq!(percent_decode("abc"), Some(Cow::Borrowed("abc")));
    }

    #[test]
    fn decode_escapes() {
        assert_eq!(percent_decode("a%20b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(percent_decode("%6de").as_deref(), Some("me"));
        // `+` stays literal in a path segment.
        assert_eq!(percent_decode("a+b").as_deref(), Some("a+b"));
        // multi-byte UTF-8 through escapes.
        assert_eq!(percent_decode("%E2%82%AC").as_deref(), Some("€"));
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("%"), None);
        // decoded bytes must be UTF-8
        assert_eq!(percent_decode("%ff%ff"), None);
    }
}
