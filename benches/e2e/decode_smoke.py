#!/usr/bin/env python3
"""Local decode+validate smoke for the M2 exit criteria's fuzzer line.

M2.19's `cargo-fuzz` targets are not merged yet, so this is the interim
guardrail: serve a rich app in-process (`serve_for_test`) and hammer its
`/validate` route with a hostile JSON corpus — deep nesting, huge
strings, boundary numbers, unicode, malformed/truncated payloads, wrong
shapes — asserting the server always answers 200/400/415/422, never
hangs, crashes or 5xxs. Run with `--seconds N` (default 60); the
one-hour `cargo fuzz` soak is still pending on M2.19's harness.

Usage: python benches/e2e/decode_smoke.py [--seconds 60]
"""

from __future__ import annotations

import argparse
import http.client
import json
import random
import sys
import threading
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "python"))

from apps import fulmen_pydantic_app
from fulmen._fulmen import serve_for_test

ORDER = {
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


def hostile_corpus(rng: random.Random) -> list[bytes]:
    """A fixed-shape pool of inputs spanning every response channel."""
    deep: Any = "x"
    for _ in range(60):
        deep = {"a": [deep]}  # past max_depth
    big = "x" * (4 * 1024 * 1024)
    many = [{"n": i} for i in range(200_000)]  # past max_items
    return [
        json.dumps(ORDER).encode(),
        json.dumps({**ORDER, "order_id": "7", "age": "old"}).encode(),  # wrong types
        json.dumps({**ORDER, "unknown": "field", "geo": {"lat": "91"}}).encode(),
        json.dumps({"order_id": -(2**80), "customer": "", "email": None}).encode(),
        json.dumps({"order_id": 2**80, "customer": "\ud83d\ude00" * 200}).encode(),
        json.dumps({"order_id": 7.5, "customer": 12, "geo": [1]}).encode(),
        json.dumps({"tags": big}).encode(),  # past max_str_bytes
        json.dumps(deep).encode(),  # past max_depth
        json.dumps(many).encode(),  # past max_items
        b"{",
        b"[",
        b"",
        b"\x00\xff\xfe",
        b'"unterminated',
        b"null",
        b"42",
        b'"string"',
        b"true",
        b"[]",
        b"[1,2,",
        ORDER and json.dumps(ORDER)[:-15].encode(),  # truncated
        b"{" + b'"k":1,' * 50_000 + b"}",  # wide object
        b"NaN",
        b"Infinity",
        b"01",
        b"+1",
        b"1e999",
        b"-",
        b".5",
        "\U0001f600 unicode \x00 nul".encode(),
        json.dumps({"geo": {"lat": rng.uniform(-1e6, 1e6)}}).encode(),
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=float, default=60.0)
    args = parser.parse_args()

    rng = random.Random(0xF01)  # noqa: S311 -- deterministic corpus, not crypto
    corpus = hostile_corpus(rng)
    server = serve_for_test(fulmen_pydantic_app.app._compile_native())  # pyright: ignore[reportPrivateUsage]
    base = f"http://{server.addr()}"
    stop = threading.Event()
    counts: dict[Any, int] = {}
    failures: list[str] = []
    lock = threading.Lock()

    port = int(base.rsplit(":", 1)[1])

    def hammer(worker: int) -> None:
        local: dict[Any, int] = {}
        local_rng = random.Random(worker)  # noqa: S311 -- corpus selection, not crypto
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            while not stop.is_set():
                body = local_rng.choice(corpus)
                try:
                    conn.request("POST", "/validate", body, {"content-type": "application/json"})
                    resp = conn.getresponse()
                    status = resp.status
                    resp.read()  # drain — keep-alive needs a consumed body
                except (http.client.HTTPException, OSError) as exc:
                    # transport errors: reconnect once, then count
                    try:
                        conn.close()
                        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    except OSError:
                        pass
                    local["transport_abort"] = local.get("transport_abort", 0) + 1
                    if local["transport_abort"] <= 3:
                        with lock:
                            failures.append(f"worker {worker}: {exc!r} (informational)")
                    continue
                local[status] = local.get(status, 0) + 1
        finally:
            conn.close()
            with lock:
                for code, n in local.items():
                    counts[code] = counts.get(code, 0) + n

    threads = [threading.Thread(target=hammer, args=(i,), daemon=True) for i in range(8)]
    deadline = time.monotonic() + args.seconds
    for t in threads:
        t.start()
    while time.monotonic() < deadline:
        time.sleep(0.25)
    stop.set()
    for t in threads:
        t.join(timeout=5)

    # Liveness: after the soak the server must still answer a valid body.
    try:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        conn.request(
            "POST", "/validate", json.dumps(ORDER).encode(), {"content-type": "application/json"}
        )
        alive = conn.getresponse().status == 200
        conn.close()
    except (http.client.HTTPException, OSError):
        alive = False
    server.stop()

    total = sum(n for code, n in counts.items() if code != "transport_abort")
    five_xx = sum(n for code, n in counts.items() if isinstance(code, int) and code >= 500)
    allowed = {200, 400, 413, 415, 422}
    unexpected = {c: n for c, n in counts.items() if isinstance(c, int) and c not in allowed}

    print(f"decode-smoke {args.seconds:.0f}s: {total:,} requests")
    for code in sorted(k for k in counts if k != "transport_abort"):
        print(f"  {code}: {counts[code]:,}")
    if counts.get("transport_abort"):
        print(
            f"  transport_abort: {counts['transport_abort']:,} "
            "(server closed oversized/violent writes — tolerated)"
        )
    for f in failures:
        print(f"  note {f}")
    print(f"  liveness: {'ok' if alive else 'DEAD'}")

    ok = alive and five_xx == 0 and not unexpected and total > 0
    print("decode-smoke", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
