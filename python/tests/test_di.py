"""Dependency-injection tests (DESIGN.md §12, milestone M3.10-M3.13).

Providers declare through `@provider`/`app.provide`/`ctx.add_provider`
and inject into handlers and other providers through `Annotated[T, p]`
markers, plugin handles, or bare provided-type annotations. The graph
validates at compile (cycles, missing providers, scope and async rules
aggregating into `StartupError`); request-scoped values cache on
`request.state`, generator providers clean up after the response, and
`app.override` substitutes declarations without mutating them.
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from collections.abc import AsyncIterator, Callable, Iterator
from typing import Annotated, Any

import pytest
from conftest import fetch
from fulmen import Fulmen, Provider, Request, Response, Scope, StartupError, provider
from fulmen._fulmen import serve_for_test

Serve = Callable[..., str]


def _app() -> Fulmen:
    return Fulmen(openapi_url=None)


def _wait_for(condition: Callable[[], bool], timeout: float = 5.0) -> None:
    """Poll `condition` — post-response work (after_response hooks,
    generator cleanup) races the client read."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(0.01)
    raise AssertionError("condition never became true")


# -- declaration (M3.10) -----------------------------------------------------


def test_provider_decorator_forms() -> None:
    """`@provider` bare is request-scoped; arguments set scope/provides/name."""

    @provider
    def bare() -> int:
        return 1

    @provider(scope=Scope.APP, name="the-answer")
    def configured() -> int:
        return 42

    assert isinstance(bare, Provider)
    assert bare.scope is Scope.REQUEST
    assert configured.scope is Scope.APP
    assert configured.name == "the-answer"
    # A `Provider` delegates to the wrapped function when called.
    assert configured() == 42


def test_provider_scope_accepts_strings_and_rejects_garbage() -> None:
    def fn() -> int:
        return 0

    assert provider(fn, scope="app").scope is Scope.APP
    assert provider(fn, scope="transient").scope is Scope.TRANSIENT
    with pytest.raises(ValueError, match="unknown provider scope"):
        provider(fn, scope="weekly")
    with pytest.raises(TypeError, match="Scope or str"):
        provider(fn, scope=object())


def test_provide_registers_values_and_callables() -> None:
    """`app.provide` accepts `Provider`s, callables, and constant values —
    the return doubles as an `Annotated` marker."""
    app = _app()

    class Settings:
        pass

    settings = Settings()
    registered = app.provide(settings)
    assert isinstance(registered, Provider)
    assert registered.const
    assert registered.scope is Scope.APP

    @app.provide(scope=Scope.APP)
    def pool() -> str:
        return "pool"

    @app.get("/s")
    def show(s: Annotated[Settings, registered]) -> str:
        assert s is settings
        return "ok"

    @app.get("/p")
    def pool_route(p: Annotated[str, pool]) -> str:
        return p

    server = serve_for_test(app._compile_native())
    try:
        assert urllib.request.urlopen(f"http://{server.addr()}/s").read() == b"ok"
        assert urllib.request.urlopen(f"http://{server.addr()}/p").read() == b"pool"
    finally:
        server.stop()


def test_provider_marker_registers_on_first_use(serve: Serve) -> None:
    """`@provider` alone declares — a handler `Annotated` marker makes it
    reachable; no `app.provide` call needed."""
    app = _app()

    @provider
    def greeting() -> str:
        return "hello"

    @app.get("/g")
    def g(text: Annotated[str, greeting]) -> str:
        return text

    assert fetch(f"{serve(app)}/g").body == b"hello"


def test_provider_depends_on_provider(serve: Serve) -> None:
    """Provider parameters recurse: `Annotated` markers resolve through
    the same graph (§12.1)."""
    app = _app()

    @provider(scope=Scope.APP)
    def base() -> str:
        return "base"

    @provider
    def middle(b: Annotated[str, base]) -> str:
        return f"{b}+middle"

    @provider
    def top(m: Annotated[str, middle]) -> str:
        return f"{m}+top"

    @app.get("/t")
    def t(value: Annotated[str, top]) -> str:
        return value

    assert fetch(f"{serve(app)}/t").body == b"base+middle+top"


def test_provider_bare_type_dependency(serve: Serve) -> None:
    """A provider parameter typed `settings: Settings` resolves to the
    registered provider of `Settings` — no marker needed (§12.1)."""
    app = _app()

    class Settings:
        url = "sqlite:///x.db"

    app.provide(Settings())

    @provider
    def repo(settings: Settings) -> str:
        return settings.url

    @app.get("/r")
    def r(url: Annotated[str, repo]) -> str:
        return url

    assert fetch(f"{serve(app)}/r").body == b"sqlite:///x.db"


def test_positional_only_provider_params(serve: Serve) -> None:
    """Positional-only provider parameters resolve positionally."""
    app = _app()

    @provider(scope=Scope.APP)
    def base() -> str:
        return "b"

    @provider
    def inner(b: Annotated[str, base], /) -> str:
        return f"{b}+i"

    @app.get("/i")
    def i(v: Annotated[str, inner]) -> str:
        return v

    assert fetch(f"{serve(app)}/i").body == b"b+i"


def test_handler_mixes_request_wire_and_provider_params(serve: Serve) -> None:
    """DI parameters coexist with `Request`, path and query parameters —
    each keeps its own binding."""
    app = _app()

    @provider
    def tag() -> str:
        return "tagged"

    @app.get("/items/{item_id}")
    def show(
        request: Request,
        item_id: int,
        tag_value: Annotated[str, tag],
        q: str = "?",
    ) -> str:
        return f"{request.method}:{item_id}:{q}:{tag_value}"

    res = fetch(f"{serve(app)}/items/3?q=abc")
    assert res.body == b"GET:3:abc:tagged"


# -- scopes (§12.1) ----------------------------------------------------------


def test_app_scope_resolves_once_at_startup(serve: Serve) -> None:
    """APP providers resolve during startup — before the first request —
    and share one instance with every injection site."""
    app = _app()
    calls: list[str] = []

    @provider(scope=Scope.APP)
    def shared() -> list[str]:
        calls.append("constructed")
        return []

    @provider
    def seen(pool: Annotated[list[str], shared]) -> str:
        pool.append("x")
        return str(len(pool))

    @app.get("/n")
    def n(pool: Annotated[list[str], shared], count: Annotated[str, seen]) -> str:
        return f"{count}:{len(pool)}"

    base = serve(app)
    # The startup hook already constructed it — no request was needed.
    assert calls == ["constructed"]
    assert fetch(f"{base}/n").body == b"1:1"
    assert fetch(f"{base}/n").body == b"2:2"
    assert calls == ["constructed"]


def test_request_scope_caches_within_a_request(serve: Serve) -> None:
    """One REQUEST provider resolves once per request — the handler and a
    dependent provider see the same instance — and again per request."""
    app = _app()
    calls: list[str] = []

    @provider
    def ticket() -> list[str]:
        calls.append("opened")
        return []

    @provider
    def pair(t: Annotated[list[str], ticket]) -> str:
        t.append("a")
        return "pair"

    @app.get("/r")
    def r(t: Annotated[list[str], ticket], p: Annotated[str, pair]) -> str:
        t.append("b")
        return f"{p}:{len(t)}"

    base = serve(app)
    assert fetch(f"{base}/r").body == b"pair:2"
    assert calls == ["opened"]
    assert fetch(f"{base}/r").body == b"pair:2"
    assert calls == ["opened", "opened"]


def test_transient_resolves_at_every_site(serve: Serve) -> None:
    """TRANSIENT providers produce a fresh object per injection — two
    handler parameters and a provider dependency each get their own."""
    app = _app()

    @provider(scope=Scope.TRANSIENT)
    def fresh() -> list[str]:
        return []

    @provider
    def holder(f: Annotated[list[str], fresh]) -> list[str]:
        return f

    @app.get("/t")
    def t(
        a: Annotated[list[str], fresh],
        b: Annotated[list[str], fresh],
        held: Annotated[list[str], holder],
    ) -> str:
        assert a is not b
        assert a is not held
        assert b is not held
        return "distinct"

    assert fetch(f"{serve(app)}/t").body == b"distinct"


def test_generator_providers_clean_up_after_response(serve: Serve) -> None:
    """`yield` providers release in reverse resolution order once the
    response is out — the request-scoped cleanup lane."""
    app = _app()
    events: list[str] = []

    @provider
    def first() -> Iterator[str]:
        events.append("acquire:first")
        try:
            yield "first"
        finally:
            events.append("release:first")

    @provider
    def second(_f: Annotated[str, first]) -> Iterator[str]:
        events.append("acquire:second")
        try:
            yield "second"
        finally:
            events.append("release:second")

    @app.get("/g")
    def g(v: Annotated[str, second]) -> str:
        return v

    res = fetch(f"{serve(app)}/g")
    assert res.body == b"second"
    _wait_for(
        lambda: events == ["acquire:first", "acquire:second", "release:second", "release:first"]
    )


def test_generator_provider_cleans_up_when_the_handler_raises(serve: Serve) -> None:
    """Cleanup runs on the error path too — after_response always fires."""
    app = _app()
    events: list[str] = []

    @provider
    def resource() -> Iterator[str]:
        try:
            yield "res"
        finally:
            events.append("released")

    @app.get("/boom")
    def boom(_r: Annotated[str, resource]) -> str:
        raise ValueError("handler blew up")

    res = fetch(f"{serve(app)}/boom")
    assert res.status == 500
    _wait_for(lambda: events == ["released"])


def test_app_scope_generator_cleans_up_at_shutdown(serve: Serve) -> None:
    """APP generators hold for the app's life; `server.stop()`'s shutdown
    hooks close them (§12.1)."""
    app = _app()
    events: list[str] = []

    @provider(scope=Scope.APP)
    def pool() -> Iterator[str]:
        events.append("open")
        try:
            yield "pool"
        finally:
            events.append("closed")

    @app.get("/p")
    def p(v: Annotated[str, pool]) -> str:
        return v

    server = serve_for_test(app._compile_native())
    assert events == ["open"]
    assert urllib.request.urlopen(f"http://{server.addr()}/p").read() == b"pool"
    server.stop()
    assert events == ["open", "closed"]


def test_routes_without_providers_have_no_di_overhead(serve: Serve) -> None:
    """A plain route stays a plain route — no wrapper, no cleanup hook."""
    app = _app()

    @app.get("/plain")
    def plain() -> str:
        return "plain"

    assert fetch(f"{serve(app)}/plain").body == b"plain"


# -- async providers (§7.4 bridge) ---------------------------------------------


def test_async_provider_in_async_handler(serve: Serve) -> None:
    """`async def` providers run on the asyncio loop; async handlers
    await them inside the same job."""
    app = _app()

    @provider
    async def token() -> str:
        return "tok-1"

    @app.get("/t")
    async def t(tok: Annotated[str, token]) -> str:
        return tok

    assert fetch(f"{serve(app)}/t").body == b"tok-1"


def test_async_app_provider_resolves_at_startup(serve: Serve) -> None:
    """Async APP providers await through the startup lifecycle hook."""
    app = _app()
    calls: list[str] = []

    @provider(scope=Scope.APP)
    async def remote() -> str:
        calls.append("fetched")
        return "config"

    @app.get("/c")
    def c(cfg: Annotated[str, remote]) -> str:
        return cfg

    base = serve(app)
    assert calls == ["fetched"]
    assert fetch(f"{base}/c").body == b"config"


def test_async_generator_provider_cleans_up(serve: Serve) -> None:
    """Async generators close with `aclose()` on the async cleanup lane."""
    app = _app()
    events: list[str] = []

    @provider
    async def stream() -> AsyncIterator[str]:
        try:
            yield "chunk"
        finally:
            events.append("closed")

    @app.get("/s")
    async def s(v: Annotated[str, stream]) -> str:
        return v

    assert fetch(f"{serve(app)}/s").body == b"chunk"
    _wait_for(lambda: events == ["closed"])


def test_sync_handler_cannot_consume_async_provider() -> None:
    """A sync handler reaching an async provider in request context is a
    startup problem — the sync pool cannot await (§12.2)."""
    app = _app()

    @provider
    async def token() -> str:
        return "x"

    @app.get("/t")
    def t(_tok: Annotated[str, token]) -> str:
        return "x"

    with pytest.raises(StartupError, match="sync handler depends on async provider"):
        app._compile_native()


def test_async_provider_under_sync_provider_is_also_a_problem() -> None:
    """The reachability rule applies through providers — a sync provider
    under a sync handler cannot pull an async provider either."""
    app = _app()

    @provider
    async def inner() -> str:
        return "x"

    @provider
    def outer(i: Annotated[str, inner]) -> str:
        return i

    @app.get("/o")
    def o(_v: Annotated[str, outer]) -> str:
        return "x"

    with pytest.raises(StartupError, match="sync handler depends on async provider"):
        app._compile_native()


# -- native providers (M3.12): `Request` and request.state --------------------


def test_provider_receives_the_request_object(serve: Serve) -> None:
    """A provider parameter annotated `Request` injects the request —
    the same object hooks see, resolved natively (§12.1)."""
    app = _app()

    @provider
    def agent(request: Request) -> str:
        return request.headers.get("user-agent", "none")

    @app.get("/ua")
    def ua(a: Annotated[str, agent]) -> str:
        return a

    res = fetch(f"{serve(app)}/ua", headers={"User-Agent": "fulmen-test"})
    assert res.body == b"fulmen-test"


def test_request_scope_state_shared_with_hooks(serve: Serve) -> None:
    """The provider's `request` is the shared object — a `resolve`-phase
    hook's dict return lands in `request.state` first."""
    app = _app()

    @app.resolve
    def seed(_request: Request) -> dict[str, Any]:
        return {"seeded": "by-hook"}

    @provider
    def read_state(request: Request) -> str:
        return str(request.state.get("seeded"))

    @app.get("/s")
    def s(v: Annotated[str, read_state]) -> str:
        return v

    assert fetch(f"{serve(app)}/s").body == b"by-hook"


# -- graph validation (M3.11) --------------------------------------------------


def test_missing_provider_marker_is_a_startup_error() -> None:
    """`Annotated[T, fn]` for an unregistered `fn` names the marker and
    the parameter in one startup problem."""
    app = _app()

    def not_a_provider() -> str:
        return "x"

    @app.get("/m")
    def m(_v: Annotated[str, not_a_provider]) -> str:
        return "x"

    with pytest.raises(StartupError, match="is not a registered provider"):
        app._compile_native()


def test_missing_bare_type_dependency_is_a_startup_error() -> None:
    """A provider parameter typed `x: Missing` with no provider producing
    `Missing` aggregates into `StartupError` at compile."""
    app = _app()

    class Missing:
        pass

    @provider
    def repo(_m: Missing) -> str:
        return "x"

    @app.get("/r")
    def r(_v: Annotated[str, repo]) -> str:
        return "x"

    with pytest.raises(StartupError, match="no provider produces"):
        app._compile_native()


def test_ambiguous_bare_type_is_a_startup_error() -> None:
    """Two providers of the same type make bare-type lookup ambiguous —
    `Annotated` disambiguates."""
    app = _app()

    class Cache:
        pass

    app.provide(Cache())
    app.provide(Cache())

    @provider
    def repo(_c: Cache) -> str:
        return "x"

    @app.get("/r")
    def r(_v: Annotated[str, repo]) -> str:
        return "x"

    with pytest.raises(StartupError, match=r"ambiguous|Annotated"):
        app._compile_native()


def test_cycle_is_a_startup_error() -> None:
    """Mutual provider dependencies aggregate into a `StartupError`
    naming the cycle."""
    app = _app()

    @provider
    def a(_b: Annotated[str, b]) -> str:
        return "a"

    @provider
    def b(_a: Annotated[str, a]) -> str:
        return "b"

    @app.get("/c")
    def c(_v: Annotated[str, a]) -> str:
        return "x"

    with pytest.raises(StartupError, match="cycle"):
        app._compile_native()


def test_self_cycle_is_a_startup_error() -> None:
    app = _app()

    @provider
    def snake(_s: Annotated[str, snake]) -> str:
        return "x"

    @app.get("/s")
    def s(_v: Annotated[str, snake]) -> str:
        return "x"

    with pytest.raises(StartupError, match="cycle"):
        app._compile_native()


def test_app_scope_cannot_depend_on_request_scope() -> None:
    """An APP provider consuming a REQUEST provider could never resolve —
    the request does not exist at startup (§12.2)."""
    app = _app()

    @provider
    def per_request() -> str:
        return "x"

    @provider(scope=Scope.APP)
    def singleton(_v: Annotated[str, per_request]) -> str:
        return "x"

    @app.get("/s")
    def s(_v: Annotated[str, singleton]) -> str:
        return "x"

    with pytest.raises(StartupError, match="request-scope"):
        app._compile_native()


def test_app_scope_cannot_depend_on_request() -> None:
    """`request` is a request-context value — no APP provider may take it."""
    app = _app()

    @provider(scope=Scope.APP)
    def bad(_r: Request) -> str:
        return "x"

    @app.get("/b")
    def b(_v: Annotated[str, bad]) -> str:
        return "x"

    with pytest.raises(StartupError, match="depends on `request`"):
        app._compile_native()


def test_transient_under_app_cannot_pull_request_scoped_deps() -> None:
    """TRANSIENT providers resolve inside their consumer's context — one
    under an APP provider inherits the startup restriction."""
    app = _app()

    @provider
    def per_request() -> str:
        return "x"

    @provider(scope=Scope.TRANSIENT)
    def middle(_v: Annotated[str, per_request]) -> str:
        return "x"

    @provider(scope=Scope.APP)
    def top(_m: Annotated[str, middle]) -> str:
        return "x"

    @app.get("/t")
    def t(_v: Annotated[str, top]) -> str:
        return "x"

    with pytest.raises(StartupError, match="request-scope"):
        app._compile_native()


def test_app_scope_may_depend_on_app_and_transient() -> None:
    """APP → APP/TRANSIENT is legal: every dependency exists at startup."""
    app = _app()

    @provider(scope=Scope.TRANSIENT)
    def fresh() -> str:
        return "t"

    @provider(scope=Scope.APP)
    def top(_f: Annotated[str, fresh]) -> str:
        return "ok"

    @app.get("/t")
    def t(_v: Annotated[str, top]) -> str:
        return "x"

    app._compile_native()


def test_variadic_provider_parameter_is_a_startup_error() -> None:
    app = _app()

    @provider
    def greedy(*_args: str) -> str:
        return "x"

    @app.get("/g")
    def g(_v: Annotated[str, greedy]) -> str:
        return "x"

    with pytest.raises(StartupError, match="variadic"):
        app._compile_native()


def test_provider_problems_aggregate() -> None:
    """Several broken declarations list as several problems in one
    `StartupError` — nothing defers to the first request (§14.1)."""
    app = _app()

    def ghost() -> str:
        return "x"

    @app.get("/a")
    def a(_v: Annotated[str, ghost]) -> str:
        return "x"

    @app.get("/b")
    def b(_v: Annotated[str, ghost]) -> str:
        return "x"

    with pytest.raises(StartupError) as caught:
        app._compile_native()
    assert str(caught.value).count("\n  - ") >= 2


# -- failure routing ----------------------------------------------------------


class DbDownError(Exception):
    pass


def test_provider_failure_routes_to_on_error(serve: Serve) -> None:
    """A provider raising mid-request is a handler exception — the
    route's `on_error` table renders it (§12.1, §17)."""
    app = _app()

    @provider
    def flaky() -> str:
        raise DbDownError("connection refused")

    @app.get("/d")
    def d(_v: Annotated[str, flaky]) -> str:
        return "x"

    @app.on_error(DbDownError)
    def on_down(exc: DbDownError) -> Response:
        return Response({"detail": str(exc)}, status=503)

    res = fetch(f"{serve(app)}/d")
    assert res.status == 503
    assert json.loads(res.body) == {"detail": "connection refused"}


def test_unhandled_provider_failure_is_a_500(serve: Serve) -> None:
    app = _app()

    @provider
    def flaky() -> str:
        raise RuntimeError("kaput")

    @app.get("/f")
    def f(_v: Annotated[str, flaky]) -> str:
        return "x"

    res = fetch(f"{serve(app)}/f")
    assert res.status == 500
    assert res.header("Content-Type") == "application/problem+json"


def test_app_provider_failure_fails_startup() -> None:
    """An APP provider raising at startup aborts the serve — the app
    never starts half-initialized."""
    app = _app()

    @provider(scope=Scope.APP)
    def broken() -> str:
        raise DbDownError("no database")

    @app.get("/b")
    def b(_v: Annotated[str, broken]) -> str:
        return "x"

    with pytest.raises(DbDownError, match="no database"):
        serve_for_test(app._compile_native())


def test_generator_provider_must_yield(serve: Serve) -> None:
    """A generator provider that yields nothing fails the resolution —
    surfaced as a handler error at request time."""
    app = _app()

    @provider
    def empty() -> Iterator[str]:
        return
        yield  # pragma: no cover -- makes this a generator

    @app.get("/e")
    def e(_v: Annotated[str, empty]) -> str:
        return "x"

    res = fetch(f"{serve(app)}/e")
    assert res.status == 500


# -- plugin providers (§13.1 seam) ---------------------------------------------


class _DbPlugin:
    """Plugin declaring a provider through `ctx.add_provider`."""

    name = "db-plugin"
    version = "1.0"

    def setup(self, ctx):
        @provider(scope=Scope.APP)
        def conn() -> str:
            return "plugin-conn"

        ctx.add_provider(conn)


def test_plugin_provider_injects_via_handle(serve: Serve) -> None:
    """`ctx.add_provider` feeds the shared graph — the plugin's handle is
    the `Annotated` marker to reach it (§12.1, §13.1)."""
    app = _app()
    plugin = app.use(_DbPlugin())

    @app.get("/db")
    def db(c: Annotated[str, plugin]) -> str:
        return c

    assert fetch(f"{serve(app)}/db").body == b"plugin-conn"


def test_plugin_provider_usable_by_other_providers(serve: Serve) -> None:
    """Plugin providers resolve for app providers too — one graph."""
    app = _app()
    plugin = app.use(_DbPlugin())

    @provider
    def repo(c: Annotated[str, plugin]) -> str:
        return f"repo({c})"

    @app.get("/r")
    def r(v: Annotated[str, repo]) -> str:
        return v

    assert fetch(f"{serve(app)}/r").body == b"repo(plugin-conn)"


def test_plugin_provider_validates_with_the_graph() -> None:
    """A plugin provider's missing dependency fails startup just like an
    app declaration's would — annotations resolve against `setup()`'s
    own locals."""
    app = _app()

    class Plugin:
        name = "needy"
        version = "1.0"

        def setup(self, ctx):
            class Missing:
                pass

            @provider
            def needy(_m: Missing) -> str:
                return "x"

            ctx.add_provider(needy)

    plugin = app.use(Plugin())

    @app.get("/n")
    def n(_v: Annotated[str, plugin]) -> str:
        return "x"

    with pytest.raises(StartupError, match="no provider produces"):
        app._compile_native()


def test_handle_marker_for_a_providerless_plugin_is_a_problem() -> None:
    app = _app()

    class Empty:
        name = "empty"
        version = "1.0"

        def setup(self, ctx):
            return None

    plugin = app.use(Empty())

    @app.get("/e")
    def e(_v: Annotated[str, plugin]) -> str:
        return "x"

    with pytest.raises(StartupError, match="declares no providers"):
        app._compile_native()


def test_handle_marker_for_an_unused_plugin_is_a_problem() -> None:
    app = _app()
    handle = _plugins_handle_only()

    @app.get("/u")
    def u(_v: Annotated[str, handle]) -> str:
        return "x"

    with pytest.raises(StartupError, match="not used by this application"):
        app._compile_native()


def _plugins_handle_only():
    """A `PluginHandle` for a plugin `use()` was never called on."""
    from fulmen.plugins import PluginHandle

    class Ghost:
        name = "ghost"
        version = "1.0"

        def setup(self, ctx):
            return None

    return PluginHandle(Ghost(), {})


# -- overrides (M3.13) ----------------------------------------------------------


def test_override_substitutes_the_provider(serve: Serve) -> None:
    """`app.override` makes every injection site resolve the replacement —
    the app itself is untouched (§12.3)."""
    app = _app()

    @provider
    def db() -> str:
        return "real"

    @app.get("/d")
    def d(v: Annotated[str, db]) -> str:
        return v

    app.override(db, lambda: "fake")
    assert fetch(f"{serve(app)}/d").body == b"fake"


def test_override_reaches_dependent_providers(serve: Serve) -> None:
    """Providers that depend on the overridden provider resolve the
    replacement — the graph stays consistent (§12.3)."""
    app = _app()

    @provider(scope=Scope.APP)
    def base() -> str:
        return "real-base"

    @provider
    def consumer(b: Annotated[str, base]) -> str:
        return f"consumer({b})"

    @app.get("/c")
    def c(v: Annotated[str, consumer]) -> str:
        return v

    app.override(base, lambda: "stub-base")
    assert fetch(f"{serve(app)}/c").body == b"consumer(stub-base)"


def test_override_does_not_mutate_the_provider() -> None:
    """The `Provider` object and the `app.provide` list survive the
    override — a second compile sees the original declaration."""
    app = _app()

    @provider
    def db() -> str:
        return "real"

    @app.get("/d")
    def d(v: Annotated[str, db]) -> str:
        return v

    with app.override(db, lambda: "fake"):
        native = app._compile_native()
        server = serve_for_test(native)
        assert urllib.request.urlopen(f"http://{server.addr()}/d").read() == b"fake"
        server.stop()

    # Override reverted: the original declaration serves again.
    server = serve_for_test(app._compile_native())
    assert urllib.request.urlopen(f"http://{server.addr()}/d").read() == b"real"
    server.stop()


def test_override_of_an_unregistered_target_is_a_problem() -> None:
    """Overriding something the graph never saw names the mistake."""
    app = _app()

    def unknown() -> str:
        return "x"

    app.override(unknown, lambda: "y")

    @app.get("/x")
    def x() -> str:
        return "x"

    with pytest.raises(StartupError, match="not a registered provider"):
        app._compile_native()


def test_override_participates_in_validation() -> None:
    """An override's own dependencies validate — a replacement depending
    on a missing provider is a startup problem (§12.3)."""
    app = _app()

    class Missing:
        pass

    @provider
    def db() -> str:
        return "real"

    def replacement(_m: Missing) -> str:
        return "fake"

    app.override(db, replacement)

    @app.get("/d")
    def d(_v: Annotated[str, db]) -> str:
        return "x"

    with pytest.raises(StartupError, match="no provider produces"):
        app._compile_native()


def test_override_keeps_the_scope_contract(serve: Serve) -> None:
    """A request-scoped provider overridden by a constant still resolves
    — the replacement keeps the original's scope (§12.3)."""
    app = _app()

    @provider
    def ticket() -> list[str]:
        return []

    @app.get("/t")
    def t(_v: Annotated[list[str], ticket]) -> str:
        return "ok"

    app.override(ticket, ["fixed"])
    assert fetch(f"{serve(app)}/t").body == b"ok"


# -- OpenAPI exclusion ---------------------------------------------------------


def test_provider_parameters_do_not_reach_openapi() -> None:
    """DI parameters are framework-injected — the generated document
    carries only wire parameters (§11.3)."""
    app = Fulmen(title="t", version="0")

    @provider
    def db() -> str:
        return "x"

    @app.get("/items/{item_id}")
    def show(item_id: int, q: str, _db: Annotated[str, db]) -> str:
        return "x"

    document = app.openapi()
    params = document["paths"]["/items/{item_id}"]["get"]["parameters"]
    names = {p["name"] for p in params}
    assert names == {"item_id", "q"}


# -- startup hook ordering -------------------------------------------------------


def test_app_providers_resolve_before_startup_hooks(serve: Serve) -> None:
    """`on_startup` hooks see resolved APP providers — the synthetic DI
    hook runs first (§12.1)."""
    app = _app()
    events: list[str] = []

    @provider(scope=Scope.APP)
    def config() -> str:
        events.append("provider")
        return "cfg"

    @app.on_startup
    def check() -> None:
        events.append("startup")

    @app.get("/c")
    def c(_v: Annotated[str, config]) -> str:
        return "x"

    serve(app)
    assert events == ["provider", "startup"]
