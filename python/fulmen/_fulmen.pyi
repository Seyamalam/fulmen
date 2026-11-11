"""Type stubs for the native extension module built from crates/fulmen-py."""

from collections.abc import Callable
from types import TracebackType

__version__: str

def native_components() -> list[str]: ...

class StartupError(Exception):
    """Raised when an application fails to compile or bind at startup.

    The message lists every declaration or binding problem found."""

class NativeApp:
    """Recorded route declarations, consumed by `serve`/`serve_for_test`.

    Built and filled by `fulmen.Fulmen._compile_native`; not part of the
    public API."""

    def __init__(self) -> None: ...
    def add_route(
        self,
        spec: tuple[
            str,
            str,
            list[tuple[str, str]],
            list[bool],
            Callable[..., object],
            bool,
        ],
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
) -> TestServer:
    """Compile `app` and serve it on an ephemeral port for tests.

    `uvloop` selects the asyncio loop implementation exactly like `serve`.
    Raises the same errors as `serve`."""
