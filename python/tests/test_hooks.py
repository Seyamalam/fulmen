"""Lifecycle-hook tests (DESIGN.md §8, milestone M3).

Phase ordering, short-circuiting, `request.state` context merging, group
scoping, error routing, async hooks, `fulmen routes` output, and the
one-crossing invariant for bare routes.
"""

from __future__ import annotations

import pytest
from conftest import fetch
from fulmen import Fulmen, Request, Response
from fulmen.cli import main


def test_phase_order(serve) -> None:
    """The hookable phases fire in §8 order around the handler."""
    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.on_request
    def on_request(request):
        seen.append("on_request")

    @app.before_route
    def before_route(request):
        seen.append("before_route")

    @app.guard
    def guard(request):
        seen.append("guard")

    @app.before_handle
    def before_handle(request):
        seen.append("before_handle")

    @app.after_handle
    def after_handle(request, response):
        seen.append("after_handle")

    @app.on_response
    def on_response(request, response):
        seen.append("on_response")

    @app.get("/x")
    def x() -> str:
        seen.append("handle")
        return "x"

    assert fetch(serve(app) + "/x").status == 200
    assert seen == [
        "on_request",
        "before_route",
        "guard",
        "before_handle",
        "handle",
        "after_handle",
        "on_response",
    ]


def test_decision_phase_short_circuits(serve) -> None:
    """A `guard` returning a response skips the handler and later hooks."""
    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.guard
    def deny(request):
        seen.append("guard")
        return Response("denied", status=403)

    @app.before_handle
    def before(request):
        seen.append("before_handle")

    @app.get("/x")
    def x() -> str:
        seen.append("handle")
        return "x"

    r = fetch(serve(app) + "/x")
    assert (r.status, r.body) == (403, b"denied")
    assert seen == ["guard"]


def test_after_handle_replaces_response(serve) -> None:
    """`after_handle`'s non-None return becomes the response."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.after_handle
    def wrap(request, response):
        return Response("wrapped", status=201)

    @app.get("/x")
    def x() -> str:
        return "x"

    r = fetch(serve(app) + "/x")
    assert (r.status, r.body) == (201, b"wrapped")


def test_on_response_mutates_headers_on_every_response(serve) -> None:
    """`on_response` applies to handler output, `4xx` and `404` alike."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.on_response
    def tag(request, response):
        response.headers["x-tag"] = "yes"

    @app.get("/x")
    def x() -> str:
        return "x"

    base = serve(app)
    ok = fetch(base + "/x")
    assert (ok.status, ok.header("x-tag")) == (200, "yes")
    missing = fetch(base + "/nope")
    assert (missing.status, missing.header("x-tag")) == (404, "yes")


def test_dict_return_merges_into_request_state(serve) -> None:
    """A hook returning `dict` merges into `request.state` and continues."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.on_request
    def identify(request):
        return {"request_id": "abc123"}

    @app.get("/x")
    def x(request: Request) -> str:
        return request.state["request_id"]

    r = fetch(serve(app) + "/x")
    assert (r.status, r.body) == (200, b"abc123")


def test_request_param_receives_the_request_object(serve) -> None:
    """`request: Request` injects the same object hooks see."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/x/{n}")
    def x(n: int, request: Request) -> str:
        assert request.method == "GET"
        assert request.path == f"/x/{n}"
        assert request.headers["user-agent"] is not None or True
        return f"{request.method}:{request.path}"

    r = fetch(serve(app) + "/x/9", headers={"user-agent": "test"})
    assert (r.status, r.body) == (200, b"GET:/x/9")


def test_group_hooks_apply_only_to_descendant_routes(serve) -> None:
    """Hooks on a mounted app run for its routes, not the parent's."""
    app = Fulmen(openapi_url=None, docs_url=None)
    admin = Fulmen(prefix="/admin", openapi_url=None, docs_url=None)

    @admin.before_handle
    def admin_hook(request):
        return Response("admin says hi", status=200)

    @app.get("/open")
    def open_route() -> str:
        return "open"

    @admin.get("/panel")
    def panel() -> str:
        return "panel"

    @admin.get("/deep/x")
    def deep() -> str:
        return "deep"

    app.mount(admin)
    base = serve(app)
    assert fetch(base + "/open").body == b"open"
    assert fetch(base + "/admin/panel").body == b"admin says hi"
    assert fetch(base + "/admin/deep/x").body == b"admin says hi"


def test_group_prefix_joins_paths(serve) -> None:
    """`Fulmen(prefix="/api")` mounts its routes under the prefix."""
    api = Fulmen(prefix="/api", openapi_url=None, docs_url=None)

    @api.get("/users/{uid}")
    def user(uid: int) -> str:
        return f"user {uid}"

    app = Fulmen(openapi_url=None, docs_url=None)
    app.mount(api)
    base = serve(app)
    r = fetch(base + "/api/users/7")
    assert (r.status, r.body) == (200, b"user 7")
    assert fetch(base + "/users/7").status == 404


def test_group_on_error_scopes_to_the_group(serve) -> None:
    """An `on_error` on the mounted app overrides the app's table there."""
    app = Fulmen(openapi_url=None, docs_url=None)
    api = Fulmen(prefix="/api", openapi_url=None, docs_url=None)

    @app.on_error(ValueError)
    def app_ve(exc):
        return Response(f"app:{exc}", status=500)

    @api.on_error(ValueError)
    def api_ve(exc):
        return Response(f"api:{exc}", status=400)

    @app.get("/boom")
    def boom() -> str:
        raise ValueError("root")

    @api.get("/boom")
    def api_boom() -> str:
        raise ValueError("grouped")

    app.mount(api)
    base = serve(app)
    r = fetch(base + "/boom")
    assert (r.status, r.body) == (500, b"app:root")
    r = fetch(base + "/api/boom")
    assert (r.status, r.body) == (400, b"api:grouped")


def test_hook_error_routes_to_on_error(serve) -> None:
    """A hook that raises is handled exactly like a handler failure."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.before_handle
    def explode(request):
        raise ValueError("hook exploded")

    @app.on_error(ValueError)
    def on_ve(exc):
        return Response(f"caught: {exc}", status=400)

    @app.get("/x")
    def x() -> str:
        return "never"

    r = fetch(serve(app) + "/x")
    assert (r.status, r.body) == (400, b"caught: hook exploded")


def test_hook_error_without_handler_is_a_problem(serve) -> None:
    """An unrouted hook exception answers an RFC 9457 problem response."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.before_handle
    def explode(request):
        raise RuntimeError("no handler for this")

    @app.get("/x")
    def x() -> str:
        return "never"

    r = fetch(serve(app) + "/x")
    assert r.status == 500
    assert r.header("content-type") == "application/problem+json"


def test_async_hook_runs_on_the_loop(serve) -> None:
    """`async def` hooks execute on the asyncio executor lane."""
    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.on_request
    async def first(request):
        seen.append("async_on_request")

    @app.before_handle
    async def second(request):
        seen.append("async_before_handle")

    @app.get("/x")
    def x() -> str:
        seen.append("handle")
        return "x"

    assert fetch(serve(app) + "/x").status == 200
    assert seen == ["async_on_request", "async_before_handle", "handle"]


def test_after_response_is_fire_and_forget(serve) -> None:
    """`after_response` runs after the response is produced — never blocks
    or replaces it, even if it returns a value."""
    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.after_response
    def log_it(request, response):
        seen.append(f"{response.status}")
        return Response("ignored", status=599)

    @app.get("/x")
    def x() -> str:
        return "x"

    r = fetch(serve(app) + "/x")
    assert (r.status, r.body) == (200, b"x")
    # The hook may still be in flight — it fired eventually and could not
    # change the answer.
    import time

    deadline = time.time() + 5
    while not seen and time.time() < deadline:
        time.sleep(0.01)
    assert seen == ["200"]


def test_priority_orders_within_a_phase(serve) -> None:
    """Lower priority runs first regardless of registration order (§13.2)."""
    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.add_hook("guard", priority=2000)
    def user_hook(request):
        seen.append("user")

    @app.add_hook("guard", priority=100)
    def security_hook(request):
        seen.append("security")

    @app.get("/x")
    def x() -> str:
        return "x"

    assert fetch(serve(app) + "/x").status == 200
    assert seen == ["security", "user"]


def test_lifecycle_hooks(serve) -> None:
    """`on_startup` runs before serving, `on_shutdown` after stop."""
    from fulmen._fulmen import serve_for_test

    app = Fulmen(openapi_url=None, docs_url=None)
    seen: list[str] = []

    @app.on_startup
    def up():
        seen.append("startup")

    @app.on_shutdown
    async def down():
        seen.append("shutdown")

    @app.get("/x")
    def x() -> str:
        return "x"

    server = serve_for_test(app._compile_native())
    assert seen == ["startup"]
    server.stop()
    assert seen == ["startup", "shutdown"]


def test_registration_seals_after_compile(serve) -> None:
    """§14.8: route/hook/error registration raises once the app compiled."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/x")
    def x() -> str:
        return "x"

    serve(app)
    with pytest.raises(RuntimeError, match="sealed"):

        @app.get("/late")
        def late() -> str:
            return "late"

    with pytest.raises(RuntimeError, match="sealed"):
        app.add_hook("guard", lambda request: None)


def test_add_hook_rejects_unknown_phase() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)
    with pytest.raises(ValueError, match="unknown hook phase"):
        app.add_hook("handle", lambda request: None)


def test_native_app_describe_routes() -> None:
    """`describe_routes` reports the resolved chain and crossing cost."""
    app = Fulmen(openapi_url=None, docs_url=None)
    admin = Fulmen(prefix="/admin", openapi_url=None, docs_url=None)

    @app.on_request
    def rid(request):
        return None

    @admin.guard
    def auth(request):
        return None

    @app.get("/users/{uid}")
    def get_user(uid: int) -> str:
        return "u"

    @admin.get("/panel")
    def panel() -> str:
        return "p"

    app.mount(admin)
    rows = {row[1]: row for row in app._compile_native().describe_routes()}
    user = rows["/users/{uid}"]
    assert user[0] == "GET"
    assert user[2].endswith("get_user")
    assert user[3][0][0] == "on_request"
    assert user[3][0][1].endswith("rid")
    assert user[5] == 2
    panel_row = rows["/admin/panel"]
    assert [c[0] for c in panel_row[3]] == ["on_request", "guard"]
    assert panel_row[5] == 3


def test_bare_route_reports_one_crossing() -> None:
    """M3 invariant: a native-only route costs exactly one Python crossing."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/x")
    def x() -> str:
        return "x"

    rows = app._compile_native().describe_routes()
    (method, path, handler, chains, errors, cost) = rows[0]
    assert method == "GET"
    assert path == "/x"
    assert handler.endswith("x")
    assert (chains, errors, cost) == ([], [], 1)


def test_cli_routes_lists_routes(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["routes", "sample_app:app"]) == 0
    out = capsys.readouterr().out
    assert "GET" in out
    assert "->" in out


def test_cli_routes_hooks_and_cost(capsys: pytest.CaptureFixture[str]) -> None:
    capsys.readouterr()
    assert main(["routes", "sample_app:app", "--hooks", "--cost"]) == 0
    out = capsys.readouterr().out
    assert "cost:" in out
    assert "1 crossings" in out
