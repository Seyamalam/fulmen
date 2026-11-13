"""`fulmen.testkit` — the Python adapter conformance runner (DESIGN.md §15.3).

`fulmen-testkit` on the Rust side ships one conformance suite per
adapter interface; this package is the Python-facing half: a pytest
plugin plus the bundled suites under `fulmen.testkit.suites`.

    pytest --fulmen-adapter=my_pkg:MyStore

imports `my_pkg:MyStore`, instantiates it (no arguments — a conformance
run configures its adapter through its own defaults), and collects the
suite modules. Every check runs against any class satisfying the
corresponding `Protocol` — a third-party backend proves it behaves like
the built-ins (TTL expiry, missing keys, lease/redelivery, rate-limit
windows, validator supports/compile agreement) by passing it.

Suites that need a parameter the interface cannot carry (the
`Validator` pair of supported/unsupported types) read the adapter's
optional `conformance_types` attribute; without it that suite skips.
"""

from __future__ import annotations

__all__ = ["run"]

import asyncio
from collections.abc import Awaitable


def run[T](awaitable: Awaitable[T]) -> T:
    """Drive one suite coroutine to completion — one `asyncio.run`
    per check, so every check gets its own loop and a suite run never
    carries loop state between checks."""
    return asyncio.run(awaitable)
