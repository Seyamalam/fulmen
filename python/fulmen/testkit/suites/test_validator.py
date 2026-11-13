"""`Validator` conformance: `supports`/`compile`/`json_schema`
agreement — a supported type compiles to a spec tree and produces a
schema; an unsupported one errors instead of silently accepting.

The `validator` fixture yields `(adapter, supported, unsupported)` —
the pair comes from the adapter's `conformance_types` attribute; the
suite skips without it."""

from __future__ import annotations

from typing import Any

import pytest

from fulmen.adapters.interfaces import AdapterError, Validator


def test_supports_declares(validator: tuple[Validator, Any, Any]) -> None:
    adapter, supported, unsupported = validator
    assert adapter.supports(supported), f"supports({supported!r}) must be true for the claimed type"
    assert not adapter.supports(unsupported), (
        f"supports({unsupported!r}) must be false for the unclaimed type"
    )


def test_compile_supported_gives_descriptor(validator: tuple[Validator, Any, Any]) -> None:
    adapter, supported, _ = validator
    descriptor = adapter.compile(supported)
    assert descriptor is not None


def test_compile_unsupported_errors(validator: tuple[Validator, Any, Any]) -> None:
    adapter, supported, unsupported = validator
    # Consistency the other way: supported must compile.
    assert adapter.compile(supported) is not None
    with pytest.raises((AdapterError, ValueError, TypeError, RuntimeError)):
        adapter.compile(unsupported)


def test_json_schema_is_dict(validator: tuple[Validator, Any, Any]) -> None:
    adapter, supported, _ = validator
    schema = adapter.json_schema(supported)
    assert isinstance(schema, dict)
