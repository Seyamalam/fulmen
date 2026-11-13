"""Plugin-system tests (DESIGN.md §13, milestone M3.5-M3.9).

`use()`/handle semantics, dependency ordering, requires/conflicts/cycle
failures, configuration slices, every `PluginContext` extension point,
group scoping, and post-compile sealing (§14.8).
"""

from __future__ import annotations

import json
from typing import ClassVar

import pytest
from conftest import HttpResponse, fetch
from fulmen import Created, Fulmen, JSONResponse, Model, Request, StartupError
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


class _Item(Model):
    x: int


_Item.__qualname__ = "Item"


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


# -- handle and context surfaces ------------------------------------------------


def test_handle_repr_and_private_fallthrough() -> None:
    app = _app()
    handle = app.use(Probe("probe"))
    assert repr(handle) == "<fulmen.PluginHandle probe 1.0>"
    # `__getattr__` guards `_plugin` itself so the object pickles/copies safely.
    with pytest.raises(AttributeError):
        handle.__getattr__("_plugin")
    with pytest.raises(AttributeError):
        handle.__getattr__("missing_attribute")


def test_context_identity_properties() -> None:
    """`ctx.app`/`ctx.plugin_name` expose the registration owner."""
    app = _app()
    probe = Probe("probe")
    app.use(probe)
    app._compile_native()
    assert probe.ctx.app is app
    assert probe.ctx.plugin_name == "probe"


def test_add_hook_rejects_an_unknown_phase() -> None:
    class BadPhase:
        name = "bad-phase"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(ValueError, match="unknown hook phase"):
                ctx.add_hook("not-a-phase", lambda request: None)

    app = _app()
    app.use(BadPhase())
    app._compile_native()


def test_every_ctx_hook_shortcut(serve) -> None:
    """The phase shortcuts all funnel into `add_hook` — registering one
    per phase exercises each wrapper."""
    calls: list[str] = []

    class AllHooks:
        name = "all-hooks"
        version = "1.0"

        def setup(self, ctx):
            for phase in (
                "on_request",
                "before_route",
                "guard",
                "resolve",
                "before_handle",
                "after_handle",
                "on_response",
                "after_response",
            ):
                getattr(ctx, phase)(lambda request, *rest, p=phase: calls.append(p) or None)

        @staticmethod
        def _ok(*args):
            return None

    app = _app()
    app.use(AllHooks())

    @app.get("/x")
    def x() -> str:
        return "x"

    base = serve(app)
    assert fetch(f"{base}/x").status == 200


def test_ctx_route_method_shortcuts(serve) -> None:
    """put/patch/delete/head/options mirror `get`/`post`."""
    responses = {"PUT": "p", "PATCH": "pa", "DELETE": "d", "HEAD": "h", "OPTIONS": "o"}

    class MethodPlugin:
        name = "methods"
        version = "1.0"

        def setup(self, ctx):
            for method in ("put", "patch", "delete", "head", "options"):
                upper = method.upper()
                getattr(ctx, method)(f"/{method}")(lambda m=upper: responses[m])

    app = _app()
    app.use(MethodPlugin())
    base = serve(app)
    for method, expected in (("PUT", "p"), ("PATCH", "pa"), ("DELETE", "d"), ("OPTIONS", "o")):
        response = fetch(f"{base}/{method.lower()}", method=method)
        assert response.status == 200
        assert response.body == expected.encode()


def test_ctx_mount_rejects_non_fulmen() -> None:
    class BadMount:
        name = "bad-mount"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(TypeError, match="mount expects a Fulmen"):
                ctx.mount(object())

    app = _app()
    app.use(BadMount())
    app._compile_native()


def test_ctx_on_error_rejects_non_exception() -> None:
    class BadOnError:
        name = "bad-onerror"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(TypeError, match="expects an exception class"):
                ctx.on_error("ValueError")

    app = _app()
    app.use(BadOnError())
    app._compile_native()


# -- OpenAPI component and CLI validation ---------------------------------------


def test_component_unknown_section_raises() -> None:
    class BadSection:
        name = "bad-section"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(ValueError, match="unknown OpenAPI components section"):
                ctx.add_openapi_component("not-a-section", "X", {})

    app = _app()
    app.use(BadSection())
    app._compile_native()


def test_component_redeclare_in_one_plugin_raises() -> None:
    class Redeclarer:
        name = "redeclarer"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_openapi_component("schemas", "Same", {"type": "object"})
            with pytest.raises(ValueError, match="redeclares OpenAPI component"):
                ctx.add_openapi_component("schemas", "Same", {"type": "object"})

    app = _app()
    app.use(Redeclarer())
    app._compile_native()


def test_cli_command_name_validation() -> None:
    class BadNames:
        name = "bad-names"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(ValueError, match="invalid plugin command name"):
                ctx.cli_command("Not-A-Command")
            with pytest.raises(ValueError, match="collides with a built-in"):
                ctx.cli_command("run")
            ctx.cli_command("good-name", lambda app, argv: 0)
            with pytest.raises(ValueError, match="redeclares CLI command"):
                ctx.cli_command("good-name", lambda app, argv: 0)

    app = _app()
    app.use(BadNames())
    app._compile_native()


def test_check_rule_uses_docstring_when_no_description() -> None:
    captured: dict[str, str] = {}

    class DocRule:
        name = "doc-rule"
        version = "1.0"

        def setup(self, ctx):
            @ctx.check_rule("FUL-DOC-001", fixable=True)
            def rule(app):
                """The docstring becomes the description."""
                return []

            captured["description"] = ctx._check_rules[0].description

    app = _app()
    app.use(DocRule())
    app._compile_native()
    assert captured["description"] == "The docstring becomes the description."


def test_cross_plugin_cli_command_collision_is_a_problem(capsys) -> None:
    """Two plugins registering the same `fulmen <cmd>` is a startup problem."""

    class First:
        name = "first"
        version = "1.0"

        def setup(self, ctx):
            ctx.cli_command("shared-cmd", lambda app, argv: 0)

    class Second:
        name = "second"
        version = "1.0"

        def setup(self, ctx):
            ctx.cli_command("shared-cmd", lambda app, argv: 0)

    app = _app()
    app.use(First())
    app.use(Second())
    with pytest.raises(StartupError, match="both register CLI command `shared-cmd`"):
        app._compile_native()


def test_cross_plugin_check_rule_collision_is_a_problem() -> None:
    class First:
        name = "first"
        version = "1.0"

        def setup(self, ctx):
            @ctx.check_rule("FUL-SAME-001")
            def rule(app):
                return []

    class Second:
        name = "second"
        version = "1.0"

        def setup(self, ctx):
            @ctx.check_rule("FUL-SAME-001")
            def rule(app):
                return []

    app = _app()
    app.use(First())
    app.use(Second())
    with pytest.raises(StartupError, match="both register check rule `FUL-SAME-001`"):
        app._compile_native()


# -- requires/conflicts validation (§13.2) ---------------------------------------


def test_requires_and_conflicts_must_be_name_sequences() -> None:
    class BadRequires:
        name = "bad-requires"
        version = "1.0"
        requires = "other"  # a bare string is not a sequence of names

        def setup(self, ctx):
            return None

    app = _app()
    with pytest.raises(TypeError, match="`requires` must be a sequence"):
        app.use(BadRequires())

    class BadConflictItem:
        name = "bad-conflict-item"
        version = "1.0"
        conflicts: ClassVar = ["other", 42]

        def setup(self, ctx):
            return None

    with pytest.raises(TypeError, match="`conflicts` must be a sequence"):
        app.use(BadConflictItem())

    class EmptyName:
        name = "empty-name"
        version = "1.0"
        requires: ClassVar = [""]

        def setup(self, ctx):
            return None

    with pytest.raises(TypeError, match="`requires` must be a sequence"):
        app.use(EmptyName())

    class NonIterable:
        name = "non-iterable"
        version = "1.0"
        requires = 42

        def setup(self, ctx):
            return None

    with pytest.raises(TypeError, match="`requires` must be a sequence"):
        app.use(NonIterable())


def test_requires_none_is_treated_as_empty() -> None:
    class NoneRequires:
        name = "none-requires"
        version = "1.0"
        requires = None

        def setup(self, ctx):
            return None

    app = _app()
    app.use(NoneRequires())
    app._compile_native()


def test_order_integrity_flags_require_pointing_forwards() -> None:
    """A plugin `use()`d on the app cannot `requires` a plugin that only
    exists inside a plugin-mounted group — that one always runs later."""

    class Inner:
        name = "inner"
        version = "1.0"

        def setup(self, ctx):
            return None

    class Mounter:
        name = "mounter"
        version = "1.0"

        def setup(self, ctx):
            group = Fulmen(prefix="/mounted", openapi_url=None)
            group.use(Inner())
            ctx.mount(group)

    class Early:
        name = "early"
        version = "1.0"
        requires: ClassVar = ["inner"]

        def setup(self, ctx):
            return None

    app = _app()
    app.use(Early())
    app.use(Mounter())
    with pytest.raises(StartupError, match="requires `inner`, which runs later"):
        app._compile_native()


def test_plugin_mount_depth_is_capped() -> None:
    """A plugin whose `setup` mounts a group that `use()`s another of the
    same kind would nest forever — the walk bails after 16 waves."""

    class Matryoshka:
        name = "matryoshka"
        version = "1.0"

        def setup(self, ctx):
            group = Fulmen(prefix="/g", openapi_url=None)
            group.use(Matryoshka())
            ctx.mount(group)

    app = _app()
    app.use(Matryoshka())
    with pytest.raises(StartupError, match="mount nesting exceeds the supported depth"):
        app._compile_native()


def test_component_collision_with_generated_schema(serve) -> None:
    """`merge_components` reports a plugin component colliding with a
    generated `schemas/<Model>` entry — a `StartupError`, not a silent win."""

    class Collider:
        name = "collider"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_openapi_component("schemas", "Item", {"type": "object"})

    app = Fulmen(openapi_url=None)
    app.use(Collider())

    @app.get("/item")
    def item() -> _Item:
        return _Item(x=1)

    with pytest.raises(StartupError, match="collides with a generated component"):
        app.openapi()


def test_sealed_context_rejects_remaining_registrations() -> None:
    """§14.8: after `setup()` returns every registration surface raises."""
    app = _app()
    probe = Probe("sealed")
    app.use(probe)
    app._compile_native()
    ctx = probe.ctx
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.mount(Fulmen(prefix="/g", openapi_url=None))
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.on_error(ValueError)
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.add_provider(object())
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.cli_command("late-cmd", lambda app, argv: 0)
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.check_rule("FUL-LATE-001")
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.on_startup(lambda: None)
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.on_shutdown(lambda: None)
    with pytest.raises(RuntimeError, match="context is closed"):
        ctx.limits(max_items=1)


# -- plugin request limits (§14.3/§14.8, M3.23) ---------------------------------


class _Limits:
    """Toy plugin declaring `ctx.limits` from constructor kwargs."""

    def __init__(self, name: str, **limits: int) -> None:
        self.name = name
        self.version = "1.0"
        self._limits = limits

    def setup(self, ctx):
        ctx.limits(**self._limits)


def _items_app(app: Fulmen, path: str = "/items") -> None:
    """A JSON route echoing the body's list length — `max_items` probe."""

    @app.post(path)
    def items(body: dict) -> int:
        return len(body["xs"])


def _post(url: str, xs: list[int]) -> HttpResponse:
    return fetch(
        url,
        method="POST",
        data=json.dumps({"xs": xs}).encode(),
        headers={"content-type": "application/json"},
    )


def test_app_limits_field_validation() -> None:
    """`Fulmen(limits=…)` rejects bad shapes eagerly (§14.3)."""
    with pytest.raises(ValueError, match="unknown limit 'max_body'"):
        Fulmen(openapi_url=None, limits={"max_body": 10})
    with pytest.raises(ValueError, match="must be a positive integer"):
        Fulmen(openapi_url=None, limits={"max_items": "5"})
    with pytest.raises(ValueError, match="must be a positive integer"):
        Fulmen(openapi_url=None, limits={"max_items": True})
    with pytest.raises(ValueError, match="must be a positive integer"):
        Fulmen(openapi_url=None, limits={"max_items": 0})


def test_app_limits_looser_than_default_is_rejected() -> None:
    """A declared app limit may not exceed its §14.3 default."""
    with pytest.raises(
        ValueError,
        match=r"the app declares a looser `max_depth` limit: 128 exceeds the default 64",
    ):
        Fulmen(openapi_url=None, limits={"max_depth": 128})
    with pytest.raises(ValueError, match=r"looser `max_items`.*exceeds the default 100000"):
        Fulmen(openapi_url=None, limits={"max_items": 200_000})


def test_app_limits_apply_to_requests(serve) -> None:
    """A tightened app limit binds every route's decoding."""
    app = Fulmen(openapi_url=None, limits={"max_items": 3})
    _items_app(app)
    base = serve(app)
    assert _post(f"{base}/items", [1, 2]).status == 200
    assert _post(f"{base}/items", [1, 2, 3, 4]).status == 422


def test_ctx_limits_validation() -> None:
    """`ctx.limits` validates values and merges repeated calls by min."""
    seen: dict[str, int] = {}

    class Merge:
        name = "merge"
        version = "1.0"

        def setup(self, ctx):
            with pytest.raises(ValueError, match="must be a positive integer"):
                ctx.limits(max_items=0)
            with pytest.raises(ValueError, match="must be a positive integer"):
                ctx.limits(max_str_bytes=-1)
            ctx.limits(max_items=8, max_depth=4)
            ctx.limits(max_items=12)  # looser repeat keeps 8
            ctx.limits(max_items=2)
            seen.update(ctx.staged.limits)

    app = _app()
    app.use(Merge())
    app._compile_native()
    assert seen == {"max_items": 2, "max_depth": 4}


def test_plugin_limits_tighten_their_group_scope_only(serve) -> None:
    """A group-scoped plugin binds its group's routes, not the app's."""
    app = _app()
    group = Fulmen(prefix="/g", openapi_url=None)
    group.use(_Limits("tight", max_items=3))
    _items_app(group)
    _items_app(app)
    app.mount(group)
    base = serve(app)
    assert _post(f"{base}/g/items", [1, 2, 3, 4]).status == 422
    assert _post(f"{base}/g/items", [1, 2]).status == 200
    # The same payload on the unscoped root route is unaffected.
    assert _post(f"{base}/items", [1, 2, 3, 4]).status == 200


def test_app_scope_plugin_limits_cover_mounted_groups(serve) -> None:
    """`app.use(p)` limits bind every route, mounted groups included."""
    app = _app()
    app.use(_Limits("tight", max_items=2))
    group = Fulmen(prefix="/g", openapi_url=None)
    _items_app(group)
    _items_app(app)
    app.mount(group)
    base = serve(app)
    assert _post(f"{base}/items", [1, 2, 3]).status == 422
    assert _post(f"{base}/g/items", [1, 2, 3]).status == 422
    assert _post(f"{base}/g/items", [1]).status == 200


def test_plugin_limits_combine_with_app_limits(serve) -> None:
    """Effective bound = app ∧ plugin, per field (§14.8)."""
    app = Fulmen(openapi_url=None, limits={"max_items": 5, "max_depth": 8})
    group = Fulmen(prefix="/g", openapi_url=None)
    group.use(_Limits("tight", max_items=3))
    _items_app(group)
    _items_app(app)
    app.mount(group)
    base = serve(app)
    # Scoped: plugin's 3 beats the app's 5.
    assert _post(f"{base}/g/items", [1, 2, 3, 4]).status == 422
    # Unscoped: the app's 5 applies.
    assert _post(f"{base}/items", [1, 2, 3, 4]).status == 200
    assert _post(f"{base}/items", [1, 2, 3, 4, 5, 6]).status == 422


def test_plugin_limits_looser_than_app_is_a_startup_error() -> None:
    """A plugin field looser than the app's names both values (§14.8)."""
    app = Fulmen(openapi_url=None, limits={"max_items": 5})
    app.use(_Limits("greedy", max_items=99))
    with pytest.raises(
        StartupError,
        match=r"plugin `greedy` declares a looser `max_items` limit: 99 exceeds the app's 5",
    ):
        app._compile_native()


def test_plugin_limits_looser_than_default_is_a_startup_error() -> None:
    """With no app declaration, the §14.3 default is the bound."""
    app = _app()
    app.use(_Limits("greedy", max_depth=999))
    with pytest.raises(
        StartupError,
        match=r"plugin `greedy` declares a looser `max_depth` limit: 999 exceeds the default 64",
    ):
        app._compile_native()


def test_plugin_limits_loosening_fails_openapi_too() -> None:
    """The check aggregates like every other declaration problem."""
    app = Fulmen(openapi_url=None, limits={"max_items": 5})
    app.use(_Limits("greedy", max_items=99))
    with pytest.raises(StartupError, match="looser `max_items`"):
        app.openapi()


def test_mounted_group_limits_are_refused() -> None:
    """`limits=` is declared once, on the root application."""
    group = Fulmen(prefix="/g", openapi_url=None, limits={"max_items": 4})
    app = _app()
    app.mount(group)
    with pytest.raises(StartupError, match="declared on the root application"):
        app._compile_native()


def test_plugin_limits_setup_problem_keeps_message_shape() -> None:
    """A `ctx.limits` ValueError inside setup joins the setup problems."""

    class Bad:
        name = "bad"
        version = "1.0"

        def setup(self, ctx):
            ctx.limits(max_items="nope")

    app = _app()
    app.use(Bad())
    with pytest.raises(StartupError, match=r"plugin `bad`: setup\(\) failed"):
        app._compile_native()
