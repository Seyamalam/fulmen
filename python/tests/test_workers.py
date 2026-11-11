"""M1.23: multi-process workers sharing one `host:port` via `SO_REUSEPORT`.

Children are forked from the test process — safe because the native
bridge never parks a thread through libdispatch (macOS kills those in
children of a multi-threaded parent; the bridge uses `Mutex` + `Condvar`
and `call_soon_threadsafe` instead).
"""

from __future__ import annotations

import os
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
from conftest import fetch
from fulmen import Fulmen, StartupError
from fulmen import workers as worker_mod

POSIX = os.name == "posix"
pytestmark = pytest.mark.skipif(
    not POSIX, reason="workers > 1 need fork + SO_REUSEPORT (POSIX only)"
)

_TESTS_DIR = Path(__file__).parent


def _app() -> Fulmen:
    """One sync and one async route: children exercise both bridges."""
    app = Fulmen()

    @app.get("/ping")
    def ping() -> str:
        return "pong"

    @app.get("/aping")
    async def aping() -> str:
        return "apong"

    return app


def _free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


class _ProcStub:
    """A `multiprocessing.Process` stand-in that is never alive."""

    def __init__(self) -> None:
        self.pid = 1
        self.exitcode = -9

    def is_alive(self) -> bool:
        return False

    def join(self, timeout: float | None = None) -> None:
        del timeout

    def terminate(self) -> None:
        pass

    def kill(self) -> None:
        pass


def test_spawn_workers_serves() -> None:
    group = worker_mod.spawn_workers(_app(), host="127.0.0.1", port=0, workers=2)
    try:
        group.wait_ready(timeout=10)
        assert len(group.pids()) == 2
        base = f"http://127.0.0.1:{group.port}"
        for _ in range(4):
            assert fetch(f"{base}/ping").body == b"pong"
            assert fetch(f"{base}/aping").body == b"apong"
    finally:
        group.stop()
    assert not group.is_alive()


def test_spawn_workers_refused_off_posix(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(worker_mod, "_supports_workers", lambda: False)
    with pytest.raises(StartupError, match="POSIX"):
        worker_mod.spawn_workers(_app(), host="127.0.0.1", port=0, workers=2)


def test_wait_ready_times_out(monkeypatch: pytest.MonkeyPatch) -> None:
    def refuse(*args: object, **kwargs: object) -> socket.socket:
        raise OSError("connection refused")

    monkeypatch.setattr(worker_mod.socket, "create_connection", refuse)
    group = worker_mod.WorkerGroup(_app(), host="127.0.0.1", port=_free_port(), workers=1)
    with pytest.raises(TimeoutError, match="did not accept"):
        group.wait_ready(timeout=0.3)


def test_stop_without_start_is_a_noop() -> None:
    group = worker_mod.WorkerGroup(_app(), host="127.0.0.1", port=_free_port(), workers=1)
    group.stop()
    assert not group.is_alive()


def test_supervise_restarts_a_dead_worker() -> None:
    group = worker_mod.spawn_workers(_app(), host="127.0.0.1", port=0, workers=1)
    supervisor = threading.Thread(target=group.supervise)
    try:
        group.wait_ready(timeout=10)
        supervisor.start()
        killed = group.pids()[0]
        os.kill(killed, signal.SIGKILL)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if group.pids() != [killed] and group.is_alive():
                break
            time.sleep(0.05)
        else:
            pytest.fail("the dead worker was never restarted")
        group.wait_ready(timeout=10)
        assert fetch(f"http://127.0.0.1:{group.port}/ping").body == b"pong"
    finally:
        group.stop()
        supervisor.join(timeout=10)


def test_supervise_gives_up_after_crash_budget(monkeypatch: pytest.MonkeyPatch) -> None:
    group = worker_mod.WorkerGroup(_app(), host="127.0.0.1", port=_free_port(), workers=1)
    group._procs = [_ProcStub()]
    group._restarts = [[]]
    monkeypatch.setattr(group, "_spawn", lambda index: _ProcStub())
    with pytest.raises(StartupError, match="crashed"):
        group.supervise()
    assert group._stopping


def test_run_workers_supervises_until_stopped(monkeypatch: pytest.MonkeyPatch) -> None:
    """`run_workers` blocks supervising; `stop()` on the group ends it.
    Runs off the main thread, exercising the signals-not-installed path."""
    port = _free_port()
    captured: dict[str, worker_mod.WorkerGroup] = {}
    real_spawn = worker_mod.spawn_workers

    def spy(app: Fulmen, **kwargs: object) -> worker_mod.WorkerGroup:
        group = real_spawn(app, **kwargs)
        captured["group"] = group
        return group

    monkeypatch.setattr(worker_mod, "spawn_workers", spy)
    done = threading.Event()

    def target() -> None:
        worker_mod.run_workers(_app(), host="127.0.0.1", port=port, workers=2)
        done.set()

    thread = threading.Thread(target=target)
    thread.start()
    try:
        deadline = time.monotonic() + 15
        while "group" not in captured and time.monotonic() < deadline:
            time.sleep(0.05)
        group = captured["group"]
        group.wait_ready(timeout=10)
        assert fetch(f"http://127.0.0.1:{port}/ping").body == b"pong"
    finally:
        if "group" in captured:
            captured["group"].stop()
        thread.join(timeout=15)
    assert done.is_set()


def test_signal_handlers_stop_the_group() -> None:
    group = worker_mod.WorkerGroup(_app(), host="127.0.0.1", port=_free_port(), workers=1)
    old_int = signal.getsignal(signal.SIGINT)
    old_term = signal.getsignal(signal.SIGTERM)
    try:
        worker_mod._install_signal_handlers(group)
        handler = signal.getsignal(signal.SIGTERM)
        assert callable(handler)
        handler(signal.SIGTERM, None)
        assert group._stopping
    finally:
        signal.signal(signal.SIGINT, old_int)
        signal.signal(signal.SIGTERM, old_term)


def test_cli_run_workers_end_to_end() -> None:
    """`fulmen run --workers 2` in a clean subprocess: the real user path."""
    port = _free_port()
    proc = subprocess.Popen(  # noqa: S603 -- controlled argv, repo venv python
        [
            sys.executable,
            "-m",
            "fulmen.cli",
            "run",
            "sample_app:app",
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--workers",
            "2",
        ],
        cwd=_TESTS_DIR,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        deadline = time.monotonic() + 20
        while True:
            try:
                res = fetch(f"http://127.0.0.1:{port}/ping")
                break
            except OSError:
                if proc.poll() is not None:
                    pytest.fail(f"fulmen run exited early (code {proc.returncode})")
                if time.monotonic() > deadline:
                    pytest.fail("workers never served")
                time.sleep(0.1)
        assert res.body == b"pong"
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    assert proc.returncode == 0
