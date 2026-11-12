"""`Annotated` constraint and metadata markers (DESIGN.md §5.3, §11.2).

Every marker is a small immutable value placed inside `Annotated[...]`::

    age: Annotated[int, Range(ge=0)]
    name: Annotated[str, Len(min=1), Strip()]
    email: Annotated[str, Description("login contact")]

`Field(...)` additionally works as a declared default on `Model`
attributes and handler parameters — it is the dataclass-transform field
specifier for `Model`.
"""

from __future__ import annotations

import dataclasses
from collections.abc import Callable
from decimal import Decimal
from typing import Any, Final

#: Sentinel for "no default declared" (a plain `object` — never a value).
UNSET: Final = object()

#: Numeric bound domains: `int`, `float`, or `Decimal` literal.
Num = int | float | Decimal


@dataclasses.dataclass(frozen=True)
class Len:
    """`Len(min, max)` — length bound for `str`, `bytes` and containers."""

    min: int | None = None
    max: int | None = None


@dataclasses.dataclass(frozen=True)
class Range:
    """`Range(ge, gt, le, lt)` — numeric bounds for `int`/`float`/`Decimal`."""

    ge: Num | None = None
    gt: Num | None = None
    le: Num | None = None
    lt: Num | None = None


@dataclasses.dataclass(frozen=True)
class Pattern:
    """`Pattern(regex)` — a regular expression checked on `str` values."""

    pattern: str


@dataclasses.dataclass(frozen=True)
class MultipleOf:
    """`MultipleOf(n)` — the value must be a multiple of `n`."""

    value: Num


@dataclasses.dataclass(frozen=True)
class Unique:
    """`Unique()` — all items of a `list` must compare distinct."""

    enabled: bool = True


class Strip:
    """`Strip` — trim leading/trailing whitespace before validation."""

    __slots__ = ()


class Lower:
    """`Lower` — `str.lower()` before validation."""

    __slots__ = ()


class Upper:
    """`Upper` — `str.upper()` before validation."""

    __slots__ = ()


@dataclasses.dataclass(frozen=True)
class Default:
    """`Default(v)` — the value an absent parameter/field falls back to."""

    value: Any


@dataclasses.dataclass(frozen=True)
class Alias:
    """`Alias(name)` — the wire name of the parameter/field."""

    name: str


@dataclasses.dataclass(frozen=True)
class Description:
    """`Description(text)` — documentation for `OpenAPI`."""

    text: str


@dataclasses.dataclass(frozen=True)
class Example:
    """`Example(v)` — an example value for `OpenAPI`."""

    value: Any


class Deprecated:
    """`Deprecated` — marks the parameter/field deprecated in `OpenAPI`."""

    __slots__ = ()


@dataclasses.dataclass(frozen=True)
class AfterValidator:
    """`AfterValidator(fn)` — `fn(value) -> value` runs on the materialized
    Python object after the schema's own checks pass."""

    fn: Callable[[Any], Any]


@dataclasses.dataclass(frozen=True)
class Discriminator:
    """`Discriminator(field)` — the tag field of a discriminated union."""

    field: str


@dataclasses.dataclass(frozen=True)
class _FieldSpec:
    """What `Field(...)` returns — merged into the param/field spec."""

    default: Any = UNSET
    default_factory: Callable[[], Any] | None = None
    alias: str | None = None
    description: str | None = None
    example: Any = UNSET
    deprecated: bool = False
    ge: Num | None = None
    gt: Num | None = None
    le: Num | None = None
    lt: Num | None = None
    min_length: int | None = None
    max_length: int | None = None
    min_items: int | None = None
    max_items: int | None = None
    pattern: str | None = None
    multiple_of: Num | None = None
    unique: bool = False


def Field(
    default: Any = UNSET,
    *,
    default_factory: Callable[[], Any] | None = None,
    alias: str | None = None,
    description: str | None = None,
    example: Any = UNSET,
    deprecated: bool = False,
    ge: Num | None = None,
    gt: Num | None = None,
    le: Num | None = None,
    lt: Num | None = None,
    min_length: int | None = None,
    max_length: int | None = None,
    min_items: int | None = None,
    max_items: int | None = None,
    pattern: str | None = None,
    multiple_of: Num | None = None,
    unique: bool = False,
) -> Any:
    """Declare a field default and/or constraints (DESIGN.md §5.3, §11.5).

    `x: int = Field(5)` declares a default; `x: list[str] =
    Field(default_factory=list)` a per-instance factory; constraint
    keywords behave like their `Annotated` marker equivalents.
    """
    if default is not UNSET and default_factory is not None:
        raise ValueError("Field() cannot take both `default` and `default_factory`")
    return _FieldSpec(
        default=default,
        default_factory=default_factory,
        alias=alias,
        description=description,
        example=example,
        deprecated=deprecated,
        ge=ge,
        gt=gt,
        le=le,
        lt=lt,
        min_length=min_length,
        max_length=max_length,
        min_items=min_items,
        max_items=max_items,
        pattern=pattern,
        multiple_of=multiple_of,
        unique=unique,
    )
