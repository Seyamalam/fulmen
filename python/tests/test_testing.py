"""Test-client tests (DESIGN.md §22, milestone M3.21).

`TestClient`/`AsyncTestClient` drive an app through the same compiled
pipeline `run()` uses: every HTTP verb, JSON bodies, headers/cookies,
query params, lifecycle hooks on enter/exit, and `raise_server_errors`.
"""

from __future__ import annotations

import asyncio
from typing import Annotated

import pytest
from fulmen import (
    Cookie,
    Form,
    Fulmen,
    Header,
    NotFound,
    Query,
    Raw,
    Redirect,
    Request,
    Response,
    ValidationError,
)
from fulmen.testing import AsyncTestClient, TestClient


def _app() -> Fulmen:
    """A small app covering every verb + extraction source."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/users")
    def list_users() -> list[dict]:
        return [{"id": 1, "name": "Ada"}]

    @app.get("/users/{user_id}")
    def get_user(user_id: int) -> dict:
        return {"id": user_id, "name": "Ada"}

    @app.post("/users")
    def create_user(body: dict) -> Response:
        return Response({"id": 2, **body}, status=201)

    @app.put("/users/{user_id}")
    def replace_user(user_id: int, body: dict) -> dict:
        return {"id": user_id, **body}

    @app.patch("/users/{user_id}")
    def patch_user(user_id: int, body: dict) -> dict:
        return {"id": user_id, "patched": body}

    @app.delete("/users/{user_id}")
    def delete_user(user_id: int) -> Response:
        return Response(status=204)

    @app.head("/ping")
    def ping() -> Response:
        return Response(status=200, headers={"X-Pong": "yes"})

    @app.options("/users")
    def options_users() -> Response:
        return Response(status=200, headers={"Allow": "GET, POST"})

    @app.get("/echo-headers")
    def echo_headers(
        request: Request,
        api_key: Annotated[str | None, Header(alias="X-Api-Key")] = None,
    ) -> dict:
        return {
            "api_key": api_key,
            "custom": request.headers.get("x-test", ""),
        }

    @app.get("/echo-cookies")
    def echo_cookies(session: Annotated[str | None, Cookie()] = None) -> dict:
        return {"session": session}

    @app.get("/search")
    def search(q: str, page: int = 1) -> dict:
        return {"q": q, "page": page}

    return app


def _form_app() -> Fulmen:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.post("/form")
    def submit(name: Annotated[str, Form()], age: Annotated[int, Form()] = 0) -> dict:
        return {"name": name, "age": age}

    return app


class _HandledError(Exception):
    pass


def _boom_app() -> Fulmen:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/boom")
    def boom() -> str:
        raise RuntimeError("kaboom")

    @app.get("/notfound")
    def nf() -> str:
        raise NotFound(detail="gone")

    @app.get("/unprocessable")
    def up() -> str:
        raise ValidationError(
            [{"loc": "/body/x", "code": "invalid", "message": "bad x", "source": "body"}]
        )

    @app.get("/handled")
    def handled() -> str:
        raise _HandledError("handled by on_error")

    @app.on_error(_HandledError)
    def on_value(exc: _HandledError) -> Response:
        return Response({"handled": str(exc)}, status=400)

    return app


def _redirect_app() -> Fulmen:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/old")
    def old() -> Redirect:
        return Redirect("/new")

    @app.get("/new")
    def new() -> str:
        return "arrived"

    @app.post("/moved")
    def moved() -> Redirect:
        return Redirect("/new", status=303)

    return app


def test_every_method() -> None:
    """`.get/.post/.put/.patch/.delete/.head/.options` all round-trip."""
    with TestClient(_app()) as client:
        r = client.get("/users")
        assert r.status == 200
        assert r.json() == [{"id": 1, "name": "Ada"}]

        assert client.get("/users/42").json() == {"id": 42, "name": "Ada"}

        r = client.post("/users", json={"name": "Grace"})
        assert r.status == 201
        assert r.json() == {"id": 2, "name": "Grace"}

        assert client.put("/users/2", json={"name": "x"}).json() == {"id": 2, "name": "x"}
        assert client.patch("/users/2", json={"name": "x"}).json() == {
            "id": 2,
            "patched": {"name": "x"},
        }
        assert client.delete("/users/2").status == 204

        head = client.head("/ping")
        assert head.status == 200
        assert head.header("X-Pong") == "yes"
        assert head.body == b""

        options = client.options("/users")
        assert options.status == 200
        assert options.header("Allow") == "GET, POST"

        assert client.request("GET", "/users/1").status == 200


def test_headers_and_cookies() -> None:
    """Per-client and per-request headers/cookies reach the handler."""
    with TestClient(_app(), headers={"X-Api-Key": "k1"}, cookies={"session": "abc"}) as client:
        r = client.get("/echo-headers", headers={"X-Test": "v"})
        assert r.json() == {"api_key": "k1", "custom": "v"}
        r = client.get("/echo-cookies")
        assert r.json() == {"session": "abc"}
        # A request cookie merges with the client's, not replaces.
        r = client.get("/echo-cookies", cookies={"session": "override"})
        assert r.json() == {"session": "override"}


def test_query_params_and_form() -> None:
    """`params` build the query string; a `data` mapping posts a form."""
    with TestClient(_app()) as client:
        r = client.get("/search", params={"q": "fulmen", "page": "2"})
        assert r.json() == {"q": "fulmen", "page": 2}

    with TestClient(_form_app()) as client:
        r = client.post("/form", data={"name": "ada", "age": "36"})
        assert r.status == 200
        assert r.json() == {"name": "ada", "age": 36}


def test_text_and_raw_bodies() -> None:
    """`str`/`bytes` data sends as-is; `json=` sets content-type."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.post("/raw")
    def raw(request: Request) -> dict:
        return {
            "body": request.body.decode(),
            "content_type": request.headers.get("content-type", ""),
        }

    with TestClient(app) as client:
        r = client.post("/raw", data="plain")
        assert r.json()["body"] == "plain"
        r = client.post("/raw", json={"a": 1})
        assert r.json()["content_type"] == "application/json"
        # An explicit content-type always wins.
        r = client.post("/raw", json={"a": 1}, headers={"Content-Type": "application/vnd.api+json"})
        assert r.json()["content_type"] == "application/vnd.api+json"


def test_response_surface() -> None:
    """`status`/`json()`/`text`/`body`/`header()` expose the response."""
    with TestClient(_app()) as client:
        r = client.get("/users/1")
        assert r.status == 200
        assert r.json() == {"id": 1, "name": "Ada"}
        assert "Ada" in r.text
        assert '"id":1' in r.text or '"id": 1' in r.text
        assert r.header("content-type") == "application/json"
        assert r.header("no-such") is None
        assert "200" in repr(r)
        # The default transport dispatches in-process — no port exists.
        assert client.base_url == "http://testserver"


def test_404_and_405() -> None:
    """Router-level errors come back as statuses, not exceptions."""
    with TestClient(_app()) as client:
        assert client.get("/nope").status == 404
        assert client.post("/users/1").status == 405


def test_lifecycle_hooks() -> None:
    """Startup runs on enter, shutdown on exit — and only then."""
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
        client.get("/x")
        assert events == ["startup"]
    assert events == ["startup", "shutdown"]

    # `start()`/`stop()` drive the same lifecycle outside `with`.
    events.clear()
    client2 = TestClient(app)
    client2.start()
    assert events == ["startup"]
    client2.stop()
    assert events == ["startup", "shutdown"]


def test_lazy_start_on_first_request() -> None:
    """A bare request compiles and starts the server by itself."""
    app = Fulmen(openapi_url=None, docs_url=None)
    events: list[str] = []

    @app.on_startup
    def up() -> None:
        events.append("startup")

    @app.get("/x")
    def x() -> str:
        return "x"

    client = TestClient(app)
    assert events == []
    assert client.get("/x").status == 200
    assert events == ["startup"]
    client.stop()


def test_raise_server_errors() -> None:
    """Unhandled handler exceptions re-raise; `HTTPError`s don't."""
    with TestClient(_boom_app()) as client:
        with pytest.raises(RuntimeError, match="kaboom"):
            client.get("/boom")
        # Problem responses are wire behaviour, not server errors.
        assert client.get("/notfound").status == 404
        assert client.get("/unprocessable").status == 422
        # A registered on_error still wins over the capture fallback.
        r = client.get("/handled")
        assert r.status == 400
        assert r.json() == {"handled": "handled by on_error"}


def test_raise_server_errors_disabled() -> None:
    """`raise_server_errors=False` returns the 500 problem response."""
    with TestClient(_boom_app(), raise_server_errors=False) as client:
        r = client.get("/boom")
        assert r.status == 500
        assert r.json()["title"] == "Internal Server Error"
        assert r.header("content-type") == "application/problem+json"


def test_redirects() -> None:
    """Redirects follow by default; `follow_redirects=False` captures the 30x."""
    with TestClient(_redirect_app()) as client:
        r = client.get("/old")
        assert r.status == 200
        assert r.text == "arrived"

        r = client.get("/old", follow_redirects=False)
        assert r.status == 307
        assert r.header("location") == "/new"

        # 303 rewrites POST → GET.
        r = client.post("/moved")
        assert r.status == 200
        assert r.text == "arrived"


def test_async_client() -> None:
    """`AsyncTestClient` runs under `asyncio.run` with the same surface."""

    async def main() -> None:
        events: list[str] = []
        app = _app()

        @app.on_startup
        def up() -> None:
            events.append("startup")

        @app.on_shutdown
        def down() -> None:
            events.append("shutdown")

        async with AsyncTestClient(app) as client:
            assert events == ["startup"]
            r = await client.get("/users/7")
            assert r.status == 200
            assert r.json() == {"id": 7, "name": "Ada"}
            r = await client.post("/users", json={"name": "x"})
            assert r.status == 201
            r = await client.put("/users/1", json={"name": "y"})
            assert r.json() == {"id": 1, "name": "y"}
            r = await client.patch("/users/1", json={"a": 1})
            assert r.status == 200
            r = await client.delete("/users/1")
            assert r.status == 204
            r = await client.head("/ping")
            assert r.header("X-Pong") == "yes"
            r = await client.options("/users")
            assert r.status == 200
            r = await client.request("GET", "/search", params={"q": "z"})
            assert r.json() == {"q": "z", "page": 1}
        assert events == ["startup", "shutdown"]

    asyncio.run(main())


def test_async_raise_server_errors() -> None:
    """The async client re-raises handler exceptions too."""

    async def main() -> None:
        async with AsyncTestClient(_boom_app()) as client:
            with pytest.raises(RuntimeError, match="kaboom"):
                await client.get("/boom")

    asyncio.run(main())


# -- internals: capture fallback, request prep edges --------------------------------


def test_capture_renders_problem_responses() -> None:
    """`_capture` mirrors `problem_for` — `HTTPError` without a marker,
    anything else stashed behind the marker header."""
    from fulmen.testing import _CAPTURE_HEADER, _capture

    response = _capture(NotFound(detail="gone"))
    assert response.status == 404
    assert response.content_type == "application/problem+json"
    assert _CAPTURE_HEADER not in response.headers

    before = len(__import__("fulmen.testing", fromlist=["_CAPTURED"])._CAPTURED)
    response = _capture(RuntimeError("boom"))
    assert response.status == 500
    marker = response.headers[_CAPTURE_HEADER]
    assert int(marker) == before


def test_stop_before_start_is_a_noop() -> None:
    client = TestClient(_app())
    client.stop()  # never started — nothing to shut down


def test_send_without_start_raises() -> None:
    client = TestClient(_app())
    with pytest.raises(RuntimeError, match="request before start"):
        client._send("GET", "/", None, {}, False, 0)


def test_absolute_url_on_this_server() -> None:
    """An absolute URL to the client's own host is split to path+query."""
    with TestClient(_app()) as client:
        response = client.get(f"{client.base_url}/users/3?ignored=1")
        assert response.status == 200
        assert response.json() == {"id": 3, "name": "Ada"}


def test_params_as_pair_list_repeats_keys() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/pairs")
    def pairs(q: Query[list[str]]) -> list[str]:
        return q

    with TestClient(app) as client:
        response = client.get("/pairs", params=[("q", "a"), ("q", "b")])
        assert response.json() == ["a", "b"]


def test_explicit_cookie_header_merges_with_client_cookies() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/cookies")
    def cookies(request: Request) -> dict:
        return {"cookie": request.headers.get("cookie", "")}

    with TestClient(app, cookies={"session": "s1"}) as client:
        response = client.get("/cookies", headers={"Cookie": "extra=e2"})
        assert response.json()["cookie"] == "extra=e2; session=s1"


def test_bytes_data_posts_as_is() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.post("/raw")
    def raw(body: Raw) -> dict:
        return {"length": len(body)}

    with TestClient(app) as client:
        assert client.post("/raw", data=b"\x00\x01\x02").json() == {"length": 3}


def test_redirect_location_with_query_is_followed() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/old")
    def old() -> Redirect:
        return Redirect("/new?from=old")

    @app.get("/new")
    def new(from_: Annotated[str, Query(alias="from")]) -> dict:
        return {"from": from_}

    with TestClient(app) as client:
        assert client.get("/old").json() == {"from": "old"}


def test_async_client_properties() -> None:
    client = AsyncTestClient(_app())
    assert client.app is not None
    assert client.raise_server_errors is True
    assert client.base_url == ""  # not started yet


def test_absolute_url_without_query() -> None:
    """An absolute URL with no query still splits to just the path."""
    with TestClient(_app()) as client:
        response = client.get(f"{client.base_url}/users/3")
        assert response.status == 200
        assert response.json() == {"id": 3, "name": "Ada"}
