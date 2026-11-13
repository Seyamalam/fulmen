"""The `--fulmen-adapter` pytest plugin (DESIGN.md §15.3).

Registered as the `fulmen-testkit` `pytest11` entry point, so an
installed `fulmen` makes this available everywhere:

    pytest --fulmen-adapter=my_pkg:MyStore

* `pytest_load_initial_conftests` rewrites raw argv *before* `testpaths`
  kicks in: when the adapter flag is given and no positional path
  exists on disk, the bundled `suites/` directory is injected so a bare
  `pytest --fulmen-adapter=...` collects exactly the conformance
  suites.
* The `fulmen_adapter` fixture resolves `pkg:Cls` → a no-arg instance.
* Per-interface fixtures (`session_store`, `cache_store`,
  `rate_limit_store`, `job_queue`, `validator`) hand the suite tests a
  conforming adapter — or `skip` it when the class doesn't satisfy that
  protocol, so one flag runs every suite a given adapter can.

Without an installed entry point (a bare source tree), `-p
fulmen.testkit.plugin` registers the plugin manually.
"""

from __future__ import annotations

import importlib
from pathlib import Path
from typing import Any

import pytest

_SUITES = Path(__file__).parent / "suites"

#: `Validator` adapters declare (supported, unsupported) probe types
#: under this attribute; see `suites/test_validator.py`.
CONFORMANCE_TYPES_ATTR = "conformance_types"


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--fulmen-adapter",
        metavar="PKG:CLASS",
        default=None,
        help=(
            "run the fulmen-testkit conformance suites against the "
            "adapter class PKG:CLASS (imported, then instantiated with "
            "no arguments)"
        ),
    )


def pytest_load_initial_conftests(args: list[str], **_kwargs: Any) -> None:
    """Inject the bundled suites dir into argv before collection paths
    are resolved — only when `--fulmen-adapter` is given and no
    positional arg names an existing path (so `pytest --fulmen-adapter`
    runs the suites, while `-k`/`-m` filters and explicit paths still
    work)."""
    remaining: list[str] = []
    spec: str | None = None
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--fulmen-adapter" and i + 1 < len(args):
            spec = args[i + 1]
            i += 2
            continue
        if arg.startswith("--fulmen-adapter="):
            spec = arg.partition("=")[2]
            i += 1
            continue
        remaining.append(arg)
        i += 1
    if spec is None:
        return
    has_path = any(not a.startswith("-") and Path(a).exists() for a in remaining)
    if not has_path:
        remaining.append(str(_SUITES))
    remaining.append(f"--fulmen-adapter={spec}")
    args[:] = remaining


def load_adapter(spec: str) -> Any:
    """`pkg:Cls` (or `pkg:outer.Cls`) → a no-arg instance.

    The adapter configures itself through its own defaults — a
    conformance run cannot pass constructor arguments.
    """
    module_name, sep, qualname = spec.partition(":")
    if not sep or not module_name or not qualname:
        raise pytest.UsageError(f"--fulmen-adapter expects `pkg:Class`, got {spec!r}")
    module = importlib.import_module(module_name)
    target: Any = module
    for part in qualname.split("."):
        target = getattr(target, part)
    if not isinstance(target, type):
        raise pytest.UsageError(f"{spec!r} does not resolve to a class")
    return target()


@pytest.fixture
def fulmen_adapter(request: pytest.FixtureRequest) -> Any:
    """The adapter instance the suites run against."""
    spec = request.config.getoption("fulmen_adapter")
    if spec is None:
        pytest.skip("no --fulmen-adapter given")
    return load_adapter(spec)


@pytest.fixture
def session_store(fulmen_adapter: Any) -> Any:
    """`fulmen_adapter` narrowed to `SessionStore` — suites skip
    otherwise."""
    from fulmen.adapters.interfaces import SessionStore

    if not isinstance(fulmen_adapter, SessionStore):
        pytest.skip(f"{type(fulmen_adapter).__name__} is not a SessionStore")
    return fulmen_adapter


@pytest.fixture
def cache_store(fulmen_adapter: Any) -> Any:
    """`fulmen_adapter` narrowed to `CacheStore`."""
    from fulmen.adapters.interfaces import CacheStore

    if not isinstance(fulmen_adapter, CacheStore):
        pytest.skip(f"{type(fulmen_adapter).__name__} is not a CacheStore")
    return fulmen_adapter


@pytest.fixture
def rate_limit_store(fulmen_adapter: Any) -> Any:
    """`fulmen_adapter` narrowed to `RateLimitStore`."""
    from fulmen.adapters.interfaces import RateLimitStore

    if not isinstance(fulmen_adapter, RateLimitStore):
        pytest.skip(f"{type(fulmen_adapter).__name__} is not a RateLimitStore")
    return fulmen_adapter


@pytest.fixture
def job_queue(fulmen_adapter: Any) -> Any:
    """`fulmen_adapter` narrowed to `JobQueue`."""
    from fulmen.adapters.interfaces import JobQueue

    if not isinstance(fulmen_adapter, JobQueue):
        pytest.skip(f"{type(fulmen_adapter).__name__} is not a JobQueue")
    return fulmen_adapter


@pytest.fixture
def validator(fulmen_adapter: Any) -> tuple[Any, Any, Any]:
    """`(adapter, supported, unsupported)` for the `Validator` suite —
    the pair comes from the adapter's `conformance_types` attribute."""
    from fulmen.adapters.interfaces import Validator

    if not isinstance(fulmen_adapter, Validator):
        pytest.skip(f"{type(fulmen_adapter).__name__} is not a Validator")
    types = getattr(fulmen_adapter, CONFORMANCE_TYPES_ATTR, None)
    if types is None:
        pytest.skip(
            f"{type(fulmen_adapter).__name__} does not declare "
            f"{CONFORMANCE_TYPES_ATTR} = (supported, unsupported)"
        )
    supported, unsupported = types
    return fulmen_adapter, supported, unsupported
