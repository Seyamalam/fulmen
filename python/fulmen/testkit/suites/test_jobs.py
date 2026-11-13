"""`JobQueue` conformance: §19.2's at-least-once contract —
enqueue/dequeue FIFO, delivery leases, `ack` finality, `nack`
redelivery (immediate and `retry_at`-deferred), `attempts` counting,
and unique-key dedup.

Waits stay under ~200 ms; the lease test uses a `Job`-level
`visibility` so it needs no store-constructor knowledge."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta

import pytest

from fulmen.adapters.interfaces import AdapterError, Job, JobId, JobQueue
from fulmen.testkit import run


def test_enqueue_returns_distinct_ids(job_queue: JobQueue) -> None:
    async def check() -> None:
        a = await job_queue.enqueue(Job(queue="q", payload=b"1"))
        b = await job_queue.enqueue(Job(queue="q", payload=b"2"))
        assert a != b
        assert str(a)

    run(check())


def test_dequeue_fifo_per_queue(job_queue: JobQueue) -> None:
    async def check() -> None:
        await job_queue.enqueue(Job(queue="fifo", payload=b"one"))
        await job_queue.enqueue(Job(queue="fifo", payload=b"two"))
        first = await job_queue.dequeue(["fifo"], timedelta(seconds=2))
        second = await job_queue.dequeue(["fifo"], timedelta(seconds=2))
        assert first is not None
        assert second is not None
        assert first.payload == b"one"
        assert second.payload == b"two"
        assert first.id is not None
        assert second.id is not None
        await job_queue.ack(first.id)
        await job_queue.ack(second.id)

    run(check())


def test_dequeue_selects_queues(job_queue: JobQueue) -> None:
    async def check() -> None:
        await job_queue.enqueue(Job(queue="select-a", payload=b""))
        early = await job_queue.dequeue(["select-b"], timedelta(milliseconds=30))
        assert early is None
        job = await job_queue.dequeue(["select-a"], timedelta(seconds=2))
        assert job is not None
        assert job.id is not None
        await job_queue.ack(job.id)

    run(check())


def test_dequeue_times_out(job_queue: JobQueue) -> None:
    async def check() -> None:
        start = asyncio.get_running_loop().time()
        none = await job_queue.dequeue(["empty"], timedelta(milliseconds=50))
        assert none is None
        assert asyncio.get_running_loop().time() - start >= 0.04

    run(check())


def test_dequeue_waits_for_enqueue(job_queue: JobQueue) -> None:
    async def check() -> None:
        async def producer() -> None:
            await asyncio.sleep(0.04)
            await job_queue.enqueue(Job(queue="waiter", payload=b"9"))

        task = asyncio.create_task(producer())
        job = await job_queue.dequeue(["waiter"], timedelta(seconds=2))
        assert job is not None
        assert job.payload == b"9"
        assert job.id is not None
        await job_queue.ack(job.id)
        await task

    run(check())


def test_payload_roundtrip(job_queue: JobQueue) -> None:
    async def check() -> None:
        payload = b"\x00\xffbinary-payload"
        await job_queue.enqueue(Job(queue="payload", payload=payload))
        job = await job_queue.dequeue(["payload"], timedelta(seconds=2))
        assert job is not None
        assert job.payload == payload
        assert job.id is not None
        await job_queue.ack(job.id)

    run(check())


def test_ack_is_final(job_queue: JobQueue) -> None:
    async def check() -> None:
        await job_queue.enqueue(Job(queue="ack", payload=b""))
        job = await job_queue.dequeue(["ack"], timedelta(seconds=2))
        assert job is not None
        assert job.id is not None
        await job_queue.ack(job.id)
        none = await job_queue.dequeue(["ack"], timedelta(milliseconds=30))
        assert none is None

    run(check())


def test_unacked_job_redelivers(job_queue: JobQueue) -> None:
    async def check() -> None:
        job = Job(queue="lease", payload=b"", visibility=timedelta(milliseconds=40))
        await job_queue.enqueue(job)
        first = await job_queue.dequeue(["lease"], timedelta(seconds=2))
        assert first is not None
        # Never ack — let the lease lapse.
        await asyncio.sleep(0.08)
        second = await job_queue.dequeue(["lease"], timedelta(seconds=2))
        assert second is not None
        assert second.id == first.id, "the same job must come back"
        assert second.id is not None
        await job_queue.ack(second.id)

    run(check())


def test_nack_redelivers_immediately(job_queue: JobQueue) -> None:
    async def check() -> None:
        await job_queue.enqueue(Job(queue="nack-now", payload=b""))
        first = await job_queue.dequeue(["nack-now"], timedelta(seconds=2))
        assert first is not None
        assert first.id is not None
        await job_queue.nack(first.id, None)
        second = await job_queue.dequeue(["nack-now"], timedelta(seconds=2))
        assert second is not None
        assert second.id == first.id
        assert second.id is not None
        await job_queue.ack(second.id)

    run(check())


def test_nack_retry_at_delays(job_queue: JobQueue) -> None:
    async def check() -> None:
        await job_queue.enqueue(Job(queue="nack-later", payload=b""))
        first = await job_queue.dequeue(["nack-later"], timedelta(seconds=2))
        assert first is not None
        assert first.id is not None
        retry_at = datetime.now(UTC) + timedelta(milliseconds=80)
        await job_queue.nack(first.id, retry_at)
        early = await job_queue.dequeue(["nack-later"], timedelta(milliseconds=30))
        assert early is None, "nacked job must not deliver before retry_at"
        second = await job_queue.dequeue(["nack-later"], timedelta(milliseconds=200))
        assert second is not None
        assert second.id == first.id
        assert second.id is not None
        await job_queue.ack(second.id)

    run(check())


def test_attempts_counts_deliveries(job_queue: JobQueue) -> None:
    async def check() -> None:
        job = Job(queue="attempts", payload=b"", visibility=timedelta(milliseconds=40))
        await job_queue.enqueue(job)
        first = await job_queue.dequeue(["attempts"], timedelta(seconds=2))
        assert first is not None
        assert first.attempts == 1
        await asyncio.sleep(0.08)
        second = await job_queue.dequeue(["attempts"], timedelta(seconds=2))
        assert second is not None
        assert second.attempts >= 2, "attempts must count deliveries"
        assert second.id is not None
        await job_queue.ack(second.id)

    run(check())


def test_scheduled_job_not_early(job_queue: JobQueue) -> None:
    async def check() -> None:
        job = Job(
            queue="scheduled",
            payload=b"",
            run_at=datetime.now(UTC) + timedelta(milliseconds=60),
        )
        await job_queue.enqueue(job)
        early = await job_queue.dequeue(["scheduled"], timedelta(milliseconds=20))
        assert early is None, "a scheduled job must not deliver early"
        delivered = await job_queue.dequeue(["scheduled"], timedelta(milliseconds=200))
        assert delivered is not None
        assert delivered.id is not None
        await job_queue.ack(delivered.id)

    run(check())


def test_unique_dedups_enqueue(job_queue: JobQueue) -> None:
    async def check() -> None:
        first = await job_queue.enqueue(Job(queue="uniq", payload=b"", unique="key-1"))
        dupe = await job_queue.enqueue(Job(queue="uniq", payload=b"", unique="key-1"))
        assert first == dupe, "a live unique key must dedup, not duplicate"
        job = await job_queue.dequeue(["uniq"], timedelta(seconds=2))
        assert job is not None
        assert job.id is not None
        assert await job_queue.dequeue(["uniq"], timedelta(milliseconds=30)) is None
        await job_queue.ack(job.id)
        third = await job_queue.enqueue(Job(queue="uniq", payload=b"", unique="key-1"))
        assert third != first
        job = await job_queue.dequeue(["uniq"], timedelta(seconds=2))
        assert job is not None
        assert job.id is not None
        await job_queue.ack(job.id)

    run(check())


def test_finished_lease_errors(job_queue: JobQueue) -> None:
    async def check() -> None:
        job_id = await job_queue.enqueue(Job(queue="known", payload=b""))
        job = await job_queue.dequeue(["known"], timedelta(seconds=2))
        assert job is not None
        assert job.id is not None
        await job_queue.ack(job.id)
        with pytest.raises(AdapterError):
            await job_queue.ack(job_id)
        with pytest.raises(AdapterError):
            await job_queue.nack(job_id, None)

    run(check())


def test_jobid_is_a_str() -> None:
    """`JobId` is a `str` newtype — usable anywhere a str id is."""
    job_id = JobId("job-7")
    assert isinstance(job_id, str)
    assert job_id == "job-7"
