"""`CacheStore` conformance: `get`/`set`/`delete` semantics and TTL —
what the response-cache plugin relies on.

Eviction is deliberately not in the suite: the bound is a constructor
choice outside the protocol's contract, so a third-party store answers
it under its own policy."""

from __future__ import annotations

from datetime import timedelta

from fulmen.adapters.interfaces import CacheStore
from fulmen.testkit import run


def test_set_then_get(cache_store: CacheStore) -> None:
    async def check() -> None:
        await cache_store.set("k1", b"value", None)
        assert await cache_store.get("k1") == b"value"

    run(check())


def test_get_missing_is_none(cache_store: CacheStore) -> None:
    async def check() -> None:
        assert await cache_store.get("missing-cache") is None

    run(check())


def test_set_overwrites(cache_store: CacheStore) -> None:
    async def check() -> None:
        await cache_store.set("k2", b"first", None)
        await cache_store.set("k2", b"second", None)
        assert await cache_store.get("k2") == b"second"

    run(check())


def test_delete_drops_key(cache_store: CacheStore) -> None:
    async def check() -> None:
        await cache_store.set("k3", b"x", None)
        await cache_store.delete("k3")
        assert await cache_store.get("k3") is None

    run(check())


def test_delete_absent_is_noop(cache_store: CacheStore) -> None:
    async def check() -> None:
        await cache_store.delete("never-cached")

    run(check())


def test_ttl_expires(cache_store: CacheStore) -> None:
    import asyncio

    async def check() -> None:
        await cache_store.set("k4", b"x", timedelta(milliseconds=30))
        await asyncio.sleep(0.06)
        assert await cache_store.get("k4") is None

    run(check())


def test_no_ttl_persists(cache_store: CacheStore) -> None:
    import asyncio

    async def check() -> None:
        await cache_store.set("k5", b"x", None)
        await asyncio.sleep(0.05)
        assert await cache_store.get("k5") == b"x"

    run(check())
