"""Socket-free dispatch tests (DESIGN.md §22, milestone M3.21 follow-up).

`dispatch_for_test`/`DispatchClient` run requests through the compiled
pipeline without a socket; `TestClient` prefers that path and keeps
`use_sockets=True` as the loopback escape hatch. These tests prove the
pipeline is not bypassed (hooks, guards, extraction, DI, error routing
all run) and that both transports answer the same app the same way.
"""

from __future__ import annotations

import asyncio
import json
from typing import Annotated

import pytest
from fulmen import (
    Cookie,
    Fulmen,
    Header,
    HTTPError,
    Query,
    Request,
    Response,
)
from fulmen.testing import AsyncTestClient, TestClient, TestResponse


def _pipeline_app(events: list[str]) -> Fulmen:
    """An app recording every §8 hook phase it runs."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.on_request
    def on_req(request: Request) -> None:
        events.append("on_request")

    @app.before_route
    def before(request: Request) -> None:
        events.append("before_route")

    @app.guard
    def guard(request: Request) -> None:
        events.append("guard")
        if request.path == "/denied":
            raise HTTPError(403, "denied")

    @app.before_handle
    def before_handle(request: Request) -> None:
        events.append("before_handle")

    @app.after_handle
    def after_handle(request: Request, result: object) -> None:
        events.append("after_handle")

    @app.on_response
    def on_resp(request: Request, response: Response) -> Response:
        events.append("on_response")
        response.headers["x-pipeline"] = "seen"
        return response

    @app.get("/hi/{name}")
    def hi(
        name: str,
        request: Request,
        q: Annotated[int, Query()] = 0,
        api_key: Annotated[str | None, Header(alias="X-Api-Key")] = None,
    ) -> dict:
        return {"name": name, "q": q, "api_key": api_key, "remote": request.remote_addr}

    @app.get("/denied")
    def denied() -> str:
        return "never"

    @app.post("/items")
    def items(body: dict) -> int:
        return len(body["xs"])

    @app.get("/async")
    async def ahi() -> str:
        return "async-ok"

    return app


# -- the pipeline is not bypassed ------------------------------------------------


def test_dispatch_runs_every_pipeline_phase() -> None:
    """Hooks before and after the handler all fire on dispatch."""
    events: list[str] = []
    with TestClient(_pipeline_app(events)) as client:
        r = client.get("/hi/ada", params={"q": "7"}, headers={"X-Api-Key": "k"})
        assert r.status == 200
        assert r.json()["name"] == "ada"
        assert r.json()["q"] == 7
        assert r.json()["api_key"] == "k"
        assert r.header("x-pipeline") == "seen"
    assert events == [
        "on_request",
        "before_route",
        "guard",
        "before_handle",
        "after_handle",
        "on_response",
    ]


def test_dispatch_runs_guards_and_error_routing() -> None:
    """A guard's `HTTPError` renders as a problem response."""
    events: list[str] = []
    with TestClient(_pipeline_app(events)) as client:
        r = client.get("/denied")
        assert r.status == 403
        assert r.json()["status"] == 403
        # The handler never ran; the response hooks still observed it.
        assert "before_handle" not in events
        assert "on_response" in events
        assert r.header("x-pipeline") == "seen"


def test_dispatch_extracts_body_and_404_gets_app_hooks() -> None:
    events: list[str] = []
    with TestClient(_pipeline_app(events)) as client:
        assert client.post("/items", json={"xs": [1, 2, 3]}).text == "3"
        events.clear()
        assert client.get("/nope").status == 404
        # A 404 runs the app-scope request/response chains (§8).
        assert "on_request" in events
        assert "on_response" in events


def test_dispatch_reports_the_fabricated_remote() -> None:
    """Handlers see `127.0.0.1:0` — the honest no-socket peer (§22)."""
    with TestClient(_pipeline_app([])) as client:
        assert client.get("/hi/ada").json()["remote"] == "127.0.0.1:0"


def test_dispatch_runs_async_handlers() -> None:
    with TestClient(_pipeline_app([])) as client:
        assert client.get("/async").text == "async-ok"


def test_dispatch_response_carries_transport_headers() -> None:
    """`date`/`content-length` arrive like they do over a socket."""
    with TestClient(_pipeline_app([])) as client:
        r = client.get("/hi/ada")
        assert r.header("date") is not None
        assert r.header("content-length") == str(len(r.body))


# -- parity with the socket path -------------------------------------------------


def _battery(client: TestClient) -> list[tuple[int, str, str]]:
    """Same requests on any transport → (status, content-type, body)."""
    out = []
    out.append(_trio(client.get("/hi/ada", params={"q": "3"}, headers={"X-Api-Key": "k"})))
    out.append(_trio(client.post("/items", json={"xs": [1, 2, 3, 4]})))
    out.append(_trio(client.get("/denied")))
    out.append(_trio(client.get("/nope")))
    out.append(_trio(client.get("/async")))
    return out


def _trio(r: TestResponse) -> tuple[int, str, str]:
    # `remote` is transport-honest — `127.0.0.1:0` in-process, the real
    # peer port on a socket — so it leaves the comparison.
    body = r.text
    if '"remote"' in body:
        parsed = json.loads(body)
        parsed.pop("remote")
        body = json.dumps(parsed)
    return r.status, r.header("content-type") or "", body


def test_dispatch_and_sockets_answer_identically() -> None:
    """Both transports run the same pipeline for the same app."""
    app = _pipeline_app([])
    with TestClient(app) as inproc:
        dispatched = _battery(inproc)
    with TestClient(app, use_sockets=True) as socked:
        looped = _battery(socked)
    assert dispatched == looped


def test_use_sockets_binds_a_real_port() -> None:
    with TestClient(_pipeline_app([]), use_sockets=True) as client:
        assert client.base_url.startswith("http://127.0.0.1:")
        # Absolute URLs on the client's own server resolve to the path.
        assert client.get(f"{client.base_url}/hi/ada").status == 200


# -- lifecycle and edge cases ----------------------------------------------------


def test_dispatch_lifecycle_hooks() -> None:
    """`on_startup`/`on_shutdown` bracket the client's lifetime."""
    app = Fulmen(openapi_url=None, docs_url=None)
    events: list[str] = []

    @app.on_startup
    def up() -> None:
        events.append("startup")

    @app.on_shutdown
    def down() -> None:
        events.append("shutdown")

    @app.get("/x")
    def x() -> str:
        return "x"

    client = TestClient(app)
    assert events == []
    with client:
        assert events == ["startup"]
    assert events == ["startup", "shutdown"]


def test_dispatch_stop_is_idempotent_and_requests_restart() -> None:
    """`stop()` is idempotent; a later request lazily compiles anew."""
    client = TestClient(_pipeline_app([]))
    client.start()
    client.stop()
    client.stop()
    assert client.get("/hi/ada").status == 200
    client.stop()


def test_dispatch_raise_server_errors_still_raises() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/boom")
    def boom() -> str:
        raise RuntimeError("kaboom")

    with TestClient(app) as client, pytest.raises(RuntimeError, match="kaboom"):
        client.get("/boom")


def test_dispatch_follows_redirects() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/old")
    def old() -> Response:
        return Response(status=302, headers={"Location": "/new"})

    @app.get("/new")
    def new() -> str:
        return "landed"

    with TestClient(app) as client:
        assert client.get("/old").text == "landed"
        assert client.get("/old", follow_redirects=False).status == 302


def test_async_client_dispatches_in_process() -> None:
    async def run() -> None:
        async with AsyncTestClient(_pipeline_app([])) as client:
            r = await client.get("/hi/ada", params={"q": "1"})
            assert r.status == 200
            assert r.json()["q"] == 1
            assert client.base_url == "http://testserver"

    asyncio.run(run())


def test_dispatch_cookies_and_headers_roundtrip() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/who")
    def who(
        session: Annotated[str | None, Cookie()] = None,
        api_key: Annotated[str | None, Header(alias="X-Api-Key")] = None,
    ) -> dict:
        return {"session": session, "api_key": api_key}

    with TestClient(app, cookies={"session": "s1"}, headers={"X-Api-Key": "k"}) as client:
        assert client.get("/who").json() == {"session": "s1", "api_key": "k"}


def test_dispatch_form_and_raw_bodies() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.post("/echo")
    def echo(request: Request) -> dict:
        return {"body": (request.body or b"").decode()}

    with TestClient(app) as client:
        r = client.post("/echo", data={"a": "1", "b": "2"})
        assert json.loads(r.text)["body"] == "a=1&b=2"
