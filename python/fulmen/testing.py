"""In-process test clients (DESIGN.md §22).

```python
from fulmen.testing import TestClient

def test_create_user():
    with TestClient(app) as client:
        r = client.post("/users", json={"name": "Ada"})
        assert r.status == 201
        assert r.json()["id"] > 0
```

`TestClient` compiles the app and serves it on an ephemeral loopback
port via the same `serve_for_test` path `run()` compiles to — every
request exercises the real router, hooks, extractors and error routing;
no socket-free `dispatch` entry point exists on the native side yet, so
loopback is the honest in-process stand-in for §22's dispatch drive
(tracked as a native follow-up). Startup hooks run on `start()` /
entering the `with` block, shutdown hooks on `stop()` / exiting.

With `raise_server_errors=True` (the default) an unhandled handler
exception is re-raised in the test instead of returning the `500`
problem response; registered `on_error` handlers still win and
`HTTPError`/`ValidationError` keep their normal rendering.

`AsyncTestClient` exposes the same surface as `async def` methods for
`asyncio.run` tests — requests run in a thread, so the app's own async
pipeline keeps driving handlers.
"""

from __future__ import annotations

import asyncio
import http.client
import json as _json
import typing
import urllib.parse
from collections.abc import Iterable, Mapping
from typing import Any

from fulmen._fulmen import TestServer, serve_for_test
from fulmen.app import Fulmen
from fulmen.constraints import UNSET
from fulmen.errors import HTTPError, ValidationError, problem_for
from fulmen.request import Headers
from fulmen.responses import Response

_CAPTURE_HEADER = "x-fulmen-test-error"
_REDIRECT_STATUSES = frozenset({301, 302, 303, 307, 308})
_MAX_REDIRECTS = 10

# Process-wide capture list: every `raise_server_errors` client registers
# the same `_capture` handler on an app, so marker indices must resolve
# no matter which client issued the request.
_CAPTURED: list[BaseException] = []


def _capture(exc: BaseException) -> Response:
    """`on_error` fallback that stashes 500-worthy exceptions.

    `HTTPError`/`ValidationError` reproduce `problem_for` exactly so the
    wire looks unchanged; anything else additionally carries a marker
    header pointing at the stashed exception for `request` to re-raise.
    """
    status, body, headers = problem_for(exc)
    if not isinstance(exc, HTTPError | ValidationError):
        _CAPTURED.append(exc)
        headers[_CAPTURE_HEADER] = str(len(_CAPTURED) - 1)
    return Response(
        body,
        status=status,
        headers=headers,
        content_type="application/problem+json",
    )


_Body = bytes | str | Mapping[str, Any] | None
_Params = Mapping[str, str] | Iterable[tuple[str, str]] | None


class TestResponse:
    """One response from a `TestClient` request."""

    __test__ = False  # not a pytest class
    __slots__ = ("body", "headers", "status")

    def __init__(self, status: int, headers: list[tuple[str, str]], body: bytes) -> None:
        self.status = status
        self.headers = Headers(headers)
        self.body = body

    @property
    def text(self) -> str:
        return self.body.decode("utf-8")

    def json(self) -> Any:
        return _json.loads(self.body)

    def header(self, name: str) -> str | None:
        """A response header by name, case-insensitive (`None` if absent)."""
        return self.headers.get(name)

    def __repr__(self) -> str:
        return f"TestResponse(status={self.status}, body={self.body!r})"


class TestClient:
    """Drive a `Fulmen` app in tests without standing up a real server.

    ```python
    with TestClient(app) as client:
        r = client.post("/users", json={"name": "Ada"})
        assert r.status == 201
    ```

    Requests go through the app's compiled pipeline on an ephemeral
    loopback server (`serve_for_test`), so routers, hooks, extractors
    and error routing behave exactly as in production. Startup and
    shutdown lifecycle hooks run on enter/exit — `start()`/`stop()` work
    outside a `with` block too.

    Args:
        app: The application under test.
        raise_server_errors: Re-raise unhandled handler exceptions in
            the test instead of returning the `500` problem response
            (default `True`). Exceptions routed to a registered
            `on_error` handler, and `HTTPError`/`ValidationError`
            problem responses, are never re-raised.
        headers: Headers sent on every request.
        cookies: Cookies sent on every request.
        follow_redirects: Follow `Location` redirects (default `True`).
        uvloop / debug: Forwarded to `serve_for_test`.
    """

    __test__ = False  # not a pytest class

    def __init__(
        self,
        app: Fulmen,
        *,
        raise_server_errors: bool = True,
        headers: Mapping[str, str] | None = None,
        cookies: Mapping[str, str] | None = None,
        follow_redirects: bool = True,
        uvloop: bool | None = None,
        debug: bool = False,
    ) -> None:
        self.app = app
        self.raise_server_errors = raise_server_errors
        self.headers = dict(headers) if headers else {}
        self.cookies = dict(cookies) if cookies else {}
        self.follow_redirects = follow_redirects
        self._uvloop = uvloop
        self._debug = debug
        self._server: TestServer | None = None
        # A `BaseException` catch-all loses MRO routing to every
        # user-registered `on_error`, so it only ever sees exceptions
        # that would render as the built-in problem response. It is
        # registered once per app no matter how many clients wrap it.
        if raise_server_errors and not any(
            handler is _capture
            for _c, handler, _a in app._error_handlers  # pyright: ignore[reportPrivateUsage]
        ):
            app.on_error(BaseException)(_capture)

    # -- lifecycle ----------------------------------------------------------

    @property
    def base_url(self) -> str:
        """`http://host:port` once started (`""` before)."""
        return f"http://{self._server.addr()}" if self._server is not None else ""

    def start(self) -> None:
        """Compile the app, run startup hooks and bind the test server."""
        if self._server is None:
            self._server = serve_for_test(
                self.app._compile_native(),  # pyright: ignore[reportPrivateUsage] - testing lives in-package, like conftest
                uvloop=self._uvloop,
                debug=self._debug,
            )

    def stop(self) -> None:
        """Shut the test server down, running shutdown hooks once."""
        if self._server is not None:
            self._server.stop()
            self._server = None

    def __enter__(self) -> TestClient:
        self.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop()

    # -- requests -----------------------------------------------------------

    def request(
        self,
        method: str,
        url: str,
        *,
        params: _Params = None,
        data: _Body = None,
        json: Any = UNSET,
        headers: Mapping[str, str] | None = None,
        cookies: Mapping[str, str] | None = None,
        follow_redirects: bool | None = None,
    ) -> TestResponse:
        """Send one HTTP request and return the `TestResponse`.

        `url` is a path (`"/users/42"`) or absolute URL on this client's
        server. `params` become the query string (list values repeat the
        key); `json` serializes the body as JSON; a `data` mapping sends
        `application/x-www-form-urlencoded`, `bytes`/`str` send as-is.
        An explicit `Content-Type` header always wins.
        """
        self.start()
        target, body, request_headers = self._prepare(url, params, data, json, headers, cookies)
        response = self._send(
            method.upper(),
            target,
            body,
            request_headers,
            self.follow_redirects if follow_redirects is None else follow_redirects,
            0,
        )
        marker = response.header(_CAPTURE_HEADER)
        if marker is not None and self.raise_server_errors:
            raise _CAPTURED[int(marker)]
        return response

    def get(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("GET", url, **kwargs)

    def post(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("POST", url, **kwargs)

    def put(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("PUT", url, **kwargs)

    def patch(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("PATCH", url, **kwargs)

    def delete(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("DELETE", url, **kwargs)

    def head(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("HEAD", url, **kwargs)

    def options(self, url: str, **kwargs: Any) -> TestResponse:
        return self.request("OPTIONS", url, **kwargs)

    # -- internals ----------------------------------------------------------

    def _prepare(
        self,
        url: str,
        params: _Params,
        data: _Body,
        json: Any,
        headers: Mapping[str, str] | None,
        cookies: Mapping[str, str] | None,
    ) -> tuple[str, bytes | None, dict[str, str]]:
        """Resolve `url` → `path?query`, the body bytes and merged headers."""
        target = url
        if target.startswith(("http://", "https://")):
            parts = urllib.parse.urlsplit(target)
            target = parts.path or "/"
            if parts.query:
                target = f"{target}?{parts.query}"
        if params:
            if isinstance(params, Mapping):
                query = urllib.parse.urlencode(typing.cast("Mapping[str, str]", params), doseq=True)
            else:
                query = urllib.parse.urlencode(list(params), doseq=True)
            target = f"{target}{'&' if '?' in target else '?'}{query}"

        request_headers: dict[str, str] = {}
        cookie_parts: list[str] = []
        for key, value in {**self.headers, **(headers or {})}.items():
            if key.lower() == "cookie":
                cookie_parts.append(value)
            else:
                request_headers[key] = value
        for key, value in {**self.cookies, **(cookies or {})}.items():
            cookie_parts.append(f"{key}={value}")
        if cookie_parts:
            request_headers["Cookie"] = "; ".join(cookie_parts)

        body: bytes | None = None
        content_type: str | None = None
        if json is not UNSET:
            body = _json.dumps(json).encode("utf-8")
            content_type = "application/json"
        elif data is not None:
            if isinstance(data, Mapping):
                body = urllib.parse.urlencode(data, doseq=True).encode("utf-8")
                content_type = "application/x-www-form-urlencoded"
            elif isinstance(data, str):
                body = data.encode("utf-8")
            else:
                body = data
        if (
            body is not None
            and content_type is not None
            and not any(key.lower() == "content-type" for key in request_headers)
        ):
            request_headers["Content-Type"] = content_type
        return target, body, request_headers

    def _send(
        self,
        method: str,
        target: str,
        body: bytes | None,
        headers: dict[str, str],
        follow_redirects: bool,
        redirects: int,
    ) -> TestResponse:
        """One round-trip over `http.client`, following redirects."""
        server = self._server
        if server is None:  # request() starts lazily, so this is unreachable
            raise RuntimeError("TestClient request before start()")
        host, _, port = server.addr().rpartition(":")
        conn = http.client.HTTPConnection(host, int(port), timeout=30)
        try:
            conn.request(method, target, body=body, headers=headers)
            raw = conn.getresponse()
            status, response_headers, response_body = (
                raw.status,
                raw.getheaders(),
                raw.read(),
            )
        finally:
            conn.close()

        location = next(
            (value for name, value in response_headers if name.lower() == "location"),
            None,
        )
        if (
            not follow_redirects
            or redirects >= _MAX_REDIRECTS
            or status not in _REDIRECT_STATUSES
            or location is None
        ):
            return TestResponse(status, response_headers, response_body)

        next_target = urllib.parse.urljoin(target, location)
        parts = urllib.parse.urlsplit(next_target)
        next_target = parts.path or "/"
        if parts.query:
            next_target = f"{next_target}?{parts.query}"
        if status == 303 or (status in (301, 302) and method not in ("GET", "HEAD")):
            method, body = "GET", None
        return self._send(method, next_target, body, headers, follow_redirects, redirects + 1)


class AsyncTestClient:
    """`TestClient`'s surface as `async def` for `asyncio.run` tests.

    ```python
    async with AsyncTestClient(app) as client:
        r = await client.get("/users")
    ```

    Requests run on a worker thread (`asyncio.to_thread`) against the
    same in-process server the sync client uses — lifecycle hooks and
    `raise_server_errors` behave identically.
    """

    __test__ = False  # not a pytest class

    def __init__(self, app: Fulmen, **kwargs: Any) -> None:
        self._client = TestClient(app, **kwargs)

    @property
    def app(self) -> Fulmen:
        return self._client.app

    @property
    def raise_server_errors(self) -> bool:
        return self._client.raise_server_errors

    @property
    def base_url(self) -> str:
        return self._client.base_url

    async def start(self) -> None:
        await asyncio.to_thread(self._client.start)

    async def stop(self) -> None:
        await asyncio.to_thread(self._client.stop)

    async def __aenter__(self) -> AsyncTestClient:
        await self.start()
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.stop()

    async def request(
        self,
        method: str,
        url: str,
        *,
        params: _Params = None,
        data: _Body = None,
        json: Any = UNSET,
        headers: Mapping[str, str] | None = None,
        cookies: Mapping[str, str] | None = None,
        follow_redirects: bool | None = None,
    ) -> TestResponse:
        return await asyncio.to_thread(
            self._client.request,
            method,
            url,
            params=params,
            data=data,
            json=json,
            headers=headers,
            cookies=cookies,
            follow_redirects=follow_redirects,
        )

    async def get(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("GET", url, **kwargs)

    async def post(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("POST", url, **kwargs)

    async def put(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("PUT", url, **kwargs)

    async def patch(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("PATCH", url, **kwargs)

    async def delete(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("DELETE", url, **kwargs)

    async def head(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("HEAD", url, **kwargs)

    async def options(self, url: str, **kwargs: Any) -> TestResponse:
        return await self.request("OPTIONS", url, **kwargs)
