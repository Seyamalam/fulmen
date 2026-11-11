#!/usr/bin/env python3
"""End-to-end benchmark runner (docs/DESIGN.md §25.3, ROADMAP.md M1.25).

For each app in `APPS` and each scenario in `SCENARIOS`: start the server
as a subprocess on its fixed port, wait until `GET /` answers 200, run a
short `oha` warmup, then a measured `oha` run with `--output-format json`,
kill the process group cleanly, and record requests/sec, latency
percentiles, status codes and error counts into normalized JSON at
`benches/e2e/results/latest.json`.

`--set-baseline` additionally writes the same document to
`benches/baseline.json` (the reference `just bench-check` compares
against; values are the single-pass medians — the matrix runs each
scenario once per app, so the median is that observation).

`--sweep-gil-batch 1,8,32,64` measures only the sync fulmen app on the
JSON scenario, once per batch size (Q2 evidence, DESIGN.md §7.5).

`--soak SECONDS` runs one app/scenario pair under `oha` for that long and
samples the process tree's RSS every `--soak-interval` seconds (M1 exit
criterion: RSS stable within 5%).

Fairness: every Python app gets the same `--workers` process count (N);
Robyn's analogue is `--processes N`. The ceiling binary is a single
process with one tokio thread per core — it is the bound, not a
competitor. No app serves an access log.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import http.client
import json
import os
import platform
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
E2E_DIR = Path(__file__).resolve().parent
VENV = E2E_DIR / ".venv"
RESULTS_DIR = E2E_DIR / "results"
BASELINE_PATH = REPO_ROOT / "benches" / "baseline.json"
CEILING_BIN = REPO_ROOT / "target" / "release" / "ceiling-server"

# The three M1 scenarios (benches/e2e/README.md): name -> request path.
SCENARIOS: dict[str, str] = {
    "plaintext": "/",
    "json": "/json",
    "item": "/items/42",
}

# Apps whose numbers reflect Fulmen's own code. `just bench-check` only
# gates on these; the third-party apps exist for ratio context and are
# noisy across machines.
SELF_APPS = ("fulmen", "fulmen_async", "ceiling")

FRAMEWORKS = (
    "fastapi",
    "uvicorn",
    "litestar",
    "robyn",
    "flask",
    "gunicorn",
    "uvloop",
    "fulmen",
)


def venv_bin(name: str) -> str:
    """Absolute path to a binary inside the benchmark venv."""
    return str(VENV / "bin" / name)


def fulmen_command(port: int, workers: int, module: str, extra: list[str]) -> list[str]:
    return [
        venv_bin("fulmen"),
        "run",
        f"{module}:app",
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
        "--workers",
        str(workers),
        *extra,
    ]


@dataclasses.dataclass(frozen=True)
class App:
    """One server under test: how to start it and what process model it runs."""

    key: str
    port: int
    note: str
    command: Any  # (workers: int, extra: list[str]) -> list[str]
    env: dict[str, str] = dataclasses.field(default_factory=dict)


def _uvicorn(workers: int, extra: list[str], *, module: str, port: int) -> list[str]:
    return [
        venv_bin("uvicorn"),
        f"{module}:app",
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
        "--workers",
        str(workers),
        "--no-access-log",
        *extra,
    ]


APPS: dict[str, App] = {
    "fastapi": App(
        "fastapi",
        8100,
        "uvicorn workers=N",
        lambda w, extra: _uvicorn(w, extra, module="apps.fastapi_app", port=8100),
    ),
    "litestar": App(
        "litestar",
        8101,
        "uvicorn workers=N",
        lambda w, extra: _uvicorn(w, extra, module="apps.litestar_app", port=8101),
    ),
    "robyn": App(
        "robyn",
        8102,
        "--processes N",
        lambda w, extra: [
            venv_bin("python"),
            "apps/robyn_app.py",
            "--processes",
            str(w),
            *extra,
        ],
        env={"ROBYN_HOST": "127.0.0.1", "ROBYN_PORT": "8102"},
    ),
    "flask": App(
        "flask",
        8103,
        "gunicorn workers=N",
        lambda w, extra: [
            venv_bin("gunicorn"),
            "--workers",
            str(w),
            "--bind",
            "127.0.0.1:8103",
            "apps.flask_app:app",
            *extra,
        ],
    ),
    "fulmen": App(
        "fulmen",
        8104,
        "fulmen run --workers N",
        lambda w, extra: fulmen_command(8104, w, "apps.fulmen_app", extra),
    ),
    "fulmen_async": App(
        "fulmen_async",
        8105,
        "fulmen run --workers N (async def)",
        lambda w, extra: fulmen_command(8105, w, "apps.fulmen_async_app", extra),
    ),
    "ceiling": App(
        "ceiling",
        8106,
        "single process, tokio threads = cores",
        lambda w, extra: [str(CEILING_BIN), "--host", "127.0.0.1", "--port", "8106", *extra],
    ),
}


@dataclasses.dataclass
class RunResult:
    """Normalized result of one app/scenario measurement."""

    requests_per_sec: float
    p50_ms: float
    p99_ms: float
    requests: int
    status_codes: dict[str, int]
    errors: dict[str, int]

    def as_dict(self) -> dict[str, Any]:
        return dataclasses.asdict(self)


def sh(args: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, capture_output=True, text=True, check=False, **kwargs)


def port_open(port: int) -> bool:
    """Whether something is already accepting TCP on `port`."""
    try:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=0.5)
        conn.connect()
    except OSError:
        return False
    conn.close()
    return True


def wait_ready(port: int, server: ServerProc, timeout: float) -> None:
    """Poll `GET /` until it answers 200 or the deadline passes.

    Fails fast when `server` exits while we wait — otherwise a stale
    process already on `port` would answer and silently contaminate the
    measurement.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if server.crashed():
            raise RuntimeError(f"server on port {port} exited; see {server.log_path}")
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
            conn.request("GET", "/")
            status = conn.getresponse().status
            conn.close()
            if status == 200:
                return
        except OSError:
            time.sleep(0.1)
    raise TimeoutError(f"port {port} did not answer 200 within {timeout:.0f}s")


def run_oha(
    url: str, duration_s: float, connections: int, *, json_out: bool = True
) -> dict[str, Any]:
    """One `oha` run; `-w` waits for in-flight requests at the deadline so
    they are not counted as aborts."""
    args = [
        "oha",
        "-z",
        f"{duration_s}s",
        "-c",
        str(connections),
        "-w",
        "--no-tui",
        "--output-format",
        "json" if json_out else "quiet",
        url,
    ]
    proc = sh(args)
    if not json_out:
        return {}
    if proc.returncode != 0:
        raise RuntimeError(f"oha failed on {url}: {proc.stderr.strip()}")
    return json.loads(proc.stdout)  # type: ignore[no-any-return]


def measure(
    port: int, path: str, duration_s: float, connections: int, warmup_s: float
) -> RunResult:
    url = f"http://127.0.0.1:{port}{path}"
    if warmup_s > 0:
        run_oha(url, warmup_s, connections, json_out=False)
    data = run_oha(url, duration_s, connections)
    summary = data["summary"]
    percentiles = data["latencyPercentiles"]
    return RunResult(
        requests_per_sec=float(summary["requestsPerSec"]),
        p50_ms=float(percentiles["p50"]) * 1000,
        p99_ms=float(percentiles["p99"]) * 1000,
        requests=int(sum(data["statusCodeDistribution"].values())),
        status_codes={str(k): int(v) for k, v in data["statusCodeDistribution"].items()},
        errors={str(k): int(v) for k, v in data["errorDistribution"].items()},
    )


class ServerProc:
    """A server subprocess plus its whole process group."""

    def __init__(self, args: list[str], cwd: Path, env: dict[str, str]) -> None:
        self.log_path = RESULTS_DIR / "server.log"
        self.log = self.log_path.open("w")
        self.proc = subprocess.Popen(
            args,
            cwd=cwd,
            env={**os.environ, **env},
            stdout=self.log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    def stop(self, timeout: float = 8.0) -> None:
        """SIGTERM the process group, then SIGKILL whatever is left."""
        if self.proc.poll() is not None:
            self.log.close()
            return
        pgid = os.getpgid(self.proc.pid)
        with contextlib.suppress(ProcessLookupError):
            os.killpg(pgid, signal.SIGTERM)
        deadline = time.monotonic() + timeout
        while self.proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        if self.proc.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(pgid, signal.SIGKILL)
            self.proc.wait(timeout=5)
        self.log.close()

    def crashed(self) -> bool:
        return self.proc.poll() is not None


def bench_app(
    app: App,
    scenarios: dict[str, str],
    args: argparse.Namespace,
    extra: list[str] | None = None,
) -> dict[str, RunResult]:
    """Warm up and measure every scenario for one app, then stop it."""
    command = app.command(args.workers, extra or [])
    print(f"[{app.key}] {' '.join(command)}  ({app.note})", flush=True)
    if port_open(app.port):
        raise RuntimeError(
            f"port {app.port} is already in use; kill the stale server before benching {app.key}"
        )
    server = ServerProc(command, E2E_DIR, app.env)
    out: dict[str, RunResult] = {}
    try:
        wait_ready(app.port, server, timeout=args.startup_timeout)
        for name, path in scenarios.items():
            if server.crashed():
                raise RuntimeError(f"{app.key} exited early; see {server.log_path}")
            result = measure(app.port, path, args.duration, args.connections, args.warmup)
            if server.crashed():
                raise RuntimeError(f"{app.key} died under load; see {server.log_path}")
            out[name] = result
            print(
                f"  {name:<10} {result.requests_per_sec:>10.0f} req/s  "
                f"p50 {result.p50_ms:.3f} ms  p99 {result.p99_ms:.3f} ms  "
                f"errors {result.errors or '{}'}",
                flush=True,
            )
    finally:
        server.stop()
    return out


def sysctl(name: str) -> str:
    return sh(["sysctl", "-n", name]).stdout.strip()


def framework_versions() -> dict[str, str]:
    """Package versions inside the benchmark venv, in one interpreter call."""
    names = " ".join(FRAMEWORKS)
    code = (
        "import importlib.metadata as m; "
        f"print('\\n'.join(f'{{p}}={{m.version(p)}}' for p in '{names}'.split()))"
    )
    proc = sh([venv_bin("python"), "-c", code])
    versions: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        key, _, value = line.partition("=")
        versions[key] = value
    return versions


def metadata(args: argparse.Namespace) -> dict[str, Any]:
    cores = os.cpu_count() or 0
    chip = (
        sysctl("machdep.cpu.brand_string")
        if platform.system() == "Darwin"
        else platform.processor()
    )
    oha = sh(["oha", "--version"]).stdout.strip()
    rustc = sh(["rustc", "--version"]).stdout.strip()
    return {
        "date": time.strftime("%Y-%m-%d %H:%M:%S %z"),
        "hardware": f"{chip}, {cores} cores",
        "os": f"{platform.system()} {platform.release()} ({platform.version()})",
        "python": platform.python_version(),
        "oha": oha.removeprefix("oha "),
        "rustc": rustc.removeprefix("rustc "),
        "versions": framework_versions(),
        "workers": args.workers,
        "connections": args.connections,
        "duration_s": args.duration,
        "warmup_s": args.warmup,
        "command": "python benches/e2e/run.py " + " ".join(sys.argv[1:]),
    }


def descendants(pid: int) -> list[int]:
    """Every descendant PID of `pid` (via `pgrep -P`, POSIX only)."""
    kids = [int(p) for p in sh(["pgrep", "-P", str(pid)]).stdout.split()]
    out = list(kids)
    for kid in kids:
        out.extend(descendants(kid))
    return out


def total_rss_kib(pids: list[int]) -> int:
    if not pids:
        return 0
    proc = sh(["ps", "-o", "rss=", "-p", ",".join(str(p) for p in pids)])
    return sum(int(line) for line in proc.stdout.split())


def soak(app: App, path: str, args: argparse.Namespace) -> dict[str, Any]:
    """Hold `oha` on one route for `--soak` seconds, sampling RSS."""
    command = app.command(args.workers, [])
    print(f"[soak {app.key}] {' '.join(command)}", flush=True)
    server = ServerProc(command, E2E_DIR, app.env)
    samples: list[dict[str, Any]] = []
    try:
        wait_ready(app.port, server, timeout=args.startup_timeout)
        url = f"http://127.0.0.1:{app.port}{path}"
        run_oha(url, args.warmup, args.connections, json_out=False)
        oha_proc = subprocess.Popen(
            [
                "oha",
                "-z",
                f"{args.soak}s",
                "-c",
                str(args.connections),
                "-w",
                "--no-tui",
                "--output-format",
                "json",
                "-o",
                str(RESULTS_DIR / "soak-oha.json"),
                url,
            ]
        )
        start = time.monotonic()
        while oha_proc.poll() is None:
            pids = [server.proc.pid, *descendants(server.proc.pid)]
            samples.append(
                {
                    "t_s": round(time.monotonic() - start, 1),
                    "rss_mib": round(total_rss_kib(pids) / 1024, 1),
                    "pids": len(pids),
                }
            )
            time.sleep(args.soak_interval)
    finally:
        server.stop()
    rss = [s["rss_mib"] for s in samples]
    head = rss[:3]
    tail = rss[-3:]
    summary = {
        "duration_s": args.soak,
        "samples": len(samples),
        "first_samples_median_mib": sorted(head)[len(head) // 2] if head else 0.0,
        "last_samples_median_mib": sorted(tail)[len(tail) // 2] if tail else 0.0,
        "min_mib": min(rss) if rss else 0.0,
        "max_mib": max(rss) if rss else 0.0,
    }
    first = summary["first_samples_median_mib"]
    last = summary["last_samples_median_mib"]
    summary["drift_pct"] = round((last - first) / first * 100, 2) if first else 0.0
    return {"summary": summary, "samples": samples}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apps", help=f"comma list, default all: {', '.join(APPS)}")
    parser.add_argument("--scenarios", help=f"comma list, default all: {', '.join(SCENARIOS)}")
    parser.add_argument("--workers", type=int, default=12, help="server processes (default: 12)")
    parser.add_argument("--duration", type=float, default=10.0, help="oha seconds (default: 10)")
    parser.add_argument(
        "--connections", type=int, default=100, help="oha connections (default: 100)"
    )
    parser.add_argument("--warmup", type=float, default=3.0, help="warmup seconds (default: 3)")
    parser.add_argument("--startup-timeout", type=float, default=30.0)
    parser.add_argument("--out", type=Path, default=RESULTS_DIR / "latest.json")
    parser.add_argument("--set-baseline", action="store_true", help=f"also write {BASELINE_PATH}")
    parser.add_argument(
        "--sweep-gil-batch",
        metavar="SIZES",
        help="comma list of gil_batch_size values; fulmen sync app, json scenario only",
    )
    parser.add_argument("--soak", type=float, metavar="SECONDS", help="soak duration for one app")
    parser.add_argument("--soak-interval", type=float, default=30.0, help="RSS sample seconds")
    return parser.parse_args()


def pick(mapping: dict[str, Any], names: str | None) -> dict[str, Any]:
    if not names:
        return mapping
    wanted = [n.strip() for n in names.split(",") if n.strip()]
    unknown = [n for n in wanted if n not in mapping]
    if unknown:
        raise SystemExit(f"unknown name(s) {unknown}; choose from {sorted(mapping)}")
    return {n: mapping[n] for n in wanted}


def write_json(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"wrote {path}", flush=True)


def main() -> int:
    args = parse_args()
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    apps = pick(APPS, args.apps)
    scenarios = pick(SCENARIOS, args.scenarios)

    if args.soak:
        app = next(iter(apps.values()))
        name, path = next(iter(scenarios.items()))
        document = {
            "metadata": metadata(args),
            "app": app.key,
            "scenario": name,
            **soak(app, path, args),
        }
        write_json(RESULTS_DIR / f"soak-{app.key}.json", document)
        print(json.dumps(document["summary"], indent=2), flush=True)
        return 0

    if args.sweep_gil_batch:
        sizes = [s.strip() for s in args.sweep_gil_batch.split(",") if s.strip()]
        app = APPS["fulmen"]
        sweep: dict[str, Any] = {}
        for size in sizes:
            extra = ["--gil-batch-size", size]
            results = bench_app(app, {"json": "/json"}, args, extra=extra)
            sweep[size] = results["json"].as_dict()
        document = {"metadata": metadata(args), "sweep_gil_batch_size": sweep}
        write_json(RESULTS_DIR / "gil_batch_sweep.json", document)
        return 0

    document: dict[str, Any] = {"metadata": metadata(args), "results": {}}
    for app in apps.values():
        results = bench_app(app, scenarios, args)
        document["results"][app.key] = {name: r.as_dict() for name, r in results.items()}
    write_json(args.out, document)
    if args.set_baseline:
        write_json(BASELINE_PATH, document)
    return 0


if __name__ == "__main__":
    sys.exit(main())
