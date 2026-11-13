"""Native-plugin tests (DESIGN.md §13.4, ROADMAP M3.14-M3.17, M3.24).

`NativePlugin` resolves a shared library's `fulmen.plugin.v1` descriptor
capsule, validates its ABI, and lands its hooks in the phase machinery as
zero-crossing native hooks — exercised here through the real loading path
against `examples/native-plugin`'s cdylib: request-id headers on a served
app, JSON config, an `expected …, found …` ABI failure both eagerly and
through compile, a panicking hook answering `500`, and the one-crossing
route cost.
"""

from __future__ import annotations

import ctypes
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import fetch
from fulmen import Fulmen, NativePlugin, StartupError
from fulmen import plugins as _plugins
from fulmen._fulmen import describe_native_plugin

_ROOT = Path(__file__).resolve().parents[2]
_LIB_NAMES = (
    "libfulmen_example_plugin.dylib",
    "libfulmen_example_plugin.so",
    "fulmen_example_plugin.dll",
)


@pytest.fixture(scope="session")
def plugin_lib() -> Path:
    """The example plugin's cdylib — built once for the suite."""
    cargo = shutil.which("cargo")
    if cargo is None:
        pytest.fail("cargo not on PATH — cannot build the example plugin")
    subprocess.run(  # noqa: S603 -- fixed argv; cargo resolved via shutil.which
        [cargo, "build", "-p", "fulmen-example-plugin", "--quiet"],
        cwd=_ROOT,
        check=True,
    )
    for name in _LIB_NAMES:
        candidate = _ROOT / "target" / "debug" / name
        if candidate.exists():
            try:
                ctypes.CDLL(str(candidate))
            except OSError as exc:  # e.g. x86_64 interpreter vs arm64 dylib
                pytest.skip(f"interpreter cannot load the example plugin: {exc}")
            return candidate
    pytest.fail(f"example plugin library missing after build (tried {_LIB_NAMES})")


def _app() -> Fulmen:
    return Fulmen(openapi_url=None, docs_url=None)


# -- descriptor introspection (M3.15) -----------------------------------------


def test_native_plugin_describes_itself(plugin_lib: Path) -> None:
    """The descriptor's metadata lands on the plugin object — name and
    version come from the descriptor, `abi_version` matches the host."""
    plugin = NativePlugin(plugin_lib)
    assert plugin.name == "request-id"
    assert plugin.version == "0.1.0"
    assert plugin.abi_version == 1
    assert plugin.phases == ("on_response",)


def test_describe_rejects_non_capsule() -> None:
    with pytest.raises(TypeError):
        describe_native_plugin(object())


def test_describe_rejects_wrong_capsule_name() -> None:
    capsule = _plugins._PyCapsule_New(ctypes.c_void_p(8), b"fulmen.plugin.v2", None)
    with pytest.raises(ValueError, match="incorrect name"):
        describe_native_plugin(capsule)


def test_missing_symbol_is_a_clear_error(plugin_lib: Path) -> None:
    with pytest.raises(ValueError, match="not a fulmen native plugin"):
        NativePlugin(plugin_lib, symbol="fulmen_no_such_symbol")


# -- the real loading path (M3.16/M3.17) --------------------------------------


def test_request_id_header(serve, plugin_lib: Path) -> None:
    """The example's `on_response` stamps `x-request-id` — a counter id
    when the client sent none, the client's own id echoed back."""
    app = _app()
    app.use(NativePlugin(plugin_lib))

    @app.get("/x")
    def x() -> str:
        return "x"

    base = serve(app)
    res = fetch(f"{base}/x")
    assert res.status == 200
    assert res.header("x-request-id") == "req-1"
    res = fetch(f"{base}/x")
    assert res.header("x-request-id") == "req-2"
    res = fetch(f"{base}/x", headers={"X-Request-Id": "client-given"})
    assert res.header("x-request-id") == "client-given"


def test_use_config_reaches_setup(serve, plugin_lib: Path) -> None:
    """`use(plugin, config)` serialises to the plugin's `config_json`."""
    app = _app()
    app.use(NativePlugin(plugin_lib), {"prefix": "myapp"})

    @app.get("/x")
    def x() -> str:
        return "x"

    res = fetch(f"{serve(app)}/x")
    assert res.header("x-request-id") == "myapp-1"


def test_handle_state_is_the_plugin(plugin_lib: Path) -> None:
    """`setup` returns the plugin — `handle.state` settles at compile."""
    app = _app()
    handle = app.use(NativePlugin(plugin_lib))
    app._compile_native()
    assert handle.state is handle.plugin


# -- ABI compatibility (M3.24) -------------------------------------------------


def test_wrong_abi_fails_at_construction(plugin_lib: Path) -> None:
    """The deliberately-wrong-version export fails `describe` — and so
    `NativePlugin` construction — with expected vs found."""
    with pytest.raises(StartupError, match="expected 1, found 9999"):
        NativePlugin(plugin_lib, symbol="fulmen_plugin_v1_bad")


def test_wrong_abi_fails_compile_through_ctx(plugin_lib: Path) -> None:
    """A wrong-version capsule staged through `ctx.add_native_plugin`
    surfaces as an aggregated startup problem naming expected/found."""
    owner, capsule = _plugins._load_plugin_capsule(plugin_lib, "fulmen_plugin_v1_bad")

    class BadAbi:
        name = "bad-abi"
        version = "1.0"

        def setup(self, ctx):
            ctx.add_native_plugin(capsule, keep=owner)

    app = _app()
    app.use(BadAbi())
    with pytest.raises(StartupError, match="expected 1, found 9999"):
        app._compile_native()


# -- containment (M3.16) --------------------------------------------------------


def test_panicking_hook_answers_500(serve, plugin_lib: Path) -> None:
    """A plugin hook panic is contained at the ABI boundary and becomes a
    `500` problem response — never an unwind through `extern "C"`."""
    app = _app()
    app.use(NativePlugin(plugin_lib, symbol="fulmen_plugin_v1_panic"))

    @app.get("/x")
    def x() -> str:
        return "x"

    res = fetch(f"{serve(app)}/x")
    assert res.status == 500
    assert res.header("content-type").startswith("application/problem+json")


# -- the cost model (M3.17) -----------------------------------------------------


def test_native_only_route_costs_one_crossing(plugin_lib: Path) -> None:
    """A route whose only hook is the plugin's reports `native` kind and
    `cost == 1` — native hooks never cross into Python (§8)."""
    app = _app()
    app.use(NativePlugin(plugin_lib))

    @app.get("/x")
    def x() -> str:
        return "x"

    (row,) = app._compile_native().describe_routes()
    (method, path, _handler, chains, _errors, cost) = row
    assert (method, path) == ("GET", "/x")
    assert chains == [("on_response", "request-id:on_response", "native")]
    assert cost == 1


def test_python_hook_still_counts_crossings(plugin_lib: Path) -> None:
    """A native + Python hook mix: only the Python hook adds a crossing,
    and the plugin's (priority 1000) runs in registration order."""
    app = _app()
    app.use(NativePlugin(plugin_lib))

    @app.on_response
    def stamp(_request, _response) -> None:
        return None

    @app.get("/x")
    def x() -> str:
        return "x"

    (row,) = app._compile_native().describe_routes()
    (_method, _path, _handler, chains, _errors, cost) = row
    assert chains == [
        ("on_response", "request-id:on_response", "native"),
        ("on_response", chains[1][1], "python"),
    ]
    assert cost == 2


# -- `fulmen routes --cost` (M3.17) ----------------------------------------------


def test_cli_routes_reports_native_chain(
    plugin_lib: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """`fulmen routes` shows the plugin's hook as `native` and the route
    still at 1 crossing."""
    import types

    from fulmen.cli import main

    module = types.ModuleType("native_plugin_fixture")
    app = _app()
    app.use(NativePlugin(plugin_lib))

    @app.get("/x")
    def x() -> str:
        return "x"

    module.app = app
    sys.modules[module.__name__] = module
    try:
        capsys.readouterr()
        assert main(["routes", "native_plugin_fixture:app", "--hooks", "--cost"]) == 0
        out = capsys.readouterr().out
        assert "request-id:on_response" in out
        assert "native" in out
        assert "cost: 1 crossings" in out
    finally:
        del sys.modules[module.__name__]
