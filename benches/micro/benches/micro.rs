//! Criterion micro-benchmarks for Fulmen (`docs/DESIGN.md` §25.3,
//! `docs/ROADMAP.md` M1.24).
//!
//! Groups:
//!
//! - `router_match/<table>/<case>` — `CompiledApp::route` latency on a
//!   20-route and a 200-route table.
//! - `router_compile/routes/<n>` — `CompiledApp::compile` cold-start cost
//!   for 50, 200 and 1000 routes.
//! - `dispatch/...` — the pure-Rust dispatcher ceiling that M1.27
//!   compares Python dispatch against: `Response` construction plus
//!   `TestDispatcher` requests awaited on a tokio runtime.
//!
//! Run with `cargo bench -p fulmen-bench`; see `benches/micro/README.md`.

use std::hint::black_box;
use std::net::SocketAddr;

use bytes::Bytes;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use fulmen_core::{AppDecl, CompiledApp, RouteDecl};
use fulmen_server::testing::TestDispatcher;
use fulmen_server::{Dispatcher, Request, Response};
use http::{HeaderMap, Method, StatusCode, Uri};
use tokio::runtime::Runtime;

/// Compile `decls`, aborting the bench run on an invalid fixture.
#[expect(
    clippy::unwrap_used,
    reason = "bench fixtures are valid by construction; a compile error is a fixture bug that must abort the run"
)]
fn compiled(decls: AppDecl) -> CompiledApp {
    CompiledApp::compile(decls).unwrap()
}

/// Push `decl` onto `app`, giving it the handler index that matches its
/// position in the route table.
fn add(app: &mut AppDecl, mut decl: RouteDecl) {
    decl.handler_index = u32::try_from(app.declarations().len()).unwrap_or(u32::MAX);
    app.route(decl);
}

/// The 20-route table: statics, typed `{param}` segments, a nested
/// `{int}` route and two `{*catch-all}`s, like a small real app.
fn small_app() -> AppDecl {
    let mut app = AppDecl::new();
    for path in ["/", "/health", "/metrics", "/version", "/users", "/search"] {
        add(&mut app, RouteDecl::new(Method::GET, path, 0));
    }
    add(&mut app, RouteDecl::new(Method::POST, "/users", 0));
    add(&mut app, RouteDecl::new(Method::GET, "/users/me", 0));
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/users/{id}", 0).param("id", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/users/{id}/posts", 0).param("id", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/users/{id}/posts/{post}", 0)
            .param("id", "int")
            .param("post", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/items/{id}", 0).param("id", "int"),
    );
    add(&mut app, RouteDecl::new(Method::POST, "/items", 0));
    add(
        &mut app,
        RouteDecl::new(Method::PUT, "/items/{id}", 0).param("id", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::DELETE, "/items/{id}", 0).param("id", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/tags/{tag}", 0).param("tag", "str"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/files/{*path}", 0).param("path", "str"),
    );
    add(&mut app, RouteDecl::new(Method::GET, "/api/v1/status", 0));
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/api/v1/users/{user}/posts/{post}", 0)
            .param("user", "int")
            .param("post", "int"),
    );
    add(
        &mut app,
        RouteDecl::new(Method::GET, "/docs/{*page}", 0).param("page", "str"),
    );
    app
}

/// The 200-route table: the same shape scaled up, spread over a few
/// prefixes and methods.
fn large_app() -> AppDecl {
    let mut app = AppDecl::new();
    // 40 resources x (GET list + GET item) = 80.
    for i in 0..40 {
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("/api/v1/resource{i}"), 0),
        );
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("/api/v1/resource{i}/{{id}}"), 0)
                .param("id", "int"),
        );
    }
    // POST on the same 40 resources = 40 (120 total).
    for i in 0..40 {
        add(
            &mut app,
            RouteDecl::new(Method::POST, format!("/api/v1/resource{i}"), 0),
        );
    }
    // 20 things x (GET static + GET str param) = 40 (160 total).
    for i in 0..20 {
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("/api/v2/thing{i}"), 0),
        );
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("/api/v2/thing{i}/{{slug}}"), 0)
                .param("slug", "str"),
        );
    }
    // PUT + DELETE on 4 resources = 8 (168 total).
    for i in 0..4 {
        add(
            &mut app,
            RouteDecl::new(Method::PUT, format!("/api/v1/resource{i}/{{id}}"), 0)
                .param("id", "int"),
        );
        add(
            &mut app,
            RouteDecl::new(Method::DELETE, format!("/api/v1/resource{i}/{{id}}"), 0)
                .param("id", "int"),
        );
    }
    // Statics = 8 (176 total).
    for path in [
        "/",
        "/health",
        "/metrics",
        "/version",
        "/search",
        "/docs",
        "/admin",
        "/admin/users",
    ] {
        add(&mut app, RouteDecl::new(Method::GET, path, 0));
    }
    // Web sections = 20 (196 total).
    for i in 0..20 {
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("/web/section{i}/page"), 0),
        );
    }
    // Catch-all routes = 3 (199 total).
    for root in ["/files", "/static", "/cdn"] {
        add(
            &mut app,
            RouteDecl::new(Method::GET, format!("{root}/{{*rest}}"), 0).param("rest", "str"),
        );
    }
    // Deepest route = 1 (200 total).
    add(
        &mut app,
        RouteDecl::new(
            Method::GET,
            "/api/v1/orgs/{org}/teams/{team}/members/{member}",
            0,
        )
        .param("org", "int")
        .param("team", "int")
        .param("member", "int"),
    );
    app
}

/// `n` distinct routes for the compile bench: a realistic mix of
/// statics, `{param}` segments and `{*catch-all}`s.
fn compile_fixture(n: usize) -> AppDecl {
    let mut app = AppDecl::new();
    for i in 0..n {
        match i % 4 {
            0 => add(
                &mut app,
                RouteDecl::new(Method::GET, format!("/static/{i}"), 0),
            ),
            1 => add(
                &mut app,
                RouteDecl::new(Method::GET, format!("/param/{i}/{{id}}"), 0).param("id", "int"),
            ),
            2 => add(
                &mut app,
                RouteDecl::new(Method::POST, format!("/deep/a/{i}/b"), 0),
            ),
            _ => add(
                &mut app,
                RouteDecl::new(Method::GET, format!("/svc/{i}/{{*rest}}"), 0).param("rest", "str"),
            ),
        }
    }
    app
}

/// A `GET /health` exists in both tables; `DELETE` on it exercises the
/// `405` / `Allow` scan.
fn small_cases() -> [(&'static str, Method, &'static str); 7] {
    [
        ("static_hit", Method::GET, "/users/me"),
        ("int_param_hit", Method::GET, "/users/42"),
        ("catch_all_hit", Method::GET, "/files/a/b/c.txt"),
        ("deepest_hit", Method::GET, "/api/v1/users/42/posts/7"),
        ("miss_404", Method::GET, "/no/such/path"),
        ("percent_encoded", Method::GET, "/tags/caf%C3%A9"),
        ("method_not_allowed", Method::DELETE, "/health"),
    ]
}

/// Same cases as [`small_cases`], against the 200-route table.
fn large_cases() -> [(&'static str, Method, &'static str); 7] {
    [
        ("static_hit", Method::GET, "/api/v1/resource20"),
        ("int_param_hit", Method::GET, "/api/v1/resource20/123"),
        ("catch_all_hit", Method::GET, "/cdn/assets/app/main.css"),
        (
            "deepest_hit",
            Method::GET,
            "/api/v1/orgs/1/teams/2/members/3",
        ),
        ("miss_404", Method::GET, "/api/v1/missing/path/segments"),
        ("percent_encoded", Method::GET, "/api/v2/thing7/caf%C3%A9"),
        ("method_not_allowed", Method::DELETE, "/health"),
    ]
}

/// Single-threaded runtime for the dispatch benches: no I/O happens, so
/// a current-thread loop keeps `block_on` cheap and repeatable.
#[expect(
    clippy::unwrap_used,
    reason = "building a runtime cannot fail short of OS resource exhaustion; a bench may abort"
)]
fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A wire request for [`TestDispatcher`]; the target is a literal so
/// `Uri::from_static` cannot fail.
fn request(method: Method, target: &'static str, body: Bytes) -> Request {
    Request {
        method,
        uri: Uri::from_static(target),
        headers: HeaderMap::new(),
        body,
        remote_addr: SocketAddr::from(([127, 0, 0, 1], 1)),
    }
}

/// A 1 KiB request body for the echo dispatch bench.
fn body_1kib() -> Bytes {
    Bytes::from_static(&[b'x'; 1024])
}

/// `CompiledApp::route` latency across match kinds and table sizes.
fn router_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("router_match");
    let tables = [
        ("small_20", compiled(small_app()), small_cases()),
        ("large_200", compiled(large_app()), large_cases()),
    ];
    for (table, app, cases) in tables {
        for (case, method, path) in cases {
            group.bench_with_input(
                BenchmarkId::new(table, case),
                &(method, path),
                |b, (m, p)| {
                    b.iter(|| app.route(black_box(m), black_box(*p)));
                },
            );
        }
    }
    group.finish();
}

/// `CompiledApp::compile` cold-start cost. The 200-route number is
/// checked against the 300 ms startup target (`docs/DESIGN.md` §25.1).
fn router_compile(c: &mut Criterion) {
    let mut group = c.benchmark_group("router_compile");
    for n in [50usize, 200, 1000] {
        group.bench_with_input(BenchmarkId::new("routes", n), &n, |b, n| {
            b.iter_batched(
                || compile_fixture(*n),
                |decls| black_box(CompiledApp::compile(decls)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// The pure-Rust ceiling: `Response` construction and `TestDispatcher`
/// requests dispatched through a tokio runtime, no sockets involved.
fn dispatch(c: &mut Criterion) {
    let rt = runtime();
    let dispatcher = TestDispatcher::hello_world().route_echo(Method::POST, "/echo");
    let mut group = c.benchmark_group("dispatch");

    for (case, method, path, body) in [
        ("plaintext", Method::GET, "/plaintext", Bytes::new()),
        ("json", Method::GET, "/json", Bytes::new()),
        ("not_found", Method::GET, "/missing", Bytes::new()),
        ("echo_1kib", Method::POST, "/echo", body_1kib()),
    ] {
        group.bench_function(format!("block_on/{case}"), |b| {
            b.iter_batched(
                || request(method.clone(), path, body.clone()),
                |req| rt.block_on(dispatcher.dispatch(black_box(req))),
                BatchSize::SmallInput,
            );
        });
    }

    // The server's real per-request shape: dispatch inside its own task
    // (`docs/DESIGN.md` §14.6), so spawn + join overhead is included.
    group.bench_function("spawn/plaintext", |b| {
        let dispatcher = &dispatcher;
        b.iter_batched(
            || request(Method::GET, "/plaintext", Bytes::new()),
            |req| rt.block_on(async move { tokio::task::spawn(dispatcher.dispatch(req)).await }),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("response/text", |b| {
        b.iter(|| Response::text(black_box("hello world")));
    });
    group.bench_function("response/json", |b| {
        b.iter(|| Response::json(black_box(r#"{"hello":"world"}"#)));
    });
    group.bench_function("response/bytes_1kib", |b| {
        b.iter(|| Response::bytes(black_box(body_1kib())));
    });
    group.bench_function("response/status_404", |b| {
        b.iter(|| Response::status(black_box(StatusCode::NOT_FOUND)));
    });
    group.finish();
}

/// A plugin whose `on_request` returns `Continue` untouched — the pure
/// cross-ABI call cost the M3 exit bar measures.
#[derive(Debug)]
struct NoopPlugin;

impl fulmen_plugin_sdk::FulmenPlugin for NoopPlugin {
    const NAME: &'static str = "bench-noop";
    const VERSION: &'static str = "0.0.0";

    fn setup(_config: &[u8]) -> Result<Self, fulmen_plugin_sdk::PluginError> {
        Ok(Self)
    }

    fn on_request(
        &self,
        _cx: &fulmen_plugin_sdk::RequestView<'_>,
    ) -> Result<fulmen_plugin_sdk::Decision, fulmen_plugin_sdk::PluginError> {
        Ok(fulmen_plugin_sdk::Decision::Continue)
    }
}

/// Load `descriptor`'s plugin through the real host path; a bench
/// fixture failure is a fixture bug that must abort the run.
#[expect(
    clippy::unwrap_used,
    reason = "bench descriptors are valid by construction; a load failure is a fixture bug that must abort the run"
)]
fn load_plugin(
    descriptor: &fulmen_plugin_sdk::FulmenPluginDescriptorV1,
) -> fulmen_plugin_sdk::host::LoadedPlugin {
    let plugin =
        fulmen_plugin_sdk::host::LoadedPlugin::load(std::ptr::NonNull::from(descriptor)).unwrap();
    plugin.create(b"{}").unwrap();
    plugin
}

/// The M3 exit bar (docs/DESIGN.md §13.4, ROADMAP M3): a native plugin
/// hook must cost under 1µs per request. `LoadedPlugin::run_hook` covers
/// the entire marginal cost — `HookArgs`/`FfiHookCxV1` assembly, the
/// `catch_unwind` boundary, the indirect `extern "C"` call and outcome
/// decoding — measured for a no-op `on_request` (pure overhead) and the
/// example plugin's real `on_response` (a header read, an atomic counter
/// bump and a header set).
fn native_plugin(c: &mut Criterion) {
    use fulmen_plugin_sdk::Phase;
    use fulmen_plugin_sdk::host::{HookArgs, ResponseMut};

    let remote = SocketAddr::from(([10, 0, 0, 1], 44123));
    let mut request_headers = HeaderMap::new();
    request_headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("bench/1"),
    );
    let method = Method::GET;
    let path = "/bench";

    let noop_desc = fulmen_plugin_sdk::describe::<NoopPlugin>(&[Phase::OnRequest]);
    let noop = load_plugin(&noop_desc);
    let rid_desc =
        fulmen_plugin_sdk::describe::<fulmen_example_plugin::RequestIdPlugin>(&[Phase::OnResponse]);
    let request_id = load_plugin(&rid_desc);

    let mut group = c.benchmark_group("native_plugin");
    group.bench_function("run_hook/on_request_continue", |b| {
        let body = Bytes::new();
        b.iter(|| {
            let mut args = HookArgs {
                method: method.as_str(),
                path,
                query: Some("q=1"),
                headers: &request_headers,
                body: &body,
                remote_addr: remote,
                response: None,
            };
            black_box(noop.run_hook(Phase::OnRequest.index(), black_box(&mut args)))
        });
    });
    group.bench_function("run_hook/on_response_request_id", |b| {
        let body = Bytes::new();
        b.iter_batched(
            || (StatusCode::OK, HeaderMap::new(), Bytes::new()),
            |(mut status, mut headers, mut res_body)| {
                let mut args = HookArgs {
                    method: method.as_str(),
                    path,
                    query: None,
                    headers: &request_headers,
                    body: &body,
                    remote_addr: remote,
                    response: Some(ResponseMut {
                        status: &mut status,
                        headers: &mut headers,
                        body: &mut res_body,
                    }),
                };
                black_box(request_id.run_hook(Phase::OnResponse.index(), black_box(&mut args)))
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = router_match, router_compile, dispatch, native_plugin
}
criterion_main!(benches);
