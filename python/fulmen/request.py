"""The request object (DESIGN.md §10.1, §8).

`Request` is injected into handlers that declare `request: Request` and
passed to every lifecycle hook — decision-phase hooks receive
`hook(request)`, post-handler phases `hook(request, response)`. It is a
read-mostly snapshot built once per request and shared by every hook and
the handler: mutating `state` (or returning a `dict` from a hook, which
merges into it) carries context between phases.
"""

from __future__ import annotations

import json
from collections.abc import Iterator, Mapping


class Headers(Mapping[str, str]):
    """A read-only, case-insensitive view of the request headers.

    Wire headers arrive as `(name, value)` pairs; lookup is
    case-insensitive and duplicate names join with `", "` (RFC 9110
    list semantics).
    """

    __slots__ = ("_items",)

    def __init__(self, items: list[tuple[str, str]]) -> None:
        merged: dict[str, str] = {}
        for name, value in items:
            key = name.lower()
            merged[key] = value if key not in merged else f"{merged[key]}, {value}"
        self._items = merged

    def __getitem__(self, name: str) -> str:
        return self._items[name.lower()]

    def __iter__(self) -> Iterator[str]:
        return iter(self._items)

    def __len__(self) -> int:
        return len(self._items)

    def __repr__(self) -> str:
        return f"Headers({self._items!r})"


class Request:
    """One incoming HTTP request.

    Attributes:
        method: The HTTP method (`"GET"`, `"POST"`, …).
        path: The decoded request path (`"/users/42"`).
        query_string: The raw query string without `?` (`""` when absent).
        headers: The request headers as a case-insensitive mapping.
        body: The raw request body.
        remote_addr: The peer address as `"ip:port"`.
        state: The per-request context dict. Hooks may write into it
            directly or return a `dict`, which merges here; handlers and
            later hooks read the accumulated context.
    """

    __slots__ = (
        "body",
        "headers",
        "method",
        "path",
        "query_string",
        "remote_addr",
        "state",
    )

    def __init__(
        self,
        method: str,
        path: str,
        query_string: str,
        headers: Headers,
        body: bytes,
        remote_addr: str,
    ) -> None:
        self.method = method
        self.path = path
        self.query_string = query_string
        self.headers = headers
        self.body = body
        self.remote_addr = remote_addr
        self.state: dict[str, object] = {}

    @classmethod
    def _from_wire(
        cls,
        method: str,
        path: str,
        query: str,
        headers: list[tuple[str, str]],
        body: bytes,
        remote: str,
    ) -> Request:
        """Build the request object from the wire snapshot.

        Called by the Rust bridge — not public API.
        """
        return cls(method, path, query, Headers(list(headers)), bytes(body), remote)

    def json(self) -> object:
        """Decode `body` as JSON (raises `json.JSONDecodeError` on bad input)."""
        return json.loads(self.body)

    def __repr__(self) -> str:
        return f"Request({self.method} {self.path})"
