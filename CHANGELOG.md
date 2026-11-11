# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, minor versions may
break the API; every break is listed under **Breaking**.

`just release X.Y.Z` refuses to run unless a non-empty `## [X.Y.Z]` section
exists. Move the `Unreleased` entries under the new version heading first.

## [Unreleased]

### Added

- Python bridge (`fulmen-py`) and public API: `Fulmen` app with
  `get`/`post`/`put`/`patch`/`delete`/`head`/`options` decorators and
  `route(path, methods)`; `fulmen.Response` for explicit status/headers;
  `app.run(host, port, workers, debug)` and `fulmen run module:app`; a
  bounded OS-thread pool for sync handlers (GIL held only around the call);
  return conversion `str`/`bytes`/`dict`/`list`/`tuple`/`Response` with
  Rust-side JSON serialization; `404`/`405`+`Allow`, automatic `OPTIONS`
  and `HEAD`, `308` trailing-slash redirects, `503`+`Retry-After` queue
  shedding; `StartupError` aggregating Python and core declaration errors;
  handler exceptions and dispatch panics surface as `500` with tracebacks
  in the `fulmen` logger only; `fulmen._fulmen.serve_for_test` (plus
  `NativeApp`/`TestServer`) for real-server tests; `mimalloc` as the
  extension's global allocator. Refs: M1.6, M1.13, M1.14, M1.19–M1.22.
- Async dispatch (M1.15): `async def` handlers run on a real `asyncio` event
  loop on a dedicated OS thread per app (`fulmen-asyncio`). Dispatch
  materializes arguments, queues the job (1 024 deep, 5 s deadline), and
  wakes the loop with `loop.call_soon_threadsafe`; a pump drains the queue
  into `loop.create_task` and the done-callback completes the awaiting
  request through a tokio oneshot. Full or overdue queues answer `503` +
  `Retry-After`; a dead loop answers `500`.
- uvloop selection (M1.16): `uvloop=` on `app.run`/`serve`/`serve_for_test`
  and `--uvloop`/`--no-uvloop` on the CLI. `None` uses uvloop when
  installed, `False` forces plain asyncio, `True` requires uvloop or fails
  startup.
- GIL batching (M1.17): when more sync handlers are queued than the pool
  can run in parallel, up to `gil_batch_size` (default 32) of the surplus
  run under one GIL acquisition — `gil_batch_size` on `app.run`/`serve`/CLI;
  disabled on free-threaded builds. Batches only form under real backlog,
  so small bursts still spread across the pool.
- Free-threaded support verified (M1.18): importing `fulmen` does not
  re-enable the GIL on free-threaded interpreters, and pool workers overlap
  under concurrency (`test_freethreaded.py`, `just test-matrix` 3.14t leg).
- Multi-process workers (M1.23, decision D15): `app.run(workers=N)` and
  `fulmen run --workers N` spawn N processes on POSIX, each binding the
  same `host:port` with `SO_REUSEPORT` (Linux and macOS — verified). A
  Python supervisor restarts dead workers and tears the group down after a
  slot crashes more than 5 times in 60 s; SIGINT/SIGTERM stop the whole
  group. `workers > 1` is refused with a clear error off POSIX.
- Benchmark suite (M1.24–M1.29): `benches/e2e/run.py` drives `oha` against
  seven apps (FastAPI+uvicorn, Litestar+uvicorn, Robyn, Flask+gunicorn,
  Fulmen sync `def`, Fulmen `async def`, and a pure-Rust `ceiling-server`
  binary in `benches/micro`) over plaintext/JSON/path-param scenarios;
  `just bench` produces `benches/baseline.json`, `just bench-check`
  enforces the 5% regression rule against it and joins `just ci` once a
  baseline exists. First results and decisions in
  `benches/results/0.1.0.md` and DESIGN.md D16/D17.
- `fulmen-server::serve_listener` serves on an already-bound
  `std::net::TcpListener`, sharing the accept loop with `serve` (the
  `SO_REUSEPORT` entry point workers use).
- `fulmen-server`: pure-Rust HTTP/1.1 transport on tokio + hyper with a
  `Dispatcher` trait boundary (`serve`/`Server`, `ServerConfig`,
  `Request`/`Response` wire types), keep-alive and pipelining, request
  limits (431/414/413), read and keep-alive-idle timeouts, SIGINT/SIGTERM
  and programmatic graceful shutdown with a 30 s drain deadline, cached
  `Date` header, and a pure-Rust `TestDispatcher` for tests and
  benchmarks. Refs: M1.1–M1.7.
- Project skeleton: Cargo workspace (`fulmen-core`, `-server`, `-schema`,
  `-security`, `-py`), Python package with type stubs, `fulmen info` CLI.
- Local CI/CD through `just`, git hooks, and repository guardrails.
- Design document and v1 roadmap.
- Router in `fulmen-core` (M1.8–M1.12): route declarations compiled into an
  immutable `CompiledApp` (one `matchit` tree per HTTP method), typed `int`/
  `str` path parameters returned in declaration order, automatic `HEAD`,
  404/405 results with `Allow` (incl. `OPTIONS` metadata), route groups and
  `mount` with exact prefix joining, strict/redirect/ignore trailing-slash
  policy, and startup errors that aggregate duplicates, pattern conflicts,
  and parameter-type mismatches.
