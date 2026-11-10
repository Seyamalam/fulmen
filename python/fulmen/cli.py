"""`fulmen` command-line entry point.

Subcommands planned for v1 (see docs/ROADMAP.md): new, dev, run, routes,
check, doctor, fix, gen client, gen openapi, add.
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence

from fulmen import __version__, native_components


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="fulmen", description="Fulmen command-line tools.")
    parser.add_argument("--version", action="version", version=f"fulmen {__version__}")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("info", help="show build information")

    args = parser.parse_args(argv)
    if args.command == "info":
        gil = getattr(sys, "_is_gil_enabled", lambda: True)()
        print(f"fulmen {__version__}")
        print(f"python {sys.version.split()[0]} (GIL {'enabled' if gil else 'disabled'})")
        print(f"native components: {', '.join(native_components())}")
        return 0
    parser.print_help()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
