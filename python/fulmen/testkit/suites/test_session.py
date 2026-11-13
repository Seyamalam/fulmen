"""`SessionStore` conformance: `load`/`save`/`delete` semantics and the
rolling TTL — what §16.5's sessions plugin relies on."""

from __future__ import annotations

from datetime import timedelta

from fulmen.adapters.interfaces import SessionStore
from fulmen.testkit import run


def test_save_then_load(session_store: SessionStore) -> None:
    async def check() -> None:
        await session_store.save("s1", b"payload", timedelta(seconds=60))
        assert await session_store.load("s1") == b"payload"

    run(check())


def test_load_missing_is_none(session_store: SessionStore) -> None:
    async def check() -> None:
        assert await session_store.load("missing-session") is None

    run(check())


def test_save_overwrites(session_store: SessionStore) -> None:
    async def check() -> None:
        await session_store.save("s2", b"first", timedelta(seconds=60))
        await session_store.save("s2", b"second", timedelta(seconds=60))
        assert await session_store.load("s2") == b"second"

    run(check())


def test_delete_drops_key(session_store: SessionStore) -> None:
    async def check() -> None:
        await session_store.save("s3", b"x", timedelta(seconds=60))
        await session_store.delete("s3")
        assert await session_store.load("s3") is None

    run(check())


def test_delete_absent_is_noop(session_store: SessionStore) -> None:
    async def check() -> None:
        await session_store.delete("never-existed")

    run(check())


def test_ttl_expires(session_store: SessionStore) -> None:
    import asyncio

    async def check() -> None:
        await session_store.save("s4", b"x", timedelta(milliseconds=30))
        await asyncio.sleep(0.06)
        assert await session_store.load("s4") is None

    run(check())


def test_save_refreshes_ttl(session_store: SessionStore) -> None:
    import asyncio

    async def check() -> None:
        ttl = timedelta(milliseconds=80)
        await session_store.save("s5", b"x", ttl)
        await asyncio.sleep(0.05)
        await session_store.save("s5", b"y", ttl)
        await asyncio.sleep(0.05)
        # 100ms since the first save but only 50ms since the last.
        assert await session_store.load("s5") == b"y"
        await asyncio.sleep(0.06)
        assert await session_store.load("s5") is None

    run(check())


def test_keys_are_independent(session_store: SessionStore) -> None:
    async def check() -> None:
        await session_store.save("s6a", b"a", timedelta(seconds=60))
        await session_store.save("s6b", b"b", timedelta(seconds=60))
        await session_store.delete("s6a")
        assert await session_store.load("s6a") is None
        assert await session_store.load("s6b") == b"b"

    run(check())
