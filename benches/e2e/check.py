#!/usr/bin/env python3
"""`just bench-check`: compare a benchmark run against `benches/baseline.json`.

Guardrails rule (docs/GUARDRAILS.md, "Performance rules"): a change that
makes any tracked scenario more than 5% slower fails. "Slower" is measured
as requests/sec: a scenario regresses when

    latest.requests_per_sec < baseline.requests_per_sec * (1 - 0.05)

Only the apps Fulmen owns (`run.py:SELF_APPS` — `fulmen`, `fulmen_async`,
`ceiling`) are gated; third-party frameworks are context for the ratio
report, not a regression signal for this codebase. p50/p99 are printed
for information.

M2 additions (M2.17): a `__openapi__` entry under a self app is the
structural check of the served `/openapi.json` — every boolean must be
true. A `python_calls` count on the `reject` scenario proves invalid
bodies never entered Python: fulmen apps must report 0 (Rust answers
422 without invoking the handler); third-party apps print their count
for information.

Exit 0 when every gated scenario holds the line, 1 on any regression or
missing data (a missing app/scenario in the latest run is a failure —
a scenario that cannot be measured cannot be checked).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
E2E_DIR = Path(__file__).resolve().parent
DEFAULT_LATEST = E2E_DIR / "results" / "latest.json"
DEFAULT_BASELINE = REPO_ROOT / "benches" / "baseline.json"

# Apps gated by the 5% rule (mirrors run.py; kept local so check.py stays
# stdlib-only and runnable without importing the harness).
SELF_APPS = ("fulmen", "fulmen_async", "fulmen_pydantic", "ceiling")

REGRESSION = 0.05


def load(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())  # type: ignore[no-any-return]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--latest", type=Path, default=DEFAULT_LATEST)
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    args = parser.parse_args()

    if not args.baseline.is_file():
        print(f"bench-check: no baseline at {args.baseline}; run `just bench` first")
        return 1
    if not args.latest.is_file():
        print(
            f"bench-check: no run at {args.latest}; run `python benches/e2e/run.py` "
            "or `just bench` first"
        )
        return 1

    baseline = load(args.baseline)["results"]
    latest = load(args.latest)["results"]

    failed = False
    header = f"{'app':<14}{'scenario':<12}{'baseline':>12}{'latest':>12}{'delta':>8}"
    print(header)
    print("-" * len(header))
    for app in SELF_APPS:
        if app not in baseline:
            continue
        checks = latest.get(app, {}).get("__openapi__")
        if checks is not None:
            ok = sum(1 for v in checks.values() if v)
            flag = "" if ok == len(checks) else "  FAIL"
            failed = failed or bool(flag)
            print(f"{app:<14}{'openapi':<12}{ok:>12}/{len(checks)} checks{flag}")
        for scenario, base in baseline[app].items():
            if scenario.startswith("__"):
                continue
            base_rps = float(base["requests_per_sec"])
            if app not in latest or scenario not in latest[app]:
                print(f"{app:<14}{scenario:<12}{base_rps:>12.0f}{'missing':>12}{'':>8}  FAIL")
                failed = True
                continue
            now = latest[app][scenario]
            now_rps = float(now["requests_per_sec"])
            delta = (now_rps - base_rps) / base_rps * 100
            flag = "  FAIL" if delta < -REGRESSION * 100 else ""
            calls = now.get("python_calls")
            if scenario == "reject" and calls is not None and calls > 0:
                flag = "  FAIL"
            failed = failed or bool(flag)
            extra = f"  python_calls={calls}" if calls is not None else ""
            print(
                f"{app:<14}{scenario:<12}{base_rps:>12.0f}{now_rps:>12.0f}"
                f"{delta:>+7.1f}%{flag}{extra}"
            )
    print()
    if failed:
        print("bench-check FAILED: a tracked scenario regressed more than 5% (requests/sec)")
        print(
            "fix the regression, or update benches/baseline.json in the same commit with a reason"
        )
        return 1
    print("bench-check passed: all tracked scenarios within 5% of baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
