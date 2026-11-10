//! Integration tests for the compiled router, exercising only the public
//! API (`fulmen_core::*`) the way `fulmen-server`/`fulmen-py` will.

use fulmen_core::{
    AppDecl, CompileError, CompiledApp, Method, PathValue, RouteDecl, RouteLookup, TrailingSlash,
};

/// Build an `AppDecl` from route declarations and compile it.
macro_rules! compile {
    ($($decl:expr),+ $(,)?) => {{
        let mut app = AppDecl::new();
        $(app.route($decl);)+
        CompiledApp::compile(app).unwrap()
    }};
}

/// Route `method` + `path`, requiring a hit (and unwrapping it).
macro_rules! must_hit {
    ($app:expr, $method:expr, $path:expr) => {{
        match $app.route(&$method, $path) {
            RouteLookup::Hit(hit) => hit,
            other => panic!("expected a hit for {} {}, got {other:?}", $method, $path),
        }
    }};
}

#[test]
fn get_and_post_are_separate_trees() {
    let app = compile![
        RouteDecl::new(Method::GET, "/users", 0),
        RouteDecl::new(Method::POST, "/users", 1),
    ];
    assert_eq!(must_hit!(app, Method::GET, "/users").route.index(), 0);
    assert_eq!(must_hit!(app, Method::POST, "/users").route.index(), 1);
}

#[test]
fn head_is_served_by_get() {
    let app = compile![RouteDecl::new(Method::GET, "/users/{id}", 0).param("id", "int"),];
    let hit = must_hit!(app, Method::HEAD, "/users/5");
    assert!(hit.implicit_head);
    assert_eq!(hit.route.index(), 0);
    // ...while GET is not implicit.
    assert!(!must_hit!(app, Method::GET, "/users/5").implicit_head);
}

#[test]
fn explicit_head_beats_get_fallback() {
    let app = compile![
        RouteDecl::new(Method::GET, "/x", 0),
        RouteDecl::new(Method::HEAD, "/x", 1),
    ];
    let hit = must_hit!(app, Method::HEAD, "/x");
    assert_eq!(hit.route.index(), 1);
    assert!(!hit.implicit_head);
}

#[test]
fn miss_on_existing_path_is_405_with_allow() {
    let app = compile![
        RouteDecl::new(Method::GET, "/users", 0),
        RouteDecl::new(Method::POST, "/users", 1),
        RouteDecl::new(Method::GET, "/other", 2),
    ];
    let RouteLookup::MethodNotAllowed(allow) = app.route(&Method::PUT, "/users") else {
        panic!("expected 405");
    };
    // GET implies HEAD in the Allow set.
    assert!(allow.contains(&Method::GET));
    assert!(allow.contains(&Method::HEAD));
    assert!(allow.contains(&Method::POST));
    assert!(!allow.contains(&Method::PUT));
    assert_eq!(allow.header(), "GET, HEAD, POST, OPTIONS");

    // OPTIONS gets the same metadata: the dispatcher answers automatically.
    let RouteLookup::MethodNotAllowed(allow) = app.route(&Method::OPTIONS, "/users") else {
        panic!("expected 405 for OPTIONS");
    };
    assert_eq!(allow.header(), "GET, HEAD, POST, OPTIONS");

    // But an explicit OPTIONS route still wins.
    let app = compile![
        RouteDecl::new(Method::GET, "/x", 0),
        RouteDecl::new(Method::OPTIONS, "/x", 1),
    ];
    assert_eq!(must_hit!(app, Method::OPTIONS, "/x").route.index(), 1);
}

#[test]
fn unknown_path_is_404() {
    let app = compile![RouteDecl::new(Method::GET, "/users", 0)];
    assert!(matches!(
        app.route(&Method::GET, "/nope"),
        RouteLookup::NotFound
    ));
    // ...including for a method that exists elsewhere.
    assert!(matches!(
        app.route(&Method::POST, "/nope"),
        RouteLookup::NotFound
    ));
}

#[test]
fn head_does_not_leak_into_404() {
    // HEAD falls back to GET, but a GET-only path still 404s for HEAD when
    // nothing matches.
    let app = compile![RouteDecl::new(Method::POST, "/only-post", 0)];
    assert!(matches!(
        app.route(&Method::HEAD, "/only-post"),
        RouteLookup::MethodNotAllowed(_)
    ));
}

#[test]
fn trailing_slash_is_strict_by_default() {
    let app = compile![RouteDecl::new(Method::GET, "/a", 0)];
    assert!(matches!(
        app.route(&Method::GET, "/a/"),
        RouteLookup::NotFound
    ));
    let app = compile![RouteDecl::new(Method::GET, "/a/", 0)];
    assert!(matches!(
        app.route(&Method::GET, "/a"),
        RouteLookup::NotFound
    ));
}

#[test]
fn trailing_slash_redirect() {
    let mut decl = AppDecl::new();
    decl.route(RouteDecl::new(Method::GET, "/a", 0));
    let app = CompiledApp::compile(decl.trailing_slash_policy(TrailingSlash::Redirect)).unwrap();
    match app.route(&Method::GET, "/a/") {
        RouteLookup::Redirect(target) => assert_eq!(target, "/a"),
        other => panic!("expected a redirect, got {other:?}"),
    }
    // ...and in the opposite direction.
    let mut decl = AppDecl::new();
    decl.route(RouteDecl::new(Method::GET, "/b/", 0));
    let app = CompiledApp::compile(decl.trailing_slash_policy(TrailingSlash::Redirect)).unwrap();
    match app.route(&Method::GET, "/b") {
        RouteLookup::Redirect(target) => assert_eq!(target, "/b/"),
        other => panic!("expected a redirect, got {other:?}"),
    }
}

#[test]
fn trailing_slash_ignore() {
    let mut decl = AppDecl::new();
    decl.route(RouteDecl::new(Method::GET, "/a/{x}", 0).param("x", "int"));
    let app = CompiledApp::compile(decl.trailing_slash_policy(TrailingSlash::Ignore)).unwrap();
    let hit = must_hit!(app, Method::GET, "/a/5/");
    assert_eq!(hit.param("x").and_then(PathValue::as_int), Some(5));
    // HEAD via GET fallback still works across the slash boundary.
    assert!(must_hit!(app, Method::HEAD, "/a/5/").implicit_head);
}

#[test]
fn mounted_group_joins_exactly() {
    let mut api = AppDecl::with_prefix("/api/v1").unwrap();
    api.route(RouteDecl::new(Method::GET, "/me", 0));
    api.route(RouteDecl::new(Method::GET, "/users/{id}", 1).param("id", "int"));

    let mut app = AppDecl::new();
    app.route(RouteDecl::new(Method::GET, "/health", 2));
    app.mount(api);
    let app = CompiledApp::compile(app).unwrap();

    let me = must_hit!(app, Method::GET, "/api/v1/me");
    assert_eq!(app.route_info(me.route).handler_index(), 0);
    let hit = must_hit!(app, Method::GET, "/api/v1/users/7");
    assert_eq!(app.route_info(hit.route).handler_index(), 1);
    assert_eq!(hit.param("id").and_then(PathValue::as_int), Some(7));
    let health = must_hit!(app, Method::GET, "/health");
    assert_eq!(app.route_info(health.route).handler_index(), 2);

    // No doubled or missing slashes anywhere.
    assert!(matches!(
        app.route(&Method::GET, "/api//v1/me"),
        RouteLookup::NotFound | RouteLookup::MethodNotAllowed(_)
    ));
    assert!(matches!(
        app.route(&Method::GET, "/apiv1/me"),
        RouteLookup::NotFound
    ));
}

#[test]
fn app_prefix_applies_at_compile() {
    let mut api = AppDecl::with_prefix("/api").unwrap();
    api.route(RouteDecl::new(Method::GET, "/x", 0));
    let app = CompiledApp::compile(api).unwrap();
    assert_eq!(must_hit!(app, Method::GET, "/api/x").route.index(), 0);
    assert!(matches!(
        app.route(&Method::GET, "/x"),
        RouteLookup::NotFound
    ));
}

#[test]
fn encoded_slash_never_splits_segments() {
    // §9.2: decoding happens after matching; `%2F` is segment content, not a
    // separator. `/files/a%2Fb` is ONE segment, so the catch-all's sibling
    // parameter route must not match it — and it does not.
    let app = compile![
        RouteDecl::new(Method::GET, "/files/{name}", 0).param("name", "str"),
        RouteDecl::new(Method::GET, "/{*rest}", 1).param("rest", "str"),
    ];
    let hit = must_hit!(app, Method::GET, "/files/a%2Fb");
    assert_eq!(hit.route.index(), 0);
    assert_eq!(hit.param("name").and_then(PathValue::as_str), Some("a/b"));
}

#[test]
fn static_segments_match_raw() {
    // Matching happens on the encoded path: `/users/%6de` does not equal the
    // static segment `me`.
    let app = compile![
        RouteDecl::new(Method::GET, "/users/me", 0),
        RouteDecl::new(Method::GET, "/users/{id}", 1).param("id", "str"),
    ];
    let hit = must_hit!(app, Method::GET, "/users/%6de");
    assert_eq!(hit.route.index(), 1);
    assert_eq!(hit.param("id").and_then(PathValue::as_str), Some("me"));
}

#[test]
fn int_check_applies_to_decoded_value() {
    let app = compile![RouteDecl::new(Method::GET, "/n/{v}", 0).param("v", "int"),];
    // `%35` decodes to `5` before the type check.
    assert_eq!(
        must_hit!(app, Method::GET, "/n/%35")
            .param("v")
            .and_then(PathValue::as_int),
        Some(5)
    );
    // Invalid escapes decode to nothing, so the route does not match.
    assert!(matches!(
        app.route(&Method::GET, "/n/%zz"),
        RouteLookup::NotFound
    ));
}

#[test]
fn catch_all_needs_a_segment() {
    let app = compile![
        RouteDecl::new(Method::GET, "/files/{*rest}", 0).param("rest", "str"),
        RouteDecl::new(Method::GET, "/files", 1),
    ];
    let hit = must_hit!(app, Method::GET, "/files/a/b/c.txt");
    assert_eq!(hit.route.index(), 0);
    assert_eq!(
        hit.param("rest").and_then(PathValue::as_str),
        Some("a/b/c.txt")
    );
    // The catch-all alone does not match `/files` or `/files/`.
    assert_eq!(must_hit!(app, Method::GET, "/files").route.index(), 1);
    assert!(matches!(
        app.route(&Method::GET, "/files/"),
        RouteLookup::NotFound
    ));
}

#[test]
fn extended_methods_work() {
    let mkcol = Method::from_bytes(b"MKCOL").unwrap();
    let app = compile![
        RouteDecl::new(Method::GET, "/r", 0),
        RouteDecl::new(mkcol.clone(), "/r", 1),
    ];
    assert_eq!(must_hit!(app, mkcol, "/r").route.index(), 1);
    // ...and show up in the Allow set for 405s.
    let RouteLookup::MethodNotAllowed(allow) = app.route(&Method::DELETE, "/r") else {
        panic!("expected 405");
    };
    assert!(allow.contains(&Method::from_bytes(b"MKCOL").unwrap()));
    assert_eq!(allow.header(), "GET, HEAD, MKCOL, OPTIONS");
}

#[test]
fn duplicate_routes_are_errors() {
    let mut app = AppDecl::new();
    app.route(RouteDecl::new(Method::GET, "/dup", 0));
    app.route(RouteDecl::new(Method::GET, "/dup", 1));
    // Same path, different method: not a duplicate.
    app.route(RouteDecl::new(Method::POST, "/dup", 2));
    let err = CompiledApp::compile(app).unwrap_err();
    let dups: Vec<_> = err
        .iter()
        .filter(|e| matches!(e, CompileError::DuplicateRoute { .. }))
        .collect();
    assert_eq!(dups.len(), 1);
}

#[test]
fn param_type_mismatches_are_errors() {
    let mut app = AppDecl::new();
    // `a` is typed but not scalar, `b` and `d` are typed but absent from the
    // pattern, `d` is declared twice.
    app.route(
        RouteDecl::new(Method::GET, "/x/{a}", 0)
            .param("a", "list[int]")
            .param("b", "str")
            .param("d", "str")
            .param("d", "int"),
    );
    // `{q}` has no declared type.
    app.route(RouteDecl::new(Method::GET, "/y/{q}", 1));
    let err = CompiledApp::compile(app).unwrap_err();
    let kinds: Vec<&'static str> = err
        .iter()
        .map(|e| match e {
            CompileError::ParamWithoutType { .. } => "missing-type",
            CompileError::TypeWithoutParam { .. } => "no-param",
            CompileError::NonScalarParam { .. } => "non-scalar",
            CompileError::DuplicateParamType { .. } => "dup-type",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds.len(), 5);
    assert!(kinds.contains(&"missing-type"));
    assert!(kinds.contains(&"non-scalar"));
    assert!(kinds.contains(&"dup-type"));
    assert_eq!(kinds.iter().filter(|k| **k == "no-param").count(), 2);
}

#[test]
fn all_problems_are_reported_at_once() {
    let mut app = AppDecl::new();
    app.route(RouteDecl::new(Method::GET, "/ok", 0));
    app.route(RouteDecl::new(Method::GET, "/ok", 1)); // duplicate
    app.route(RouteDecl::new(Method::GET, "/bad/{p}", 2)); // no type for `p`
    app.route(RouteDecl::new(Method::GET, "no-slash", 3)); // bad path
    app.route(RouteDecl::new(Method::GET, "/also/{ok}", 4).param("ok", "str")); // fine
    let err = CompiledApp::compile(app).unwrap_err();
    assert_eq!(err.len(), 3);
    let msg = err.to_string();
    assert!(msg.contains("GET /ok"));
    assert!(msg.contains("GET /bad/{p}"));
    assert!(msg.contains("GET no-slash"));
}

#[test]
fn shadowed_patterns_are_conflicts() {
    let mut app = AppDecl::new();
    app.route(RouteDecl::new(Method::GET, "/a/{x}", 0).param("x", "str"));
    app.route(RouteDecl::new(Method::GET, "/a/{y}", 1).param("y", "str"));
    let err = CompiledApp::compile(app).unwrap_err();
    assert!(
        err.iter()
            .any(|e| matches!(e, CompileError::RouteConflict { .. }))
    );
}

#[test]
fn catch_all_must_be_last() {
    let mut app = AppDecl::new();
    app.route(RouteDecl::new(Method::GET, "/a/{*rest}/b", 0).param("rest", "str"));
    let err = CompiledApp::compile(app).unwrap_err();
    assert!(
        err.iter()
            .any(|e| matches!(e, CompileError::InvalidPattern { .. }))
    );
}

#[test]
fn route_info_exposes_declarations() {
    let app = compile![RouteDecl::new(Method::GET, "/users/{id}", 42).param("id", "int"),];
    let hit = must_hit!(app, Method::GET, "/users/3");
    let info = app.route_info(hit.route);
    assert_eq!(info.method(), &Method::GET);
    assert_eq!(info.path(), "/users/{id}");
    assert_eq!(info.handler_index(), 42);
    assert_eq!(info.params().len(), 1);
    assert_eq!(info.params()[0].name(), "id");
    assert!(!info.params()[0].is_catch_all());
}
