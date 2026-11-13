"""In-memory adapter implementations (DESIGN.md §15.2).

The reference backends — the same four stores `fulmen-core` provides in
Rust, for apps and tests that stay on the Python side or want a Python
adapter to read against.

Threading/GIL, honestly:

* `MemorySessionStore`, `MemoryCacheStore`, `MemoryRateLimitStore` —
  every method takes a `threading.Lock` for its whole read-modify-write,
  so they are correct on the GIL and on free-threaded builds alike. The
  coroutines resolve on first poll (no suspension inside).
* `MemoryJobQueue` — single event loop, like every asyncio primitive:
  mutation happens synchronously between awaits, so no lock is needed;
  `dequeue` waits poll on a ≤10 ms cadence instead of holding a
  condition, which keeps the queue usable from any loop that calls it —
  but all calls of one store must share a process. Contents live in
  process memory and are lost on restart; the redis adapters (M4.15)
  share state across workers.
"""

from __future__ import annotations

import asyncio
import dataclasses
import itertools
import threading
import time
from collections import OrderedDict, deque
from collections.abc import Sequence
from datetime import UTC, datetime, timedelta

from fulmen.adapters.interfaces import (
    CacheStore,
    Decision,
    Job,
    JobId,
    JobQueue,
    NotFoundError,
    Rate,
    RateLimitStore,
    SessionStore,
    StoreFullError,
)

__all__ = [
    "MemoryCacheStore",
    "MemoryJobQueue",
    "MemoryRateLimitStore",
    "MemorySessionStore",
]

# Default bound on entries/jobs — a flooding client cannot grow the maps
# without limit (§3: secure defaults).
_DEFAULT_CAPACITY = 10_000
_DEFAULT_MAX_KEYS = 128_000
_DEFAULT_JOB_CAPACITY = 65_536
# Idle `dequeue` waiters re-check eligibility at most this often.
_DEQUEUE_POLL_S = 0.01


class _ExpiringLru:
    """An `OrderedDict` LRU with per-entry deadlines — the shared core
    of `MemorySessionStore` and `MemoryCacheStore`.

    `get`/`set` promote entries to most-recently-used; expiry is lazy:
    a dead entry is dropped on access and opportunistically when an
    insert needs room, so a store of only-expired entries never evicts
    a live one. NOT internally synchronized — callers hold the store's
    lock.
    """

    def __init__(self, capacity: int) -> None:
        self._cap = max(1, capacity)
        self._map: OrderedDict[str, tuple[bytes, float | None]] = OrderedDict()

    def get(self, key: str) -> bytes | None:
        entry = self._map.get(key)
        if entry is None:
            return None
        value, deadline = entry
        if deadline is not None and deadline <= time.monotonic():
            del self._map[key]
            return None
        self._map.move_to_end(key)
        return value

    def set(self, key: str, value: bytes, ttl: timedelta | None) -> None:
        deadline = None if ttl is None else time.monotonic() + ttl.total_seconds()
        self._map[key] = (value, deadline)
        self._map.move_to_end(key)
        if len(self._map) > self._cap:
            self._purge_expired()
        while len(self._map) > self._cap:
            self._map.popitem(last=False)

    def delete(self, key: str) -> None:
        self._map.pop(key, None)

    def _purge_expired(self) -> None:
        now = time.monotonic()
        for key, (_, deadline) in list(self._map.items()):
            if deadline is not None and deadline <= now:
                del self._map[key]


class MemorySessionStore(SessionStore):
    """§15.2's `memory` session backend: an in-process bounded LRU.

    Lock-guarded (`threading.Lock` around each operation) — safe to hit
    from any thread or loop; contents are process-local and lost on
    restart. A second process never sees these sessions — use the redis
    adapter (M4.15) for shared or durable sessions.
    """

    def __init__(self, capacity: int = _DEFAULT_CAPACITY) -> None:
        self._map = _ExpiringLru(capacity)
        self._lock = threading.Lock()

    async def load(self, session_id: str) -> bytes | None:
        with self._lock:
            return self._map.get(session_id)

    async def save(self, session_id: str, data: bytes, ttl: timedelta) -> None:
        with self._lock:
            self._map.set(session_id, data, ttl)

    async def delete(self, session_id: str) -> None:
        with self._lock:
            self._map.delete(session_id)


class MemoryCacheStore(CacheStore):
    """§15.2's `memory` cache backend: an in-process bounded LRU.

    Same locking and process-locality contract as
    `MemorySessionStore`.
    """

    def __init__(self, capacity: int = _DEFAULT_CAPACITY) -> None:
        self._map = _ExpiringLru(capacity)
        self._lock = threading.Lock()

    async def get(self, key: str) -> bytes | None:
        with self._lock:
            return self._map.get(key)

    async def set(self, key: str, value: bytes, ttl: timedelta | None) -> None:
        with self._lock:
            self._map.set(key, value, ttl)

    async def delete(self, key: str) -> None:
        with self._lock:
            self._map.delete(key)


class MemoryRateLimitStore(RateLimitStore):
    """§15.2's `memory` rate-limit backend: in-process GCRA — one
    theoretical arrival time (a `monotonic_ns` timestamp) per key.

    A decision is a lock, one dict lookup and integer math; `Denied`
    leaves the key's TAT untouched, so denied calls never charge the
    budget. The map is bounded (`max_keys`); keys whose TAT has passed
    are reclaimed first, then the most-drained budget. Limits are
    per-process — the redis adapter gives a cluster-wide budget.
    """

    def __init__(self, max_keys: int = _DEFAULT_MAX_KEYS) -> None:
        self._tats: dict[str, int] = {}
        self._max_keys = max(1, max_keys)
        self._lock = threading.Lock()

    async def acquire(self, key: str, cost: int, rate: Rate) -> Decision:
        with self._lock:
            return self._acquire_inner(key, cost, rate, time.monotonic_ns())

    def _acquire_inner(self, key: str, cost: int, rate: Rate, now: int) -> Decision:
        interval = rate.per / rate.count  # GCRA emission interval T, as timedelta
        interval_ns = _ns(interval)
        tolerance_ns = interval_ns * (rate.burst if rate.burst is not None else rate.count)
        charge_ns = interval_ns * cost

        stored = self._tats.get(key, now)
        base = max(stored, now)
        new_tat = base + charge_ns
        ahead = max(0, new_tat - now)

        if ahead <= tolerance_ns:
            self._insert_bounded(key, new_tat)
            return Decision.allow(
                remaining=(tolerance_ns - ahead) // interval_ns,
                reset_after=timedelta(microseconds=ahead / 1_000),
            )
        return Decision.deny(
            retry_after=timedelta(microseconds=(ahead - tolerance_ns) / 1_000),
            reset_after=timedelta(microseconds=max(0, stored - now) / 1_000),
        )

    def _insert_bounded(self, key: str, tat: int) -> None:
        if len(self._tats) < self._max_keys or key in self._tats:
            self._tats[key] = tat
            return
        now = time.monotonic_ns()
        for k in [k for k, t in self._tats.items() if t <= now]:
            del self._tats[k]
        if len(self._tats) >= self._max_keys:
            victim = min(self._tats, key=lambda k: self._tats[k])
            del self._tats[victim]
        self._tats[key] = tat


def _ns(delta: timedelta) -> int:
    """`timedelta` → nanoseconds, exact."""
    return (delta.days * 86_400 + delta.seconds) * 1_000_000_000 + delta.microseconds * 1_000


@dataclasses.dataclass(slots=True)
class _Entry:
    """A pending job plus its global enqueue order."""

    seq: int
    job: Job
    run_at_mono: float | None  # `run_at` converted to loop-monotonic time


class MemoryJobQueue(JobQueue):
    """§15.2's `in-process` job backend: a per-queue FIFO with
    at-least-once delivery leases.

    Single-loop, single-process: all mutation runs synchronously inside
    the caller's loop, so nothing interleaves and no lock is needed —
    but the store only works where `asyncio` runs, and every call must
    share the process. `dequeue` wakes on a ≤10 ms poll rather than a
    condition variable: slightly coarser wakeup in exchange for working
    on whatever loop calls it, with no loop-binding surprises across
    tests. Lease expiries and `run_at`/`retry_at` boundaries are checked
    on the same poll, so deadlines still land on time.

    Delivery semantics (§19.2): a dequeued job is leased for
    `job.visibility` (or the store default); `ack` removes it for good,
    `nack` requeues it (immediately or at `retry_at`), and a lapsed
    lease redelivers it with `attempts` bumped. `unique` keys dedup
    against pending + in-flight jobs; `capacity` bounds tracked jobs
    with `StoreFullError` backpressure.
    """

    def __init__(
        self,
        default_visibility: timedelta = timedelta(seconds=30),
        capacity: int = _DEFAULT_JOB_CAPACITY,
    ) -> None:
        self._visibility = default_visibility
        self._capacity = max(1, capacity)
        self._pending: dict[str, deque[_Entry]] = {}
        self._inflight: dict[JobId, tuple[_Entry, float]] = {}
        self._unique: dict[str, JobId] = {}
        self._next_id = itertools.count()
        self._seq = itertools.count()

    async def enqueue(self, job: Job) -> JobId:
        now = asyncio.get_running_loop().time()
        if job.unique is not None and job.unique in self._unique:
            return self._unique[job.unique]
        tracked = sum(len(q) for q in self._pending.values()) + len(self._inflight)
        if tracked >= self._capacity:
            raise StoreFullError("job queue is at capacity")
        job_id = JobId(f"job-{next(self._next_id)}")
        job.id = job_id
        job.enqueued_at = datetime.now(UTC)
        job.attempts = 0
        entry = _Entry(
            seq=next(self._seq),
            job=dataclasses.replace(job),
            run_at_mono=_to_loop_time(job.run_at, now),
        )
        self._pending.setdefault(job.queue, deque()).append(entry)
        if job.unique is not None:
            self._unique[job.unique] = job_id
        return job_id

    async def dequeue(
        self,
        queues: Sequence[str],
        # the dequeue wait bound per DESIGN §15.1 — a domain parameter,
        # not a trio-style timeout
        timeout: timedelta,  # noqa: ASYNC109
    ) -> Job | None:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout.total_seconds()
        wanted = set(queues)
        while True:
            job = self._try_take(wanted, loop.time())
            if job is not None:
                return job
            remaining = deadline - loop.time()
            if remaining <= 0:
                return None
            await asyncio.sleep(min(_DEQUEUE_POLL_S, remaining))

    async def ack(self, job_id: JobId) -> None:
        flight = self._inflight.pop(job_id, None)
        if flight is None:
            raise NotFoundError(f"unknown or expired lease {job_id}")
        unique = flight[0].job.unique
        if unique is not None:
            self._unique.pop(unique, None)

    async def nack(self, job_id: JobId, retry_at: datetime | None) -> None:
        loop = asyncio.get_running_loop()
        flight = self._inflight.pop(job_id, None)
        if flight is None:
            raise NotFoundError(f"unknown or expired lease {job_id}")
        entry = flight[0]
        entry.run_at_mono = _to_loop_time(retry_at, loop.time())
        self._pending.setdefault(entry.job.queue, deque()).append(entry)

    def _try_take(self, queues: set[str], now: float) -> Job | None:
        """Move expired leases back to pending, then lease the eligible
        job with the smallest enqueue order. All synchronous — callers
        never suspend inside."""
        self._expire_inflight(now)
        best: tuple[str, int, _Entry] | None = None
        for queue in queues:
            deque_ = self._pending.get(queue)
            if deque_ is None:
                continue
            for index, entry in enumerate(deque_):
                if entry.run_at_mono is not None and entry.run_at_mono > now:
                    continue
                if best is None or entry.seq < best[2].seq:
                    best = (queue, index, entry)
        if best is None:
            return None
        queue, index, entry = best
        del self._pending[queue][index]
        entry.job.attempts += 1
        visibility = entry.job.visibility or self._visibility
        lease = now + visibility.total_seconds()
        job_id = entry.job.id
        if job_id is not None:
            self._inflight[job_id] = (entry, lease)
        return dataclasses.replace(entry.job)

    def _expire_inflight(self, now: float) -> None:
        expired = [job_id for job_id, (_, dl) in self._inflight.items() if dl <= now]
        for job_id in expired:
            entry, _ = self._inflight.pop(job_id)
            self._pending.setdefault(entry.job.queue, deque()).appendleft(entry)


def _to_loop_time(when: datetime | None, now: float) -> float | None:
    """A wall-clock `datetime` → loop-monotonic deadline; `None` or past
    times mean "eligible now"."""
    if when is None:
        return None
    delay = when.timestamp() - time.time()
    if delay <= 0:
        return None
    return now + delay
