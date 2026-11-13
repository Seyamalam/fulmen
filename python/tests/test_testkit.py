"""Direct tests for the `fulmen-testkit` pytest plugin (DESIGN.md §15.3).

The plugin module loads as a `pytest11` entry point before pytest-cov
starts tracing, so driving its fixtures through real collection can't
cover them. Calling the wrapped fixture functions with a fake
`FixtureRequest` exercises the same skip/narrow logic the suites rely
on.
"""

from __future__ import annotations

import typing
from types import SimpleNamespace

import pytest
from fulmen.testkit import plugin


class _Config(SimpleNamespace):
    """`request.config` stand-in exposing `getoption` like pytest does."""

    def __init__(self, spec: str | None) -> None:
        self.spec = spec

    def getoption(self, name: str) -> str | None:
        assert name == "fulmen_adapter"
        return self.spec


def _request(spec: str | None) -> typing.Any:
    return SimpleNamespace(config=_Config(spec))


def _adapter_fixture(request: typing.Any) -> typing.Any:
    return plugin.fulmen_adapter._get_wrapped_function()(request)


# -- load_adapter ------------------------------------------------------------


def test_load_adapter_requires_pkg_class() -> None:
    with pytest.raises(pytest.UsageError, match="pkg:Class"):
        plugin.load_adapter("no-colon")
    with pytest.raises(pytest.UsageError, match="pkg:Class"):
        plugin.load_adapter(":OnlyClass")
    with pytest.raises(pytest.UsageError, match="pkg:Class"):
        plugin.load_adapter("only_module:")


def test_load_adapter_rejects_non_class() -> None:
    with pytest.raises(pytest.UsageError, match="does not resolve to a class"):
        plugin.load_adapter("fulmen.testkit.plugin:load_adapter")


def test_load_adapter_resolves_dotted_qualname() -> None:
    adapter = plugin.load_adapter("fulmen.adapters.native:NativeValidator")
    assert type(adapter).__name__ == "NativeValidator"


# -- argv rewriting ----------------------------------------------------------


def test_argv_rewrite_injects_suites_dir() -> None:
    args = ["-q", "--fulmen-adapter=fulmen.adapters.native:NativeValidator"]
    plugin.pytest_load_initial_conftests(args)
    assert any("suites" in arg for arg in args)
    assert args[-1] == "--fulmen-adapter=fulmen.adapters.native:NativeValidator"


def test_argv_rewrite_space_separated_flag() -> None:
    args = ["--fulmen-adapter", "fulmen.adapters.native:NativeValidator"]
    plugin.pytest_load_initial_conftests(args)
    assert any("suites" in arg for arg in args)


def test_argv_rewrite_keeps_explicit_paths() -> None:
    args = ["--fulmen-adapter=pkg:Cls", "python/tests"]
    plugin.pytest_load_initial_conftests(args)
    assert not any(arg.endswith("suites") for arg in args)


def test_argv_rewrite_no_flag_is_a_noop() -> None:
    args = ["-q", "python/tests"]
    plugin.pytest_load_initial_conftests(args)
    assert args == ["-q", "python/tests"]


def test_argv_rewrite_flag_without_value_is_ignored() -> None:
    args = ["--fulmen-adapter"]
    plugin.pytest_load_initial_conftests(args)
    assert args == ["--fulmen-adapter"]


# -- fixtures ----------------------------------------------------------------


def test_fulmen_adapter_fixture_skips_without_flag() -> None:
    with pytest.raises(pytest.skip.Exception):
        _adapter_fixture(_request(None))


def test_fulmen_adapter_fixture_loads_the_spec() -> None:
    adapter = _adapter_fixture(_request("fulmen.adapters.native:NativeValidator"))
    assert type(adapter).__name__ == "NativeValidator"


def test_protocol_fixtures_skip_nonconforming_adapters() -> None:
    """A `NativeValidator` satisfies `Validator` but none of the store
    protocols — each narrowing fixture skips exactly that adapter."""
    adapter = _adapter_fixture(_request("fulmen.adapters.native:NativeValidator"))
    for name in ("session_store", "cache_store", "rate_limit_store", "job_queue"):
        fixture = getattr(plugin, name)._get_wrapped_function()
        with pytest.raises(pytest.skip.Exception, match="is not a"):
            fixture(adapter)


def test_protocol_fixtures_pass_conforming_adapters() -> None:
    from fulmen.adapters.memory import (
        MemoryCacheStore,
        MemoryJobQueue,
        MemoryRateLimitStore,
        MemorySessionStore,
    )

    cases = [
        (plugin.session_store, MemorySessionStore()),
        (plugin.cache_store, MemoryCacheStore()),
        (plugin.rate_limit_store, MemoryRateLimitStore()),
        (plugin.job_queue, MemoryJobQueue()),
    ]
    for fixture_fn, adapter in cases:
        assert fixture_fn._get_wrapped_function()(adapter) is adapter


def test_validator_fixture_skips_non_validators() -> None:
    """An adapter that isn't a `Validator` never reaches the
    `conformance_types` check — the protocol guard skips first."""
    from fulmen.adapters.memory import MemorySessionStore

    with pytest.raises(pytest.skip.Exception, match="is not a Validator"):
        plugin.validator._get_wrapped_function()(MemorySessionStore())


def test_validator_fixture_yields_conformance_types() -> None:
    adapter = _adapter_fixture(_request("fulmen.adapters.native:NativeValidator"))
    narrowed, supported, unsupported = plugin.validator._get_wrapped_function()(adapter)
    assert narrowed is adapter
    assert supported is not unsupported


def test_validator_fixture_skips_without_conformance_types() -> None:
    from fulmen.adapters.interfaces import Validator

    class BareValidator:
        def supports(self, tp: typing.Any) -> bool:
            return True

        def compile(self, tp: typing.Any) -> typing.Any:
            raise NotImplementedError

        def json_schema(self, tp: typing.Any) -> dict[str, typing.Any]:
            return {}

    assert isinstance(BareValidator(), Validator)
    with pytest.raises(pytest.skip.Exception, match="conformance_types"):
        plugin.validator._get_wrapped_function()(BareValidator())


# -- the suite runner ---------------------------------------------------------


def test_run_drives_one_awaitable_to_completion() -> None:
    """`fulmen.testkit.run` is the suites' per-check `asyncio.run`."""
    from fulmen.testkit import run

    async def check() -> int:
        return 42

    assert run(check()) == 42


def test_native_validator_json_schema_components() -> None:
    """`json_schema` attaches the components block when the spec
    references named schemas."""
    from fulmen import Model
    from fulmen.adapters.native import NativeValidator

    class Addr(Model):
        street: str

    schema = NativeValidator().json_schema(Addr)
    assert "components" in schema
    assert any(name.endswith("Addr") for name in schema["components"]["schemas"])


def test_generic_spec_if_pydantic_ignores_plain_generics() -> None:
    """A non-pydantic `G[int]` is not the pydantic generic seam."""
    import typing

    from fulmen._typing import SpecBuilder
    from fulmen.adapters.pydantic import generic_spec_if_pydantic

    T = typing.TypeVar("T")

    class G(typing.Generic[T]):
        pass

    assert generic_spec_if_pydantic(SpecBuilder(), G, G[int], "t") is None
