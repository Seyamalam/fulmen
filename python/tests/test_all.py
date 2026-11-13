from importlib.metadata import version

import fulmen
import pytest
from fulmen.cli import main


def test_native_module_loads():
    assert fulmen.__version__ == version("fulmen")
    assert fulmen.native_components() == [
        "core",
        "plugin-sdk",
        "schema",
        "security",
        "server",
    ]


def test_cli_info(capsys):
    assert main(["info"]) == 0
    assert (
        "native components: core, plugin-sdk, schema, security, server" in capsys.readouterr().out
    )


def test_cli_without_command_prints_help(capsys):
    assert main([]) == 0
    assert "usage: fulmen" in capsys.readouterr().out


def test_lazy_exports_resolve_and_dir_lists_all():
    """Every name in `__all__` resolves through the lazy `__getattr__`
    (PEP 562), and `dir(fulmen)` reports the full public surface."""
    for name in fulmen.__all__:
        assert getattr(fulmen, name) is not None, name
    assert set(dir(fulmen)) >= set(fulmen.__all__)


def test_lazy_export_unknown_name_raises_attribute_error():
    import pytest

    with pytest.raises(AttributeError, match="has no attribute"):
        fulmen.__getattr__("not_a_real_export")


def test_lazy_submodule_import():
    """`fulmen.openapi` resolves through the lazy submodule path."""
    import fulmen.openapi

    assert fulmen.openapi is fulmen.openapi
    assert hasattr(fulmen.openapi, "generate")


def test_lazy_submodule_import_propagates_nested_failures(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A `ModuleNotFoundError` raised *inside* a found submodule (not for
    the submodule itself) re-raises instead of becoming AttributeError."""
    import fulmen

    def _boom(name: str) -> object:
        raise ModuleNotFoundError("No module named 'dep_xyz'", name="dep_xyz")

    monkeypatch.setattr("fulmen.import_module", _boom)
    with pytest.raises(ModuleNotFoundError):
        fulmen.__getattr__("bogus_xyz")
