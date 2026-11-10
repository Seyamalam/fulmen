# Fulmen design

Status: draft 2. This is the source of truth for architecture decisions.
Any change that departs from it updates this document in the same commit.
Decisions that are settled are recorded in [§28](#28-decision-log); open
ones are in [§29](#29-open-questions).

Contents

1. [Purpose](#1-purpose)
2. [Goals and non-goals](#2-goals-and-non-goals)
3. [Principles](#3-principles)
4. [The core idea](#4-the-core-idea-declare-it-and-rust-runs-it)
5. [User API tour](#5-user-api-tour)
6. [Architecture](#6-architecture)
7. [Process and threading model](#7-process-and-threading-model)
8. [Request lifecycle](#8-request-lifecycle)
9. [Router](#9-router)
10. [Requests and responses](#10-requests-and-responses)
11. [Schema: validation and serialization](#11-schema-validation-and-serialization)
12. [Dependency injection](#12-dependency-injection)
13. [Plugins](#13-plugins)
14. [Runtime guardrails](#14-runtime-guardrails)
15. [Adapters](#15-adapters)
16. [Security features](#16-security-features)
17. [Errors](#17-errors)
18. [Real-time: WebSockets, SSE, streaming](#18-real-time-websockets-sse-streaming)
19. [Background jobs](#19-background-jobs)
20. [Configuration](#20-configuration)
21. [Observability](#21-observability)
22. [Testing apps](#22-testing-apps)
23. [Type safety](#23-type-safety)
24. [CLI](#24-cli)
25. [Performance](#25-performance)
26. [Packaging and platforms](#26-packaging-and-platforms)
27. [Versioning and stability](#27-versioning-and-stability)
28. [Decision log](#28-decision-log)
29. [Open questions](#29-open-questions)
30. [Risks](#30-risks)

---

## 1. Purpose

Fulmen is a Python backend framework with a Rust core. Users write ordinary,
fully typed Python. Fulmen runs the HTTP server, routing, parsing, validation,
serialization and security checks in Rust, so only the handler body runs as
Python code.

It is meant to be the obvious choice over FastAPI, Flask, Litestar and Robyn
for new Python APIs, for these reasons:

| Reason | How |
| --- | --- |
| Speed | Own Rust server, no ASGI layer. Declared behaviour runs natively. Rejected requests never enter Python. |
| Type safety | One handler signature drives validation, OpenAPI, typed clients and static checks. |
| Safe by default | Limits, timeouts, security headers, deny-by-default CORS, fail-fast startup checks. |
| Small and modular | Tiny core. Plugins add features, adapters swap infrastructure. Built-ins use the public API. |
| Tooling | `fulmen check`, `doctor` and `fix` find and repair problems before deploy. |

Inspiration: **Hono** (tiny core, middleware composition, typed RPC client,
multiple runtimes) and **Elysia** (lifecycle hooks, plugin composition with
types flowing from plugin to handler, end-to-end type safety).

## 2. Goals and non-goals

### Goals for v1

- HTTP/1.1 and HTTP/2 server in Rust, TLS through rustls.
- `def` and `async def` handlers, both first class.
- CPython 3.12, 3.13, 3.14, including free-threaded 3.13t and 3.14t.
- Native validation and serialization driven by type hints; Pydantic as an adapter.
- Plugins and adapters as public, documented extension points, including native (Rust) plugins.
- All security features in [§16](#16-security-features).
- WebSockets, SSE, streaming, background jobs, static files, response caching.
- OpenAPI 3.1 generation, typed TypeScript and Python client generation.
- CLI: `new`, `dev`, `run`, `routes`, `check`, `doctor`, `fix`, `add`, `gen`.
- Reproducible, published benchmarks.

### Non-goals for v1

- **ASGI or WSGI compatibility.** ASGI middleware will not run. An ASGI adapter
  may come later ([§29](#29-open-questions)).
- **An ORM.** Fulmen ships database adapters, not a query layer.
- **Templating beyond a thin adapter.** APIs first; HTML rendering through a Jinja2 adapter.
- **PyPy, GraalPy.** CPython only.
- **Serverless or edge runtimes.** Long-running servers first.
- **A package manager, bundler or runtime.** Use `uv`.
- **Python < 3.12.**

## 3. Principles

1. **Speed is the first tiebreaker** after correctness and security.
   Features that cost speed for every request must be opt-in.
2. **Declare, then compile.** Anything known at startup (routes, types,
   policies) is compiled into native data structures once.
3. **Pay for what you use.** A route without auth runs no auth code. An app
   without sessions loads no session code.
4. **Fail at startup, not at 3 a.m.** Configuration and type errors are found
   when the app boots, or earlier by `fulmen check`.
5. **Secure defaults, explicit opt-outs.** Weakening a default needs code
   that says so, and production mode warns about it.
6. **The handler signature is the contract.** No separate schema files,
   decorators restating types, or response-model arguments.
7. **Built-ins have no special access.** If a built-in plugin needs a hook,
   that hook is public.
8. **Plain Python on the outside.** No metaclass tricks that confuse type
   checkers; everything passes `pyright --strict`.

## 4. The core idea: declare it, and Rust runs it

Calling into Python is the expensive part of a request: entering the
interpreter, taking the GIL on GIL builds, and creating Python objects.
Frameworks that move only the socket layer to Rust leave most of that cost in
place.

Fulmen asks users to declare routes, types, limits, auth rules and policies.
At startup it compiles those declarations into native structures. A request
crosses into Python once, for the handler, with its arguments already parsed
and validated.

```
             ┌─────────────────────────── Rust, GIL released ───────────────────────────┐
socket ───▶  │ read+parse ▶ limits ▶ request id ▶ route ▶ CORS ▶ rate limit ▶ auth       │
             │ ▶ CSRF ▶ extract + validate (path, query, headers, cookies, body)         │
             └───────────────────────────────────────┬───────────────────────────────────┘
                                                     │ one GIL acquisition
                                       ┌─────────────▼─────────────┐
                                       │ Python: handler body      │
                                       │ (+ any Python hooks)      │
                                       └─────────────┬─────────────┘
             ┌───────────────────────────────────────▼───────────────────────────────────┐
socket ◀───  │ encode response ▶ security headers ▶ compress ▶ cache store ▶ access log  │
             └─────────────────────────── Rust, GIL released ───────────────────────────┘
```

Python hooks still exist for logic that must be Python. Each Python hook on
a request's path costs another trip into Python. `fulmen routes --cost` shows
how many Python crossings each route makes.

## 5. User API tour

This is the API we are building toward. Names may change before 1.0; the
shape should not.

### 5.1 App and routes

```python
from fulmen import Fulmen

app = Fulmen()

@app.get("/health")
def health() -> dict[str, str]:
    return {"status": "ok"}

@app.get("/users/{user_id}")
async def get_user(user_id: int) -> User:
    return await users.get(user_id)
```

Route decorators: `get`, `post`, `put`, `patch`, `delete`, `head`, `options`,
`route(path, methods=[...])`, `websocket`, `sse`.

### 5.2 Where parameters come from

| Declaration | Source |
| --- | --- |
| Name matches a `{param}` in the path | Path |
| `Model`, dataclass, `TypedDict`, `list[...]`, `dict[...]` | JSON body |
| Scalar with no marker (`int`, `str`, `bool`, `Enum`, ...) | Query string |
| `Query[T]`, `Header[T]`, `Cookie[T]`, `Body[T]`, `Form[T]`, `File` | Explicit |
| `Request`, `WebSocket`, `BackgroundTasks` | Injected by the framework |
| `Annotated[T, provider]` | Dependency injection ([§12](#12-dependency-injection)) |

```python
from typing import Annotated
from fulmen import Query, Header, Len, Range

@app.get("/search")
def search(
    q: Annotated[str, Len(1, 200)],
    page: Annotated[int, Range(ge=1)] = 1,
    tags: Query[list[str]] = [],
    user_agent: Header[str | None] = None,
) -> SearchResult: ...
```

Header names are derived from the parameter (`user_agent` → `User-Agent`)
unless given explicitly: `Header[str, "X-Api-Key"]`.

### 5.3 Models

```python
from fulmen import Model, Email, Len, Field

class NewUser(Model):
    name: Annotated[str, Len(1, 100)]
    email: Email
    age: Annotated[int, Range(ge=13)] | None = None
    tags: list[str] = Field(default_factory=list, max_items=20)

class User(NewUser):
    id: int
```

### 5.4 Responses

```python
from fulmen import Created, NoContent, Response, Redirect, Stream, FileResponse

@app.post("/users")
def create_user(body: NewUser) -> Created[User]: ...        # 201, body is User

@app.delete("/users/{user_id}")
def delete_user(user_id: int) -> NoContent: ...             # 204

@app.get("/old")
def old() -> Redirect: return Redirect("/new", status=308)

@app.get("/report")
def report() -> FileResponse: return FileResponse("report.pdf")
```

A handler can declare several outcomes with a union. Each member becomes a
documented response in OpenAPI:

```python
@app.get("/users/{user_id}")
def get_user(user_id: int) -> User | NotFound[Problem]: ...
```

### 5.5 Groups and sub-apps

```python
api = Fulmen(prefix="/api/v1")
api.use(jwt(...))                 # applies only inside this group

@api.get("/me")
def me(user: CurrentUser) -> User: ...

app.mount(api)                    # Hono-style composition
```

### 5.6 Plugins and typed injection

```python
from fulmen.plugins import cors, rate_limit, jwt

app.use(cors(origins=["https://example.com"], credentials=True))
app.use(rate_limit("100/minute", key="ip"))
auth = app.use(jwt(issuer="https://auth.example.com", audience="api", jwks_url="..."))

CurrentUser = Annotated[auth.Claims, auth]

@app.get("/me")
async def me(user: CurrentUser) -> User: ...      # protected: needs a valid token
```

`app.use()` returns a typed handle. Asking for it protects the route, and
the handler receives typed claims. This is Elysia's `derive`/`decorate`
expressed with standard typing.

### 5.7 Lifecycle and hooks

```python
@app.on_startup
async def connect() -> None: await db.connect()

@app.on_shutdown
async def disconnect() -> None: await db.close()

@app.before_handle
def audit(request: Request) -> None: ...         # Python hook: costs a crossing

@app.on_error(PaymentFailed)
def payment_failed(exc: PaymentFailed) -> Problem: ...
```

### 5.8 Running

```python
if __name__ == "__main__":
    app.run()                          # dev defaults
```

```sh
fulmen dev app:app                     # auto-reload
fulmen run app:app --workers 8         # production
```

## 6. Architecture

### 6.1 Crates

```
crates/
  fulmen-core/        App model, route table, hook pipeline, plugin/adapter registry,
                      error types, config. No I/O, no Python.
  fulmen-schema/      Type descriptor IR, validator compiler, JSON decoder/encoder,
                      form/multipart parsing, error collection.
  fulmen-security/    CORS, CSRF, rate limiter (GCRA), JWT/JWKS, sessions,
                      secure headers, trusted proxy handling.
  fulmen-server/      tokio runtime, hyper connection handling, TLS, workers,
                      graceful shutdown, static files, compression, WebSocket/SSE.
  fulmen-py/          The only crate using PyO3. Converts Python declarations to
                      core IR, bridges handler calls, owns the GIL policy.
  fulmen-plugin-sdk/  (M3) Safe Rust API + C ABI for third-party native plugins.
  fulmen-testkit/     (M3) Conformance tests for adapters and plugins.
```

### 6.2 Dependency graph

```
fulmen-py ──▶ fulmen-server ──▶ fulmen-core
    │              │  ▲               ▲
    │              ▼  │               │
    ├────────▶ fulmen-security ───────┤
    └────────▶ fulmen-schema ─────────┘
```

Rules, enforced by `scripts/guardrails.py` and review:

- `fulmen-core` depends on no other Fulmen crate.
- Only `fulmen-py` depends on `pyo3`.
- `fulmen-server` knows nothing about Python. It calls a `Dispatcher` trait
  that `fulmen-py` implements. This keeps the server benchmarkable with a
  pure-Rust dispatcher, which gives the upper bound on speed.

### 6.3 The Python package

```
python/fulmen/
  __init__.py         public API re-exports
  _fulmen.pyi         stubs for the native module
  app.py              Fulmen class, decorators (thin, builds declarations)
  params.py           Query, Header, Cookie, Body, Form, File markers
  model.py            Model base class (native constructor)
  responses.py        Response types
  errors.py           HTTPError and Problem types
  plugins/            built-in plugins (cors, rate_limit, jwt, sessions, csrf, ...)
  adapters/           built-in adapters (pydantic, redis, sqlalchemy, ...)
  testing.py          TestClient
  cli/                CLI commands
```

The Python layer only collects declarations. At `app.run()` (or `TestClient`
creation) it hands them to Rust, which compiles them into the frozen app.

### 6.4 Startup sequence

1. Import the user's module. Decorators record declarations; nothing is compiled yet.
2. `app.run()` resolves plugins in dependency order and calls each `setup()`.
3. Fulmen inspects every handler: `inspect.signature`, `typing.get_type_hints(include_extras=True)`.
4. Each parameter and return type is converted to the schema IR ([§11.3](#113-compilation)).
5. Dependency graphs are resolved and checked for cycles and scope errors.
6. Startup guardrails run ([§14.1](#141-startup-validation)). Any error stops the boot with all problems listed at once.
7. The route table, validators, encoders and middleware chains are frozen into an immutable `CompiledApp` shared by all threads.
8. `on_startup` hooks run. Then the server binds and starts accepting connections.

## 7. Process and threading model

### 7.1 Overview

```
process (one per worker on GIL builds; one total on free-threaded builds)
├── tokio runtime: N I/O threads (default = CPU cores)
│     accept, parse, route, native middleware, validation, encoding
├── sync pool: M OS threads (default = 4 × cores on GIL builds, 2 × cores free-threaded)
│     runs `def` handlers
└── asyncio loop thread(s): K (default 1 on GIL builds, = cores free-threaded)
      runs `async def` handlers and Python async hooks
```

### 7.2 Workers

- **GIL builds:** `fulmen run --workers N` forks N worker processes. On Linux
  each binds the port with `SO_REUSEPORT`, so the kernel balances connections.
  On macOS and Windows a supervisor accepts and passes sockets to workers.
- **Free-threaded builds:** default is one process. Python code runs in
  parallel across threads, so there is no need to copy the app N times.
- A supervisor process restarts crashed workers, with exponential backoff
  after repeated crashes ([§14.6](#146-process-supervision)).

### 7.3 Sync handlers

Rust picks a thread from the sync pool, attaches to the interpreter
(`Python::attach`), converts the validated arguments, calls the handler, and
converts the result before detaching. The tokio thread that owns the
connection never blocks; it awaits a oneshot channel.

### 7.4 Async handlers

Each loop thread runs a standard asyncio loop (uvloop when installed and
enabled). Rust submits a coroutine with `loop.call_soon_threadsafe` and
attaches a done-callback that completes a tokio oneshot. Staying on real
asyncio means httpx, asyncpg, redis-py, aiobotocore and the rest work
unchanged.

Rejected alternative: polling coroutines directly from Rust. It is faster in
microbenchmarks but breaks every library that expects an asyncio loop.

### 7.5 GIL batching

On GIL builds, when several requests are ready for Python at the same time,
the dispatcher hands them over in one GIL acquisition (up to a batch limit,
default 32, and a time budget, default 200 µs). This amortizes the cost of
taking the GIL under load. On free-threaded builds batching is off.

### 7.6 Backpressure

Each stage has a bounded queue. When the Python dispatch queue is full,
new requests get `503 Service Unavailable` with `Retry-After` instead of
piling up in memory ([§14.4](#144-load-shedding)).

## 8. Request lifecycle

| # | Phase | Runs in | Can short-circuit | Notes |
| --- | --- | --- | --- | --- |
| 1 | `accept` | Rust | close | Connection limits, TLS handshake |
| 2 | `parse` | Rust | 400, 413, 414, 431 | Size and count limits, timeouts |
| 3 | `on_request` | Rust / Python | any response | Request ID, trusted proxy resolution |
| 4 | `route` | Rust | 404, 405 | Radix tree, typed path params |
| 5 | `before_route` hooks | Rust / Python | any response | CORS preflight answered here |
| 6 | `guard` | Rust / Python | 401, 403, 429 | Rate limit, auth, CSRF, custom guards |
| 7 | `extract` | Rust | 400, 415, 422 | Parse and validate all inputs |
| 8 | `resolve` | Rust / Python | any response | Dependency injection |
| 9 | `before_handle` hooks | Rust / Python | any response | |
| 10 | `handle` | Python | | The handler |
| 11 | `after_handle` hooks | Rust / Python | replace response | |
| 12 | `encode` | Rust | 500 | Serialize return value |
| 13 | `on_response` hooks | Rust / Python | modify headers | Security headers, cache store |
| 14 | `compress` | Rust | | gzip, br, zstd negotiation |
| 15 | `write` | Rust | | |
| 16 | `after_response` | Rust / Python | | Background tasks, access log, metrics |
| — | `on_error` | Rust / Python | error response | Any exception from 3–13 |

Hooks in each phase run in plugin registration order, then app hooks.
Group-scoped hooks run only for routes in that group.

## 9. Router

### 9.1 Syntax

| Pattern | Matches |
| --- | --- |
| `/users` | exact |
| `/users/{user_id}` | one segment |
| `/files/{*path}` | rest of the path (catch-all, last segment only) |

Parameter types come from the handler signature, not from the pattern.
`{user_id}` with `user_id: int` only matches integer segments; `/users/abc`
gets 404 (configurable to 422).

### 9.2 Matching rules

- One radix tree (`matchit`) per method, built once and frozen.
- Static segments win over parameters, and parameters win over catch-all segments.
- `HEAD` is served automatically by `GET` handlers (body dropped).
- `OPTIONS` is answered automatically (with CORS when enabled).
- A path that exists with other methods returns 405 with an `Allow` header.
- Trailing slash policy: `strict` (default), `redirect` (308) or `ignore`.
- Percent-decoding happens once, after matching; encoded `/` is never treated as a separator.

### 9.3 Startup checks

- Duplicate routes and routes shadowed by an earlier pattern are errors.
- A `{param}` with no matching handler argument is an error, and so is the reverse.
- Path parameter types must be scalars (`int`, `str`, `UUID`, `Enum`, ...).

## 10. Requests and responses

### 10.1 Request object

Most handlers never need `Request`. When requested, it is a lazy view over the
native request: headers, cookies and query are decoded on first access, and
the raw body is exposed as `bytes` without copying until read.

```python
class Request(Protocol):
    method: str
    path: str
    url: URL
    headers: Headers            # case-insensitive, multi-value
    cookies: Mapping[str, str]
    query: QueryParams
    client: Address | None      # resolved through trusted proxies
    id: str                     # request ID
    state: State                # per-request typed storage for hooks
    async def body(self) -> bytes: ...
    def stream(self) -> AsyncIterator[bytes]: ...
```

### 10.2 Return value conversion

| Handler returns | Response |
| --- | --- |
| `Model`, dataclass, `TypedDict`, `dict`, `list`, `tuple` | `200 application/json` |
| `str` | `200 text/plain; charset=utf-8` |
| `bytes` | `200 application/octet-stream` |
| `None` with `-> None` or `-> NoContent` | `204` |
| `Response` subclass | as built |
| `Created[T]`, `Accepted[T]`, ... | that status, body encoded as `T` |

Content negotiation for other formats (msgpack, CBOR) is a plugin.

### 10.3 Response types

`Response`, `JSONResponse`, `TextResponse`, `HTMLResponse`, `Redirect`,
`FileResponse` (sendfile where available, `Range` and `ETag` support),
`Stream` (sync or async iterator), `SSE`. All are implemented natively; the
Python classes are thin wrappers.

## 11. Schema: validation and serialization

### 11.1 Supported types

| Category | Types |
| --- | --- |
| Scalars | `bool`, `int`, `float`, `str`, `bytes`, `Decimal`, `None` |
| Time | `datetime` (RFC 3339), `date`, `time`, `timedelta` (ISO 8601 duration) |
| Identifiers | `UUID`, `Email`, `Url`, `IPv4Address`, `IPv6Address` |
| Containers | `list[T]`, `tuple[T, ...]`, `tuple[A, B]`, `set[T]`, `frozenset[T]`, `dict[K, V]` |
| Choices | `Literal[...]`, `Enum`, `IntEnum`, `StrEnum` |
| Unions | `T \| None`, `A \| B` (tagged with `Discriminator("kind")` for speed) |
| Records | `fulmen.Model`, `@dataclass`, `TypedDict` (incl. `NotRequired`) |
| Generic | `Model` generics (`Page[T]`), `NewType` |
| Escape hatch | `Any` (no validation), `Raw` (keep bytes) |

### 11.2 Constraints

Through `Annotated`: `Len(min, max)`, `Range(ge, gt, le, lt)`, `Pattern(regex)`
(Rust `regex`, no backtracking, so no ReDoS), `MultipleOf`, `Unique`,
`Strip`, `Lower`, `Upper`, `Default`, `Alias`, `Description`, `Example`,
`Deprecated`. Custom validators are Python functions and cost a crossing:
`AfterValidator(fn)`.

### 11.3 Compilation

Python types are converted once, at startup, into a type descriptor IR:

```rust
enum TypeDesc {
    Bool, Int { range: Option<IntRange> }, Float { .. }, Str { len: .., pattern: .. },
    List { item: Box<TypeDesc>, len: .. }, Dict { key: .., value: .. },
    Record { class: ClassRef, fields: Vec<Field>, extra: ExtraPolicy },
    Union { variants: Vec<TypeDesc>, discriminator: Option<String> },
    Literal(Vec<Value>), Optional(Box<TypeDesc>), ...
}
```

The IR is compiled into a validator tree with each node specialised (for
example, a `list[int]` node with a fast path for homogeneous arrays). The IR
also feeds OpenAPI generation and client generation, so there is a single
source of truth.

### 11.4 Decoding

- The JSON body is parsed and validated in one pass, building the final
  Python objects directly (no intermediate `dict`, no second validation pass).
  Parsing and validation of the bytes happen with the GIL released; only
  object construction needs the interpreter.
- Strings that repeat (dict keys, enum values) are interned per field.
- **Strict by default for JSON:** `"1"` is not an `int`. **Lax for query,
  path, headers and forms**, because those are always strings.
- Unknown fields: `ignore` (default), `forbid`, or `keep` per model.
- Limits: max depth 64, max items per container 100 000, max string length
  = body limit. Exceeding them is a 422, never a crash.

### 11.5 Model

`fulmen.Model` is a slotted class whose `__init__` is native. It validates on
construction from user code too, so a `User(...)` built inside a handler is
as trustworthy as one from a request. Options: `frozen`, `extra`,
`rename="camel"`, `strict`. Models are plain classes to type checkers,
via `dataclass_transform` (PEP 681).

### 11.6 Encoding

The encoder walks Python objects using the compiled descriptor of the return
type, so it knows field order and types in advance and does no
`isinstance` probing. Output is written straight into the response buffer.
`datetime` → RFC 3339, `Decimal` → string (configurable), `UUID` → string,
`bytes` → base64, `Enum` → value. Unknown types are a startup error, not a
runtime surprise.

In debug mode the encoder also checks that the value matches the declared
return type and reports mismatches.

### 11.7 Validation errors

All errors in a request are collected in one pass and returned as RFC 9457
problem details:

```json
{
  "type": "https://fulmen.dev/problems/validation",
  "title": "Request validation failed",
  "status": 422,
  "errors": [
    { "loc": "/body/email", "code": "email", "message": "not a valid email address" },
    { "loc": "/query/page", "code": "ge", "message": "must be >= 1", "limit": 1 }
  ]
}
```

Input values are not echoed back by default (they may contain secrets).

### 11.8 Pydantic adapter

`Fulmen(validator=PydanticValidator())` or per route. Pydantic models are then
accepted wherever `Model` is, validated by `pydantic-core`, and documented in
OpenAPI from their JSON schema. It costs extra Python crossings compared to
the native path; the docs state the cost.

## 12. Dependency injection

### 12.1 Providers

```python
from fulmen import provider, Scope

@provider(scope=Scope.APP)
async def database(settings: Settings) -> AsyncIterator[Database]:
    db = await Database.connect(settings.database_url)
    yield db
    await db.close()

@provider(scope=Scope.REQUEST)
async def repo(db: Annotated[Database, database]) -> UserRepo:
    return UserRepo(db)

@app.get("/users/{user_id}")
async def get_user(user_id: int, users: Annotated[UserRepo, repo]) -> User: ...
```

- Scopes: `APP` (once, at startup), `REQUEST` (once per request, cached
  within it), `TRANSIENT` (every injection).
- Generator providers run cleanup after the response is sent, in reverse order.
- Native providers (plugins written in Rust, such as `jwt`) are resolved
  without entering Python. Their output is converted only if the handler asks for it.

### 12.2 Compile-time checks

The dependency graph is built at startup. Cycles, missing providers, a
`REQUEST` provider injected into an `APP` provider, and async providers
used from sync-only contexts are startup errors.

### 12.3 Overrides

`app.override(database, fake_database)` replaces a provider, for tests.

## 13. Plugins

### 13.1 What a plugin is

```python
class Plugin(Protocol):
    name: str                          # unique, e.g. "fulmen.jwt"
    version: str
    requires: Sequence[str]            # plugin names that must be registered first
    conflicts: Sequence[str]           # plugins that cannot be used together
    def setup(self, ctx: PluginContext) -> object: ...   # return value = typed handle
```

`PluginContext` lets a plugin:

- add hooks for any phase in [§8](#8-request-lifecycle),
- add routes and groups,
- register providers (typed injection),
- register native middleware (from a native plugin),
- add OpenAPI components (security schemes, shared schemas),
- add CLI commands (`fulmen <plugin> ...`),
- add `fulmen check` rules,
- read typed configuration ([§20](#20-configuration)).

### 13.2 Ordering

1. Plugins are topologically sorted by `requires`. Ties keep registration order.
2. Within a phase, hooks run in that order, then app-level hooks.
3. Security plugins declare a fixed phase priority so that, for example, rate
   limiting always runs before authentication, which runs before CSRF,
   regardless of registration order. `fulmen routes --hooks` prints the
   effective chain for each route.

### 13.3 Scoping

`group.use(plugin)` applies the plugin to that group and its children only.
A plugin may declare `scope="app"` to forbid group use (for example, the
metrics plugin).

### 13.4 Native plugins

A third-party wheel can ship Rust code that runs in the native pipeline
without the GIL. Third-party plugins are compiled separately from Fulmen, so
the boundary is a C ABI:

```rust
#[repr(C)]
pub struct FulmenPluginV1 {
    pub abi_version: u32,                       // must equal FULMEN_PLUGIN_ABI (1)
    pub name: *const c_char,
    pub create: extern "C" fn(config_json: *const u8, len: usize) -> *mut c_void,
    pub on_request: Option<extern "C" fn(state: *mut c_void, req: *const RawRequest, out: *mut Decision) -> i32>,
    pub on_response: Option<extern "C" fn(state: *mut c_void, res: *mut RawResponse) -> i32>,
    pub destroy: extern "C" fn(state: *mut c_void),
}
```

- The table is passed to Fulmen inside a `PyCapsule` named `fulmen.plugin.v1`.
- `fulmen-plugin-sdk` wraps this in a safe trait and a `export_plugin!`
  macro, so plugin authors write no `unsafe`. The SDK is the one crate besides
  `fulmen-py` that needs `unsafe` (listed in `UNSAFE_ALLOWED` when created).
- Fulmen checks `abi_version` at load time and refuses mismatches with a clear message.
- A plugin that panics is caught at the boundary (`catch_unwind` in the SDK);
  the request gets a 500 and the plugin is reported in logs.
- The ABI is marked unstable until Fulmen 1.0.

### 13.5 Built-in plugins

`cors`, `csrf`, `rate_limit`, `jwt`, `sessions`, `secure_headers`,
`request_id`, `compression`, `static_files`, `cache`, `openapi`, `metrics`,
`otel`, `health`, `background`, `websocket` helpers. All are built with the
same public API as third-party plugins.

## 14. Runtime guardrails

Guardrails protect the people running Fulmen apps from common mistakes and
from abusive clients. Every guardrail is on by default, configurable, and
logged when disabled. Production mode (`FULMEN_ENV=production`, or
`fulmen run`) turns some warnings into errors.

### 14.1 Startup validation

The app refuses to start, listing every problem at once, when:

- a handler parameter type cannot be validated or a return type cannot be encoded,
- routes conflict or shadow each other,
- the dependency graph has a cycle, missing provider or scope violation,
- a plugin is missing a required plugin, or two conflicting plugins are registered,
- a plugin needs configuration that is missing (for example `jwt` without a key),
- a native plugin's ABI version does not match.

### 14.2 Production mode refusals

In production mode, startup fails if:

- debug mode is on,
- CORS allows `*` together with credentials,
- `jwt` allows the `none` algorithm or uses an HMAC secret shorter than 32 bytes,
- the session secret is the development default, shorter than 32 bytes, or missing,
- cookie sessions are enabled without CSRF protection and without `SameSite=Strict`,
- the app binds to a public interface without TLS and without a trusted proxy configured
  (overridable with `allow_plain_http=True` for setups that terminate TLS elsewhere),
- a limit from [§14.3](#143-request-limits) is set to "unlimited".

### 14.3 Request limits

| Limit | Default | Response |
| --- | --- | --- |
| Request body | 1 MiB (per route override) | 413 |
| Multipart total / per file | 10 MiB / 5 MiB | 413 |
| Header block | 16 KiB | 431 |
| Header count | 100 | 431 |
| URL length | 8 KiB | 414 |
| Query parameters | 256 | 400 |
| JSON depth | 64 | 422 |
| JSON items per container | 100 000 | 422 |
| WebSocket message | 1 MiB | close 1009 |
| Concurrent connections per worker | 10 000 | refuse |
| HTTP/2 streams per connection | 100 | `REFUSED_STREAM` |

### 14.4 Timeouts and load shedding

| Timeout | Default |
| --- | --- |
| TLS handshake | 10 s |
| Read request headers | 10 s |
| Read request body | 30 s, plus minimum rate 1 KiB/s |
| Handler | 30 s (per route override), returns 504 |
| Keep-alive idle | 60 s |
| Graceful shutdown | 30 s, then remaining connections are closed |

The handler timeout cancels async handlers (`CancelledError`). Sync handlers
cannot be interrupted safely in Python; Fulmen returns 504 to the client,
logs the stuck handler with its stack, and counts the thread as busy until it
returns.

**Load shedding:** when the Python dispatch queue exceeds its limit (default
1 024 per worker) or a request has waited longer than its queue deadline
(default 5 s), Fulmen responds `503` with `Retry-After` immediately.
Health endpoints bypass the queue.

### 14.5 Event loop protection

- **Blocking detector:** in debug mode, a watchdog measures asyncio loop lag.
  If a callback blocks the loop for more than 100 ms, Fulmen logs the handler
  and the stack captured at the time of the block.
- `fulmen check` statically flags known blocking calls (`time.sleep`,
  `requests.*`, synchronous file I/O, `subprocess.run`) inside `async def`
  handlers and suggests `fulmen fix`.
- A sync handler called from an async-only context is moved to the sync pool
  automatically, never run on the loop.

### 14.6 Process supervision

- Panics in Rust are caught at the dispatch boundary and turned into 500 with
  the request ID; the worker keeps running. The release profile keeps
  `panic = "unwind"` for this reason. Panics are still bugs: lints forbid
  `unwrap`/`panic!` on the request path ([GUARDRAILS.md](GUARDRAILS.md)).
- Unhandled Python exceptions are 500s with the traceback logged, never sent
  to the client outside debug mode.
- The supervisor restarts a worker that dies, and stops restarting after 5
  crashes within 60 s, so a crash loop does not hide the problem.
- Optional memory ceiling per worker: when exceeded, the worker stops
  accepting, drains, and is replaced.

### 14.7 Data hygiene

- Logs redact `Authorization`, `Cookie`, `Set-Cookie`, `Proxy-Authorization`
  and any header or field named in `redact=[...]`.
- `Secret[str]` type for settings: prints as `**********`, excluded from
  `repr`, logs and error pages.
- Validation errors do not echo input values unless `echo_input=True`.
- Error responses never include stack traces, file paths or library versions
  in production mode. The `Server` header is off by default.

### 14.8 Plugin isolation

- A plugin can only touch the phases it declared in `setup()`. Registering a
  hook after startup is an error.
- Python plugin hooks that raise are treated as handler errors (500), with
  the plugin name in the log.
- Plugins cannot change limits set by the app; they can only make them stricter.

## 15. Adapters

### 15.1 Interfaces

Each adapter interface is a Rust trait in core and a Python `Protocol`.
Adapters can be written in either language.

```python
class SessionStore(Protocol):
    async def load(self, session_id: str) -> bytes | None: ...
    async def save(self, session_id: str, data: bytes, ttl: timedelta) -> None: ...
    async def delete(self, session_id: str) -> None: ...

class RateLimitStore(Protocol):
    async def acquire(self, key: str, cost: int, rate: Rate) -> Decision: ...

class CacheStore(Protocol):
    async def get(self, key: str) -> bytes | None: ...
    async def set(self, key: str, value: bytes, ttl: timedelta | None) -> None: ...
    async def delete(self, key: str) -> None: ...

class JobQueue(Protocol):
    async def enqueue(self, job: Job) -> JobId: ...
    async def dequeue(self, queues: Sequence[str], timeout: timedelta) -> Job | None: ...
    async def ack(self, job_id: JobId) -> None: ...
    async def nack(self, job_id: JobId, retry_at: datetime | None) -> None: ...

class Validator(Protocol):
    def supports(self, tp: type) -> bool: ...
    def compile(self, tp: type) -> CompiledValidator: ...
    def json_schema(self, tp: type) -> dict[str, object]: ...
```

Database adapters expose a pool as an `APP`-scoped provider and a
connection or session as a `REQUEST`-scoped provider, with transaction
handling tied to the response status.

### 15.2 Built-in adapters

| Interface | Built-ins | Native? |
| --- | --- | --- |
| `Validator` | native (default), pydantic | native / Python |
| `SessionStore` | signed cookie (default), memory, redis | native |
| `RateLimitStore` | memory (default), redis | native |
| `CacheStore` | memory (default), redis | native |
| `JobQueue` | in-process (default), redis | native |
| Database | SQLAlchemy async, asyncpg | Python |
| Templates | Jinja2 | Python |

Redis adapters are native (Rust `redis` crate) so rate limiting and sessions
stay off the GIL.

### 15.3 Conformance tests

`fulmen-testkit` ships a test suite per interface. A third-party adapter
passes `pytest --fulmen-adapter=my_pkg:MyStore` to prove it behaves like the
built-ins (TTL expiry, concurrency, error handling).

## 16. Security features

### 16.1 CORS

Native. Exact origins, suffix patterns (`*.example.com`) or a predicate.
Preflight answered in the `before_route` phase without entering Python.
`Vary: Origin` always set. `*` with credentials is rejected at startup.

### 16.2 CSRF

Native double-submit token (signed, bound to the session) for unsafe methods
when cookie sessions are on. Also checks `Origin`/`Sec-Fetch-Site` and rejects
cross-site requests. Exempt routes are declared explicitly.

### 16.3 Rate limiting

GCRA (generic cell rate algorithm): one timestamp per key, exact, no bursts
beyond the configured burst size. Keys: IP (after trusted proxy resolution),
user (from auth), header, route, or a custom function. Multiple limits per
route (`"10/second;1000/hour"`). Responses carry `RateLimit` and `Retry-After`
headers (IETF RateLimit header fields draft). Stores: memory, redis.

### 16.4 JWT

- Verification is native. Algorithms must be listed explicitly (`RS256`,
  `ES256`, `EdDSA`, `HS256`, ...). `none` is always rejected, and an HMAC key
  is never accepted for an asymmetric algorithm (prevents algorithm confusion).
- `exp`, `nbf`, `iat` checked with configurable leeway (default 30 s).
  `iss` and `aud` checked when configured (required in production mode).
- JWKS: fetched at startup and refreshed in the background, cached by `kid`,
  with rate-limited refetch on unknown `kid`.
- Claims are decoded into a typed `Model` declared by the user.
- Token signing helpers for apps that issue their own tokens.

### 16.5 Sessions

- Default store: signed cookie (HMAC-SHA256), optional encryption (AES-256-GCM).
- Server-side stores (memory, redis) keep only a random 256-bit ID in the cookie.
- Key rotation: a list of keys, the first signs, all verify.
- Cookie defaults: `HttpOnly`, `Secure` (except on localhost in dev),
  `SameSite=Lax`, `Path=/`, `__Host-` prefix when possible.
- Session ID regenerated on login (`session.rotate()`) to prevent fixation.

### 16.6 Secure headers

Defaults: `X-Content-Type-Options: nosniff`, `Referrer-Policy:
strict-origin-when-cross-origin`, `X-Frame-Options: DENY`,
`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy:
same-origin`, `Strict-Transport-Security: max-age=63072000; includeSubDomains`
when served over TLS. CSP is opt-in with a typed builder (APIs rarely need it).

### 16.7 Trusted proxies

`X-Forwarded-For`, `X-Forwarded-Proto` and `Forwarded` are ignored unless the
direct peer is in `trusted_proxies` (CIDR list). Client IP for rate limits and
logs comes from this resolution, never from raw headers.

### 16.8 Request IDs

Accepts an incoming `X-Request-ID` only from trusted proxies, otherwise
generates a UUIDv7. Exposed as `request.id`, returned in the response
header, attached to every log line.

### 16.9 Supply chain

Dependency policy in [GUARDRAILS.md](GUARDRAILS.md): permissive licenses,
crates.io only, advisories checked on every CI run. Release artifacts are
built locally from a clean, tagged `main`.

## 17. Errors

- `HTTPError(status, detail=None, headers=None)` raised anywhere returns that status.
- Typed shortcuts: `NotFound`, `Conflict`, `Unauthorized`, `Forbidden`, ...
- Every error response is RFC 9457 `application/problem+json`. Apps can
  extend the problem type with their own `Model`.
- `@app.on_error(ExcType)` maps exceptions to responses. Handlers are chosen
  by the closest class in the MRO.
- Native errors (limits, validation, auth) produce the same problem format,
  so clients handle one shape.
- Debug mode adds the traceback and a link to the source line.

## 18. Real-time: WebSockets, SSE, streaming

### 18.1 WebSockets

```python
@app.websocket("/chat/{room}")
async def chat(ws: WebSocket[ChatIn, ChatOut], room: str, user: CurrentUser) -> None:
    async for message in ws:            # ChatIn, validated natively
        await ws.send(ChatOut(...))     # encoded natively
```

- Handshake goes through the normal pipeline (auth, rate limit, CORS-like origin check).
- Frames are read and written by Rust; text/JSON messages are validated
  against the declared type before the handler sees them.
- Ping/pong and idle timeouts are native. Per-message deflate is opt-in.
- `Broadcast` helper for rooms, with memory and redis backends (adapter).

### 18.2 Server-sent events

```python
@app.sse("/events")
async def events() -> AsyncIterator[Event[Update]]: ...
```

Keep-alive comments, `Last-Event-ID` resume support, retry hints.

### 18.3 Streaming

`Stream` responses accept sync or async iterators of `bytes`/`str`/models
(NDJSON). Request bodies can be streamed with `request.stream()`; the body
limit still applies.

## 19. Background jobs

### 19.1 Fire-and-forget tasks

`BackgroundTasks` runs callables after the response is sent, in the same
worker. Failures are logged. For small jobs only.

### 19.2 Job queue

```python
jobs = app.use(background(queue=RedisQueue(url=...)))

@jobs.task(retries=5, backoff="exponential", timeout=60)
async def send_email(to: Email, template: str) -> None: ...

await send_email.enqueue(to="a@b.com", template="welcome")
```

- Arguments are validated and serialized with the schema engine.
- Retries with exponential backoff and jitter, dead-letter queue, timeouts,
  uniqueness keys, scheduled (`enqueue_at`) and periodic (`cron="..."`) jobs.
- `fulmen worker app:app` runs job workers separately from the HTTP server.
- Delivery is at-least-once; docs show how to make jobs idempotent.

## 20. Configuration

```python
from fulmen import Settings, Secret

class AppSettings(Settings, env_prefix="APP_"):
    database_url: Secret[str]
    debug: bool = False
    allowed_origins: list[str] = []

app = Fulmen(settings=AppSettings)
```

- Sources, lowest to highest priority: defaults, `fulmen.toml`, `.env`
  (development only; ignored in production mode with a warning), environment
  variables, CLI flags.
- Validated at startup with the schema engine. Missing required settings
  stop the boot with every missing name listed.
- Settings are injectable: `settings: AppSettings` in any handler or provider.
- Plugins read their own section (`[tool.fulmen.jwt]` or `FULMEN_JWT_*`).

## 21. Observability

- **Logging:** native `tracing` in Rust, bridged to Python `logging`. JSON
  lines in production, readable colour output in dev. Access log is native.
- **Metrics plugin:** Prometheus endpoint with request counts, latency
  histograms per route, queue depth, GIL wait time, Python crossings per request,
  worker restarts. Counters are native atomics.
- **OpenTelemetry plugin:** traces with W3C `traceparent` propagation, spans
  for each lifecycle phase (sampled), export over OTLP.
- **Health plugin:** `/healthz` (liveness, answered natively) and `/readyz`
  (runs registered checks).

## 22. Testing apps

```python
from fulmen.testing import TestClient

def test_create_user():
    with TestClient(app) as client:          # runs startup/shutdown hooks
        r = client.post("/users", json={"name": "Ada", "email": "ada@example.com"})
        assert r.status == 201
        assert r.json()["id"] > 0
```

- In-process: requests go straight into the native pipeline without sockets,
  so tests are fast and exercise exactly the production code path.
- Sync and async clients, WebSocket client, provider overrides, frozen clock.
- `TestClient(app, raise_server_errors=True)` re-raises handler exceptions
  for clear test failures.

## 23. Type safety

### 23.1 In the user's code

- The public API is fully typed and passes `pyright --strict` and `mypy --strict`.
- Decorators preserve handler signatures (`ParamSpec`), so handlers stay
  callable and typed in unit tests.
- `Model` uses `dataclass_transform`, so constructors are type-checked.
- Typed handles from plugins (`Annotated[auth.Claims, auth]`) give typed
  values in handlers without casts.
- `python/tests/typing/` holds type tests (`assert_type`, `reveal_type`
  snapshots) checked by pyright in CI, so typing regressions are caught.

### 23.2 Across the wire

- `fulmen gen openapi` writes OpenAPI 3.1 from the schema IR. Every route,
  parameter, response union member, error and security scheme is included.
- `fulmen gen client --lang ts` writes a typed TypeScript client (fetch-based,
  no runtime dependency): `client.users.get({ user_id: 1 })` returns `User`.
- `fulmen gen client --lang python` writes a typed Python client (httpx).
- `fulmen check --contract openapi.json` fails if the API changed in a
  breaking way compared to a committed OpenAPI file.

### 23.3 Runtime checks

Debug mode validates return values against declared types, so a handler
that returns the wrong shape fails in development instead of in a client.

## 24. CLI

The CLI is a Python entry point (`fulmen`); heavy work (file watching,
serving, code scanning) runs in Rust.

| Command | What it does |
| --- | --- |
| `fulmen new [name]` | Interactive project setup. Asks: sync/async default, which plugins (auth, sessions, rate limit, jobs, DB, WebSockets, metrics), database adapter, validator (native/pydantic). Writes a `uv` project with tests and a `justfile`. Non-interactive with flags. |
| `fulmen dev app:app` | Dev server with auto-reload (native watcher, debounced), debug mode, readable logs, blocking detector on. |
| `fulmen run app:app` | Production server: workers, production mode guardrails, JSON logs. |
| `fulmen worker app:app` | Background job worker. |
| `fulmen routes` | Route table: method, path, handler, plugins, Python crossings (`--cost`), hook chain (`--hooks`). |
| `fulmen check` | Static and startup checks (below). Exit code 1 on errors. |
| `fulmen doctor` | Environment report: Python build and GIL state, Fulmen and plugin versions, ABI compatibility, uvloop presence, settings sources, ports, TLS files. Suggests fixes. |
| `fulmen fix` | Applies automatic fixes for `check` findings. Shows a diff; `--write` to apply. |
| `fulmen add <plugin>` | Installs a plugin with `uv add` and inserts the `app.use(...)` call. |
| `fulmen gen openapi` / `gen client` | See [§23.2](#232-across-the-wire). |
| `fulmen migrate fastapi` | Codemod from FastAPI: decorators, `Depends`, `Query`/`Path`/`Body`, `HTTPException`, pydantic models (kept via adapter or converted to `Model`). |
| `fulmen info` | Version and build information. |

### 24.1 `fulmen check` rules

| Code | Check | Fixable |
| --- | --- | --- |
| FUL001 | Unsupported parameter or return type | – |
| FUL002 | Route conflict or shadowed route | – |
| FUL003 | Path parameter without matching argument (or reverse) | yes |
| FUL004 | Blocking call inside `async def` | yes (`asyncio.to_thread` or async library) |
| FUL005 | Missing configuration for a registered plugin | – |
| FUL006 | Dependency cycle or scope violation | – |
| FUL007 | Insecure setting that production mode would refuse | some |
| FUL008 | Handler missing return annotation | yes (inferred where possible) |
| FUL009 | Mutable default argument on a handler | yes |
| FUL010 | Breaking change against committed OpenAPI contract | – |
| FUL011 | Plugin ABI mismatch | – |
| FUL012 | Secret-looking literal in settings or code | – |

Plugins can register their own rules (`FUL-<plugin>-NNN`).

## 25. Performance

### 25.1 Targets

These are goals to verify in M1 and track afterwards, measured on the same
machine with the same worker count against FastAPI + uvicorn (uvloop,
httptools):

- Plain text and JSON hello world: at least 5× the requests per second.
- Validated JSON body (10 fields): at least 5×.
- Rejected request (401 or 422): at least 20×, because Python is never entered.
- p99 latency under load no worse than 50% of FastAPI's.
- Idle memory per worker under 30 MiB above the interpreter baseline.
- Startup for a 200-route app under 300 ms.

If M1 shows a target is unreachable, the target is revised here with the
measured reason.

### 25.2 Techniques

- Zero-copy header and body handling with `bytes::Bytes`; one allocation for
  the response buffer, sized from a per-route running estimate.
- `mimalloc` as the Rust global allocator in the extension module.
- Cached `Date` header (updated once per second), pre-encoded static headers.
- Route table, validators and encoders frozen at startup; no locks on the
  request path; per-thread scratch buffers.
- Interned Python strings for field names and common values.
- GIL released for all parsing, validation and encoding of bytes.
- GIL batching on GIL builds ([§7.5](#75-gil-batching)).
- Static files with `sendfile` and precompressed variants.

### 25.3 Benchmarks

- `benches/micro/`: criterion benchmarks for router, decoder, validator,
  encoder, rate limiter, JWT verification.
- `benches/e2e/`: scenarios driven by `oha`, each run against Fulmen,
  FastAPI, Litestar, Robyn and Flask + gunicorn.
- Scenarios: plain text, JSON, path + query, validated body, auth route,
  rejected request, 50 KiB JSON response, DB-bound route (Postgres), WebSocket echo.
- `benches/baseline.json` stores results; `just bench-check` fails on a
  regression above 5% ([GUARDRAILS.md](GUARDRAILS.md)).
- Published results include hardware, OS, versions and the exact commands.

## 26. Packaging and platforms

- Wheels per CPython version (no `abi3`, which cannot target free-threaded
  builds): `cp312`, `cp313`, `cp313t`, `cp314`, `cp314t`.
- Platforms: Linux x86_64 and aarch64 (manylinux_2_28 and musllinux_1_2),
  macOS arm64 and x86_64, Windows x86_64. Built locally by `just release`
  ([GUARDRAILS.md](GUARDRAILS.md)); Windows via `cargo-xwin` cross-compilation (M6).
- The sdist builds anywhere with a Rust toolchain.
- Optional extras: `fulmen[pydantic]`, `fulmen[redis]`, `fulmen[sqlalchemy]`,
  `fulmen[uvloop]`, `fulmen[otel]`, `fulmen[jinja]`.
- The native module declares free-threading support, so importing it on a
  free-threaded build does not re-enable the GIL. A smoke test in the release
  pipeline checks this.

## 27. Versioning and stability

- Semantic versioning. Before 1.0, minor versions may break the API; every
  break is listed in CHANGELOG.md under **Breaking**.
- From 1.0: deprecations warn for at least one minor version before removal.
- The native plugin ABI has its own version number (`FULMEN_PLUGIN_ABI`),
  independent of the package version, unstable until 1.0.
- Supported Python versions: new CPython releases supported within one month;
  a version is dropped when CPython ends its support.

## 28. Decision log

| # | Decision | Chosen | Alternatives considered | Why |
| --- | --- | --- | --- | --- |
| D1 | Server interface | Own Rust server | ASGI app on Granian/uvicorn | ASGI forces Python objects per event and per message; speed is the main goal. |
| D2 | HTTP library | hyper 1.x + hyper-util | actix-web, axum | Lowest-level stable option; we build our own routing and pipeline anyway. |
| D3 | Router | `matchit` | own radix tree, `path-tree` | Fast, well-tested, conflict detection built in. |
| D4 | Async handlers | Real asyncio loop + thread-safe bridge | Rust-driven coroutine polling | Compatibility with the asyncio ecosystem. |
| D5 | Validation default | Native schema engine | pydantic-core only | Single-pass decode + validate without the GIL, and one IR for OpenAPI and clients. Pydantic kept as adapter. |
| D6 | Python floor | 3.12 | 3.9+ | PEP 695/698/692 typing features, faster interpreter, fewer code paths. |
| D7 | Free threading | Supported from day one | later | Shapes the dispatcher design; costly to retrofit. |
| D8 | Subinterpreters | Not used | per-worker subinterpreters (PEP 684/734) | PyO3 does not support subinterpreters; free threading covers the same need. |
| D9 | TLS | rustls | OpenSSL | Memory safety, no system dependency, easier wheels. |
| D10 | Native plugin boundary | C ABI via `PyCapsule` | Rust `dylib`, `abi_stable` | Works across separately compiled wheels and compiler versions. |
| D11 | Rate limit algorithm | GCRA | token bucket, sliding window | One value per key, exact, simple to store in redis. |
| D12 | Error format | RFC 9457 problem details | custom | Standard, supported by client tooling. |
| D13 | CI/CD | Local (`just`, git hooks) | GitHub Actions | Maintainer preference; one machine, reproducible pinned toolchain. |
| D14 | License | Apache-2.0 + trademark policy | MIT, AGPL-3.0 | Patent grant, attribution via NOTICE, name protection; permissive enough for adoption. |

## 29. Open questions

Each is decided in the milestone listed, recorded in [§28](#28-decision-log),
and removed from here.

| # | Question | Decide in |
| --- | --- | --- |
| Q1 | One asyncio loop per process vs per thread on free-threaded builds | M1 (benchmark) |
| Q2 | GIL batch size and time budget defaults | M1 (benchmark) |
| Q3 | JSON engine: `sonic-rs`, `serde_json` + custom visitor, or own parser | M2 (benchmark) |
| Q4 | 404 vs 422 for path parameters that fail type checks | M2 |
| Q5 | Whether `Model` instances are mutable by default | M2 |
| Q6 | Worker model on macOS/Windows (socket passing vs single process) | M1 |
| Q7 | Offer an ASGI adapter after 1.0 for ecosystem compatibility | after M6 |
| Q8 | Project site domain (`fulmen.dev` is assumed in docs) | M6 |

## 30. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Async bridge overhead erases gains for `async def` | High | Measure first in M1, before building on it. Fall back to per-thread loops or batching. |
| Free-threaded CPython is still maturing; C extensions in user apps may re-enable the GIL | Medium | Support both builds equally; `fulmen doctor` reports GIL state and the module that re-enabled it. |
| No ASGI means no Starlette middleware ecosystem | Medium | Ship the common middleware natively; migration codemod; consider ASGI adapter later (Q7). |
| Scope of v1 for a solo maintainer | High | Milestones each ship a usable release; strict non-goals; plugins let others build extras. |
| Native plugin ABI locks in early mistakes | Medium | Unstable until 1.0; version field; SDK hides the raw ABI. |
| PyO3 or CPython API changes | Low | Pinned versions, upgrade in dedicated commits, test matrix over all supported interpreters. |
| Benchmarks seen as cherry-picked | Medium | Publish scripts, include scenarios where the gap is small (DB-bound). |
