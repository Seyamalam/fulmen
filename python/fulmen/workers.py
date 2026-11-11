"""Multi-process worker supervision (DESIGN.md §7.2, M1.23, decision D15).

POSIX only: every worker process binds the same `host:port` with
`SO_REUSEPORT`, so the kernel distributes connections between processes —
no socket passing or single-acceptor fan-out is needed. `app.run(workers=N)`
and `fulmen run --workers N` both land here; on Windows `workers > 1` is
refused with a clear error and v1 stays single-process there.

The parent is a minimal supervisor: it restarts a dead worker in place,
gives up (and tears the whole group down) when one slot crashes more than
`_MAX_RESTARTS` times inside `_RESTART_WINDOW_S`, and brings every child
down together on SIGINT/SIGTERM or `stop()`. Full supervision policy
(gradual backoff, health checks, rolling restarts) is M4.6+ scope.
"""

from __future__ import annotations

import logging
import multiprocessing
import os
import signal
import socket
import time
from multiprocessing.process import BaseProcess
from types import FrameType
from typing import TYPE_CHECKING

from fulmen._fulmen import StartupError
from fulmen._fulmen import serve as _serve

if TYPE_CHECKING:
    from fulmen.app import Fulmen

logger = logging.getLogger("fulmen")

_MAX_RESTARTS = 5
_RESTART_WINDOW_S = 60.0
_POLL_INTERVAL_S = 0.1


def _supports_workers() -> bool:
    """Whether `workers > 1` can run here: needs `fork` + `SO_REUSEPORT`."""
    return os.name == "posix"


def _worker_main(
    app: Fulmen,
    host: str,
    port: int,
    *,
    debug: bool,
    uvloop: bool | None,
    gil_batch_size: int,
) -> None:
    """Worker entry point: compile the inherited app and serve the shared port."""
    _serve(
        app._compile_native(),  # pyright: ignore[reportPrivateUsage] -- package-internal call
        host,
        port,
        debug=debug,
        uvloop=uvloop,
        gil_batch_size=gil_batch_size,
        reuse_port=True,
    )


def _reserve_port(host: str) -> int:
    """Grab an ephemeral port the workers can share when `port=0` was asked.

    Between this probe closing and the workers binding there is a small
    race window; `port=0` with multiple workers is a test convenience.
    """
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    probe = socket.socket(family, socket.SOCK_STREAM)
    try:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        probe.bind((host, 0))
        return int(probe.getsockname()[1])
    finally:
        probe.close()


class WorkerGroup:
    """N worker processes sharing one `host:port` via `SO_REUSEPORT`."""

    def __init__(
        self,
        app: Fulmen,
        *,
        host: str,
        port: int,
        workers: int,
        debug: bool = False,
        uvloop: bool | None = None,
        gil_batch_size: int = 32,
    ) -> None:
        self._app = app
        self._host = host
        self._workers = workers
        self._kwargs: dict[str, object] = {
            "debug": debug,
            "uvloop": uvloop,
            "gil_batch_size": gil_batch_size,
        }
        self._ctx = multiprocessing.get_context("fork")
        self._procs: list[BaseProcess] = []
        self._restarts: list[list[float]] = []
        self._stopping = False
        # `port=0` picks one ephemeral port up front so every worker binds
        # the same address.
        self._port = port if port != 0 else _reserve_port(host)

    @property
    def port(self) -> int:
        """The shared port (resolved when constructed with `port=0`)."""
        return self._port

    def pids(self) -> list[int]:
        """The current worker PIDs (started, or restarted slots)."""
        return [proc.pid for proc in self._procs if proc.pid is not None]

    def is_alive(self) -> bool:
        """Whether any worker process is still running."""
        return any(proc.is_alive() for proc in self._procs)

    def start(self) -> None:
        """Spawn every worker. Does not wait for them to serve."""
        self._restarts = [[] for _ in range(self._workers)]
        for index in range(self._workers):
            self._procs.append(self._spawn(index))

    def _spawn(self, index: int) -> BaseProcess:
        proc = self._ctx.Process(
            target=_worker_main,
            name=f"fulmen-worker-{index}",
            args=(self._app, self._host, self._port),
            kwargs=self._kwargs,
            daemon=True,
        )
        proc.start()
        logger.info("worker %d started (pid %s)", index, proc.pid)
        return proc

    def wait_ready(self, timeout: float = 10.0) -> None:
        """Poll until the shared port accepts a connection.

        Raises `TimeoutError` when the workers do not come up in time.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                with socket.create_connection((self._host, self._port), timeout=0.2):
                    return
            except OSError:
                time.sleep(0.05)
        raise TimeoutError(
            f"workers did not accept connections on {self._host}:{self._port} within {timeout:.0f}s"
        )

    def stop(self, timeout: float = 10.0) -> None:
        """Bring the whole group down: SIGTERM every worker, wait, then
        SIGKILL stragglers. Idempotent."""
        self._stopping = True
        for proc in self._procs:
            if proc.is_alive():
                proc.terminate()
        deadline = time.monotonic() + timeout
        for proc in self._procs:
            proc.join(max(0.0, deadline - time.monotonic()))
            if proc.is_alive():
                proc.kill()
        for proc in self._procs:
            proc.join()

    def supervise(self) -> None:
        """Watch the workers and restart dead slots until `stop()`.

        The policy is simple: a dead worker is restarted in place, and a
        slot that crashes more than `_MAX_RESTARTS` times within
        `_RESTART_WINDOW_S` tears the whole group down with `StartupError`.
        """
        while not self._stopping:
            for index, proc in enumerate(self._procs):
                if proc.is_alive():
                    continue
                proc.join(0)
                self._record_crash(index, proc.exitcode)
                self._procs[index] = self._spawn(index)
            time.sleep(_POLL_INTERVAL_S)

    def _record_crash(self, index: int, exitcode: int | None) -> None:
        now = time.monotonic()
        crashes = [at for at in self._restarts[index] if now - at < _RESTART_WINDOW_S]
        crashes.append(now)
        self._restarts[index] = crashes
        if len(crashes) > _MAX_RESTARTS:
            logger.error(
                "worker %d crashed %d times in %.0fs; stopping all workers",
                index,
                len(crashes),
                _RESTART_WINDOW_S,
            )
            self.stop()
            raise StartupError(
                f"worker {index} crashed {len(crashes)} times in {_RESTART_WINDOW_S:.0f}s"
            )
        logger.warning("worker %d exited with code %s; restarting", index, exitcode)


def spawn_workers(
    app: Fulmen,
    *,
    host: str,
    port: int,
    workers: int,
    debug: bool = False,
    uvloop: bool | None = None,
    gil_batch_size: int = 32,
) -> WorkerGroup:
    """Create and start the worker group; returns it started (not ready).

    Raises `StartupError` off POSIX — `workers > 1` needs `fork` +
    `SO_REUSEPORT` (Linux/macOS; Windows stays single-process, D15).
    """
    if not _supports_workers():
        raise StartupError(
            "workers > 1 requires a POSIX platform with fork and SO_REUSEPORT "
            "(Linux or macOS); run with workers=1 on this platform"
        )
    group = WorkerGroup(
        app,
        host=host,
        port=port,
        workers=workers,
        debug=debug,
        uvloop=uvloop,
        gil_batch_size=gil_batch_size,
    )
    group.start()
    return group


def run_workers(
    app: Fulmen,
    *,
    host: str,
    port: int,
    workers: int,
    debug: bool = False,
    uvloop: bool | None = None,
    gil_batch_size: int = 32,
) -> None:
    """Spawn the group and supervise it; returns once every worker stopped.

    SIGINT/SIGTERM stop the whole group (handlers install best-effort — off
    the main thread supervision still works, signals just are not trapped).
    """
    group = spawn_workers(
        app,
        host=host,
        port=port,
        workers=workers,
        debug=debug,
        uvloop=uvloop,
        gil_batch_size=gil_batch_size,
    )
    _install_signal_handlers(group)
    try:
        group.supervise()
    finally:
        group.stop()


def _install_signal_handlers(group: WorkerGroup) -> None:
    """SIGINT/SIGTERM stop the whole worker group together."""

    def _stop(_signum: int, _frame: FrameType | None) -> None:
        group.stop()

    for signum in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(signum, _stop)
        except ValueError:
            # Not the main thread (embedded / test use): signals stay with
            # the process; callers can still `stop()` the group.
            return
