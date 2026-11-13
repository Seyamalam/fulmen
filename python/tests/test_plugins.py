"""Plugin-system tests (DESIGN.md §13, milestone M3.5-M3.9).

`use()`/handle semantics, dependency ordering, requires/conflicts/cycle
failures, configuration slices, every `PluginContext` extension point,
group scoping, and post-compile sealing (§14.8).
"""

from __future__ import annotations

import pytest
from conftest import fetch
from fulmen import Created, Fulmen, JSONResponse, Request, StartupError
from fulmen.cli import main


class Probe:
    """Toy plugin recording setup order and its `ctx` for assertions."""

    def __init__(
        self,
        name: str,
        order: list[str] | None = None,
        *,
        requires: list[str] | None = None,
        conflicts: list[str] | None = None,
        scope: str = "any",
    ) -> None:
        self.name = name
        self.version = "1.0"
        self.requires = requires or []
        self.conflicts = conflicts or []
        self.scope = scope
        self._order = order
        self.ctx = None

    def setup(self, ctx):
        self.ctx = ctx
        if self._order is not None:
            self._order.append(self.name)
        return {"plugin": self.name}


def _app() -> Fulmen:
    return Fulmen(openapi_url=None)


# -- use() and the typed handle (M3.5) -------------------------------------------


def test_use_returns_a_typed_handle() -> None:
    """`use()` returns the handle; `state` is setup()'s return, post-compile."""
    app = _app()
    probe = Probe("probe")
    handle = app.use(probe)
    assert handle.name == "probe"
    assert handle.version == "1.0"
    assert handle.plugin is probe
    with pytest.raises(RuntimeError, match="has not been set up"):
        _ = handle.state
    app._compile_native()
    assert handle.state == {"plugin": "probe"}


def test_handle_forwards_plugin_attributes() -> None:
    """Class-level plugin surfaces resolve through the handle at decoration
    time — `Annotated[auth.Claims, auth]` (§5.6)."""

    class Auth:
        name = "auth"
        version = "1.0"
        Claims = dict  # the type routes annotate against

        def setup(self, ctx):
            return None

    app = _app()
    auth = app.use(Auth())
    assert auth.Claims is dict


class _NoVersion:
    name = "nv"

    def setup(self, ctx): ...


class _NoSetup:
    name = "ns"
    version = "1.0"


class _BadRequires:
    name = "br"
    version = "1.0"
    requires = "oops"  # a bare string is not a name sequence

    def setup(self, ctx): ...


class _BadScope:
    name = "bs"
    version = "1.0"
    scope = "group"  # §13.3 knows only "app" / "any"

    def setup(self, ctx): ...


def test_use_validates_the_plugin_contract() -> None:
    """Malformed plugins fail eagerly at `use()`, not at compile."""
    app = _app()
    with pytest.raises(TypeError, match="name"):
        app.use(object())
    with pytest.raises(TypeError, match="version"):
        app.use(_NoVersion())
    with pytest.raises(TypeError, match="setup"):
        app.use(_NoSetup())
    with pytest.raises(TypeError, match="requires"):
        app.use(_BadRequires())
    with pytest.raises(ValueError, match="scope"):
        app.use(_BadScope())


# -- ordering (M3.6) ---------------------------------------------------------------


def test_setup_runs_in_resolved_order_not_use_order() -> None:
    """§13.2: `requires` decides; `use()` order only breaks ties."""
    order: list[str] = []
    app = _app()
    app.use(Probe("first", order, requires=["second", "third"]))
    app.use(Probe("second", order, requires=["third"]))
    app.use(Probe("third", order))
    app.use(Probe("independent", order))
    app._compile_native()
    assert order == ["third", "second", "first", "independent"]


def test_missing_requirement_is_a_startup_error() -> None:
    app = _app()
    app.use(Probe("needy", requires=["ghost"]))
    with pytest.raises(StartupError, match="ghost"):
        app._compile_native()


def test_declared_conflict_is_a_startup_error() -> None:
    app = _app()
    app.use(Probe("one", conflicts=["two"]))
    app.use(Probe("two"))
    with pytest.raises(StartupError, match="conflict"):
        app._compile_native()


def test_dependency_cycle_names_the_cycle() -> None:
    app = _app()
    app.use(Probe("aa", requires=["bb"]))
    app.use(Probe("bb", requires=["aa"]))
    with pytest.raises(StartupError, match="aa -> bb -> aa"):
        app._compile_native()


def test_problems_aggregate() -> None:
    """§14.1: every resolution failure reports in one StartupError."""
    app = _app()
    app.use(Probe("needy", requires=["ghost"]))
    app.use(Probe("x", conflicts=["y"]))
    app.use(Probe("y"))
    try:
        app._compile_native()
    except StartupError as exc:
        message = str(exc)
        assert "ghost" in message
        assert "conflict" in message
    else:
        pytest.fail("expected StartupError")


# -- configuration (M3.7) -----------------------------------------------------------


def test_plugin_config_slice() -> None:
    """`use(plugin, mapping, **kw)` merges into `ctx.config`."""
    app = _app()
    probe = Probe("cfg")
    app.use(probe, {"key": "mapped", "shared": "base"}, shared="kwarg", extra=1)
    app._compile_native()
    assert probe.ctx is not None
    assert dict(probe.ctx.config) == {"key": "mapped", "shared": "kwarg", "extra": 1}
    with pytest.raises(TypeError):
        probe.ctx.config["late"] = 1  # the slice is a read-only mapping


# -- extension points (M3.8) -------------------------------------------------------


def test_plugin_hooks_precede_app_hooks(serve) -> None:
    """§8/§13.2: plugin hooks run ahead of app hooks in each phase."""
    app = _app()
    seen: list[str] = []

    class GuardPlugin:
        name = "guardp"
        version = "1.0"

        def setup(self, ctx):
            @ctx.on_request
            def on_request(request):
                seen.append("plugin-on_request")

            @ctx.guard
            def guard(request):
                seen.append("plugin-guard")

    app.use(GuardPlugin())

    @app.guard
    def app_guard(request):
        seen.append("app-guard")

    @app.get("/x")
    def x() -> str:
        return "x"

    assert fetch(serve(app) + "/x").status == 200
    assert seen == ["plugin-on_request", "plugin-guard", "app-guard"]


def test_plugin_hook_names_carry_the_plugin_prefix() -> None:
    """`fulmen routes --hooks` attributes hooks: `plugin:name` (§24.2)."""
    app = _app()

    class H:
        name = "named"
        version = "1.0"

        def setup(self, ctx):
            @ctx.guard
            def check(request):
                return None

    app.use(H())

    @app.get("/x")
    def x() -> str:
        return "x"

    rows = app._compile_native().describe_routes()
    names = [name for _m, _p, _h, chains, _e, _c in rows for _ph, name, _k in chains]
    assert "named:check" in names


def test_plugin_routes_serve(serve) -> None:
    """`ctx.route` compiles into the same declaration as `app.route`."""
    app = _app()

    class Router:
        name = "router"
        version = "1.0"

        def setup(self, ctx):
            @ctx.get("/from-plugin")
            def from_plugin() -> str:
                return "plugin-served"

            @ctx.post("/echo")
            def echo() -> Created:
                return Created("posted")

    app.use(Router())
    base = serve(app)
    r = fetch(base + "/from-plugin")
    assert (r.status, r.body) == (200, b"plugin-served")
    assert fetch(base + "/echo", method="POST").status == 201


def test_plugin_routes_appear_in_openapi() -> None:
    app = _app()

    class Router:
        name = "router"
        version = "1.0"

        def setup(self, ctx):
            @ctx.get("/plug", tags=["plugins"], summary="plugin route")
            def plug() -> str:
                return "p"

    app.use(Router())
    document = app.openapi()
    assert document["paths"]["/plug"]["get"]["tags"] == ["plugins"]


def test_plugin_mount_joins_the_group_tree(serve) -> None:
    """`ctx.mount(group)` mounts under the app the plugin was used on."""
    app = _app()
    group = Fulmen(prefix="/v1")

    @group.get("/items")
    def items() -> str:
        return "items"

    class Mounter:
        name = "mounter"
        version = "1.0"

        def setup(self, ctx):
            ctx.mount(group)

    app.use(Mounter())
    r = fetch(serve(app) + "/v1/items")
    assert (r.status, r.body) == (200, b"items")


def test_plugin_on_error_handler_routes_exceptions(serve) -> None:
    app = _app()

    class BoomError(Exception):
        pass

    class ErrorPlugin:
        name = "errors"
        version = "1.0"

        def setup(self, ctx):
            @ctx.on_error(BoomError)
            def boom(exc):
                return JSONResponse({"detail": "plugin handled it"}, status=503)

    app.use(ErrorPlugin())

    @app.get("/boom")
    def boom_route() -> str:
        raise BoomError("kaput")

    r = fetch(serve(app) + "/boom")
    assert r.status == 503
    assert b"plugin handled it" in r.body


def test_plugin_lifecycle_hooks(serve) -> None:
    """`ctx.on_startup`/`on_shutdown` join the app's lifecycle."""
    app = _app()
    seen: list[str] = []

    class Lifecycle:
        name = "lifecycle"
        version = "1.0"

        def setup(self, ctx):
            @ctx.on_startup
            def up():
                seen.append("plugin-startup")

            @ctx.on_shutdown
            def down():
                seen.append("plugin-shutdown")

    app.use(Lifecycle())

    @app.get("/x")
    def x() -> str:
        return "x"

    base = serve(app)
    assert seen == ["plugin-startup"]
    fetch(base + "/x")


def test_providers_seam_collects_declarations() -> None:
    """§13.1/M3.10 seam: `ctx.add_provider` aggregates for the DI task."""
    app = _app()

    class JwtProvider:
        pass

    class P:
        name = "di"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_provider(JwtProvider())
            assert len(ctx.providers) == 1

    app.use(P())
    plan = app._prepare_plugins()
    assert len(plan.providers) == 1
    assert isinstance(plan.providers[0], JwtProvider)


def test_openapi_components_merge() -> None:
    """Plugin components land in the app's OpenAPI document (§13.1)."""
    app = _app()

    class Security:
        name = "security"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_openapi_component(
                "securitySchemes",
                "bearerAuth",
                {"type": "http", "scheme": "bearer"},
            )
            ctx.add_openapi_component("x-tagGroups", "admin", {"name": "admin"})

    app.use(Security())
    document = app.openapi()
    assert document["components"]["securitySchemes"]["bearerAuth"] == {
        "type": "http",
        "scheme": "bearer",
    }
    assert document["components"]["x-tagGroups"]["admin"] == {"name": "admin"}


def test_component_collisions_are_startup_errors() -> None:
    """Two plugins declaring the same component name is a problem (§14.1)."""
    app = _app()

    class A:
        name = "a"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_openapi_component("securitySchemes", "bearerAuth", {"type": "http"})

    class B:
        name = "b"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_openapi_component("securitySchemes", "bearerAuth", {"type": "http"})

    app.use(A())
    app.use(B())
    with pytest.raises(StartupError, match="both declare OpenAPI component"):
        app._compile_native()


def test_cli_command_dispatch(capsys) -> None:
    """`fulmen <cmd> <app> args` reaches the plugin's handler (§24.1)."""
    assert main(["rotate-keys", "plugin_fixture_app:app", "--force"]) == 0
    assert "rotated:--force" in capsys.readouterr().out
    assert main(["rotate-keys", "plugin_fixture_app:app"]) == 0
    assert "rotated:" in capsys.readouterr().out


def test_cli_command_unknown_name(capsys) -> None:
    assert main(["bogus-cmd", "plugin_fixture_app:app"]) == 2
    err = capsys.readouterr().err
    assert "no plugin command" in err
    assert "rotate-keys" in err


def test_cli_command_missing_app_specifier(capsys) -> None:
    assert main(["rotate-keys"]) == 2
    assert "application specifier" in capsys.readouterr().err


def test_check_rule_registration() -> None:
    """`ctx.check_rule` records `FUL-<NAME>-<NNN>` rules for `fulmen check` (§24.1)."""
    app = _app()

    class Keys:
        name = "keys"
        version = "1.0"

        def setup(self, ctx):
            @ctx.check_rule("FUL-KEYS-001", fixable=True)
            def key_length(app):
                """Key length policy."""
                return []

    app.use(Keys())
    plan = app._prepare_plugins()
    assert [r.code for r in plan.check_rules] == ["FUL-KEYS-001"]
    rule = plan.check_rules[0]
    assert rule.description == "Key length policy."
    assert rule.fixable
    assert list(rule.check(app)) == []


def test_invalid_check_rule_code_is_a_startup_problem() -> None:
    app = _app()

    class Bad:
        name = "bad"
        version = "1.0"

        def setup(self, ctx):
            @ctx.check_rule("not-a-code")
            def rule(app):
                return []

    app.use(Bad())
    with pytest.raises(StartupError, match="check-rule code"):
        app._compile_native()


def test_setup_failure_is_a_startup_problem() -> None:
    """A plugin whose `setup()` raises joins the aggregated problems (§14.1)."""
    app = _app()

    class Broken:
        name = "broken"
        version = "1.0"

        def setup(self, ctx):
            raise ValueError("boom")

    app.use(Broken())
    with pytest.raises(StartupError, match="setup\\(\\) failed: boom"):
        app._compile_native()


# -- group scoping (§13.3) ------------------------------------------------------------


def test_group_use_scopes_plugin_hooks(serve) -> None:
    """`group.use(plugin)`: contributions scope to the group's routes only."""
    app = _app()
    group = Fulmen(prefix="/g", openapi_url=None)
    seen: list[str] = []

    class GuardPlugin:
        name = "scoped"
        version = "1.0"

        def setup(self, ctx):
            @ctx.guard
            def guard(request):
                seen.append("plugin-guard")
                return {"guarded": "yes"}

    group.use(GuardPlugin())

    @group.get("/inside")
    def inside(request: Request) -> str:
        return request.state["guarded"]

    app.mount(group)

    @app.get("/outside")
    def outside() -> str:
        return "outside"

    base = serve(app)
    r = fetch(base + "/g/inside")
    assert (r.status, r.body) == (200, b"yes")
    assert fetch(base + "/outside").status == 200
    assert seen == ["plugin-guard"]  # app-scope routes bypass the group hook


def test_app_scope_plugin_rejects_group_use() -> None:
    """§13.3: `scope="app"` plugins cannot be `use()`d on a route group."""
    app = _app()
    group = Fulmen(prefix="/g", openapi_url=None)
    group.use(Probe("root-only", scope="app"))
    app.mount(group)
    with pytest.raises(StartupError, match='scope="app"'):
        app._compile_native()


# -- sealing (M3.9, §14.8) ------------------------------------------------------------


def test_use_seals_after_compile(serve) -> None:
    app = _app()

    @app.get("/x")
    def x() -> str:
        return "x"

    serve(app)
    with pytest.raises(RuntimeError, match="sealed"):
        app.use(Probe("late"))


def test_plugin_context_closes_after_setup() -> None:
    """A ctx kept past `setup()` refuses all registrations (§14.8)."""
    app = _app()
    probe = Probe("keeper")
    app.use(probe)
    app._compile_native()
    assert probe.ctx is not None
    with pytest.raises(RuntimeError, match="context is closed"):
        probe.ctx.add_hook("guard", lambda request: None)
    with pytest.raises(RuntimeError, match="context is closed"):
        probe.ctx.get("/late")
    with pytest.raises(RuntimeError, match="context is closed"):
        probe.ctx.get("/x")(lambda: "x")  # decorator application is covered too
    with pytest.raises(RuntimeError, match="context is closed"):
        probe.ctx.add_openapi_component("schemas", "X", {})


def test_mounted_groups_seal_with_the_app(serve) -> None:
    """§14.8/ M3.9: a mounted group cannot accept registrations after compile."""
    app = _app()
    group = Fulmen(prefix="/g", openapi_url=None)

    @group.get("/x")
    def x() -> str:
        return "x"

    app.mount(group)
    serve(app)
    with pytest.raises(RuntimeError, match="sealed"):
        group.get("/late")
    with pytest.raises(RuntimeError, match="sealed"):
        group.use(Probe("late"))
