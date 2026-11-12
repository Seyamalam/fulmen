# Roadmap to v1

Everything in [DESIGN.md](DESIGN.md) §2 "Goals for v1" is in scope. This
document orders that work into milestones. Each milestone ships a release
to PyPI, passes `just ci`, and meets its exit criteria before the next starts.

- **Current milestone: M1.**
- Task IDs (`M1.3`) are used in commit bodies and the changelog
  (`Refs: M1.3`), so progress can be traced.
- A checked box means merged to `main` with tests, docs and a changelog entry.

## Overview

| Milestone | Theme | Release | Depends on | Coverage floor |
| --- | --- | --- | --- | --- |
| M0 | Foundation | – | – | 80% |
| M1 | Fast core: server, router, dispatch, benchmarks | 0.1.0 | M0 | 80% |
| M2 | Types: schema engine, models, extraction, errors, OpenAPI | 0.2.0 | M1 | 85% |
| M3 | Extensibility: hooks, plugins, DI, adapters, native ABI | 0.3.0 | M2 | 85% |
| M4 | Security: limits, guardrails, CORS, CSRF, rate limit, JWT, sessions | 0.4.0 | M3 | 90% |
| M5 | Protocols and workloads: HTTP/2, TLS, WebSockets, SSE, jobs, cache, static, DB | 0.5.0 | M4 | 90% |
| M6 | Tooling, docs, release: CLI, codegen, migration, benchmarks, 1.0 | 1.0.0 | M5 | 90% |

The coverage floor is the `fail_under` value in `pyproject.toml` from that
milestone on (Python). Rust coverage is reported by `just cov-rust` and gets
the same floor from M2.

```
M0 ──▶ M1 ──▶ M2 ──▶ M3 ──▶ M4 ──▶ M5 ──▶ M6
              │             │
              └─ M2.11 OpenAPI feeds M6 codegen
                            └─ M3 plugin API is used by every M4/M5 feature
```

---

## M0: Foundation (done)

- [x] M0.1 `maturin new` project, Cargo workspace with core/server/schema/security/py crates
- [x] M0.2 Python package, type stubs, `py.typed`, `fulmen info`
- [x] M0.3 License (Apache-2.0), NOTICE, trademark policy, contribution guidelines, DCO
- [x] M0.4 Local CI/CD: `justfile`, git hooks, `scripts/guardrails.py`, `scripts/check_commits.py`, `scripts/release.py`
- [x] M0.5 Rust lints, clippy pedantic, `cargo-deny` policy, pinned toolchain, typos
- [x] M0.6 Design document, roadmap, guardrails document, agent rules
- [x] M0.7 GitHub repository renamed to `fulmen`

---

## M1: Fast core

**Goal:** a real server that answers requests from Python handlers, and
numbers that prove (or disprove) the speed thesis before anything else is
built on it.

**Ships as 0.1.0:** `Fulmen()`, `@app.get/post/...`, path params (typed as
`int`/`str` only), `def` and `async def` handlers returning `str`, `bytes`,
`dict`/`list` (JSON) or `Response`, `app.run()`, `fulmen run --workers`.

### Server (`fulmen-server`)

- [x] M1.1 tokio multi-thread runtime, configurable worker threads
- [x] M1.2 hyper 1.x HTTP/1.1 connection handling with keep-alive and pipelining
- [x] M1.3 `Dispatcher` trait: the server's only interface to handlers
- [x] M1.4 Pure-Rust test dispatcher, used for benchmarks and server tests
- [x] M1.5 Graceful shutdown on SIGINT/SIGTERM: stop accepting, drain, timeout
- [x] M1.6 Cached `Date` header, pre-encoded static headers, `mimalloc` allocator
      (the allocator half landed with the Python bridge: `mimalloc` is the
      `#[global_allocator]` of the `fulmen-py` cdylib, target-gated to
      Linux/macOS/Windows)
- [x] M1.7 Basic limits: header size, body size, read timeouts (full set in M4)

### Router (`fulmen-core`)

- [x] M1.8 Route table built from declarations, frozen into `CompiledApp`
- [x] M1.9 `matchit` per method, static > param > catch-all
- [x] M1.10 Automatic `HEAD`, 404, 405 with `Allow`
- [x] M1.11 Route groups with prefixes, `app.mount()`
- [x] M1.12 Startup errors for duplicate and shadowed routes

### Python bridge (`fulmen-py`)

- [x] M1.13 `Fulmen` class and route decorators (Python), declarations passed to Rust
- [x] M1.14 Sync dispatch: thread pool, `Python::attach` only around the call
- [x] M1.15 Async dispatch: asyncio loop thread, `call_soon_threadsafe`, oneshot completion
      (`aloop.rs`: one `fulmen-asyncio` thread per app with async routes,
      bounded 1024-job queue, empty→non-empty CST wake, pump drains into
      `loop.create_task`, done-callback settles a tokio oneshot; 503 on
      full/overdue, 500 on dead loop)
- [x] M1.16 Optional uvloop
      (tri-state `uvloop`: `None` auto, `False` forced asyncio, `True`
      required or `StartupError`; `app.run(uvloop=...)`, `serve`,
      `serve_for_test`, CLI `--uvloop`/`--no-uvloop`)
- [x] M1.17 GIL batching on GIL builds (batch size and budget configurable)
      (`gil_batch_size`/`gil_batch_budget_us` on `app.run`/`serve`/CLI;
      batches never take jobs parked siblings could absorb)
- [x] M1.18 Free-threaded build: verified parallel sync handlers, no GIL re-enable
      (`test_freethreaded.py`: `sys._is_gil_enabled()` stays `False`,
      barrier rendezvous proves overlapping handlers; 3.14t leg of
      `just test-matrix`)
- [x] M1.19 Return conversion: `str`, `bytes`, `dict`/`list` → JSON, `Response`
- [x] M1.20 Unhandled exceptions → 500, traceback in logs only
- [x] M1.21 Rust panics caught at the dispatch boundary → 500
- [x] M1.22 `app.run(host, port, workers)` and `fulmen run module:app`
      (`workers > 1` is accepted but runs a single process until M1.23)
- [x] M1.23 Multi-process workers with `SO_REUSEPORT` (Linux); decide Q6 for macOS/Windows
      (Q6→D15: `SO_REUSEPORT` on all POSIX incl. macOS — verified; Python
      supervisor with bounded crash-loop restart; Windows refuses
      `workers > 1` clearly)

### Benchmarks

- [x] M1.24 `benches/micro`: criterion for router and response writing
- [x] M1.25 `benches/e2e`: `oha` scenarios, comparison apps for FastAPI, Litestar, Robyn, Flask
      (`run.py` matrix runner, `apps/fulmen_async_app.py`, `ceiling-server` bin)
- [x] M1.26 `just bench` and `just bench-check` with `benches/baseline.json` and the 5% rule
- [x] M1.27 Benchmark the pure-Rust dispatcher (ceiling) vs Python sync vs Python async
      (≈148k vs ≈83k vs ≈140k req/s on `/json` at 12 workers — `benches/results/0.1.0.md`)
- [x] M1.28 Decide Q1 (loop per process vs per thread) and Q2 (batch defaults); record in DESIGN.md §28
      (Q1→D16 one loop per app; Q2→D17 batch size stays 32)

### Guardrails added in M1

- [x] M1.29 `bench-check` in `just ci` once a baseline exists
      (the `ci` recipe runs it only when `benches/baseline.json` is present)
- [x] M1.30 Server integration tests on all four interpreters in `just test-matrix`
      (3.14t leg verified end-to-end: `serve_for_test` real-socket tests and
      multi-process worker tests pass with the GIL disabled)

### Exit criteria

- Plain text and JSON scenarios at least 5× FastAPI + uvicorn requests/sec,
  or DESIGN.md §25.1 updated with measured numbers and the reason.
- The async path costs no more than 30% over the sync path in the JSON
  scenario, or a redesign is planned before M2.
- No crashes or leaks in a 10-minute soak test at full load (RSS stable within 5%).
- Results committed to `benches/results/0.1.0.md`.

---

## M2: Types

**Goal:** the handler signature becomes the contract. Inputs are validated
and outputs encoded natively, with the GIL released for all byte work.

**Ships as 0.2.0:** `Model`, all types in DESIGN.md §11.1, `Query`/`Header`/
`Cookie`/`Body`/`Form`/`File`, constraints, RFC 9457 errors, response unions,
OpenAPI at `/openapi.json` with a docs UI, Pydantic adapter.

### Schema engine (`fulmen-schema`)

- [x] M2.1 Type descriptor IR (DESIGN.md §11.3) and its conversion from Python types in `fulmen-py` — `TypeDesc` IR + `compile()` in `fulmen-schema`; `_typing.py` annotation→spec + `compile_descriptor`/`Descriptor` in `fulmen-py` (docs/SPEC-FORMAT.md)
- [x] M2.2 Validators for scalars, time types, identifiers (`UUID`, `Email`, `Url`, IP)
- [x] M2.3 Containers, `Literal`, enums, `Optional`, unions, discriminated unions
- [x] M2.4 Records: `Model`, dataclasses, `TypedDict` (`NotRequired`), generics, `NewType`
- [x] M2.5 Constraints: `Len`, `Range`, `Pattern` (Rust `regex`), `MultipleOf`, `Unique`, transforms
- [x] M2.6 One-pass JSON decode + validate into Python objects; decide Q3 (JSON engine) by benchmark — `decode_json`/`decode_and_validate` in `fulmen-schema` (own byte-slice parser per D18); `fulmen-py` materialization in `descriptor.rs`
- [x] M2.7 Strict mode for JSON, lax coercion for query/path/headers/forms — `decode_query`/`decode_form`/`decode_cookie`/`validate_text` wired through `extract.rs`; lax singleton→list promotion fixed in `fulmen-schema`
- [x] M2.8 Depth, item count and string length limits
- [x] M2.9 Native encoder driven by the return type descriptor; debug-mode output checking — `response_descs` drive conversion; output checking is always-on (stricter than debug-only)
- [x] M2.10 Error collection with JSON-pointer locations, no input echo by default — `ValidationErrors::to_json`/`to_problem_entries`, `ErrorEntry::loc_pointer`/`problem`

### API surface

- [x] M2.11 OpenAPI 3.1 generation from the IR; `/openapi.json`; docs UI (Scalar or Swagger UI, self-hosted assets) — Swagger UI 5.34.1 vendored gzip-compressed under `fulmen/static`; `Fulmen(title=..., version=..., openapi_url=..., docs_url=...)` + `app.openapi()`
- [x] M2.12 `Model` class: native `__init__` (`Descriptor.construct`), `dataclass_transform`, `frozen`, `extra`, `rename`, validated `__setattr__` — no `__slots__` (models keep `__dict__` for `vars()`/`model_dump`); `model_validate`/`model_dump` included
- [x] M2.13 Parameter sources (DESIGN.md §5.2): path, query, header (case-insensitive), cookie, JSON body, form-urlencoded, raw `bytes`/`Raw` — extracted and validated in `fulmen-py/src/extract.rs` without the GIL; multipart file upload remains (the `File`/`UploadFile` marker exists but has no `file` call-source yet)
- [x] M2.14 Response types: `Created[T]`, `Accepted[T]`, `NoContent`, `Redirect`, `FileResponse`, `Stream`, unions — `Stream`/`FileResponse` buffer the body; native chunked streaming is the documented M5 deviation (§10.3, §18.3)
- [x] M2.15 `HTTPError`, typed error shortcuts, `@app.on_error`, RFC 9457 everywhere — registered handlers win over built-in problem responses and resubmit through the matching executor (`JobAnswer::Deferred`)
- [x] M2.16 Decide Q4 (404 vs 422 for bad path params) and Q5 (Model mutability) — D20: `int` path mismatches 404 by construction, other scalars 422; D19: mutable `Model` with validated `__setattr__`, `frozen=True` opt-in

### Adapter

- [x] M2.17 `PydanticValidator`: pydantic models in parameters and returns, JSON schema into OpenAPI — `fulmen.adapters.pydantic` translates `model_fields`/`FieldInfo` (alias/validation_alias, constraint metadata, defaults/factories, `model_config`) into native `record` specs at registration; `pydantic-core` never runs on the request path, materialization restores `__pydantic_*` internals via `meta.after`, `RootModel`/generics supported, unsupported constructs raise `AnnotationError`; optional `fulmen[pydantic]` extra, lazy everything

### Guardrails added in M2

- [x] M2.18 Type tests in `python/tests/typing/` checked by pyright in `just ci`
- [x] M2.19 `cargo-fuzz` targets for the JSON decoder and the lax text-source parsers (`just fuzz`/`fuzz-smoke`; the one-hour run gates release/milestone exit — a multipart-parser target lands with the multipart parser itself, still pending in M2.13)
- [x] M2.20 Property tests (`proptest`) for encode → decode round trips
- [x] M2.21 Coverage floor raised to 85%

### Exit criteria

- Validated 10-field body scenario at least 5× FastAPI — **FAIL at 12 workers
  (1.89× sync / 3.06× async), PASS per worker process (21.1× sync / 35.3×
  async / 14.2× adapter)** — same shape as M1(a): FastAPI parallelizes its
  pydantic work across spawned workers while fulmen's per-process number is
  already near its pool bound (`benches/results/0.2.0.md` §a).
- 422 scenario at least 20× FastAPI (Python not entered; verified by a
  counter) — **FAIL on the multiplier (1.7× at 12 workers, 12.5× per
  process); the property holds exactly: counter-verified 0 Python calls and
  95–98% of the pure-Rust transport bound**. FastAPI's async-resident 422
  path is its fastest route on this stack, so ≥20× would need ~11× the
  machine's transport ceiling (§b).
- Generated OpenAPI passes an OpenAPI 3.1 validator for every test app —
  **PASS**: structural check inside `run.py` (10/10 per fulmen app:
  version/paths/`$ref` request bodies/components/required/constraints) plus
  `test_served_document_structure` walking the served doc.
- Fuzzers run 1 hour each without findings — **PENDING** (M2.19 targets not
  merged; `cargo-fuzz` installed, no harness). Interim: `decode_smoke.py`
  60 s — 363k hostile requests, zero 5xx/hangs (§d).

---

## M3: Extensibility

**Goal:** the public extension points. Every later feature is built on them.

**Ships as 0.3.0:** lifecycle hooks, plugins with typed handles, dependency
injection with scopes, adapter interfaces, native plugin SDK (unstable ABI),
`TestClient`.

### Pipeline (`fulmen-core`)

- [x] M3.1 Phases from DESIGN.md §8, each with native and Python hook lists — `Phase`/`HookDecl`/`RouteHooks` in `fulmen-core::pipeline`; `HookEntry::{Native, Python}` + `NativeApp.add_hook`/`add_native_hook` in `fulmen-py`
- [x] M3.2 Short-circuit semantics per phase; `on_error` routing by exception MRO — `None` continues, `dict` merges `request.state`, a value short-circuits/replaces; hook errors route the route's merged `ErrorTable`
- [x] M3.3 Group-scoped hooks; deterministic ordering; security phase priorities — `(priority, seq)` ordering, `PRIORITY_SECURITY`/`PRIORITY_SYSTEM`/`PRIORITY_USER` bands, `Fulmen(prefix=…)` + `app.mount()` scope layers
- [x] M3.4 `fulmen routes --hooks` and `--cost` (Python crossings per route) — built from `NativeApp.describe_routes()` declarations

### Plugins

- [ ] M3.5 `Plugin` protocol, `PluginContext`, `app.use()` returning typed handles
- [ ] M3.6 `requires`/`conflicts`, topological ordering, startup errors for missing/conflicting plugins
- [ ] M3.7 Plugin configuration through `Settings` sections
- [ ] M3.8 Plugins can add routes, providers, OpenAPI components, CLI commands, `check` rules
- [x] M3.9 Hook registration closed after startup (runtime guardrail, DESIGN.md §14.8) — `Fulmen` seals every declaration at compile; late registration raises `RuntimeError`

### Dependency injection

- [ ] M3.10 `@provider` with `APP`, `REQUEST`, `TRANSIENT` scopes; generator cleanup
- [ ] M3.11 Graph resolution at startup: cycles, missing providers, scope violations
- [ ] M3.12 Native providers resolved without entering Python
- [ ] M3.13 `app.override()` for tests

### Native plugins

- [ ] M3.14 `fulmen-plugin-sdk` crate: safe trait, `export_plugin!` macro, `catch_unwind` boundary
- [ ] M3.15 C ABI v1 (DESIGN.md §13.4), `PyCapsule` loading, ABI version check
- [ ] M3.16 Example native plugin in `examples/native-plugin`, built and tested in `just ci`
- [ ] M3.17 Add `fulmen-plugin-sdk` to `UNSAFE_ALLOWED` with a DESIGN.md entry (maintainer approval)

### Adapters

- [ ] M3.18 Interfaces as Rust traits and Python protocols: `Validator`, `SessionStore`, `RateLimitStore`, `CacheStore`, `JobQueue`
- [ ] M3.19 Memory implementations of each (defaults)
- [ ] M3.20 `fulmen-testkit` conformance suites; built-ins must pass them

### Testing

- [ ] M3.21 `TestClient` (sync and async), in-process native path, lifecycle hooks, `raise_server_errors`
- [ ] M3.22 `Settings` with `Secret[str]`, source precedence, validation at startup (DESIGN.md §20)

### Guardrails added in M3

- [ ] M3.23 Plugin isolation checks and startup validation (DESIGN.md §14.1, §14.8)
- [ ] M3.24 ABI compatibility test: plugin built against SDK N loads in Fulmen N, refused by a bumped ABI

### Exit criteria

- All M4/M5 features can be written using only the public plugin API (verified by a design review of their specs).
- A route with three native plugins and no Python hooks has the same crossing count as a bare route (one).
- Native plugin overhead under 1 µs per request in micro benchmarks.

---

## M4: Security

**Goal:** everything in DESIGN.md §14 and §16. After M4, a default Fulmen
app is safe to expose to the internet.

**Ships as 0.4.0:** full limits and timeouts, load shedding, production mode,
CORS, CSRF, rate limiting, JWT, sessions, secure headers, trusted proxies,
request IDs, log redaction.

### Runtime guardrails

- [ ] M4.1 Full limit table (DESIGN.md §14.3), per-route overrides, startup refusal of "unlimited" in production
- [ ] M4.2 All timeouts including minimum body rate (slow-client protection)
- [ ] M4.3 Handler timeout: cancel async, report stuck sync handlers
- [ ] M4.4 Load shedding with queue length and queue deadline, `503` + `Retry-After`
- [ ] M4.5 Production mode and its refusals (DESIGN.md §14.2)
- [ ] M4.6 Log redaction, `Secret` handling, no input echo, no stack traces in production
- [ ] M4.7 Debug-mode blocking detector for the asyncio loop

### Security plugins (`fulmen-security`)

- [ ] M4.8 `secure_headers` (on by default), `request_id` (on by default)
- [ ] M4.9 Trusted proxy resolution (CIDR list) for client IP, scheme and request ID
- [ ] M4.10 `cors`: exact, suffix, predicate origins; native preflight; startup checks
- [ ] M4.11 `rate_limit`: GCRA, multiple limits, key functions, RateLimit headers; memory and redis stores
- [ ] M4.12 `jwt`: explicit algorithms, claim checks, JWKS with background refresh, typed claims, signing helpers
- [ ] M4.13 `sessions`: signed cookie, optional AES-256-GCM, memory and redis stores, key rotation, `rotate()`
- [ ] M4.14 `csrf`: signed double-submit token, `Origin`/`Sec-Fetch-Site` checks, explicit exemptions
- [ ] M4.15 Native redis client adapter (shared by rate limit, sessions, cache, jobs)

### Guardrails added in M4

- [ ] M4.16 Security test suite: request smuggling (CL/TE), header injection, path traversal, oversized inputs, slowloris, JWT algorithm confusion, CSRF bypass attempts
- [ ] M4.17 Fuzz targets for the HTTP request path and JWT parsing
- [ ] M4.18 Self-review of every default against OWASP ASVS level 2 (sections that apply to a framework); findings fixed or documented
- [ ] M4.19 Coverage floor raised to 90%

### Exit criteria

- Security test suite passes; fuzzers run 4 hours each without findings.
- `fulmen run` refuses to start every misconfiguration listed in DESIGN.md §14.2 (one test each).
- Auth-protected scenario at least 5× FastAPI with an equivalent JWT dependency.
- Rate limiter decisions under 200 ns per request (memory store, micro benchmark).

---

## M5: Protocols and workloads

**Goal:** the remaining v1 features that real apps need.

**Ships as 0.5.0:** HTTP/2, TLS, WebSockets, SSE, streaming, background jobs,
response caching, static files, compression, database adapters,
observability plugins.

### Protocols (`fulmen-server`)

- [ ] M5.1 HTTP/2 (hyper-util auto), stream limits
- [ ] M5.2 TLS with rustls: certificate files, reload on SIGHUP, ALPN
- [ ] M5.3 Compression plugin: gzip, br, zstd negotiation, size threshold, skip for already compressed types
- [ ] M5.4 Static files: `sendfile`, `ETag`, `Range`, precompressed variants, path traversal protection

### Real-time

- [ ] M5.5 WebSockets: typed `WebSocket[In, Out]`, native framing and validation, ping/pong, limits
- [ ] M5.6 `Broadcast` helper with memory and redis backends
- [ ] M5.7 SSE: typed events, keep-alive, `Last-Event-ID`
- [ ] M5.8 Streaming request and response bodies, NDJSON

### Workloads

- [ ] M5.9 `BackgroundTasks` (after-response tasks)
- [ ] M5.10 `background` plugin: job queue, retries with backoff and jitter, dead-letter, timeouts, uniqueness, scheduled and cron jobs
- [ ] M5.11 `fulmen worker` command; in-process and redis queues
- [ ] M5.12 `cache` plugin: response caching by route and vary keys; memory and redis stores
- [ ] M5.13 Database adapters: SQLAlchemy async and asyncpg, APP/REQUEST providers, transaction-per-request option
- [ ] M5.14 Jinja2 template adapter

### Observability

- [ ] M5.15 `tracing` → Python `logging` bridge, JSON logs, native access log
- [ ] M5.16 `metrics` plugin (Prometheus), including GIL wait time and Python crossings
- [ ] M5.17 `otel` plugin: traces, W3C propagation, OTLP export
- [ ] M5.18 `health` plugin: native `/healthz`, `/readyz` with checks

### Guardrails added in M5

- [ ] M5.19 Integration tests against real redis and Postgres (Docker), `just test-integration`, part of `just ci` when Docker is available
- [ ] M5.20 WebSocket conformance with the Autobahn test suite
- [ ] M5.21 TLS configuration checks (no TLS < 1.2, no weak ciphers)

### Exit criteria

- Autobahn suite passes (strict cases).
- HTTP/2 scenario within 10% of HTTP/1.1 throughput.
- DB-bound scenario at least as fast as FastAPI + asyncpg (the honest case).
- WebSocket echo at least 3× FastAPI's.

---

## M6: Tooling, docs and 1.0

**Goal:** the developer experience that makes people switch, and a stable release.

**Ships as 1.0.0** (with `1.0.0-rc.N` releases first).

### CLI

- [ ] M6.1 `fulmen new`: interactive feature selection (DESIGN.md §24), non-interactive flags, generated project passes its own `fulmen check`
- [ ] M6.2 `fulmen dev`: native file watcher, debounced reload, debug mode, readable logs
- [ ] M6.3 `fulmen routes` (table, `--hooks`, `--cost`, `--json`)
- [ ] M6.4 `fulmen check` with rules FUL001–FUL012 and plugin rules
- [ ] M6.5 `fulmen fix` with diff preview and `--write`
- [ ] M6.6 `fulmen doctor`
- [ ] M6.7 `fulmen add <plugin>`

### Code generation and contracts

- [ ] M6.8 `fulmen gen openapi`
- [ ] M6.9 `fulmen gen client --lang ts`: fetch-based, no runtime dependency, typed errors
- [ ] M6.10 `fulmen gen client --lang python`: httpx-based, sync and async
- [ ] M6.11 `fulmen check --contract`: breaking-change detection against a committed OpenAPI file

### Migration

- [ ] M6.12 `fulmen migrate fastapi` codemod with a test corpus of real FastAPI apps

### Release engineering

- [ ] M6.13 Windows wheels via `cargo-xwin` in `scripts/release.py`
- [ ] M6.14 musllinux and Linux x86_64 wheels (emulated or cross) in the release pipeline, plus smoke tests of Linux wheels inside Docker (today only macOS wheels are smoke-tested)
- [ ] M6.15 Signed release tags and checksums file published with each release
- [ ] M6.16 Freeze the public API: review every exported name, remove what is not needed

### Documentation

- [ ] M6.17 User docs site: tutorial, guides per feature, API reference generated from docstrings, plugin and adapter author guides
- [ ] M6.18 "Coming from FastAPI / Flask" guides
- [ ] M6.19 Published benchmark report with reproduction scripts
- [ ] M6.20 Decide Q8 (domain)

### Exit criteria

- Every DESIGN.md §2 goal is implemented or explicitly moved to post-1.0 with a reason.
- All DESIGN.md §25.1 targets met or revised with measured reasons.
- Two release candidates with no API changes between them.
- Three example apps (REST API with auth + DB, real-time chat, job-processing service) built only from the docs.

---

## After 1.0 (not scheduled)

- ASGI adapter for ecosystem compatibility (Q7)
- Stabilize the native plugin ABI
- msgpack/CBOR content negotiation plugin
- GraphQL plugin
- Serverless adapters

---

## Definition of done (every task)

1. Implementation follows DESIGN.md, or DESIGN.md is updated in the same change.
2. Tests: unit tests in Rust and/or Python, plus an integration test for user-visible behaviour.
3. Public Python API is typed, documented, and listed in the stub if native.
4. Benchmarks updated for hot-path changes; `just bench-check` passes (from M1.26).
5. CHANGELOG.md `Unreleased` entry for user-visible changes, referencing the task ID.
6. `just ci` passes.
