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

M2 adds two POST scenarios to `SCENARIOS`: `validated` posts the ~10-field
order record to `/validate` (Fulmen `Body[Order]` model vs FastAPI's
pydantic body model), and `reject` posts a constraint-violating body to
the same route — Fulmen answers 422 from Rust before Python runs, which
`counter_probe` verifies by diffing each app's `counter_path` endpoint
around the measured run (`python_calls`). For apps flagged `openapi`,
a structural check of the served `/openapi.json` runs after the
scenarios and its booleans are stored as `__openapi__` (the M2 served-
document criterion; `check.py` re-verifies it from the saved file).

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


@dataclasses.dataclass(frozen=True)
class Scenario:
    """One measured request shape (benches/e2e/README.md).

    `counter_probe` fetches the app's `counter_path` before and after the
    measured run and records the delta as `python_calls` — the M2 422
    criterion's "invalid input never enters Python" evidence.
    """

    path: str
    method: str = "GET"
    body: bytes | None = None
    content_type: str | None = None
    counter_probe: bool = False


# The ~10-field order record shared by `validated` and `reject` — mixed
# str/int/float/bool plus a nested object (M2.17 §25.3).
VALID_BODY = json.dumps(
    {
        "order_id": 7,
        "customer": "ada lovelace",
        "email": "ada@fulmen.dev",
        "age": 36,
        "price": 19.95,
        "quantity": 2,
        "sku": "SKU-77",
        "tags": ["gift", "priority"],
        "geo": {"lat": 51.5, "lon": -0.12},
        "express": True,
    }
).encode()
# Schema-valid JSON that violates field constraints (order_id<1, customer
# empty, age negative, missing email, sku lowercase, lon out of range).
INVALID_BODY = json.dumps(
    {
        "order_id": 0,
        "customer": "",
        "age": -3,
        "price": 19.95,
        "quantity": 2,
        "sku": "bad",
        "tags": ["gift"],
        "geo": {"lat": 51.5, "lon": 999.0},
        "express": False,
    }
).encode()

# The M1 scenarios plus M2.17's validated-body and reject-path shapes.
SCENARIOS: dict[str, Scenario] = {
    "plaintext": Scenario("/"),
    "json": Scenario("/json"),
    "item": Scenario("/items/42"),
    "validated": Scenario("/validate", "POST", VALID_BODY, "application/json", counter_probe=True),
    "reject": Scenario("/validate", "POST", INVALID_BODY, "application/json", counter_probe=True),
}

# Apps whose numbers reflect Fulmen's own code. `just bench-check` only
# gates on these; the third-party apps exist for ratio context and are
# noisy across machines.
SELF_APPS = ("fulmen", "fulmen_async", "fulmen_pydantic", "ceiling")

FRAMEWORKS = (
    "fastapi",
    "uvicorn",
    "litestar",
    "robyn",
    "flask",
    "gunicorn",
    "uvloop",
    "fulmen",
    "pydantic",
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
    # `counter_path` serves {"calls": n} — handler-invocation evidence
    # for the reject scenario; `openapi` requests a structural check of
    # the served document after the scenario loop (M2 exit criterion).
    counter_path: str | None = None
    openapi: bool = False


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
        counter_path="/__counter",
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
        counter_path="/__counter",
        openapi=True,
    ),
    "fulmen_async": App(
        "fulmen_async",
        8105,
        "fulmen run --workers N (async def)",
        lambda w, extra: fulmen_command(8105, w, "apps.fulmen_async_app", extra),
        counter_path="/__counter",
        openapi=True,
    ),
    "fulmen_pydantic": App(
        "fulmen_pydantic",
        8107,
        "fulmen run --workers N (pydantic adapter)",
        lambda w, extra: fulmen_command(8107, w, "apps.fulmen_pydantic_app", extra),
        counter_path="/__counter",
        openapi=True,
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
    url: str,
    duration_s: float,
    connections: int,
    *,
    json_out: bool = True,
    method: str = "GET",
    body: bytes | None = None,
    content_type: str | None = None,
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
    ]
    if method != "GET":
        args += ["-m", method]
    if body is not None:
        body_path = RESULTS_DIR / "oha-body.json"
        body_path.write_bytes(body)
        args += ["-D", str(body_path)]
        if content_type:
            args += ["-H", f"content-type: {content_type}"]
    args.append(url)
    proc = sh(args)
    if not json_out:
        return {}
    if proc.returncode != 0:
        raise RuntimeError(f"oha failed on {url}: {proc.stderr.strip()}")
    return json.loads(proc.stdout)  # type: ignore[no-any-return]


def measure(
    port: int, scenario: Scenario, duration_s: float, connections: int, warmup_s: float
) -> RunResult:
    url = f"http://127.0.0.1:{port}{scenario.path}"
    call = {
        "method": scenario.method,
        "body": scenario.body,
        "content_type": scenario.content_type,
    }
    if warmup_s > 0:
        run_oha(url, warmup_s, connections, json_out=False, **call)  # type: ignore[arg-type]
    data = run_oha(url, duration_s, connections, **call)  # type: ignore[arg-type]
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


def fetch_json(port: int, path: str) -> dict[str, Any]:
    """One `GET` for JSON — counter probes and the OpenAPI check."""
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    conn.request("GET", path)
    resp = conn.getresponse()
    data = resp.read()
    status = resp.status
    conn.close()
    if status != 200:
        raise RuntimeError(f"GET {path} answered {status}")
    return json.loads(data)  # type: ignore[no-any-return]


def check_openapi(port: int) -> dict[str, bool]:
    """The M2 OpenAPI exit criterion: the served document parses and the
    rich app's surface is all there — 3.1 version, route paths, the
    `/validate` requestBody `$ref` into `components/schemas`, the nested
    `Geo` component, required fields and emitted constraints."""
    doc = fetch_json(port, "/openapi.json")
    order = (doc.get("components", {}).get("schemas") or {}).get("Order") or {}
    geo = (doc.get("components", {}).get("schemas") or {}).get("Geo") or {}
    props = order.get("properties") or {}
    post = (doc.get("paths", {}).get("/validate") or {}).get("post") or {}
    req_schema = (
        (post.get("requestBody") or {}).get("content", {}).get("application/json", {})
    ).get("schema") or {}
    checks = {
        "version_3_1": doc.get("openapi") == "3.1.0",
        "info_title": bool((doc.get("info") or {}).get("title")),
        "paths_present": all(
            p in doc.get("paths", {}) for p in ("/", "/json", "/items/{item_id}", "/validate")
        ),
        "validate_request_body": bool(req_schema.get("$ref")),
        "order_component": order.get("type") == "object" and len(props) >= 10,
        "geo_component": geo.get("type") == "object"
        and set(geo.get("properties") or {}) == {"lat", "lon"},
        "order_required": {"order_id", "customer", "email"} <= set(order.get("required") or []),
        "order_constraints": (props.get("order_id") or {}).get("minimum") == 1
        and (props.get("customer") or {}).get("minLength") == 1
        and "pattern" in (props.get("sku") or {}),
        "order_nested_geo": "geo" in props,
        "response_200": "200" in (post.get("responses") or {}),
    }
    if not all(checks.values()):
        bad = [k for k, v in checks.items() if not v]
        raise RuntimeError(f"openapi.json structural check failed: {bad}")
    return checks


def bench_app(
    app: App,
    scenarios: dict[str, Scenario],
    args: argparse.Namespace,
    extra: list[str] | None = None,
) -> dict[str, dict[str, Any]]:
    """Warm up and measure every scenario for one app, then stop it."""
    command = app.command(args.workers, extra or [])
    print(f"[{app.key}] {' '.join(command)}  ({app.note})", flush=True)
    if port_open(app.port):
        raise RuntimeError(
            f"port {app.port} is already in use; kill the stale server before benching {app.key}"
        )
    server = ServerProc(command, E2E_DIR, app.env)
    out: dict[str, dict[str, Any]] = {}
    try:
        wait_ready(app.port, server, timeout=args.startup_timeout)
        for name, scenario in scenarios.items():
            if server.crashed():
                raise RuntimeError(f"{app.key} exited early; see {server.log_path}")
            calls_before = (
                fetch_json(app.port, app.counter_path)["calls"]
                if scenario.counter_probe and app.counter_path
                else None
            )
            result = measure(app.port, scenario, args.duration, args.connections, args.warmup)
            if server.crashed():
                raise RuntimeError(f"{app.key} died under load; see {server.log_path}")
            entry = result.as_dict()
            if calls_before is not None:
                calls_after = fetch_json(app.port, app.counter_path)["calls"]
                entry["python_calls"] = calls_after - calls_before
            out[name] = entry
            print(
                f"  {name:<10} {result.requests_per_sec:>10.0f} req/s  "
                f"p50 {result.p50_ms:.3f} ms  p99 {result.p99_ms:.3f} ms  "
                f"errors {result.errors or '{}'}"
                + (f"  python_calls={entry['python_calls']}" if calls_before is not None else ""),
                flush=True,
            )
        if app.openapi:
            checks = check_openapi(app.port)
            out["__openapi__"] = checks  # type: ignore[dict-item]
            print(f"  openapi    {sum(checks.values())}/{len(checks)} checks ok", flush=True)
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
            results = bench_app(app, {"json": SCENARIOS["json"]}, args, extra=extra)
            sweep[size] = results["json"]
        document = {"metadata": metadata(args), "sweep_gil_batch_size": sweep}
        write_json(RESULTS_DIR / "gil_batch_sweep.json", document)
        return 0

    document: dict[str, Any] = {"metadata": metadata(args), "results": {}}
    for app in apps.values():
        document["results"][app.key] = bench_app(app, scenarios, args)
    write_json(args.out, document)
    if args.set_baseline:
        write_json(BASELINE_PATH, document)
    return 0


if __name__ == "__main__":
    sys.exit(main())
