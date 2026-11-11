"""Shared fixtures for the real-server test suite."""

from __future__ import annotations

import urllib.error
import urllib.request
from collections.abc import Callable, Iterator

import pytest
from fulmen import Fulmen
from fulmen._fulmen import TestServer, serve_for_test


@pytest.fixture
def serve() -> Iterator[Callable[..., str]]:
    """Serve a `Fulmen` app on an ephemeral port; yield a `app -> base-url` factory."""
    servers: list[TestServer] = []

    def factory(app: Fulmen, uvloop: bool | None = None) -> str:
        server = serve_for_test(app._compile_native(), uvloop=uvloop)
        servers.append(server)
        return f"http://{server.addr()}"

    yield factory
    for server in servers:
        server.stop()


def fetch(url: str, method: str = "GET", data: bytes | None = None) -> HttpResponse:
    """Issue one HTTP/1.1 request with stdlib urllib and capture the response.

    Never raises for HTTP error statuses: the status, headers and body are
    captured into the returned `HttpResponse` either way. Raises for transport
    failures (connection refused, …)."""
    request = urllib.request.Request(url, data=data, method=method)  # noqa: S310 -- test client for local ephemeral servers
    try:
        with urllib.request.urlopen(request) as raw:  # noqa: S310 -- test client for local ephemeral servers
            return HttpResponse(raw.status, dict(raw.headers.items()), raw.read())
    except urllib.error.HTTPError as exc:
        return HttpResponse(exc.status, dict(exc.headers.items()), exc.read())


class HttpResponse:
    """A captured HTTP response: status, headers (lower-cased keys), body."""

    __slots__ = ("body", "headers", "status")

    def __init__(self, status: int, headers: dict[str, str], body: bytes) -> None:
        self.status = status
        self.headers = {name.lower(): value for name, value in headers.items()}
        self.body = body

    def header(self, name: str) -> str | None:
        """A header value by case-insensitive name, or `None`."""
        return self.headers.get(name.lower())
