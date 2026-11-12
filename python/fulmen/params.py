"""Parameter source markers (DESIGN.md §5.2, §11.2).

A marker declares *where* a handler parameter comes from plus optional
defaults and constraints. Both styles work::

    def search(q: Query[list[str]] = [], page: Annotated[int, Query(ge=1)] = 1): ...
    def user(user_id: Path[int]): ...
    def agent(user_agent: Header[str | None] = None): ...
    def key(x_api_key: Header[str, "X-Api-Key"]): ...

`Marker[T]` evaluates to `Annotated[T, Marker()]`, so annotations stay
readable to type checkers while the runtime still sees the marker.
"""

from __future__ import annotations

from typing import Annotated, Any, ClassVar, cast

from fulmen.constraints import UNSET


class Marker:
    """Base for source markers: `Path`, `Query`, `Header`, `Cookie`,
    `Body`, `Form`, `File`."""

    source: ClassVar[str] = "?"

    def __init__(
        self,
        default: Any = UNSET,
        *,
        alias: str | None = None,
        description: str | None = None,
        example: Any = UNSET,
        deprecated: bool = False,
        ge: float | None = None,
        gt: float | None = None,
        le: float | None = None,
        lt: float | None = None,
        min_length: int | None = None,
        max_length: int | None = None,
        min_items: int | None = None,
        max_items: int | None = None,
        pattern: str | None = None,
        multiple_of: float | None = None,
        unique: bool = False,
    ) -> None:
        self.default = default
        self.alias = alias
        self.description = description
        self.example = example
        self.deprecated = deprecated
        self.ge = ge
        self.gt = gt
        self.le = le
        self.lt = lt
        self.min_length = min_length
        self.max_length = max_length
        self.min_items = min_items
        self.max_items = max_items
        self.pattern = pattern
        self.multiple_of = multiple_of
        self.unique = unique

    @classmethod
    def __class_getitem__(cls, item: Any) -> Any:
        # `Query[int]` -> Annotated[int, Query()];
        # `Header[str, "X-Api-Key"]` -> Annotated[str, Header(alias="X-Api-Key")].
        if isinstance(item, tuple):
            hint, alias = cast("tuple[Any, Any]", item)
            if not isinstance(alias, str):
                raise TypeError(
                    f"{cls.__name__}[T, name]: the wire name must be a str, got {alias!r}"
                )
            return Annotated[hint, cls(alias=alias)]
        return Annotated[item, cls()]

    def __repr__(self) -> str:
        args = "" if self.default is UNSET else repr(self.default)
        return f"{type(self).__name__}({args})"


class Path(Marker):
    """A path-template parameter (extracted from the matched URL segment)."""

    source: ClassVar[str] = "path"


class Query(Marker):
    """A query-string parameter (`?page=2`)."""

    source: ClassVar[str] = "query"


class Header(Marker):
    """A request header parameter (`Header[str, "X-Api-Key"]` renames it)."""

    source: ClassVar[str] = "header"


class Cookie(Marker):
    """A request cookie parameter."""

    source: ClassVar[str] = "cookie"


class Body(Marker):
    """A request body parameter (a `Model`/dataclass/`TypedDict`/container)."""

    source: ClassVar[str] = "body"


class Form(Marker):
    """A `application/x-www-form-urlencoded` field (the form task)."""

    source: ClassVar[str] = "form"


class File(Marker):
    """A `multipart/form-data` file field producing `bytes` or `UploadFile`."""

    source: ClassVar[str] = "file"


class UploadFile:
    """The object a `File`/`multipart` parameter materializes.

    The extraction task fills this in; the class exists now so handler
    signatures can already declare `file: UploadFile`.
    """

    __slots__ = ("content", "content_type", "filename")

    def __init__(
        self,
        content: bytes,
        *,
        filename: str | None = None,
        content_type: str | None = None,
    ) -> None:
        self.content = content
        self.filename = filename
        self.content_type = content_type

    def __repr__(self) -> str:
        return f"UploadFile(filename={self.filename!r}, {len(self.content)} bytes)"


# The request object lives in `fulmen.request`; it is re-exported here so
# `from fulmen.params import Request` keeps working and `Request` stays one
# class wherever it is imported from.
from fulmen.request import Request as Request  # noqa: E402  pylint: disable=wrong-import-position
