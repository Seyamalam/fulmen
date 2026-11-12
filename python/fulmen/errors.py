"""Errors (DESIGN.md §13.3): `HTTPError`, typed status shortcuts,
`ValidationError`, and the RFC 9457 `Problem` body model.

A handler that raises `HTTPError` (or a shortcut) gets an
`application/problem+json` response from the native dispatch path;
`@app.on_error(exc)` overrides the rendering per exception type.
"""

from __future__ import annotations

from http import HTTPStatus
from typing import TYPE_CHECKING, Any, ClassVar

if TYPE_CHECKING:
    from typing import NotRequired, TypedDict

    class ErrorDetail(TypedDict):
        loc: str
        code: str
        message: str
        source: str
        limit: NotRequired[Any]


class ValidationError(ValueError):
    """Schema validation failed. `entries` are the `ErrorEntry` dicts the
    Rust validator emits (`loc`, `code`, `message`, `source`, `limit?`)."""

    entries: list[ErrorDetail]
    truncated: bool

    def __init__(self, entries: list[ErrorDetail], truncated: bool = False) -> None:
        self.entries = entries
        self.truncated = truncated
        suffix = " (truncated)" if truncated else ""
        super().__init__(f"{len(entries)} validation error(s){suffix}")


class HTTPError[T](Exception):
    """An HTTP error → `application/problem+json` (DESIGN.md §13.3).

    ```python
    raise NotFound(detail="no user 7")
    raise HTTPError(418, "short and stout", type="https://x/teapot")
    ```
    """

    status_code: ClassVar[int] = 500

    def __init__(
        self,
        status: int | None = None,
        detail: str | None = None,
        *,
        type: str | None = None,
        title: str | None = None,
        instance: str | None = None,
        headers: dict[str, str] | None = None,
        errors: list[dict[str, Any]] | None = None,
    ) -> None:
        if status is not None:
            self.status = status
        else:
            self.status = self.__class__.status_code
        self.detail = detail
        self.type = type
        self.title = title
        self.instance = instance
        self.headers = dict(headers or {})
        self.errors = list(errors or [])
        super().__init__(detail or self.reason)

    @property
    def reason(self) -> str:
        try:
            return HTTPStatus(self.status).phrase
        except ValueError:
            return "Error"

    def problem(self, instance: str | None = None) -> dict[str, Any]:
        """The RFC 9457 body — the dispatch path serializes this."""
        body: dict[str, Any] = {
            "type": self.type or "about:blank",
            "title": self.title or self.reason,
            "status": self.status,
        }
        if self.detail is not None:
            body["detail"] = self.detail
        if self.instance or instance:
            body["instance"] = self.instance or instance
        if self.errors:
            body["errors"] = self.errors
        return body


# ---- client-error shortcuts (§13.3 list) --------------------------------------


class BadRequest[T](HTTPError[T]):
    status_code: ClassVar[int] = 400


class Unauthorized[T](HTTPError[T]):
    status_code: ClassVar[int] = 401


class Forbidden[T](HTTPError[T]):
    status_code: ClassVar[int] = 403


class NotFound[T](HTTPError[T]):
    status_code: ClassVar[int] = 404


class MethodNotAllowed[T](HTTPError[T]):
    status_code: ClassVar[int] = 405


class NotAcceptable[T](HTTPError[T]):
    status_code: ClassVar[int] = 406


class RequestTimeout[T](HTTPError[T]):
    status_code: ClassVar[int] = 408


class Conflict[T](HTTPError[T]):
    status_code: ClassVar[int] = 409


class Gone[T](HTTPError[T]):
    status_code: ClassVar[int] = 410


class LengthRequired[T](HTTPError[T]):
    status_code: ClassVar[int] = 411


class PreconditionFailed[T](HTTPError[T]):
    status_code: ClassVar[int] = 412


class PayloadTooLarge[T](HTTPError[T]):
    status_code: ClassVar[int] = 413


class URITooLong[T](HTTPError[T]):
    status_code: ClassVar[int] = 414


class UnsupportedMediaType[T](HTTPError[T]):
    status_code: ClassVar[int] = 415


class UnprocessableContent[T](HTTPError[T]):
    status_code: ClassVar[int] = 422


class TooManyRequests[T](HTTPError[T]):
    status_code: ClassVar[int] = 429


# ---- server-error shortcuts ----------------------------------------------------


class InternalServerError[T](HTTPError[T]):
    status_code: ClassVar[int] = 500


class NotImplemented[T](HTTPError[T]):
    status_code: ClassVar[int] = 501


class BadGateway[T](HTTPError[T]):
    status_code: ClassVar[int] = 502


class ServiceUnavailable[T](HTTPError[T]):
    status_code: ClassVar[int] = 503


class GatewayTimeout[T](HTTPError[T]):
    status_code: ClassVar[int] = 504


def problem_for(
    exc: BaseException, instance: str | None = None
) -> tuple[int, dict[str, Any], dict[str, str]]:
    """Any exception → `(status, problem-body, headers)`; non-HTTPError
    exceptions become a bare 500 that leaks nothing."""
    if isinstance(exc, HTTPError):
        return exc.status, exc.problem(instance), dict(exc.headers)
    if isinstance(exc, ValidationError):
        return (
            422,
            {
                "type": "about:blank",
                "title": "Unprocessable Content",
                "status": 422,
                "errors": [dict(e) for e in exc.entries],
            },
            {},
        )
    return (
        500,
        {
            "type": "about:blank",
            "title": "Internal Server Error",
            "status": 500,
        },
        {},
    )
