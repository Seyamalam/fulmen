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
        ],
    ) -> None: ...
    def add_error_handler(
        self,
        exc_class: type[BaseException],
        handler: Callable[..., object],
        is_async: bool,
    ) -> None: ...

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
