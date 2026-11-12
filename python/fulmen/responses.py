"""Response objects returned from handlers (DESIGN.md §10.2-§10.3).

`Response` is a plain data object: the Rust dispatcher reads `status`,
`headers`, `body` and `content_type`. A `dict`/`list`/`tuple`/`Model` body
is serialized as JSON; `str` as `text/plain; charset=utf-8`; `bytes` as
`application/octet-stream`; `None` sends an empty body. `content_type`
overrides the inference.

`Created[T]`/`Accepted[T]` carry a status plus the declared body type —
`def create() -> Created[User]` documents `201` with a `User` body.
`Stream`/`FileResponse` buffer their bodies today; native streaming lands
with the M5.x work (docs/DESIGN.md deviation note).
"""

from __future__ import annotations

import mimetypes
from collections.abc import AsyncIterator, Iterable
from pathlib import Path as _Path
from typing import Any, ClassVar


class Response:
    """An explicit HTTP response: `status`, `headers`, `body`.

    `headers` must be a `dict[str, str]`; `body` may be `str`, `bytes`,
    `dict`, `list`, `tuple`, a `Model`, or `None`.
    """

    __slots__ = ("body", "content_type", "headers", "status")

    #: The status this class encodes when used as a return annotation —
    #: `None` for the fully dynamic `Response`.
    status_code: ClassVar[int | None] = None
    status: int
    headers: dict[str, str]
    body: Any
    content_type: str | None

    def __init__(
        self,
        body: Any = None,
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
        content_type: str | None = None,
    ) -> None:
        self.body = body
        self.status = status
        self.headers = dict(headers) if headers else {}
        self.content_type = content_type

    def __repr__(self) -> str:
        return f"{type(self).__name__}(status={self.status}, body={self.body!r})"


class JSONResponse[T](Response):
    """`application/json` — the default for declared-body returns."""

    status_code: ClassVar[int | None] = 200

    def __init__(
        self,
        body: Any = None,
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
    ) -> None:
        super().__init__(body, status=status, headers=headers, content_type="application/json")


class TextResponse(Response):
    """`text/plain; charset=utf-8`."""

    status_code: ClassVar[int | None] = 200

    def __init__(
        self,
        body: Any = "",
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
    ) -> None:
        super().__init__(
            body, status=status, headers=headers, content_type="text/plain; charset=utf-8"
        )


class HTMLResponse(Response):
    """`text/html; charset=utf-8`."""

    status_code: ClassVar[int | None] = 200

    def __init__(
        self,
        body: Any = "",
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
    ) -> None:
        super().__init__(
            body, status=status, headers=headers, content_type="text/html; charset=utf-8"
        )


class Created[T](Response):
    """`201` with the declared body — `def create() -> Created[User]`."""

    status_code: ClassVar[int | None] = 201

    def __init__(self, body: Any = None, *, headers: dict[str, str] | None = None) -> None:
        super().__init__(body, status=201, headers=headers)


class Accepted[T](Response):
    """`202` with the declared body."""

    status_code: ClassVar[int | None] = 202

    def __init__(self, body: Any = None, *, headers: dict[str, str] | None = None) -> None:
        super().__init__(body, status=202, headers=headers)


class NoContent(Response):
    """`204` with no body."""

    status_code: ClassVar[int | None] = 204

    def __init__(self, *, headers: dict[str, str] | None = None) -> None:
        super().__init__(None, status=204, headers=headers)


class Redirect(Response):
    """`Location` redirect — `Redirect("/new", status=308)` (default 307)."""

    status_code: ClassVar[int | None] = 307

    def __init__(
        self,
        url: str,
        *,
        status: int = 307,
        headers: dict[str, str] | None = None,
    ) -> None:
        merged = dict(headers or {})
        merged["location"] = url
        super().__init__(None, status=status, headers=merged)


class FileResponse(Response):
    """A file body — `FileResponse("report.pdf")`.

    The file is read into memory (native sendfile/`Range`/`ETag` support
    is a later milestone — DESIGN.md §10.3).
    """

    status_code: ClassVar[int | None] = 200

    def __init__(
        self,
        path: str | _Path,
        *,
        filename: str | None = None,
        status: int = 200,
        headers: dict[str, str] | None = None,
        content_type: str | None = None,
    ) -> None:
        source = _Path(path)
        data = source.read_bytes()
        name = filename or source.name
        merged = dict(headers or {})
        merged.setdefault("content-disposition", f'attachment; filename="{name}"')
        guessed = content_type or mimetypes.guess_type(name)[0] or "application/octet-stream"
        super().__init__(data, status=status, headers=merged, content_type=guessed)


class Stream(Response):
    """A buffered stream body — `Stream(gen())` (DESIGN.md §10.3).

    `body` is a sync `Iterable` of `bytes`/`str`; the dispatcher joins the
    chunks. Async iterators and true chunked delivery land with native
    streaming (M5.x).
    """

    status_code: ClassVar[int | None] = 200

    def __init__(
        self,
        body: Iterable[bytes | str] | AsyncIterator[bytes | str],
        *,
        status: int = 200,
        headers: dict[str, str] | None = None,
        content_type: str | None = None,
    ) -> None:
        if hasattr(body, "__aiter__"):
            raise TypeError(
                "Stream(async_iterator) needs native streaming (M5.x) — "
                "buffer the chunks into bytes for now"
            )
        if not hasattr(body, "__iter__"):
            raise TypeError(f"Stream body must be a sync iterable, got {body!r}")
        super().__init__(body, status=status, headers=headers, content_type=content_type)
