"""M1.18: free-threaded CPython.

On a free-threaded build, importing `fulmen` must not re-enable the GIL,
and the sync pool must overlap blocking handlers for real. Both tests also
pass on regular builds — the suite runs them on every interpreter; the
`test-matrix` recipe adds the `sys._is_gil_enabled()` assertion for the
3.14t leg.
"""

from __future__ import annotations

import sys
import threading
from collections.abc import Callable

import pytest
from conftest import fetch
from fulmen import Fulmen

Serve = Callable[..., str]


def test_import_does_not_enable_the_gil() -> None:
    """`fulmen` already imported by the suite must not have re-enabled the
    GIL on a free-threaded interpreter."""
    is_gil_enabled = getattr(sys, "_is_gil_enabled", None)
    if is_gil_enabled is None:
        pytest.skip("interpreter predates the free-threaded build")
    assert is_gil_enabled() is False


def test_blocking_handlers_overlap(serve: Serve) -> None:
    """Two requests must be inside their handlers at the same time.

    `threading.Barrier(2)` deadlocks if the pool serializes them; on a
    free-threaded build this is real parallelism, on a GIL build it at
    least proves the GIL is released while a handler blocks.
    """
    barrier = threading.Barrier(2)
    app = Fulmen()

    @app.get("/meet")
    def meet() -> str:
        barrier.wait(timeout=10)
        return "met"

    base = serve(app)
    statuses: list[int] = []
    threads = [
        threading.Thread(target=lambda: statuses.append(fetch(f"{base}/meet").status))
        for _ in range(2)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=15)
    assert statuses == [200, 200]
