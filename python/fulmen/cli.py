"""`fulmen` command-line entry point.

Subcommands planned for v1 (see docs/ROADMAP.md): new, dev, check,
doctor, fix, gen client, gen openapi, add. `run` (M1.22), `info` and
`routes` (M3.4) ship now.
"""

from __future__ import annotations

import argparse
import importlib
import sys
from collections.abc import Sequence

from fulmen import Fulmen, StartupError, __version__, native_components

#: Commands the framework itself provides — plugin-registered commands
#: must not collide with these (§24).
BUILTIN_COMMANDS = frozenset({"info", "routes", "run"})


def main(argv: Sequence[str] | None = None) -> int:
    args_list = list(sys.argv[1:] if argv is None else argv)
    # Plugin-registered commands dispatch by bare name before argparse:
    # `fulmen <cmd> <module:app> [args]` (§24.1).
    if args_list and not args_list[0].startswith("-") and args_list[0] not in BUILTIN_COMMANDS:
        return _plugin_command(args_list[0], args_list[1:])
    parser = argparse.ArgumentParser(prog="fulmen", description="Fulmen command-line tools.")
    parser.add_argument("--version", action="version", version=f"fulmen {__version__}")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("info", help="show build information")

    routes = sub.add_parser("routes", help="list declared routes, hooks and crossing costs")
    routes.add_argument("app", help="application object, e.g. myapp.main:app")
    routes.add_argument(
        "--hooks",
        action="store_true",
        help="show each route's resolved hook chain per phase",
    )
    routes.add_argument(
        "--cost",
        action="store_true",
        help="show the Python-crossing count per route",
    )

    run = sub.add_parser("run", help="serve an application (module:attribute)")
    run.add_argument("app", help="application object, e.g. myapp.main:app")
    run.add_argument("--host", default="127.0.0.1", help="bind host (default: 127.0.0.1)")
    run.add_argument("--port", type=int, default=8000, help="bind port (default: 8000)")
    run.add_argument(
        "--workers",
        type=int,
        default=1,
        help="worker processes (default: 1; >1 needs POSIX + SO_REUSEPORT)",
    )
    run.add_argument(
        "--uvloop",
        dest="uvloop",
        action="store_true",
        default=None,
        help="require uvloop for the asyncio loop (default: auto — uvloop when installed)",
    )
    run.add_argument(
        "--no-uvloop",
        dest="uvloop",
        action="store_false",
        help="force plain asyncio even when uvloop is installed",
    )
    run.add_argument(
        "--gil-batch-size",
        type=int,
        default=32,
        help="max sync handlers run per GIL acquisition under backlog (default: 32)",
    )
    run.add_argument(
        "--debug",
        action="store_true",
        help="debug mode: validate every handler return against its declared response type",
    )

    args = parser.parse_args(args_list)
    if args.command == "info":
        gil = getattr(sys, "_is_gil_enabled", lambda: True)()
        print(f"fulmen {__version__}")
        print(f"python {sys.version.split()[0]} (GIL {'enabled' if gil else 'disabled'})")
        print(f"native components: {', '.join(native_components())}")
        return 0
    if args.command == "run":
        return _run(args)
    if args.command == "routes":
        return _routes(args)
    parser.print_help()
    return 0


def _routes(args: argparse.Namespace) -> int:
    """Render the compiled route table — declarations, never probes (M3.4)."""
    if "" not in sys.path:
        sys.path.insert(0, "")
    try:
        app = _load_app(args.app)
    except (ImportError, AttributeError, TypeError, ValueError) as exc:
        print(f"fulmen routes: {exc}", file=sys.stderr)
        return 2
    try:
        rows = app._compile_native().describe_routes()  # pyright: ignore[reportPrivateUsage] -- package-internal call
    except StartupError as exc:
        print(exc, file=sys.stderr)
        return 1
    plugins = [
        f"{meta.name} {meta.version}"
        for member, _base, _chain in app._walk()  # pyright: ignore[reportPrivateUsage] -- package-internal call
        for _plugin, _handle, meta in member._plugins  # pyright: ignore[reportPrivateUsage]
    ]
    if plugins:
        print(f"plugins: {', '.join(plugins)}")
    for method, path, handler, chains, errors, cost in rows:
        cost_note = f"  (cost: {cost} crossings)" if args.cost else ""
        print(f"{method:<7} {path} -> {handler}{cost_note}")
        if args.hooks:
            for phase, name, kind in chains:
                print(f"         {phase}: {name} ({kind})")
            if errors:
                print(f"         on_error: {', '.join(errors)}")
            if not chains and not errors:
                print("         (no hooks)")
    return 0


def _run(args: argparse.Namespace) -> int:
    # Console scripts don't get the working directory on `sys.path`; the app
    # module must be importable from where the user ran `fulmen run`.
    if "" not in sys.path:
        sys.path.insert(0, "")
    try:
        app = _load_app(args.app)
    except (ImportError, AttributeError, TypeError, ValueError) as exc:
        print(f"fulmen run: {exc}", file=sys.stderr)
        return 2
    try:
        app.run(
            host=args.host,
            port=args.port,
            workers=args.workers,
            debug=args.debug,
            uvloop=args.uvloop,
            gil_batch_size=args.gil_batch_size,
        )
    except StartupError as exc:
        print(exc, file=sys.stderr)
        return 1
    return 0


def _plugin_command(name: str, argv: list[str]) -> int:
    """Dispatch `fulmen <name> <module:app> [args]` to a plugin-registered
    command (§24.1)."""
    if not argv:
        print(
            f"fulmen {name}: expected an application specifier (module:attribute)",
            file=sys.stderr,
        )
        return 2
    if "" not in sys.path:
        sys.path.insert(0, "")
    try:
        app = _load_app(argv[0])
    except (ImportError, AttributeError, TypeError, ValueError) as exc:
        print(f"fulmen {name}: {exc}", file=sys.stderr)
        return 2
    commands, problems = app._plugin_cli_commands()  # pyright: ignore[reportPrivateUsage] -- package-internal call
    if problems:
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    handler = commands.get(name)
    if handler is None:
        available = ", ".join(sorted(commands)) or "none"
        print(
            f"fulmen {name}: no plugin command `{name}` (available: {available})",
            file=sys.stderr,
        )
        return 2
    try:
        return int(handler(app, argv[1:]) or 0)
    except Exception as exc:  # noqa: BLE001 -- a command failure is a clean exit, not a traceback
        print(f"fulmen {name}: {exc}", file=sys.stderr)
        return 1


def _load_app(spec: str) -> Fulmen:
    """Import `module:attribute` and require a `Fulmen` instance."""
    module_name, sep, attribute = spec.partition(":")
    if not sep or not module_name or not attribute:
        raise ValueError(f"invalid app specifier `{spec}` (expected module:attribute)")
    try:
        obj: object = importlib.import_module(module_name)
    except ImportError as exc:
        raise ImportError(f"cannot import module `{module_name}`: {exc}") from exc
    for part in attribute.split("."):
        try:
            obj = getattr(obj, part)
        except AttributeError:
            raise AttributeError(f"module `{module_name}` has no attribute `{attribute}`") from None
    if not isinstance(obj, Fulmen):
        raise TypeError(f"`{spec}` is a {type(obj).__qualname__}, not a fulmen.Fulmen app")
    return obj


if __name__ == "__main__":  # pragma: no cover -- exercised via `python -m fulmen.cli` subprocess
    raise SystemExit(main())
