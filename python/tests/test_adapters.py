"""Adapter interfaces, memory implementations and conformance suites
(DESIGN.md §15, M3.18-M3.20).

The `fulmen.testkit.suites` test functions are exercised here directly
against the `Memory*` implementations - the same functions the
`--fulmen-adapter` plugin path collects - plus unit coverage for the
shared types, the plugin's `pkg:Cls` resolution, and `NativeValidator` /
`PydanticValidator` conformance.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest
from fulmen.adapters import (
    AdapterError,
    CacheStore,
    Decision,
    Job,
    JobQueue,
    MemoryCacheStore,
    MemoryJobQueue,
    MemoryRateLimitStore,
    MemorySessionStore,
    NativeValidator,
    NotFoundError,
    Rate,
    RateLimitStore,
    SessionStore,
    StoreFullError,
    UnsupportedError,
    Validator,
)
from fulmen.testkit import run
from fulmen.testkit.suites import (
    test_cache,
    test_jobs,
    test_ratelimit,
    test_session,
    test_validator,
)

# -- the conformance suites against the built-in stores -----------------


class TestMemorySessionStoreConformance:
    @pytest.fixture
    def store(self) -> MemorySessionStore:
        return MemorySessionStore()

    def test_save_then_load(self, store: MemorySessionStore) -> None:
        test_session.test_save_then_load(store)

    def test_load_missing_is_none(self, store: MemorySessionStore) -> None:
        test_session.test_load_missing_is_none(store)

    def test_save_overwrites(self, store: MemorySessionStore) -> None:
        test_session.test_save_overwrites(store)

    def test_delete_drops_key(self, store: MemorySessionStore) -> None:
        test_session.test_delete_drops_key(store)

    def test_delete_absent_is_noop(self, store: MemorySessionStore) -> None:
        test_session.test_delete_absent_is_noop(store)

    def test_ttl_expires(self, store: MemorySessionStore) -> None:
        test_session.test_ttl_expires(store)

    def test_save_refreshes_ttl(self, store: MemorySessionStore) -> None:
        test_session.test_save_refreshes_ttl(store)

    def test_keys_are_independent(self, store: MemorySessionStore) -> None:
        test_session.test_keys_are_independent(store)


class TestMemoryCacheStoreConformance:
    @pytest.fixture
    def store(self) -> MemoryCacheStore:
        return MemoryCacheStore()

    def test_set_then_get(self, store: MemoryCacheStore) -> None:
        test_cache.test_set_then_get(store)

    def test_get_missing_is_none(self, store: MemoryCacheStore) -> None:
        test_cache.test_get_missing_is_none(store)

    def test_set_overwrites(self, store: MemoryCacheStore) -> None:
        test_cache.test_set_overwrites(store)

    def test_delete_drops_key(self, store: MemoryCacheStore) -> None:
        test_cache.test_delete_drops_key(store)

    def test_delete_absent_is_noop(self, store: MemoryCacheStore) -> None:
        test_cache.test_delete_absent_is_noop(store)

    def test_ttl_expires(self, store: MemoryCacheStore) -> None:
        test_cache.test_ttl_expires(store)

    def test_no_ttl_persists(self, store: MemoryCacheStore) -> None:
        test_cache.test_no_ttl_persists(store)


class TestMemoryRateLimitStoreConformance:
    @pytest.fixture
    def store(self) -> MemoryRateLimitStore:
        return MemoryRateLimitStore()

    def test_first_acquire_allowed(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_first_acquire_allowed(store)

    def test_burst_then_denied(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_burst_then_denied(store)

    def test_denied_reports_retry_after(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_denied_reports_retry_after(store)

    def test_denied_consumes_nothing(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_denied_consumes_nothing(store)

    def test_retry_after_wait_allows(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_retry_after_wait_allows(store)

    def test_keys_are_independent(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_keys_are_independent(store)

    def test_cost_consumes_budget(self, store: MemoryRateLimitStore) -> None:
        test_ratelimit.test_cost_consumes_budget(store)


class TestMemoryJobQueueConformance:
    @pytest.fixture
    def store(self) -> MemoryJobQueue:
        return MemoryJobQueue(default_visibility=timedelta(milliseconds=40))

    def test_enqueue_returns_distinct_ids(self, store: MemoryJobQueue) -> None:
        test_jobs.test_enqueue_returns_distinct_ids(store)

    def test_dequeue_fifo_per_queue(self, store: MemoryJobQueue) -> None:
        test_jobs.test_dequeue_fifo_per_queue(store)

    def test_dequeue_selects_queues(self, store: MemoryJobQueue) -> None:
        test_jobs.test_dequeue_selects_queues(store)

    def test_dequeue_times_out(self, store: MemoryJobQueue) -> None:
        test_jobs.test_dequeue_times_out(store)

    def test_dequeue_waits_for_enqueue(self, store: MemoryJobQueue) -> None:
        test_jobs.test_dequeue_waits_for_enqueue(store)

    def test_payload_roundtrip(self, store: MemoryJobQueue) -> None:
        test_jobs.test_payload_roundtrip(store)

    def test_ack_is_final(self, store: MemoryJobQueue) -> None:
        test_jobs.test_ack_is_final(store)

    def test_unacked_job_redelivers(self, store: MemoryJobQueue) -> None:
        test_jobs.test_unacked_job_redelivers(store)

    def test_nack_redelivers_immediately(self, store: MemoryJobQueue) -> None:
        test_jobs.test_nack_redelivers_immediately(store)

    def test_nack_retry_at_delays(self, store: MemoryJobQueue) -> None:
        test_jobs.test_nack_retry_at_delays(store)

    def test_attempts_counts_deliveries(self, store: MemoryJobQueue) -> None:
        test_jobs.test_attempts_counts_deliveries(store)

    def test_scheduled_job_not_early(self, store: MemoryJobQueue) -> None:
        test_jobs.test_scheduled_job_not_early(store)

    def test_unique_dedups_enqueue(self, store: MemoryJobQueue) -> None:
        test_jobs.test_unique_dedups_enqueue(store)

    def test_finished_lease_errors(self, store: MemoryJobQueue) -> None:
        test_jobs.test_finished_lease_errors(store)

    def test_jobid_is_a_str(self) -> None:
        test_jobs.test_jobid_is_a_str()


class TestValidatorConformance:
    """The `Validator` suite against both built-ins (§15.2)."""

    def test_native_validator(self) -> None:
        adapter = NativeValidator()
        supported, unsupported = adapter.conformance_types
        triple = (adapter, supported, unsupported)
        test_validator.test_supports_declares(triple)
        test_validator.test_compile_supported_gives_descriptor(triple)
        test_validator.test_compile_unsupported_errors(triple)
        test_validator.test_json_schema_is_dict(triple)

    def test_pydantic_validator(self) -> None:
        pytest.importorskip("pydantic")
        from fulmen.adapters import PydanticValidator

        adapter = PydanticValidator()
        triple = (adapter, *adapter.conformance_types)
        test_validator.test_supports_declares(triple)
        test_validator.test_compile_supported_gives_descriptor(triple)
        test_validator.test_compile_unsupported_errors(triple)
        test_validator.test_json_schema_is_dict(triple)


# -- shared types -------------------------------------------------------


class TestRate:
    def test_defaults_burst_to_count(self) -> None:
        rate = Rate(count=10, per=timedelta(seconds=1))
        assert rate.burst == 10

    def test_explicit_burst(self) -> None:
        rate = Rate(count=10, per=timedelta(seconds=1), burst=3)
        assert rate.burst == 3

    def test_validation(self) -> None:
        with pytest.raises(ValueError, match="count"):
            Rate(count=0, per=timedelta(seconds=1))
        with pytest.raises(ValueError, match="period"):
            Rate(count=1, per=timedelta(0))
        with pytest.raises(ValueError, match="burst"):
            Rate(count=1, per=timedelta(seconds=1), burst=-1)

    def test_frozen(self) -> None:
        rate = Rate(count=1, per=timedelta(seconds=1))
        with pytest.raises(AttributeError):
            rate.count = 5  # type: ignore[misc]


class TestDecision:
    def test_allow(self) -> None:
        decision = Decision.allow(remaining=4, reset_after=timedelta(seconds=1))
        assert decision.allowed
        assert decision.remaining == 4
        assert decision.reset_after == timedelta(seconds=1)

    def test_deny(self) -> None:
        denied = Decision.deny(
            retry_after=timedelta(milliseconds=100),
            reset_after=timedelta(seconds=1),
        )
        assert not denied.allowed
        assert denied.retry_after == timedelta(milliseconds=100)


class TestJobShape:
    def test_defaults(self) -> None:
        job = Job(queue="emails", payload=b"data")
        assert job.id is None
        assert job.run_at is None
        assert job.attempts == 0
        assert job.unique is None

    def test_full_shape(self) -> None:
        when = datetime.now(UTC)
        job = Job(
            queue="emails",
            payload=b"data",
            run_at=when,
            visibility=timedelta(seconds=5),
            unique="k",
        )
        assert job.run_at is when
        assert job.visibility == timedelta(seconds=5)


class TestErrorTaxonomy:
    def test_subclassing(self) -> None:
        assert issubclass(StoreFullError, AdapterError)
        assert issubclass(NotFoundError, AdapterError)
        assert issubclass(UnsupportedError, AdapterError)


# -- protocol conformance (runtime_checkable isinstance) ----------------


class TestProtocolConformance:
    def test_memory_impls_match_protocols(self) -> None:
        assert isinstance(MemorySessionStore(), SessionStore)
        assert isinstance(MemoryCacheStore(), CacheStore)
        assert isinstance(MemoryRateLimitStore(), RateLimitStore)
        assert isinstance(MemoryJobQueue(), JobQueue)
        assert isinstance(NativeValidator(), Validator)

    def test_non_conforming_class_fails_isinstance(self) -> None:
        class NotAStore:
            pass

        assert not isinstance(NotAStore(), SessionStore)
        assert not isinstance(NotAStore(), Validator)


# -- the plugin's pkg:Cls resolution ------------------------------------


class TestLoadAdapter:
    def test_resolves_memory_class(self) -> None:
        from fulmen.testkit.plugin import load_adapter

        store = load_adapter("fulmen.adapters.memory:MemorySessionStore")
        assert isinstance(store, MemorySessionStore)

    def test_resolves_dotted_qualname(self) -> None:
        from fulmen.testkit.plugin import load_adapter

        probe = load_adapter("fulmen.testkit.plugin:CONFORMANCE_TYPES_ATTR.__class__")
        assert isinstance(probe, str)

    def test_rejects_malformed_spec(self) -> None:
        from fulmen.testkit.plugin import load_adapter

        with pytest.raises(pytest.UsageError):
            load_adapter("no-colon-here")
        with pytest.raises(pytest.UsageError):
            load_adapter(":OnlyClass")

    def test_rejects_non_class(self) -> None:
        from fulmen.testkit.plugin import load_adapter

        with pytest.raises(pytest.UsageError, match="class"):
            load_adapter("fulmen.testkit.plugin:CONFORMANCE_TYPES_ATTR")

    def test_unknown_module_raises(self) -> None:
        from fulmen.testkit.plugin import load_adapter

        with pytest.raises(ModuleNotFoundError):
            load_adapter("fulmen.does_not_exist:Thing")


class TestLoadInitialConftests:
    """The hook's argv rewrite (plugin.py's main logic — run directly,
    since the hook itself is a plain function)."""

    def test_no_flag_leaves_argv(self) -> None:
        from fulmen.testkit.plugin import pytest_load_initial_conftests

        args = ["-q", "python/tests"]
        pytest_load_initial_conftests(args)
        assert args == ["-q", "python/tests"]

    def test_flag_injects_suites_dir(self) -> None:
        from fulmen.testkit.plugin import _SUITES, pytest_load_initial_conftests

        args = ["-q", "--fulmen-adapter=fulmen.adapters.memory:MemoryCacheStore"]
        pytest_load_initial_conftests(args)
        assert str(_SUITES) in args
        assert "--fulmen-adapter=fulmen.adapters.memory:MemoryCacheStore" in args
        assert "-q" in args

    def test_flag_space_separated(self) -> None:
        from fulmen.testkit.plugin import _SUITES, pytest_load_initial_conftests

        args = ["--fulmen-adapter", "fulmen.adapters.memory:MemoryJobQueue"]
        pytest_load_initial_conftests(args)
        assert str(_SUITES) in args

    def test_existing_path_not_overridden(self) -> None:
        from fulmen.testkit.plugin import _SUITES, pytest_load_initial_conftests

        args = ["--fulmen-adapter=x:Y", "python/tests"]
        pytest_load_initial_conftests(args)
        assert str(_SUITES) not in args
        assert "python/tests" in args

    def test_unknown_flag_is_positional(self) -> None:
        """A non-existent positional is treated as a name, not a path —
        the suites dir is still injected."""
        from fulmen.testkit.plugin import _SUITES, pytest_load_initial_conftests

        args = ["--fulmen-adapter=x:Y", "-k", "session", "does-not-exist-dir"]
        pytest_load_initial_conftests(args)
        assert str(_SUITES) in args


# -- implementation edge coverage ---------------------------------------


class TestMemoryImplEdges:
    def test_session_capacity_evicts_lru(self) -> None:
        store = MemorySessionStore(capacity=2)

        async def check() -> None:
            await store.save("a", b"1", timedelta(seconds=60))
            await store.save("b", b"1", timedelta(seconds=60))
            assert await store.load("a") == b"1"  # promote a
            await store.save("c", b"1", timedelta(seconds=60))
            assert await store.load("b") is None  # b was least recent
            assert await store.load("a") == b"1"
            assert await store.load("c") == b"1"

        run(check())

    def test_session_expired_entries_make_room_first(self) -> None:
        import asyncio

        store = MemorySessionStore(capacity=2)

        async def check() -> None:
            await store.save("old", b"1", timedelta(milliseconds=20))
            await store.save("live", b"1", timedelta(seconds=60))
            await asyncio.sleep(0.04)
            await store.save("new", b"1", timedelta(seconds=60))
            assert await store.load("live") == b"1"
            assert await store.load("new") == b"1"

        run(check())

    def test_rate_limit_bounded_keys(self) -> None:
        store = MemoryRateLimitStore(max_keys=2)
        rate = Rate(count=1, per=timedelta(milliseconds=500), burst=1)

        async def check() -> None:
            assert (await store.acquire("k1", 1, rate)).allowed
            # Push k2 (dead state? no - live) then a third key forces eviction.
            assert (await store.acquire("k2", 1, rate)).allowed
            assert (await store.acquire("k3", 1, rate)).allowed
            assert len(store._tats) <= 2  # pyright: ignore[reportPrivateUsage] -- impl edge test

        run(check())

    def test_job_queue_capacity_backpressure(self) -> None:
        store = MemoryJobQueue(capacity=2)

        async def check() -> None:
            await store.enqueue(Job(queue="q", payload=b"1"))
            await store.enqueue(Job(queue="q", payload=b"2"))
            with pytest.raises(StoreFullError):
                await store.enqueue(Job(queue="q", payload=b"3"))

        run(check())

    def test_job_queue_cross_queue_fifo_by_seq(self) -> None:
        """Global FIFO across queues: dequeue picks oldest seq, not
        first queue."""
        store = MemoryJobQueue()

        async def check() -> None:
            await store.enqueue(Job(queue="b", payload=b"first"))
            await store.enqueue(Job(queue="a", payload=b"second"))
            job = await store.dequeue(["a", "b"], timedelta(seconds=2))
            assert job is not None
            assert job.payload == b"first"
            assert job.id is not None
            await store.ack(job.id)

        run(check())

    def test_enqueue_on_closed_loop_time(self) -> None:
        """`run_at`/`retry_at` in the past are deliverable immediately."""
        store = MemoryJobQueue()

        async def check() -> None:
            past = datetime.now(UTC) - timedelta(seconds=5)
            await store.enqueue(Job(queue="q", payload=b"", run_at=past))
            job = await store.dequeue(["q"], timedelta(seconds=2))
            assert job is not None
            assert job.id is not None
            await store.ack(job.id)

        run(check())


# -- memory store internals: bounded maps -------------------------------------


class TestRateLimitEviction:
    """`_insert_bounded` keeps `_tats` under `max_keys` — expired entries
    are swept first, then the earliest-TAT victim goes."""

    def test_expired_entries_swept_first(self) -> None:
        store = MemoryRateLimitStore(max_keys=1)
        store._tats["old"] = 0  # long expired — the sweep removes it
        run(store.acquire("new", 1, Rate(count=1, per=timedelta(seconds=1))))
        assert "new" in store._tats
        assert "old" not in store._tats

    def test_lru_victim_evicted_when_full(self) -> None:
        store = MemoryRateLimitStore(max_keys=1)
        store._tats["old"] = store._tats.get("x", 0) or (__import__("time").monotonic_ns() + 10**12)
        run(store.acquire("new", 1, Rate(count=1, per=timedelta(seconds=1))))
        assert "new" in store._tats
        assert "old" not in store._tats
