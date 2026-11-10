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
    assert called == {"host": "127.0.0.1", "port": 9876, "workers": 3, "debug": False}


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
