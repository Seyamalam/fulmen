"""`fulmen run` CLI tests."""

from __future__ import annotations

import pytest
from fulmen import Fulmen, StartupError
from fulmen.cli import _load_app, main


def test_load_app_returns_fulmen() -> None:
    assert isinstance(_load_app("sample_app:app"), Fulmen)


def test_cli_run_dispatches(monkeypatch: pytest.MonkeyPatch) -> None:
    called: dict[str, object] = {}
    monkeypatch.setattr(Fulmen, "run", lambda self, **kwargs: called.update(kwargs))
    assert main(["run", "sample_app:app", "--port", "9876", "--workers", "3"]) == 0
    assert called == {
        "host": "127.0.0.1",
        "port": 9876,
        "workers": 3,
        "debug": False,
        "uvloop": None,
        "gil_batch_size": 32,
    }


def test_cli_run_uvloop_flags(monkeypatch: pytest.MonkeyPatch) -> None:
    called: dict[str, object] = {}
    monkeypatch.setattr(Fulmen, "run", lambda self, **kwargs: called.update(kwargs))
    assert main(["run", "sample_app:app", "--uvloop"]) == 0
    assert called["uvloop"] is True
    called.clear()
    assert main(["run", "sample_app:app", "--no-uvloop"]) == 0
    assert called["uvloop"] is False


def test_cli_run_batch_flags(monkeypatch: pytest.MonkeyPatch) -> None:
    called: dict[str, object] = {}
    monkeypatch.setattr(Fulmen, "run", lambda self, **kwargs: called.update(kwargs))
    assert main(["run", "sample_app:app", "--gil-batch-size", "8"]) == 0
    assert called["gil_batch_size"] == 8


def test_cli_run_missing_colon(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["run", "sample_app"]) == 2
    assert "module:attribute" in capsys.readouterr().err


def test_cli_run_missing_module(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["run", "no_such_module_xyz:app"]) == 2
    assert "cannot import module" in capsys.readouterr().err


def test_cli_run_missing_attr(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["run", "sample_app:nope"]) == 2
    assert "no attribute" in capsys.readouterr().err


def test_cli_run_not_a_fulmen_app(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["run", "fulmen.cli:main"]) == 2
    assert "not a fulmen.Fulmen app" in capsys.readouterr().err


def test_cli_run_startup_error(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fail(self: Fulmen, **kwargs: object) -> None:
        raise StartupError("application failed to start:\n  - broken")

    monkeypatch.setattr(Fulmen, "run", fail)
    assert main(["run", "sample_app:app"]) == 1
    assert "failed to start" in capsys.readouterr().err


# -- `fulmen routes` ---------------------------------------------------------------


def test_routes_lists_plugin_commands(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """`fulmen routes` names the app's plugins ahead of the route table."""
    assert main(["routes", "plugin_fixture_app:app"]) == 0
    out = capsys.readouterr().out
    assert "plugins: key-tools 1.0" in out
    assert "GET     /x -> x" in out


def test_routes_hooks_flag_lists_chains_and_errors(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert main(["routes", "hooks_fixture_app:app", "--hooks"]) == 0
    out = capsys.readouterr().out
    assert "guard" in out
    assert "allow_all" in out
    assert "ValueError" in out


def test_routes_hooks_flag_prints_fallback(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A route with no guards and no `on_error` prints `(no hooks)`."""
    assert main(["routes", "plugin_fixture_app:app", "--hooks"]) == 0
    out = capsys.readouterr().out
    assert "(no hooks)" in out


def test_routes_bad_app_specifier(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["routes", "bogus"]) == 2
    assert "module:attribute" in capsys.readouterr().err


def test_routes_compile_failure(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["routes", "badcompile_fixture_app:app"]) == 1
    assert "setup exploded" in capsys.readouterr().err


def test_routes_inserts_cwd_when_missing(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Console-script mode: `sys.path` lacks `""` — the CLI inserts it."""
    import sys

    import plugin_fixture_app  # noqa: F401 -- warm sys.modules for _load_app

    monkeypatch.setattr(sys, "path", [p for p in sys.path if p != ""])
    assert main(["routes", "plugin_fixture_app:app"]) == 0
    assert "" in sys.path


# -- plugin command dispatch --------------------------------------------------------


def test_plugin_command_bad_app_specifier(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["rotate-keys", "no_such_module_xyz:app"]) == 2
    assert "cannot import module" in capsys.readouterr().err


def test_plugin_command_reports_resolution_problems(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert main(["shared-cmd", "clidup_fixture_app:app"]) == 1
    err = capsys.readouterr().err
    assert "both register CLI command `shared-cmd`" in err


def test_plugin_command_handler_failure(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["explode", "failcli_fixture_app:app"]) == 1
    assert "command blew up" in capsys.readouterr().err


# -- `python -m fulmen.cli` -----------------------------------------------------------


def test_main_module_entrypoint() -> None:
    """`python -m fulmen.cli` is the console entry point — `runpy` can't
    drive it under pytest's import hook, so run a real interpreter."""
    import os
    import subprocess
    import sys
    from pathlib import Path

    env = {**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parent.parent)}
    result = subprocess.run(
        [sys.executable, "-m", "fulmen.cli", "info"],
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    assert result.returncode == 0
    assert "native components" in result.stdout


def test_main_reports_unresolvable_app_spec() -> None:
    """An import failure in `module:attr` prints the error and exits 2."""
    assert main(["definitely_missing_module_zzz:app"]) == 2
