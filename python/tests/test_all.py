from importlib.metadata import version

import fulmen
from fulmen.cli import main


def test_native_module_loads():
    assert fulmen.__version__ == version("fulmen")
    assert fulmen.native_components() == ["core", "schema", "security", "server"]


def test_cli_info(capsys):
    assert main(["info"]) == 0
    assert "native components: core, schema, security, server" in capsys.readouterr().out


def test_cli_without_command_prints_help(capsys):
    assert main([]) == 0
    assert "usage: fulmen" in capsys.readouterr().out
