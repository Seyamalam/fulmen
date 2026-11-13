"""Type stubs for the native extension module built from crates/fulmen-py."""

from collections.abc import Callable
from types import TracebackType
from typing import Any

__version__: str

def native_components() -> list[str]: ...

class StartupError(Exception):
    """Raised when an application fails to compile or bind at startup.

    The message lists every declaration or binding problem found."""

class Descriptor:
    """A compiled schema descriptor (docs/SPEC-FORMAT.md).

    Built by `compile_descriptor` from a spec dict — `Descriptor` is not
    directly constructible. `validate` raises
    `fulmen.errors.ValidationError` carrying `entries` + `truncated` on
    failure. `construct`/`validate_field` are the native halves of
    `Model.__init__`/`Model.__setattr__`."""

    @property
    def spec(self) -> dict[str, Any]: ...
    @property
    def field_names(self) -> list[str]: ...
    @property
    def record_class(self) -> type[Any] | None: ...
    def describe(self) -> dict[str, Any]: ...
    def validate(
        self,
        obj: Any,
        *,
        source: str = "body",
        lax: bool = False,
    ) -> Any: ...
    def construct(self, instance: Any, data: Any, *, lax: bool = False) -> None: ...
    def validate_field(self, name: str, value: Any, *, lax: bool = False) -> Any: ...

def compile_descriptor(
    spec: dict[str, Any],
    callbacks: list[Callable[..., Any]] | None = None,
) -> Descriptor: ...
def resolve_plugin_order(
    decls: list[tuple[str, list[str], list[str]]],
) -> tuple[list[int], list[str]]:
    """Resolve `use()` registrations into a plugin setup order (§13.2).

    `decls[i]` is `(name, requires, conflicts)` for the i-th registered
    plugin. Returns `(order, problems)`: `order` holds declaration
    indices — requirements first, ties in registration order — and
    `problems` aggregates every resolution error (missing requirements,
    conflicts, duplicate names, cycles) as message strings."""

def describe_native_plugin(
    capsule: Any,
) -> tuple[str, str, int, list[str]]:
    """Read a `fulmen.plugin.v1` descriptor capsule's metadata (§13.4):
    `(name, version, abi_version, hooked_phases)`. Raises `TypeError` for
    a non-capsule argument, `ValueError` when the capsule isn't named
    `fulmen.plugin.v1` or the pointer is null, and `StartupError` for an
    ABI mismatch (expected vs found)."""

class NativeApp:
    """Recorded route declarations, consumed by `serve`/`serve_for_test`.

    Built and filled by `fulmen.Fulmen._compile_native`; not part of the
    public API. `debug` turns on §14.2 output validation."""

    def __init__(self, debug: bool = False) -> None: ...
    def add_route(
        self,
        spec: tuple[
            str,
            str,
            list[tuple[str, str]],
            list[tuple[str, str, str, str, Descriptor | None, tuple[str, Any]]],
            Callable[..., object],
            bool,
            bool,
            list[tuple[int | None, Descriptor | None]],
            list[int],
        ],
    ) -> None: ...
    def add_scope(self) -> int:
        """Allocate a route-group scope id (mounted applications)."""
    def add_hook(
        self,
        phase: str,
        callable: Callable[..., object],
        is_async: bool,
        name: str,
        priority: int,
        scope: int,
    ) -> None:
        """Record one lifecycle hook under `scope` (§8)."""
    def add_error_handler(
        self,
        exc_class: type[BaseException],
        handler: Callable[..., object],
        is_async: bool,
        scope: int,
    ) -> None: ...
    def add_lifecycle_hook(
        self,
        kind: str,
        callable: Callable[..., object],
        is_async: bool,
    ) -> None:
        """Record an `on_startup`/`on_shutdown` hook (`kind`)."""
    def add_native_plugin(
        self,
        capsule: Any,
        config: bytes,
        priority: int,
        scope: int,
        keep: Any,
    ) -> None:
        """Load a `fulmen.plugin.v1` descriptor capsule and register its
        native hooks under `scope` (§13.4) — each hook runs inline on the
        dispatch thread, zero Python crossings. `config` is the plugin's
        `config_json`; `keep` is held for the app's lifetime so the
        plugin's code stays mapped. Raises `StartupError` on ABI mismatch
        or a failed/panicking plugin `setup`."""
    def set_limits(
        self,
        max_depth: int | None = None,
        max_items: int | None = None,
        max_str_bytes: int | None = None,
    ) -> None:
        """Record the app's request limits (`Fulmen(limits=…)`, §14.3);
        `None` keeps the default for that field."""
    def add_scope_limits(
        self,
        scope: int,
        plugin: str,
        max_depth: int | None = None,
        max_items: int | None = None,
        max_str_bytes: int | None = None,
    ) -> None:
        """Record one plugin's limit declaration for `scope` (§14.8);
        compile rejects any field looser than the app's."""
    def describe_routes(
        self,
    ) -> list[tuple[str, str, str, list[tuple[str, str, str]], list[str], int]]:
        """`(method, path, handler, chains, on_error, cost)` per route —
        the `fulmen routes` report, built from declarations only."""

class AsyncPump:
    """The asyncio bridge's loop callback (internal, not importable).

    Lives on the loop thread: `__call__` drains the job queue into
    `loop.create_task` and `complete` is each task's done-callback. It is
    never registered on the module or constructed from Python."""

class TestServer:
    """A running server bound to an ephemeral port (tests only).

    `addr()` returns `"host:port"`; `stop()` shuts down gracefully and is
    idempotent. Usable as a context manager."""

    def addr(self) -> str: ...
    def stop(self) -> None: ...
    def __enter__(self) -> TestServer: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> bool: ...

def serve(
    app: NativeApp,
    host: str,
    port: int,
    *,
    debug: bool = False,
    uvloop: bool | None = None,
    gil_batch_size: int = 32,
    reuse_port: bool = False,
) -> None:
    """Compile `app` and serve until the process is signalled.

    `uvloop`: `None` uses uvloop when installed, `False` forces plain
    asyncio, `True` requires uvloop. `gil_batch_size` tunes the sync
    pool's GIL batching. `reuse_port` binds with
    `SO_REUSEPORT` for multi-process workers (POSIX only).

    Raises `StartupError` for declaration problems, `ValueError` for an
    invalid host, `OSError` when the bind or the server fails."""

def serve_for_test(
    app: NativeApp,
    host: str = "127.0.0.1",
    uvloop: bool | None = None,
    debug: bool = False,
) -> TestServer:
    """Compile `app` and serve it on an ephemeral port for tests.

    `uvloop` selects the asyncio loop implementation exactly like `serve`;
    `debug` turns on §14.2 output validation.
    Raises the same errors as `serve`."""
