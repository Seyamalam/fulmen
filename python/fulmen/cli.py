"""`fulmen` command-line entry point.

Subcommands planned for v1 (see docs/ROADMAP.md): new, dev, routes, check,
doctor, fix, gen client, gen openapi, add. `run` (M1.22) and `info` ship
now.
"""

from __future__ import annotations

import argparse
import importlib
import sys
from collections.abc import Sequence

from fulmen import Fulmen, StartupError, __version__, native_components


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fulmen", description="Fulmen command-line tools.")
    parser.add_argument("--version", action="version", version=f"fulmen {__version__}")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("info", help="show build information")

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
    run.add_argument("--debug", action="store_true", help="reserved; no effect yet")

    args = parser.parse_args(argv)
    if args.command == "info":
        gil = getattr(sys, "_is_gil_enabled", lambda: True)()
        print(f"fulmen {__version__}")
        print(f"python {sys.version.split()[0]} (GIL {'enabled' if gil else 'disabled'})")
        print(f"native components: {', '.join(native_components())}")
        return 0
    if args.command == "run":
        return _run(args)
    parser.print_help()
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


if __name__ == "__main__":
    raise SystemExit(main())
