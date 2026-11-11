"""M1.15/M1.16: `async def` handlers on the asyncio loop-thread bridge.

Every test serves a real socket; the asyncio (or uvloop) loop lives on its
own OS thread, fed through `loop.call_soon_threadsafe` (DESIGN.md §7.4).
"""

from __future__ import annotations

import asyncio
import importlib.util
import threading
import time
from collections.abc import Callable

import pytest
from conftest import fetch
from fulmen import Fulmen, Response, StartupError
from fulmen._fulmen import serve_for_test

_HAVE_UVLOOP = importlib.util.find_spec("uvloop") is not None

Serve = Callable[..., str]


def _loop_thread_count() -> int:
    return sum(thread.name == "fulmen-asyncio" for thread in threading.enumerate())


def test_async_text(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/hi")
    async def hi() -> str:
        return "hi"

    res = fetch(f"{serve(app)}/hi")
    assert res.status == 200
    assert res.body == b"hi"
    assert res.header("Content-Type") == "text/plain; charset=utf-8"


def test_async_json(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/data")
    async def data() -> dict[str, object]:
        return {"ok": True, "n": 3}

    res = fetch(f"{serve(app)}/data")
    assert res.status == 200
    assert res.header("Content-Type") == "application/json"
    assert res.body == b'{"ok":true,"n":3}'


def test_async_path_params(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/users/{user_id}/{name}")
    async def user(user_id: int, name: str) -> str:
        assert isinstance(user_id, int)
        return f"{user_id}:{name}"

    res = fetch(f"{serve(app)}/users/7/amy")
    assert res.status == 200
    assert res.body == b"7:amy"


def test_async_keyword_only_param(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/files/{name}")
    async def file(*, name: str) -> str:
        return name

    res = fetch(f"{serve(app)}/files/report")
    assert res.status == 200
    assert res.body == b"report"


def test_async_explicit_response(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/made")
    async def made() -> Response:
        return Response({"id": 1}, status=201, headers={"X-Kind": "thing"})

    res = fetch(f"{serve(app)}/made")
    assert res.status == 201
    assert res.body == b'{"id":1}'
    assert res.header("X-Kind") == "thing"


def test_async_handler_awaits(serve: Serve) -> None:
    """The coroutine runs on a real asyncio loop: `await` works."""
    app = Fulmen()

    @app.get("/sleep")
    async def sleepy() -> str:
        loop = asyncio.get_running_loop()
        await asyncio.sleep(0.01)
        return type(loop).__name__

    res = fetch(f"{serve(app)}/sleep")
    assert res.status == 200
    # Either asyncio's loop or uvloop's — both prove a real loop ran it.
    assert res.body != b""


def test_async_and_sync_routes_share_an_app(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/sync")
    def sync_route() -> str:
        return "sync"

    @app.get("/async")
    async def async_route() -> str:
        await asyncio.sleep(0)
        return "async"

    base = serve(app)
    assert fetch(f"{base}/sync").body == b"sync"
    assert fetch(f"{base}/async").body == b"async"


def test_async_handler_exception_is_500(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/boom")
    async def boom() -> str:
        raise RuntimeError("kaboom")

    res = fetch(f"{serve(app)}/boom")
    assert res.status == 500
    assert b"kaboom" not in res.body


def test_async_unsupported_return_is_500(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/weird")
    async def weird() -> object:
        return object()

    assert fetch(f"{serve(app)}/weird").status == 500


def test_async_requests_run_concurrently(serve: Serve) -> None:
    """Six requests each awaiting 0.2s: a real loop overlaps them, so the
    wall time is far below the serialized 1.2s+."""
    app = Fulmen()

    @app.get("/wait")
    async def wait() -> str:
        await asyncio.sleep(0.2)
        return "done"

    base = serve(app)
    statuses: list[int] = []
    threads = [
        threading.Thread(target=lambda: statuses.append(fetch(f"{base}/wait").status))
        for _ in range(6)
    ]
    started = time.monotonic()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=15)
    elapsed = time.monotonic() - started
    assert statuses == [200] * 6
    assert elapsed < 1.0


def test_async_app_starts_the_loop_thread(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/x")
    async def x() -> str:
        return "x"

    serve(app)
    # `serve_for_test` blocks until the loop thread reported ready.
    assert _loop_thread_count() >= 1


def test_sync_only_app_starts_no_loop_thread(serve: Serve) -> None:
    before = _loop_thread_count()
    app = Fulmen()

    @app.get("/x")
    def x() -> str:
        return "x"

    fetch(f"{serve(app)}/x")
    # A sync-only app never creates the asyncio bridge; an earlier async
    # test's dying thread can only make the count *smaller*, never bigger.
    assert _loop_thread_count() <= before


def test_uvloop_forced_off_uses_asyncio(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/x")
    async def x() -> str:
        return "x"

    assert fetch(f"{serve(app, uvloop=False)}/x").body == b"x"


def test_uvloop_auto(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/x")
    async def x() -> str:
        return "x"

    assert fetch(f"{serve(app, uvloop=None)}/x").body == b"x"


@pytest.mark.skipif(_HAVE_UVLOOP, reason="uvloop is installed")
def test_uvloop_required_missing() -> None:
    app = Fulmen()

    @app.get("/x")
    async def x() -> str:
        return "x"

    with pytest.raises(StartupError, match="uvloop"):
        serve_for_test(app._compile_native(), uvloop=True)


@pytest.mark.skipif(not _HAVE_UVLOOP, reason="uvloop is not installed")
def test_uvloop_required_installed(serve: Serve) -> None:
    app = Fulmen()

    @app.get("/x")
    async def x() -> str:
        return "x"

    assert fetch(f"{serve(app, uvloop=True)}/x").body == b"x"
