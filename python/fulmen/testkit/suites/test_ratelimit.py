"""`RateLimitStore` conformance: the allow/deny contract of §16.3 —
burst admission, denial carrying a `retry_after`, a denied call
consuming nothing, and per-key independence.

The suite uses millisecond rates so a real wait can prove retry
semantics without slowing the run."""

from __future__ import annotations

import asyncio
from datetime import timedelta

from fulmen.adapters.interfaces import Rate, RateLimitStore
from fulmen.testkit import run


def test_first_acquire_allowed(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        decision = await rate_limit_store.acquire(
            "rl-first", 1, Rate(count=10, per=timedelta(seconds=1))
        )
        assert decision.allowed
        assert decision.remaining < 10

    run(check())


def test_burst_then_denied(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=4, per=timedelta(milliseconds=400), burst=4)
        for attempt in range(4):
            decision = await rate_limit_store.acquire("rl-burst", 1, rate)
            assert decision.allowed, f"attempt {attempt} within burst must pass"
        denied = await rate_limit_store.acquire("rl-burst", 1, rate)
        assert not denied.allowed, "acquire past the burst must be denied"

    run(check())


def test_denied_reports_retry_after(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=1, per=timedelta(milliseconds=200), burst=1)
        await rate_limit_store.acquire("rl-retry", 1, rate)
        denied = await rate_limit_store.acquire("rl-retry", 1, rate)
        assert not denied.allowed
        assert timedelta(0) < denied.retry_after <= timedelta(milliseconds=400)

    run(check())


def test_denied_consumes_nothing(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=1, per=timedelta(milliseconds=60), burst=1)
        await rate_limit_store.acquire("rl-free", 1, rate)
        for _ in range(3):
            assert not (await rate_limit_store.acquire("rl-free", 1, rate)).allowed
        await asyncio.sleep(0.08)
        assert (await rate_limit_store.acquire("rl-free", 1, rate)).allowed, (
            "a denied call must not charge the budget"
        )

    run(check())


def test_retry_after_wait_allows(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=1, per=timedelta(milliseconds=50), burst=1)
        await rate_limit_store.acquire("rl-wait", 1, rate)
        denied = await rate_limit_store.acquire("rl-wait", 1, rate)
        assert not denied.allowed
        await asyncio.sleep(denied.retry_after.total_seconds() + 0.02)
        assert (await rate_limit_store.acquire("rl-wait", 1, rate)).allowed

    run(check())


def test_keys_are_independent(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=1, per=timedelta(milliseconds=500), burst=1)
        await rate_limit_store.acquire("rl-a", 1, rate)
        assert not (await rate_limit_store.acquire("rl-a", 1, rate)).allowed
        assert (await rate_limit_store.acquire("rl-b", 1, rate)).allowed, (
            "key b's budget must not be spent by key a"
        )

    run(check())


def test_cost_consumes_budget(rate_limit_store: RateLimitStore) -> None:
    async def check() -> None:
        rate = Rate(count=4, per=timedelta(milliseconds=400), burst=4)
        await rate_limit_store.acquire("rl-cost", 3, rate)
        assert (await rate_limit_store.acquire("rl-cost", 1, rate)).allowed
        assert not (await rate_limit_store.acquire("rl-cost", 1, rate)).allowed, (
            "cost 3 + cost 1 spends a burst of 4 — the next must deny"
        )

    run(check())
