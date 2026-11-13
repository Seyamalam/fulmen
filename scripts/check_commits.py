"""Commit message guardrails: Conventional Commits format and DCO sign-off.

Usage:
    python scripts/check_commits.py --file .git/COMMIT_EDITMSG   # commit-msg hook
    python scripts/check_commits.py --range origin/main..HEAD    # pre-push hook / just ci
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

TYPES = "build|chore|ci|docs|feat|fix|perf|refactor|revert|security|style|test"
SCOPES = "core|server|schema|security|py|cli|plugins|adapters|docs|deps|release|repo"
HEADER = re.compile(rf"^(?:{TYPES})(?:\((?:{SCOPES})\))?!?: [a-z0-9`].{{0,70}}[^.]$")
SIGNOFF = re.compile(r"^Signed-off-by: .+ <[^@\s]+@[^@\s]+>$", re.MULTILINE)
EXEMPT = re.compile(r"^(?:Merge\b|Revert \"|fixup! |squash! )")


def problems(message: str) -> list[str]:
    lines = [ln for ln in message.splitlines() if not ln.startswith("#")]
    header = lines[0].strip() if lines else ""
    if EXEMPT.match(header):
        return []
    found: list[str] = []
    if not HEADER.match(header):
        found.append(
            f"header {header!r} must look like `type(scope): summary` "
            f"(types: {TYPES.replace('|', ', ')}; scopes: {SCOPES.replace('|', ', ')}; "
            "lowercase start, no trailing period, max 72 chars)"
        )
    if len(lines) > 1 and lines[1].strip():
        found.append("leave a blank line between the header and the body")
    if not SIGNOFF.search(message):
        found.append("missing `Signed-off-by:` line; commit with `git commit -s` (DCO)")
    return found


def commits_in(rev_range: str) -> list[tuple[str, str]]:
    out = subprocess.run(
        ["git", "log", "--format=%H%x00%B%x01", rev_range],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    result: list[tuple[str, str]] = []
    for entry in out.split("\x01"):
        entry = entry.strip("\n")
        if entry:
            sha, _, body = entry.partition("\x00")
            result.append((sha, body))
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--file", type=Path)
    group.add_argument("--range")
    args = parser.parse_args()

    if args.file:
        found = problems(args.file.read_text())
        for p in found:
            print(f"commit-msg: {p}")
        return 1 if found else 0

    failed = False
    for sha, body in commits_in(args.range):
        for p in problems(body):
            print(f"{sha[:10]}: {p}")
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
