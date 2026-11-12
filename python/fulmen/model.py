"""`fulmen.Model` — declared-data objects compiled in Rust (DESIGN.md §11.5).

A `Model` subclass's annotations become a `record` spec; the compiled
`Descriptor` validates the input dict in Rust and materializes the
instance field-by-field. `Field(...)` declarations, `Annotated` markers
and class kwargs (`frozen`, `extra`, `rename`) shape the spec.
"""

from __future__ import annotations

import dataclasses
from typing import TYPE_CHECKING, Any, ClassVar, TypeVar

from fulmen import _typing
from fulmen.constraints import Field

if TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

_T = TypeVar("_T")


class ModelConfig(dict[str, Any]):
    """The `__model_config__` dict type (`extra`, `frozen`, `rename`, `strict`)."""


class Model:
    """Base class for Fulmen's declared-data objects.

    ```python
    class User(Model):
        id: int
        name: Annotated[str, Len(min=1)]
        email: Email | None = None

    class Page(Model, Generic[T], extra="forbid"):
        items: list[T]
    ```
    """

    __fulmen_model__: ClassVar[bool] = False
    __descriptor__: ClassVar[Descriptor | None] = None
    __spec__: ClassVar[dict[str, Any] | None] = None
    __field_set__: ClassVar[frozenset[str]] = frozenset()
    __model_extra__: ClassVar[str] = "ignore"
    __model_frozen__: ClassVar[bool] = False
    __model_rename__: ClassVar[str | None] = None
    __model_strict__: ClassVar[bool] = True
    __model_config__: ClassVar[ModelConfig]

    @dataclasses.dataclass_transform(kw_only_default=True, field_specifiers=(Field,))
    def __init_subclass__(
        cls,
        *,
        frozen: bool = False,
        extra: str = "ignore",
        rename: str | None = None,
        strict: bool = True,
        **kwargs: Any,
    ) -> None:
        super().__init_subclass__(**kwargs)
        if extra not in ("ignore", "forbid", "keep"):
            raise TypeError(
                f"{cls.__qualname__}: extra must be 'ignore', 'forbid' or 'keep', got {extra!r}"
            )
        if rename not in (None, "camel", "pascal", "kebab"):
            raise TypeError(
                f"{cls.__qualname__}: rename must be 'camel', 'pascal' or 'kebab', got {rename!r}"
            )
        cls.__fulmen_model__ = True
        config = cls.__dict__.get("__model_config__") or {}
        cls.__model_frozen__ = bool(config.get("frozen", frozen))
        cls.__model_extra__ = str(config.get("extra", extra))
        cls.__model_rename__ = config.get("rename", rename)
        cls.__model_strict__ = bool(config.get("strict", strict))
        cls.__descriptor__ = None
        cls.__spec__ = None
        cls.__field_set__ = frozenset()
        if cls.__model_frozen__:
            # Frozen instances never change after construction → hashable.
            cls.__hash__ = _frozen_hash  # type: ignore[assignment]
        try:
            _typing.compile_model(cls)
        except NameError:
            # A forward ref to a not-yet-defined name: build lazily on
            # first use, when the module namespace is complete.
            cls.__descriptor__ = None

    def __init__(self, /, **kwargs: Any) -> None:
        cls = type(self)
        descriptor = _typing.model_descriptor(cls)
        descriptor.construct(self, kwargs, not cls.__model_strict__)

    @classmethod
    def model_validate(cls: type[_T], obj: Any) -> _T:
        """Validate `obj` (a dict or mapping) into a `cls` instance."""
        return _typing.model_descriptor(cls).validate(obj)  # type: ignore[return-value]

    def model_dump(self) -> dict[str, Any]:
        """The instance's field values as a plain dict."""
        return dict(vars(self))

    def __setattr__(self, name: str, value: Any) -> None:
        cls = type(self)
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return
        if cls.__model_frozen__:
            raise TypeError(f"{cls.__qualname__} is frozen")
        if name in cls.__field_set__:
            # Validated assignment: declared fields stay schema-valid (Q5/D19).
            value = _typing.model_descriptor(cls).validate_field(
                name, value, lax=not cls.__model_strict__
            )
        object.__setattr__(self, name, value)

    def __eq__(self, other: Any) -> bool:
        return type(self) is type(other) and vars(self) == vars(other)

    def __repr__(self) -> str:
        parts = ", ".join(f"{k}={v!r}" for k, v in vars(self).items())
        return f"{type(self).__qualname__}({parts})"

    __hash__: ClassVar[None] = None  # mutable models are unhashable


def _frozen_hash(self: Model) -> int:
    return hash(tuple(sorted(vars(self).items())))
