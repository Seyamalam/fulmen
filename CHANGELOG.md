# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, minor versions may
break the API; every break is listed under **Breaking**.

`just release X.Y.Z` refuses to run unless a non-empty `## [X.Y.Z]` section
exists. Move the `Unreleased` entries under the new version heading first.

## [Unreleased]

### Added

- Lifecycle hooks (M3.1–M3.4 + M3.9, DESIGN.md §8): the eight hookable
  phases (`on_request`, `before_route`, `guard`, `resolve`,
  `before_handle`, `after_handle`, `on_response`, `after_response`) each
  run native closures inline (zero crossings — the M4 security-plugin
  slot) and Python callables through the executors (`def` → sync pool,
  `async def` → asyncio loop). `hook(request)` in decision phases,
  `hook(request, response)` post-handler; `None` continues, a `dict`
  merges into `request.state`, a returned value short-circuits to or
  replaces the response, in-place `response` mutation propagates, and
  hook exceptions route through `on_error`'s exception-MRO table.
  `after_response` is fire-and-forget after the response is written.
  Hooks register via `@app.<phase>` decorators or
  `app.add_hook(phase, fn, priority=…)`; per-phase ordering is
  `(priority, registration order)` with `PRIORITY_SECURITY` /
  `PRIORITY_SYSTEM` bands reserved below `PRIORITY_USER` so security
  plugins always precede user hooks.
- Route groups (§5.5): `Fulmen(prefix="/api")` + `app.mount(api)` —
  the mounted app's routes join under its prefix, and hooks/`on_error`
  handlers declared on it apply only to those routes (nested mounts
  compose). `request: Request` handler parameters inject the request
  object; `fulmen.request.Request`/`Headers` are public.
- `@app.on_startup` / `@app.on_shutdown` lifecycle callbacks (§5.7):
  startup runs after a successful compile before bind (failures abort
  startup); shutdown runs once when the server stops (failures log).
- `fulmen routes` (M3.4): lists `method path -> handler` from the
  compiled declarations; `--hooks` prints each route's resolved
  `(phase, name, kind)` hook chain plus scoped `on_error` entries,
  `--cost` prints the Python-crossing count (`1` + Python hooks — a
  native-only route stays at the M3 invariant of one crossing).
- Registration seals at compile (§14.8, M3.9): routes, hooks,
  `on_error` handlers, mounts and lifecycle callbacks registered after
  `run()`/`_compile_native()` raise `RuntimeError`.

## [0.2.0] - 2026-11-12

Milestone M2 — "types". Handler signatures are the contract: every
parameter source and every declared return runs through the same
`TypeDesc` descriptor compiled once at startup — validation, encoding
and OpenAPI all read one IR. Results in `benches/results/0.2.0.md`:
validated bodies are never Python-bound until they pass, and 422s never
enter Python at all (0 handler calls, ~96% of the pure-Rust bound).

### Added

- Schema decoding + source adapters (`fulmen-schema`, M2.6–M2.8 + M2.10,
  Rust half): `decode_json(&[u8])` — a purpose-built one-pass byte-slice
  JSON parser (Q3 → D18; strict RFC 8259, no `unsafe`, no new deps) that
  produces `Value<'a>` with borrowed strings and `BigInt` text, enforces
  `max_depth`/`max_items`/`max_str_bytes` *mid-walk* (the bound
  `Any`/`Raw` payloads get), and reports malformed input as `{code,
  position, message}` (HTTP 400) vs `DecodeError::Limit` as §11.7
  entries (422). `decode_and_validate(&[u8], &Validator)` composes both
  steps with a `PayloadError::{Malformed, Limit, Validation}` split;
  `Validator::validate_out` + `Checked::{Same, New}` let rewritten
  values escape the composed call with the input buffer's lifetime.
  Lax text sources: `decode_query`/`decode_form` (percent-decoded
  name=value pairs, repeated keys → `Array`, bare `?a` → `""`),
  `decode_cookie` (`a=1; b=2`, no percent-decoding), and
  `validate_text(&str, &Validator, Source)` for path/header/cookie
  scalars — UTF-8/percent-escape failures are structured 400s. Wire
  errors without serde: `ErrorEntry::loc_pointer()`/`problem()`,
  `ValidationErrors::to_json()`/`to_problem_entries()`, and
  `LiteralValue::to_json()` for `limit` fields — messages never echo
  input values.
- Schema engine (`fulmen-schema`, M2.1–M2.5, Rust half): the `TypeDesc`
  descriptor IR covering every type in DESIGN.md §11.1 plus the §11.2
  constraint set, a `compile()` step that rejects invalid descriptors at
  startup (bad regex, empty `Literal`/union, empty `Range`, non-positive
  `MultipleOf`, unbound `TypeVar`, wrong generic arity, descriptor-depth
  cap), and a specialized validator tree. `Validator::validate` runs
  strict (JSON, no coercion) or lax (text sources) against a borrowed
  `Value` model — scalars, RFC 3339/ISO 8601 time types, `UUID`, `Email`,
  `Url`, IPv4/IPv6, containers, `Literal`, enums, `Optional`, unions with
  a discriminator fast path, records with aliases/extra policies/defaults,
  generic records, `NewType`, `Any`/`Raw`. Constraints run at the right
  layer: `Len` on strings (chars) and containers, `Range`/`MultipleOf`
  with arbitrary-precision `Decimal` semantics, `Pattern` via `regex`
  (linear-time, no ReDoS), `Unique`, and `Strip`/`Lower`/`Upper`
  transforms applied before checks. All violations are collected as
  `ValidationErrors` — `{loc, code, message, limit?}` with
  JSON-pointer locations (`/body/address/city`, `/query/page`), no input
  echo, a `MAX_ERRORS` cap, and `max_depth`/`max_items` guardrails.
  Dependency-light: `regex` only; no `unsafe`, no I/O, no GIL.
- Typed parameter pipeline and error routing (M2.12–M2.16): handler
  signatures compile at startup into a native route spec
  (`SPEC-FORMAT.md`), and `fulmen-py` now extracts, validates and
  materializes every parameter source without the GIL — path, query
  (`decode_query`), header (case-insensitive `validate_text`), cookie
  (`decode_cookie`), form (`decode_form`) and strict JSON body
  (`decode_and_validate`). Missing required params collect into a 422
  `application/problem+json` `errors` array with `["query", "page"]`-style
  locations; omitted optional params let Python signature defaults apply;
  marker/`Field` defaults pass literally or per-request factories.
  Content-Type routing: `application/json` decodes the body,
  `application/x-www-form-urlencoded` takes the form path, other types
  415 when a body param exists; malformed JSON is 400, schema violations
  422. `Model` gets a native `__init__` via `Descriptor.construct`, a
  validated `__setattr__` (mutable by default, `frozen=True` opt-in —
  Q5 → D19), `model_validate`/`model_dump`. Response classes complete:
  `Created`, `Accepted`, `NoContent`, `Redirect`, `FileResponse` and
  buffered `Stream` (native chunked streaming is the documented M5
  deviation, §10.3/§18.3); union returns (`X | Y`) ride `response_descs`.
  `HTTPError` typed shortcuts (`NotFound[Problem]` et al.) plus
  `@app.on_error` — the closest registered class in the MRO wins and its
  handler resubmits through the matching executor with error routing
  disabled (`JobAnswer::Deferred`). Path-param `int` mismatches stay 404
  by construction, other scalars 422 (Q4 → D20). Pyright strict clean;
  `_fulmen.pyi` documents every new `#[pyfunction]`/`#[pyclass]`.
- Descriptor-driven response encoding + debug output validation
  (M2.9, DESIGN.md §11.6/§14.2): a declared return annotation now drives
  the response walk instead of generic dict/list conversion — records
  emit fields in descriptor order under their wire keys (alias/rename),
  `datetime`/`date`/`time` → RFC 3339/ISO text, `timedelta` → ISO 8601
  durations, `Decimal` → string, `UUID`/IPs → string, `Enum` → member
  value, `bytes` → base64 inside JSON, sets → arrays, and
  `Model`/`dataclass`/`TypedDict`/dict/tuple/union/generic results
  encode recursively by descriptor (no `__dict__`/`isinstance`
  guessing). `response_descs` selects the variant by the response's
  actual status — `Created[T]`, `Response(body, status=…)` and
  same-status unions all pick the right descriptor, with a
  single-declared or unique-200 fallback; unmatched contracts fall back
  to the generic path (debug warns). §10.2 channels survive: `-> str`
  stays `text/plain`, `-> bytes` stays `application/octet-stream`,
  `Any`/`Raw`/unannotated keep the generic walk. `Fulmen(debug=True)`
  or `serve(..., debug=True)` additionally runs every result through
  the variant's `Validator` — violations answer `500
  application/problem+json` with the `errors` entries and the handler
  identity in the log; non-debug keeps a single structural pass.
  `/json` measured 5% faster and the typed-param route 13% faster than
  the generic-walk baseline (`benches/e2e/results/latest.json`).
- OpenAPI 3.1 generation and docs UI (M2.11): `app.openapi()` walks the
  frozen route spec (`call_args`, `param_types`, `response_descs`,
  `Descriptor.spec`) and renders a deterministic OpenAPI 3.1 document —
  JSON Schema 2020-12 dialect throughout (type arrays for nullable,
  numeric `exclusiveMinimum`/`exclusiveMaximum`, `const`/`enum`,
  `prefixItems` for tuples). Parameters carry `in`, `required`, schema
  with constraints (`Len`/`Pattern`/`Range`/`MultipleOf`), defaults
  (including Python signature defaults), `Alias`, `Description`,
  `Example` and `Deprecated`; bodies map to `application/json`,
  `application/octet-stream` (`bytes`/`Raw`), form-urlencoded or
  multipart (grouped `File` params); responses key by status with
  `default` for dynamic `Response` returns and a bare-200 fallback.
  Records, generic specializations and enums dedupe into
  `components/schemas` behind `$ref`; unions become `anyOf`, and
  same-status record variants sharing a single-valued `Literal` tag
  field merge into `oneOf` + `discriminator`. `Fulmen(...)` gains
  keyword-only `title`/`version`/`description`/`contact`/`license`/
  `servers`/`tags` metadata plus `openapi_url="/openapi.json"` and
  `docs_url="/docs"` switches (`None` disables; docs imply OpenAPI, so
  `openapi_url=None` drops both). The document is rendered once during
  `_compile_native` and served verbatim by synthetic GET routes —
  declared routes at the same path win, disabled mode registers nothing
  and 404s. Docs UI is Swagger UI 5.34.1 vendored gzip-compressed under
  `fulmen/static` (`swagger-ui-dist`, Apache-2.0 — self-hosted per
  ROADMAP, under the 512 KiB guardrail), decompressed once at compile
  time. Route decorators accept `tags`, `summary`, `description`,
  `operation_id`, `deprecated` and `include_in_schema`; unknown options
  raise `TypeError`.
- Pydantic v2 adapter (`fulmen.adapters.pydantic`, M2.17, DESIGN.md
  §11.8): `BaseModel` subclasses are first-class schema citizens —
  valid anywhere `Model` is (parameters, returns, nested fields,
  `Body[T]`). The adapter *translates* `model_fields`/`FieldInfo` into
  native `record` specs at route registration, so validation,
  materialization, response encoding and OpenAPI run through the same
  `Descriptor` machinery as `Model`: `pydantic-core` is never invoked
  on the request path and invalid bodies still 422 in Rust without
  entering Python. Maps `alias`/`validation_alias`, `ge`/`gt`/`le`/`lt`,
  `min_length`/`max_length`, `pattern`, `multiple_of`,
  `strip_whitespace`/`to_lower`/`to_upper`, defaults and
  `default_factory`, `extra` config (`allow`→keep, `forbid`→forbid,
  `ignore`→ignore), `alias_generator`, `AfterValidator`/`field_validator`
  (mode="after") and `model_validator(mode="after")`. Materialization
  restores `__pydantic_fields_set__`/`__pydantic_extra__`/
  `__pydantic_private__` so instances behave like `model_construct`
  output — `model_dump()`/`repr`/`eq`/private attrs all work.
  `RootModel`, generic `Page[T]`, `Literal`-discriminated unions and
  nested models covered; unsupported constructs (`Before`/`Wrap`/`Plain`
  validators, `SkipValidation`, `AliasChoices`/`AliasPath`, `SecretStr`,
  `ByteSize`, `decimal_places`, `allow_inf_nan`, recursive models) raise
  `AnnotationError` at startup rather than silently diverge. Everything
  stays lazy — `SpecBuilder` detects models by duck-typing so
  `import fulmen` never imports pydantic, `fulmen.PydanticValidator`
  resolves on first access, and the extra is optional
  (`pip install fulmen[pydantic]`); without it the adapter raises an
  informative error. E2E: `validated`/`reject` POST scenarios in
  `benches/e2e` (pydantic app on port 8107 included) measure the
  adapter at native-descriptor speed — see `benches/results/0.2.0.md`.

### Fixed

- `fulmen-schema` lax singleton→container promotion returned the scalar
  instead of the one-element array (`Outcome::Same` on a non-array
  input), producing a 500 "internal error" on e.g. `?tag=a` for
  `Query[list[str]]` — now always emits `New(Array)`.
- `TypedDict` `Required`/`NotRequired` misreported under
  `from __future__ import annotations` (`__required_keys__` marks every
  key required under PEP 563) — `_typed_dict_spec` reads the qualifier
  off the resolved hint and falls back to `__total__`.

## [0.1.0] - 2026-11-11

Milestone M1 — "fast core". First release: a real server answering requests
from Python handlers, with benchmark numbers proving the speed thesis.
Details in `docs/ROADMAP.md` §M1 and `benches/results/0.1.0.md`.

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
