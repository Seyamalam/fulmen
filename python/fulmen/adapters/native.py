"""The native `Validator` (DESIGN.md §15.2 "native" built-in).

The default validation backend: a Python `type`/`Annotated` annotation
→ spec tree (`SpecBuilder`, the `_typing.py` machinery) → compiled
`Descriptor` (`fulmen._fulmen.compile_descriptor`) → JSON Schema
(`openapi._SchemaGen`). Nothing runs through pydantic or any other
third-party validator — this is the pipeline annotations already take.

`conformance_types` is the testkit contract (§15.3): a
`(supported, unsupported)` pair the `Validator` suite exercises. Any
annotation translatable to a spec is supported; a bare callable object
is not a schema type.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from fulmen._fulmen import compile_descriptor
from fulmen._typing import AnnotationError, SpecBuilder

if TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

__all__ = ["NativeValidator"]


# `type_spec` rejects values that are not annotations — a function
# object is the canonical "not a schema type" probe.
def _unsupported_probe() -> Any:
    def _fn() -> None:
        pass

    return _fn


class NativeValidator:
    """`Validator` protocol (§15.1) over the native spec machinery.

    `supports(tp)` is true when `tp` translates to a spec tree — the
    same question annotation handling already answers.
    """

    conformance_types: tuple[Any, Any] = (int, _unsupported_probe())
    """(supported, unsupported) probe pair for the testkit's
    `Validator` suite (§15.3)."""

    @staticmethod
    def supports(tp: Any) -> bool:
        try:
            SpecBuilder().type_spec(tp)
        except AnnotationError:
            return False
        return True

    @staticmethod
    def spec(tp: Any) -> dict[str, Any]:
        """The spec tree `type_spec(tp)` produces for `tp`."""
        return SpecBuilder().type_spec(tp)

    def compile(self, tp: type[Any]) -> Descriptor:
        """Compile `tp` into a native `Descriptor`."""
        builder = SpecBuilder()
        return compile_descriptor(builder.type_spec(tp), builder.callbacks)

    def json_schema(self, tp: type[Any]) -> dict[str, Any]:
        """The spec-derived JSON Schema for `tp` (what OpenAPI emits)."""
        from fulmen.openapi import _SchemaGen  # pyright: ignore[reportPrivateUsage] -- package-internal reuse

        gen = _SchemaGen()
        schema = gen.schema(self.spec(tp))
        if gen.schemas:
            schema = {**schema, "components": {"schemas": gen.schemas}}
        return schema
