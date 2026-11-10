"""Response objects returned from handlers.

`Response` is a plain data object: the Rust dispatcher reads `status`,
`headers` and `body` and encodes them itself. A `dict`/`list`/`tuple` body is
serialized as JSON; `str` as `text/plain; charset=utf-8`; `bytes` as
`application/octet-stream`; `None` sends an empty body.
"""

from __future__ import annotations

from typing import Any


class Response:
    """An explicit HTTP response: `status`, `headers`, `body`.

    `headers` must be a `dict[str, str]`; `body` may be `str`, `bytes`,
    `dict`, `list`, `tuple` or `None`.
    """

    __slots__ = ("body", "headers", "status")

    status: int
    headers: dict[str, str]
    body: Any

    def __init__(
        self,
        body: Any = None,
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
    ) -> None:
        self.body = body
        self.status = status
        self.headers = dict(headers) if headers else {}

    def __repr__(self) -> str:
        return f"Response(status={self.status}, body={self.body!r})"
