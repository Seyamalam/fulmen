"""The five adapter interfaces (DESIGN.md §15.1) as Python `Protocol`s.

Each interface is a Rust trait in `fulmen-core` and a `Protocol` here —
adapters can be written in either language. A `struct`/`class` that
satisfies the shape passes `isinstance` checks (the protocols are
`runtime_checkable`) without inheriting anything.

How a backend is called depends on where it is registered:

* a *native* (Rust) implementation registered with the app is invoked
  from the pipeline without a Python crossing;
* a *Python* implementation — a class matching these protocols — is
  called through the executor bridge, one crossing per call, like any
  Python hook (DESIGN.md §7). `Validator` is the exception: it runs once
  per type at registration, under the GIL, so it is synchronous.

The four store protocols are `async` because real backends do I/O; the
built-in memory implementations resolve their coroutines on the first
poll (no suspension), keeping them usable from sync test code too.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any, NewType, Protocol, runtime_checkable

from fulmen._fulmen import Descriptor

__all__ = [
    "AdapterError",
    "CacheStore",
    "CompiledValidator",
    "Decision",
    "Job",
    "JobId",
    "JobQueue",
    "NotFoundError",
    "Rate",
    "RateLimitStore",
    "SessionStore",
    "StoreFullError",
    "StoreUnavailableError",
    "UnsupportedError",
    "Validator",
]

# `compile` yields the compiled validator — the native `Descriptor` the
# dispatch machinery runs.
CompiledValidator = Descriptor

# The id a store assigns on `enqueue` — a `str` newtype so signatures
# distinguish it from arbitrary strings.
JobId = NewType("JobId", str)


class AdapterError(Exception):
    """A backend call failed. The owning plugin maps it to a response —
    unavailable backends are 503s, contract violations are bugs (500)."""


class StoreUnavailableError(AdapterError):
    """The backend could not be reached or timed out."""


class StoreFullError(AdapterError):
    """A bounded store is at capacity — backpressure, not a bug."""


class NotFoundError(AdapterError):
    """The addressed entity does not exist (an unknown job id, an
    expired lease)."""


class UnsupportedError(AdapterError):
    """The adapter cannot honour this operation (e.g. `compile` for a
    type `supports` rejected)."""


@dataclass(frozen=True, slots=True)
class Rate:
    """One rate-limit declaration: `count` requests per `per`, with
    `burst` allowed in a single instant on top of the sustained drip.

    In GCRA terms `per / count` is the emission interval and
    `burst * (per / count)` the burst tolerance (§16.3, decision D11).
    `burst=None` reads as `count` — the usual "N/period" burst.
    """

    count: int
    per: timedelta
    burst: int | None = None

    def __post_init__(self) -> None:
        if self.count <= 0:
            raise ValueError(f"rate count must be > 0, got {self.count}")
        if self.per <= timedelta(0):
            raise ValueError(f"rate period must be > 0, got {self.per!r}")
        burst = self.count if self.burst is None else self.burst
        if burst < 0:
            raise ValueError(f"rate burst must be >= 0, got {burst}")
        object.__setattr__(self, "burst", burst)


@dataclass(frozen=True, slots=True)
class Decision:
    """A rate-limit verdict.

    `remaining` feeds the `RateLimit` header's remaining field,
    `reset_after` its reset field, and `retry_after` the `Retry-After`
    header (§16.3). Build via `Decision.allow`/`Decision.deny`.
    """

    allowed: bool
    remaining: int = 0
    retry_after: timedelta = field(default_factory=timedelta)
    reset_after: timedelta = field(default_factory=timedelta)

    @classmethod
    def allow(cls, *, remaining: int, reset_after: timedelta) -> Decision:
        """The request may proceed: `remaining` requests fit within the
        burst tolerance, `reset_after` until the budget is full."""
        return cls(True, remaining=remaining, reset_after=reset_after)

    @classmethod
    def deny(cls, *, retry_after: timedelta, reset_after: timedelta) -> Decision:
        """The request must wait `retry_after`; the budget is full
        `reset_after` from now."""
        return cls(False, retry_after=retry_after, reset_after=reset_after)


@dataclass(slots=True)
class Job:
    """One unit of background work (§19.2).

    Submitted through `enqueue` — the store assigns `id` and
    `enqueued_at`, and `dequeue` returns it with `attempts` bumped.
    `payload` is the schema engine's serialization of the task
    arguments; the store treats it as opaque.
    """

    queue: str
    """The queue this job belongs to; `dequeue` selects by name."""
    payload: bytes
    """Serialized task arguments (opaque to the store)."""
    id: JobId | None = None
    """The store-assigned id; `None` in a job being submitted."""
    run_at: datetime | None = None
    """When the job first becomes deliverable (§19.2 scheduled jobs).
    `None` means immediately."""
    visibility: timedelta | None = None
    """How long a dequeued delivery may stay unacknowledged before the
    job is redelivered — the at-least-once lease. `None` uses the
    store's default."""
    unique: str | None = None
    """Uniqueness key (§19.2): while a job with this key exists
    (pending or in-flight), another `enqueue` with the same key returns
    its `JobId` instead of duplicating."""
    attempts: int = 0
    """Delivery count, maintained by the store: `1` on first `dequeue`."""
    enqueued_at: datetime | None = None
    """When the store accepted the job."""


@runtime_checkable
class SessionStore(Protocol):
    """The server-side session backend (§16.5). Payloads are opaque
    bytes — the sessions plugin owns the session encoding."""

    async def load(self, session_id: str) -> bytes | None:
        """The stored payload, or `None` when absent or expired."""
        ...

    async def save(self, session_id: str, data: bytes, ttl: timedelta) -> None:
        """Store `data` under `session_id`, replacing any prior value.
        The entry expires `ttl` after this call — every `save`
        refreshes the deadline (rolling session lifetime)."""
        ...

    async def delete(self, session_id: str) -> None:
        """Drop `session_id`. Deleting an absent id is a no-op, not an
        error."""
        ...


@runtime_checkable
class RateLimitStore(Protocol):
    """The rate-limiter's decision backend (§16.3). `acquire`
    atomically checks and updates `key`'s budget — concurrent calls
    never double-spend, and a `Denied` consumes nothing."""

    async def acquire(self, key: str, cost: int, rate: Rate) -> Decision:
        """Try to consume `cost` cells of `key`'s budget under `rate`."""
        ...


@runtime_checkable
class CacheStore(Protocol):
    """The response-cache backend. Keys and values are opaque bytes —
    the cache plugin owns the key layout and payload encoding. Stores
    are bounded: a `set` past capacity evicts under the store's
    policy."""

    async def get(self, key: str) -> bytes | None:
        """The cached payload under `key`, or `None` when absent or
        expired."""
        ...

    async def set(self, key: str, value: bytes, ttl: timedelta | None) -> None:
        """Cache `value` under `key`; `ttl` bounds its lifetime, `None`
        means no expiry."""
        ...

    async def delete(self, key: str) -> None:
        """Drop `key`. Deleting an absent key is a no-op, not an
        error."""
        ...


@runtime_checkable
class JobQueue(Protocol):
    """The background-job backend (§19.2): in-order, at-least-once.

    A dequeued job is *leased*: `ack` before its visibility window
    closes or it becomes deliverable again — a dead worker never loses
    a job."""

    async def enqueue(self, job: Job) -> JobId:
        """Enqueue `job`; returns its assigned id. A `job.unique` key
        matching a live job returns the existing `JobId` — enqueue is
        idempotent. A full store raises `StoreFullError` (backpressure)."""
        ...

    async def dequeue(
        self,
        queues: Sequence[str],
        # the dequeue wait bound per DESIGN §15.1 — a domain parameter,
        # not a trio-style timeout
        timeout: timedelta,  # noqa: ASYNC109
    ) -> Job | None:
        """Take the oldest deliverable job on any of `queues` — FIFO by
        enqueue order — waiting up to `timeout`; `None` after it. The
        returned job is leased (see the class contract)."""
        ...

    async def ack(self, job_id: JobId) -> None:
        """Mark `job_id` done — never redelivered. An unknown or
        expired lease raises `NotFoundError`."""
        ...

    async def nack(self, job_id: JobId, retry_at: datetime | None) -> None:
        """Return `job_id` to the queue: deliverable at `retry_at`
        (`datetime`) or immediately (`None`). The job keeps its payload
        and uniqueness; `attempts` keeps counting. Same `NotFoundError`
        contract as `ack`."""
        ...


@runtime_checkable
class Validator(Protocol):
    """The type → validator compiler (§15.1, §11.8).

    Registration-time only — `supports`/`compile`/`json_schema` run
    once per type while the app builds, never on the request path, so
    they are synchronous. `compile` yields the *native* `Descriptor`
    (the adapter translates to spec trees, §15.2); an adapter that
    cannot express its validation as a spec cannot serve this slot.
    """

    def supports(self, tp: type[Any]) -> bool:
        """Whether this adapter can compile `tp` — the registry routes
        on it."""
        ...

    def compile(self, tp: type[Any]) -> CompiledValidator:
        """`tp` → the compiled validator. Raises `UnsupportedError` (or the
        adapter's own typed error) for a type `supports` rejects."""
        ...

    def json_schema(self, tp: type[Any]) -> dict[str, Any]:
        """The JSON Schema document for `tp` — what OpenAPI emits."""
        ...
