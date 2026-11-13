"""The `Fulmen` application object (DESIGN.md §5.1, §5.2, §8, §13.3).

Route decorators record `(method, path, handler, meta)` tuples; `on_error`
registrations record `(exc_class, handler)` pairs; `add_hook` (or the
named phase decorators) records `(phase, callable, priority)` triples —
each scoped to the application or to a mounted sub-application (§5.5).
Nothing is inspected or compiled at decoration time; `run()` (or a test
server via `Fulmen._compile_native()`) inspects each handler's signature
and type hints — `_typing.py` turns every parameter into its
source/wire-name/descriptor/missing spec — hands the declarations to
Rust, and serves. Compilation seals the declarations: registration APIs
raise `RuntimeError` afterwards (§14.8).

The same frozen spec trees feed `Fulmen.openapi()` — the OpenAPI 3.1
document (§11.3, §23.2, `fulmen.openapi`) — which `_compile_native`
renders once and mounts as `GET openapi_url` plus a self-hosted Swagger
UI under `docs_url` when configured.
"""

from __future__ import annotations

import functools
import inspect
import json
import typing
from collections.abc import Callable, Iterable, Iterator, Mapping
from typing import TYPE_CHECKING, Any, Unpack

from fulmen import _di, _typing
from fulmen import openapi as _openapi
from fulmen import plugins as _plugins
from fulmen._fulmen import NativeApp, StartupError, compile_descriptor
from fulmen._fulmen import serve as _serve
from fulmen.constraints import UNSET
from fulmen.responses import Response
from fulmen.settings import Settings

if TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

_Handler = Callable[..., object]

_METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")

# The hookable lifecycle phases (DESIGN.md §8); `route`/`extract`/`handle`/
# `encode` are Rust-internal and `on_error` is a keyed table, not a chain.
HOOK_PHASES: tuple[str, ...] = (
    "on_request",
    "before_route",
    "guard",
    "resolve",
    "before_handle",
    "after_handle",
    "on_response",
    "after_response",
)

# Ordering bands inside a phase — lower runs earlier (§8, §13.2). Bands
# below `PRIORITY_USER` are reserved for native security plugins (rate
# limit < authentication < CSRF) so security hooks precede user hooks
# regardless of registration order.
PRIORITY_SECURITY = 100
PRIORITY_SYSTEM = 500
PRIORITY_USER = 1000

# One `call_args` element of a route spec (docs/SPEC-FORMAT.md §3):
# (parameter name, source, signature kind, wire name, Descriptor|None,
#  (missing-kind, missing-object)). `missing-kind` is "required" (absence
# is a 422), "omit" (the signature default applies), "literal" (pass the
# object) or "factory" (call it per request).
_CallArg = tuple[str, str, str, str, "Descriptor | None", tuple[str, object]]

# A route declaration spec: (method, path,
# [(path-name, "int"|"str"), ...], call_args, handler, is-async,
# none-ok, [(status|None, Descriptor|None), ...]). `NativeApp.add_route`
# receives it with one more element appended — `layers`, the route's
# group scopes (the mounted apps it sits under), outermost first —
# which selects which scoped hooks and `on_error` handlers apply (§8).
# The path-param list is in handler signature order — the router returns
# extracted values in the same order. `none-ok` means a `None` return
# answers 204 (`-> None`/`-> NoContent` declared).
_RouteSpec = tuple[
    str,
    str,
    list[tuple[str, str]],
    list[_CallArg],
    _Handler,
    bool,
    bool,
    list[tuple[int | None, "Descriptor | None"]],
]


class Fulmen:
    """A Fulmen application: routes and error handlers, then `run()`.

    `title`/`version`/`description`/`contact`/`license` become the OpenAPI
    document's `info` object; `servers`/`tags` the top-level lists — each
    item a `str` (`{"url": ...}`/`{"name": ...}` shorthand) or a full
    OpenAPI object mapping. `openapi_url` mounts `GET <url>` serving the
    document; `docs_url` mounts the Swagger UI page (and its self-hosted
    assets) that reads it. `openapi_url=None` disables both endpoints —
    nothing is generated or mounted. `docs_url=None` keeps the document
    but drops the UI. A declared route wins over these convenience mounts
    (§5, §23.2).

    `debug=True` turns on §11.6 output validation: every handler return is
    checked against its declared response type, and a violation answers
    `500 application/problem+json` with the validation errors (the handler
    identity is logged). Serving flags (`run(debug=…)`, `--debug`) enable
    the same checks for an app built without the flag.

    `limits` declares request-parsing bounds (§14.3): a mapping with any
    of `max_depth`, `max_items`, `max_str_bytes`. Declared values apply
    to every route and bound every plugin's `ctx.limits` declaration —
    limits only tighten, so a value looser than a §14.3 default is a
    startup error.
    """

    __slots__ = (
        "_api_info",
        "_debug",
        "_declared",
        "_docs_url",
        "_error_handlers",
        "_hooks",
        "_limits",
        "_mounted",
        "_on_shutdown",
        "_on_startup",
        "_openapi_url",
        "_overrides",
        "_plugins",
        "_prefix",
        "_providers",
        "_sealed",
        "_settings",
        "_settings_cache",
        "_settings_instance",
    )

    def __init__(
        self,
        *,
        title: str = "Fulmen",
        version: str = "0.1.0",
        description: str | None = None,
        contact: Mapping[str, Any] | None = None,
        license: Mapping[str, Any] | None = None,
        servers: Iterable[str | Mapping[str, Any]] | None = None,
        tags: Iterable[str | Mapping[str, Any]] | None = None,
        openapi_url: str | None = "/openapi.json",
        docs_url: str | None = "/docs",
        debug: bool = False,
        prefix: str = "",
        settings: type[Settings] | Settings | None = None,
        limits: Mapping[str, int] | None = None,
    ) -> None:
        for name, url in (("openapi_url", openapi_url), ("docs_url", docs_url)):
            if url is not None and (not url.startswith("/") or '"' in url):
                raise ValueError(
                    f"{name} must be None or an absolute path starting with '/', got {url!r}"
                )
        if prefix and (not prefix.startswith("/") or prefix.endswith("/")):
            raise ValueError(
                f"prefix must be empty or `/…` without a trailing slash, got {prefix!r}"
            )
        self._limits = _plugins._validate_limits(limits or {}, owner="the app")  # pyright: ignore[reportPrivateUsage] - same package
        # §14.8's tightening rule applies to the app too: a declared
        # bound may not exceed the §14.3 default for its field.
        for field, declared in self._limits.items():
            default = _plugins._DEFAULT_LIMITS[field]  # pyright: ignore[reportPrivateUsage] - same package
            if declared > default:
                raise ValueError(
                    f"the app declares a looser `{field}` limit: "
                    f"{declared} exceeds the default {default}"
                )
        if settings is not None and not (
            isinstance(settings, Settings)
            or (isinstance(settings, type) and issubclass(settings, Settings))  # pyright: ignore[reportUnnecessaryIsInstance]
        ):
            raise TypeError(f"settings must be a Settings subclass or instance, got {settings!r}")
        self._debug = debug
        self._declared: list[tuple[str, str, _Handler, _openapi.OperationMeta]] = []
        self._error_handlers: list[tuple[type[BaseException], _Handler, bool]] = []
        self._hooks: list[tuple[str, _Handler, bool, str, int]] = []
        self._on_startup: list[tuple[_Handler, bool]] = []
        self._on_shutdown: list[tuple[_Handler, bool]] = []
        self._mounted: list[Fulmen] = []
        self._plugins: list[tuple[object, _plugins.PluginHandle[Any], _plugins.PluginMeta]] = []
        self._providers: list[_di.Provider[Any]] = []
        self._overrides: dict[object, object] = {}
        self._prefix = prefix
        self._sealed = False
        self._settings = settings
        self._settings_instance: Settings | None = (
            settings if isinstance(settings, Settings) else None
        )
        # Settings classes a handler parameter asked for (§20 injection),
        # resolved once and shared by every route of this app.
        self._settings_cache: dict[type[Settings], Settings] = {}
        self._api_info = _openapi.ApiInfo(
            title=title,
            version=version,
            description=description,
            contact=contact,
            license=license,
            servers=tuple(servers or ()),
            tags=tuple(tags or ()),
        )
        self._openapi_url = openapi_url
        self._docs_url = docs_url

    def route(
        self,
        path: str,
        methods: Iterable[str] = ("GET",),
        **options: Unpack[_openapi.RouteOptions],
    ) -> Callable[[_Handler], _Handler]:
        """Register a handler for `path` under each method in `methods`.

        `options` are documentation hints consumed by OpenAPI generation
        (`tags`, `summary`, `description`, `operation_id`, `deprecated`,
        `include_in_schema`) — they never change dispatch.
        """
        self._check_open()
        meta = _openapi.operation_meta(options)

        def decorator(handler: _Handler) -> _Handler:
            self._check_open()
            _di.stamp_localns(handler)
            for method in methods:
                self._declared.append((method.upper(), path, handler, meta))
            return handler

        return decorator

    def get(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `GET path`."""
        return self.route(path, methods=("GET",), **options)

    def post(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `POST path`."""
        return self.route(path, methods=("POST",), **options)

    def put(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PUT path`."""
        return self.route(path, methods=("PUT",), **options)

    def patch(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PATCH path`."""
        return self.route(path, methods=("PATCH",), **options)

    def delete(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `DELETE path`."""
        return self.route(path, methods=("DELETE",), **options)

    def head(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `HEAD path`."""
        return self.route(path, methods=("HEAD",), **options)

    def options(
        self, path: str, **options: Unpack[_openapi.RouteOptions]
    ) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `OPTIONS path`."""
        return self.route(path, methods=("OPTIONS",), **options)

    def on_error(self, exc_class: object) -> Callable[[_Handler], _Handler]:
        """Register a handler for exceptions route handlers raise (§17).

        The closest registered class in the raised exception's MRO wins;
        the handler receives the exception instance and its return value
        is converted like a route's. A registered handler wins over the
        built-in `application/problem+json` rendering, so `HTTPError`
        subclasses can be re-rendered per app. An error handler that
        itself raises answers `500` — error routing never recurses.
        """
        if not (isinstance(exc_class, type) and issubclass(exc_class, BaseException)):
            raise TypeError(f"on_error expects an exception class, got {exc_class!r}")

        def decorator(handler: _Handler) -> _Handler:
            self._check_open()
            self._error_handlers.append((exc_class, handler, inspect.iscoroutinefunction(handler)))
            return handler

        return decorator

    def _check_open(self) -> None:
        """§14.8: registration closes when the app compiles."""
        if self._sealed:
            raise RuntimeError(
                "application declarations are sealed — hooks, routes and "
                "error handlers must be registered before run()/compile"
            )

    def add_hook(
        self,
        phase: str,
        fn: _Handler | None = None,
        *,
        priority: int = PRIORITY_USER,
    ) -> Callable[[_Handler], _Handler] | _Handler:
        """Register `fn` in lifecycle `phase` (DESIGN.md §8).

        `phase` is one of `on_request`, `before_route`, `guard`, `resolve`,
        `before_handle`, `after_handle`, `on_response`, `after_response`.
        Usable directly (`app.add_hook("guard", fn)`) or as a decorator
        (`@app.add_hook("guard")`); the named methods
        (`app.before_handle`, `app.on_response`, …) are the same thing
        without the string.

        Hooks take `(request)` — or `(request, response)` in the
        post-handler phases (`after_handle`, `on_response`,
        `after_response`). Returning `None` continues the pipeline,
        returning a value short-circuits it to a response, and returning
        a `dict` merges it into `request.state` and continues. `def`
        hooks run on the sync pool, `async def` hooks on the asyncio
        loop, exactly like handlers.

        `priority` orders hooks inside a phase (lower runs earlier);
        bands below `PRIORITY_USER` are reserved for security plugins.
        Hooks declared on a mounted application run only for routes
        under that application (§5.5).
        """
        if phase not in HOOK_PHASES:
            names = ", ".join(HOOK_PHASES)
            raise ValueError(f"unknown hook phase {phase!r} (expected one of: {names})")

        def decorator(hook: _Handler) -> _Handler:
            self._check_open()
            name = getattr(hook, "__qualname__", "<hook>")
            self._hooks.append((phase, hook, inspect.iscoroutinefunction(hook), name, priority))
            return hook

        return decorator(fn) if fn is not None else decorator

    def on_request(self, fn: _Handler | None = None):
        """`@app.on_request` — before any routing work (§8)."""
        return self.add_hook("on_request", fn)

    def before_route(self, fn: _Handler | None = None):
        """`@app.before_route` — once the route is known (CORS preflight)."""
        return self.add_hook("before_route", fn)

    def guard(self, fn: _Handler | None = None):
        """`@app.guard` — allow/deny checks (rate limit, auth, CSRF)."""
        return self.add_hook("guard", fn)

    def resolve(self, fn: _Handler | None = None):
        """`@app.resolve` — dependency resolution (§8)."""
        return self.add_hook("resolve", fn)

    def before_handle(self, fn: _Handler | None = None):
        """`@app.before_handle` — immediately before the handler."""
        return self.add_hook("before_handle", fn)

    def after_handle(self, fn: _Handler | None = None):
        """`@app.after_handle` — after the handler; may replace the response."""
        return self.add_hook("after_handle", fn)

    def on_response(self, fn: _Handler | None = None):
        """`@app.on_response` — post-encode header mutation over any response."""
        return self.add_hook("on_response", fn)

    def after_response(self, fn: _Handler | None = None):
        """`@app.after_response` — fire-and-forget access log/metrics."""
        return self.add_hook("after_response", fn)

    def on_startup(self, fn: _Handler | None = None):
        """`@app.on_startup` — run after a successful compile, before bind (§5.7)."""

        def decorator(hook: _Handler) -> _Handler:
            self._check_open()
            self._on_startup.append((hook, inspect.iscoroutinefunction(hook)))
            return hook

        return decorator(fn) if fn is not None else decorator

    def on_shutdown(self, fn: _Handler | None = None):
        """`@app.on_shutdown` — run after the server stops (§5.7)."""

        def decorator(hook: _Handler) -> _Handler:
            self._check_open()
            self._on_shutdown.append((hook, inspect.iscoroutinefunction(hook)))
            return hook

        return decorator(fn) if fn is not None else decorator

    def mount(self, app: Fulmen) -> Fulmen:
        """Mount `app` — a `Fulmen(prefix=…)` — under its prefix (§5.5).

        The mounted application's routes join under its `prefix`; hooks
        and `on_error` handlers declared on it apply only to those routes
        (a *route group*), while app-level hooks still wrap everything.
        Mounting is registration — it closes when this app compiles.
        """
        self._check_open()
        # Runtime validation for untyped callers (pyright sees the
        # annotation as proof enough).
        if not isinstance(app, Fulmen):  # pyright: ignore[reportUnnecessaryIsInstance]
            raise TypeError(f"mount expects a Fulmen application, got {type(app).__qualname__}")
        self._mounted.append(app)
        return app

    def use(
        self,
        plugin: _plugins.PluginT,
        config: Mapping[str, Any] | None = None,
        /,
        **extra: Any,
    ) -> _plugins.PluginHandle[_plugins.PluginT]:
        """Register `plugin` and return its typed handle (DESIGN.md §13).

        `use()` does not run `setup()`: every `use()` in the app tree is
        resolved into a dependency order at compile (`requires`, §13.2),
        and each `plugin.setup(ctx)` runs then with a `PluginContext` —
        `use()` order is not execution order. `handle.state` is whatever
        `setup()` returned; attribute lookups it doesn't define fall
        through to the plugin object, so `Annotated[auth.Claims, auth]`
        works at decoration time (§5.6).

        `config` plus any keyword arguments form the plugin's
        configuration mapping, delivered as `ctx.config` — the seam
        `Settings` sections (§20, M3.22) will feed typed slices into.

        `group.use(plugin)` scopes the plugin's contributions to that
        group and its children (§13.3); a plugin declaring
        `scope = "app"` refuses group use at compile.
        """
        self._check_open()
        meta = _plugins.plugin_meta(plugin)
        merged: dict[str, Any] = {**dict(config or {}), **extra}
        handle = _plugins.PluginHandle(plugin, merged)
        self._plugins.append((plugin, handle, meta))
        return handle

    def provide(
        self,
        decl: object = None,
        /,
        *,
        scope: _di.Scope | str | None = None,
        provides: object = UNSET,
        name: str | None = None,
    ) -> Callable[[object], _di.Provider[Any]] | _di.Provider[Any]:
        """Register a provider application-wide (DESIGN.md §12.1).

        Providers reachable through `Annotated[T, provider]` markers
        register themselves on first use — `provide()` is for providers
        only reached by bare-type lookups (a provider parameter like
        `settings: Settings`) or by plugins' handle markers. The returned
        `Provider` doubles as an `Annotated` marker.

        `decl` may be a `Provider` (returned as-is), a callable (wrapped
        with `scope` — `Scope.REQUEST` unless given), or any other value
        (an application-scoped constant provider). Usable directly or as
        a decorator (`@app.provide(scope=Scope.APP)`).
        """
        self._check_open()

        def register(d: object, localns: dict[str, Any] | None) -> _di.Provider[Any]:
            self._check_open()
            registered = _di.as_provider(
                d, scope=scope, provides=provides, name=name, _localns=localns
            )
            self._providers.append(registered)
            return registered

        if decl is not None:
            return register(decl, _di.frame_locals(1))

        def decorator(d: object) -> _di.Provider[Any]:
            return register(d, _di.frame_locals(1))

        return decorator

    def override(self, original: object, replacement: object, /) -> _OverrideToken:
        """Substitute `replacement` wherever `original` is injected (§12.3).

        `original` is a provider marker — a `Provider`, a registered
        callable, or a value registered with `app.provide`/`ctx.add_provider`.
        `replacement` may be a `Provider` (keeps its own scope), a
        callable (wrapped with the original's scope), or a plain value.
        The app itself is not mutated: the substitution applies when the
        dependency graph is built at compile, and every dependent
        provider resolves the replacement — overrides validate like any
        other declaration.

        Call it plainly (`app.override(db, fake_db)`) or as a context
        manager (`with app.override(db, fake_db):`) — the token reverts
        the override on exit. Overrides are declarations: they seal at
        compile like everything else (§14.8).
        """
        self._check_open()
        _di.stamp_localns(replacement)
        token = _OverrideToken(self._overrides, original, replacement)
        self._overrides[original] = replacement
        return token

    @property
    def settings(self) -> Settings | None:
        """The app's resolved settings instance (DESIGN.md §20), or
        `None` when the app was created without `settings=`."""
        return self._resolve_app_settings()

    def _resolve_app_settings(self) -> Settings | None:
        if self._settings_instance is None and (
            isinstance(self._settings, type) and issubclass(self._settings, Settings)  # pyright: ignore[reportUnnecessaryIsInstance]
        ):
            self._settings_instance = self._settings()
        return self._settings_instance

    def _resolve_settings(self, cls: type[Settings]) -> Settings:
        """The instance injected for a `cls`-annotated handler parameter.

        A class matching the app's own settings resolves to `app.settings`;
        anything else is constructed once per app and shared by its routes.
        """
        instance = self._resolve_app_settings()
        if isinstance(instance, cls):
            return instance
        cached = self._settings_cache.get(cls)
        if cached is None:
            cached = cls()
            self._settings_cache[cls] = cached
        return cached

    def _settings_resolver_for(self, app: Fulmen) -> Callable[[type[Settings]], Settings]:
        """Which app's settings a route declared on `app` injects — a
        mounted app with its own settings uses them; otherwise the root's."""
        owner = self if app is self or app._settings is None else app
        return owner._resolve_settings

    def _walk(
        self,
        prefix: str = "",
        scopes: tuple[Fulmen, ...] = (),
        extra_mounts: dict[int, list[Fulmen]] | None = None,
    ) -> Iterator[tuple[Fulmen, str, tuple[Fulmen, ...]]]:
        """Yield `(app, full_prefix, group_chain)` for this app and every
        mounted descendant — `group_chain` is the mounted-app chain a
        route belongs to, outermost first. `extra_mounts` grafts
        plugin-mounted groups onto their owner's children (§13.1)."""
        base = _join_paths(prefix, self._prefix)
        yield self, base, scopes
        children = list(self._mounted)
        if extra_mounts is not None:
            children.extend(extra_mounts.get(id(self), ()))
        for child in children:
            yield from child._walk(base, (*scopes, child), extra_mounts)

    def run(
        self,
        host: str = "127.0.0.1",
        port: int = 8000,
        workers: int = 1,
        debug: bool = False,
        *,
        uvloop: bool | None = None,
        gil_batch_size: int = 32,
    ) -> None:
        """Compile the declarations and serve until the process is signalled.

        `workers > 1` forks worker processes that share `host:port` with
        `SO_REUSEPORT` on Linux and macOS; other platforms refuse it and
        stay single-process (DESIGN.md §7.2, decision D15).

        `debug=True` turns on §11.6 output validation for this serving —
        every handler return is checked against its declared response type
        and violations answer `500 application/problem+json`.

        `uvloop` selects the event loop for `async def` handlers (§7.4):
        `None` uses uvloop when the package is installed, `False` forces
        plain asyncio, `True` requires uvloop and fails startup without it.

        `gil_batch_size` tunes the sync pool's GIL batching (§7.5): when
        more handlers are queued than the pool can run in parallel, up to
        `gil_batch_size` of the surplus run under one GIL acquisition.
        No-op on free-threaded builds.
        """
        if workers < 1:
            raise ValueError(f"workers must be >= 1, got {workers}")
        if workers > 1:
            from fulmen.workers import run_workers

            run_workers(
                self,
                host=host,
                port=port,
                workers=workers,
                debug=debug,
                uvloop=uvloop,
                gil_batch_size=gil_batch_size,
            )
            return
        _serve(
            self._compile_native(),
            host,
            port,
            debug=debug,
            uvloop=uvloop,
            gil_batch_size=gil_batch_size,
        )

    def _prepare_plugins(self) -> _plugins.PluginPlan:
        """Resolve every `use()` in the app tree and run each `setup()`
        (§6.4.2): the first half of compilation, shared by
        `_compile_native`, `openapi()` and `fulmen <cmd>` dispatch.

        Resolution is global — `requires`/`conflicts` bind plugin names
        anywhere in the tree — and deterministic: topological by
        `requires`, ties in `use()` order (§13.2). Groups a plugin mounts
        (`ctx.mount`) join the walk, so `use()`s nested under
        plugin-mounted groups resolve in a later wave: their plugins
        always run after the plugin that mounted their group.
        """
        plan = _plugins.PluginPlan()
        seen: set[int] = set()
        uses: list[_plugins.PluginUse] = []
        done: set[int] = set()
        setup_problems: list[str] = []
        # Plugin mounts surface more `use()`s, so resolve+setup repeats
        # until a wave discovers nothing new (depth-capped).
        for _wave in range(16):
            discovered = False
            for app, _base, _chain in self._walk(extra_mounts=plan.extra_mounts):
                if id(app) in seen:
                    continue
                seen.add(id(app))
                discovered = True
                # §14.8: the bound is declared once, on the root
                # application — a mounted group's `limits=` is refused.
                if app is not self and app._limits:
                    plan.problems.append(
                        f"mounted group `{app._prefix}` declares `limits` — "
                        "request limits are declared on the root application"
                    )
                uses.extend(
                    _plugins.PluginUse(owner=app, plugin=plugin, handle=handle, meta=meta)
                    for plugin, handle, meta in app._plugins
                )
            if not uses:
                return plan
            order, _pending = _plugins.resolve_uses(uses)
            mounted_new = False
            for index in order:
                if index in done:
                    continue
                done.add(index)
                use = uses[index]
                if use.meta.app_only and use.owner is not self:
                    setup_problems.append(
                        f'plugin `{use.meta.name}` declares scope="app" '
                        "and cannot be used on a route group"
                    )
                    continue
                ctx = _plugins.PluginContext(
                    app=use.owner, plugin_name=use.meta.name, config=use.handle.config
                )
                try:
                    state = use.plugin.setup(ctx)
                except Exception as exc:  # noqa: BLE001 -- setup failures aggregate as startup problems (§14.1)
                    setup_problems.append(f"plugin `{use.meta.name}`: setup() failed: {exc}")
                    continue
                ctx.close()
                use.handle.settle(state)
                plan.entries.append((use, ctx))
                for mounted in ctx.staged.mounted:
                    plan.extra_mounts.setdefault(id(use.owner), []).append(mounted)
                    mounted_new = True
            if not discovered and not mounted_new:
                break
        else:
            setup_problems.append("plugin mount nesting exceeds the supported depth (16)")

        # Validation over the complete use set — a `requires` missing in
        # an earlier wave may have healed as plugin-mounted groups
        # surfaced, so resolution runs last, once.
        _order, resolution_problems = _plugins.resolve_uses(uses)
        plan.problems.extend(resolution_problems)
        plan.problems.extend(setup_problems)
        plan.problems.extend(_plugins.order_integrity(plan.entries))
        plan.finish()
        # §14.8: declared limits only tighten — each plugin field is
        # checked against the app's (a §14.3 default when undeclared).
        for use, ctx in plan.entries:
            for field, declared in ctx.staged.limits.items():
                allowed = self._limits.get(field)
                bound = allowed if allowed is not None else _plugins._DEFAULT_LIMITS[field]  # pyright: ignore[reportPrivateUsage] - same package
                if declared > bound:
                    who = f"the app's {allowed}" if allowed is not None else f"the default {bound}"
                    plan.problems.append(
                        f"plugin `{use.meta.name}` declares a looser `{field}` limit: "
                        f"{declared} exceeds {who}"
                    )
        return plan

    def _provider_graph(self, plan: _plugins.PluginPlan) -> _di.Graph:
        """Build the DI registry: `app.provide` declarations from every
        app in the tree plus each plugin's `ctx.add_provider`
        contributions, with `app.override` substitutions recorded for
        graph validation (§12)."""
        graph = _di.Graph()
        for app, _base, _chain in self._walk(extra_mounts=plan.extra_mounts):
            for decl in app._providers:
                graph.declare(decl)
        for use, ctx in plan.entries:
            graph.known_plugins.add(use.meta.name)
            for decl in ctx.staged.providers:
                graph.declare(decl, owner=use.meta.name)
        graph.override_map.update(self._overrides)
        return graph

    def _plugin_cli_commands(self) -> tuple[dict[str, _plugins.CliHandler], list[str]]:
        """The `fulmen <cmd>` dispatch table: every plugin-registered
        command plus the aggregated resolution problems (§13.1, §24)."""
        plan = self._prepare_plugins()
        return dict(plan.cli_commands), list(plan.problems)

    def openapi(self) -> dict[str, Any]:
        """The OpenAPI 3.1 document for the declared routes (§11.3, §23.2).

        Inspects handlers exactly like `_compile_native` — a broken app
        raises `StartupError` here too. The returned dict is freshly
        built each call; serving renders it once at startup instead.
        """
        plan = self._prepare_plugins()
        walked = list(self._walk(extra_mounts=plan.extra_mounts))
        # Group scopes only matter to the native declaration; the
        # document sees the joined paths either way.
        self._resolve_app_settings()
        graph = self._provider_graph(plan)
        specs, problems = self._route_specs(
            {id(app): 0 for app, _b, _c in walked}, plan, walked, graph
        )
        all_problems = plan.problems + problems + graph.validate()
        if all_problems:
            raise _startup_error(all_problems)
        document = _openapi.generate(
            self._api_info,
            [_route_entry(spec, meta) for spec, meta, _layers, _route_di in specs],
        )
        component_problems = _plugins.merge_components(document, plan.components)
        if component_problems:
            raise _startup_error(component_problems)
        return document

    def _compile_native(self) -> NativeApp:
        """Inspect every recorded handler and build the native declaration.

        Python-side problems (unknown parameter, bad annotation, missing
        path-param binding, uninspectable signature, plugin resolution
        failures) are aggregated and raised as one `StartupError` in the
        same format Rust produces for core compile errors. Compiling
        seals the declarations (§14.8).
        """
        self._sealed = True
        plan = self._prepare_plugins()
        walked = list(self._walk(extra_mounts=plan.extra_mounts))
        # §14.8: every app in the tree seals — mounted groups included —
        # so no registration path accepts declarations after compile.
        for app, _base, _chain in walked:
            app._sealed = True
        # §20: settings resolve and validate at startup — a settings
        # class with missing/invalid values raises `StartupError` here
        # before a single route is registered.
        self._resolve_app_settings()
        native = NativeApp(debug=self._debug)
        # §14.3/§14.8: the app's declared limits — the bound plugins can
        # only tighten. Compile reports any field looser than a default.
        if self._limits:
            native.set_limits(**self._limits)
        # One group scope per mounted application; the app's own
        # registrations stay in the application scope (`0`).
        scope_ids: dict[int, int] = {
            id(app): native.add_scope() for app, _b, _c in walked if app is not self
        }
        graph = self._provider_graph(plan)
        specs, problems = self._route_specs(scope_ids, plan, walked, graph)
        problems = plan.problems + problems + graph.validate()
        # §12.1: request-context generator providers clean up after the
        # response. Each route needing teardown gets a private hook scope
        # appended to its layers, so its `after_response` cleanup hook
        # fires for that route alone — routes without generators pay
        # nothing (§8 cost model).
        cleanup_scopes: list[tuple[int, bool]] = []
        for spec, _meta, layers, route_di in specs:
            route_layers = list(layers)
            if route_di is not None and route_di.cleanup_lane is not None:
                private = native.add_scope()
                route_layers.append(private)
                cleanup_scopes.append((private, route_di.cleanup_lane == "async"))
            native.add_route((*spec, route_layers))
        # §12.1: APP providers resolve once at startup, ahead of plugin
        # and application startup hooks, so every hook sees initialized
        # dependencies.
        if graph.startup_hook is not None:
            native.add_lifecycle_hook("startup", graph.startup_hook, graph.startup_is_async)
        # §8/§13.2: plugin contributions register ahead of app hooks so
        # plugin hooks precede app hooks in each phase's (priority, seq)
        # ordering; each plugin's registrations take its owner's scope.
        for _use, ctx in plan.entries:
            staged = ctx.staged
            scope = 0 if staged.owner is self else scope_ids[id(staged.owner)]
            # §14.8: plugin-declared limits bind the plugin's scope —
            # compile checks every field against the app's before they
            # apply to the scope's routes.
            if staged.limits:
                native.add_scope_limits(scope, _use.meta.name, **staged.limits)
            # §13.4: native descriptor capsules validate and load through
            # the same merge — an ABI mismatch or a failing plugin `setup`
            # aggregates like any other declaration problem. `ctx.config`
            # becomes the plugin's `config_json`.
            for capsule, band, keep in staged.native:
                try:
                    native.add_native_plugin(
                        capsule,
                        json.dumps(dict(ctx.config)).encode(),
                        PRIORITY_USER if band is None else band,
                        scope,
                        keep,
                    )
                except Exception as exc:  # noqa: BLE001 -- load failures aggregate as startup problems (§14.1)
                    problems.append(f"plugin `{_use.meta.name}`: {exc}")
            for phase, hook, is_async, name, priority in staged.hooks:
                native.add_hook(phase, hook, is_async, name, priority, scope)
            for exc_class, handler, is_async in staged.error_handlers:
                native.add_error_handler(exc_class, handler, is_async, scope)
            for kind, hook, is_async in staged.lifecycle:
                native.add_lifecycle_hook(kind, hook, is_async)
        for app, _base, _chain in walked:
            scope = 0 if app is self else scope_ids[id(app)]
            for phase, hook, is_async, name, priority in app._hooks:
                native.add_hook(phase, hook, is_async, name, priority, scope)
            for exc_class, handler, is_async in app._error_handlers:
                native.add_error_handler(exc_class, handler, is_async, scope)
            for hook, is_async in app._on_startup:
                native.add_lifecycle_hook("startup", hook, is_async)
            for hook, is_async in app._on_shutdown:
                native.add_lifecycle_hook("shutdown", hook, is_async)
        # APP-scope generator providers close at shutdown, after user
        # shutdown hooks (they may still use the resources).
        if graph.shutdown_hook is not None:
            native.add_lifecycle_hook("shutdown", graph.shutdown_hook, graph.shutdown_is_async)
        for private, is_async in cleanup_scopes:
            native.add_hook(
                "after_response",
                graph.cleanup_hook(is_async),
                is_async,
                "fulmen.di:cleanup",
                _di.CLEANUP_PRIORITY,
                private,
            )
        if problems:
            raise _startup_error(problems)
        self._serve_docs(native, [(spec, meta) for spec, meta, _layers, _di_route in specs], plan)
        return native

    def _route_specs(
        self,
        scope_ids: dict[int, int],
        plan: _plugins.PluginPlan | None = None,
        walked: list[tuple[Fulmen, str, tuple[Fulmen, ...]]] | None = None,
        graph: _di.Graph | None = None,
    ) -> tuple[
        list[tuple[_RouteSpec, _openapi.OperationMeta, list[int], _di.RouteDi | None]], list[str]
    ]:
        """Every declaration inspected into its `_RouteSpec` + doc meta +
        group layers + DI record, plus the aggregated problem list.
        Plugin-declared routes join after their owner app's own
        declarations."""
        if walked is None:
            walked = list(self._walk(extra_mounts=plan.extra_mounts if plan else None))
        if graph is None:
            graph = _di.Graph()
        plugin_routes = plan.route_decls() if plan is not None else {}
        specs: list[tuple[_RouteSpec, _openapi.OperationMeta, list[int], _di.RouteDi | None]] = []
        problems: list[str] = []
        for app, base, chain in walked:
            layers = [scope_ids[id(group)] for group in chain]
            resolve_settings = self._settings_resolver_for(app)
            for method, path, handler, meta in (
                *app._declared,
                *plugin_routes.get(id(app), ()),
            ):
                spec, route_di = _inspect_handler(
                    method,
                    _join_paths(base, path),
                    handler,
                    problems,
                    resolve_settings,
                    graph,
                )
                if spec is not None:
                    specs.append((spec, meta, layers, route_di))
        return specs, problems

    def _serve_docs(
        self,
        native: NativeApp,
        specs: list[tuple[_RouteSpec, _openapi.OperationMeta]],
        plan: _plugins.PluginPlan,
    ) -> None:
        """Mount `GET openapi_url`, `GET docs_url` and the UI assets, each
        serving a body rendered once here — the spec trees are frozen at
        compile time, so there is nothing left to compute per request.
        `openapi_url=None` registers nothing."""
        if self._openapi_url is None:
            return
        entries = [_route_entry(spec, meta) for spec, meta in specs]
        document = _openapi.generate(self._api_info, entries)
        component_problems = _plugins.merge_components(document, plan.components)
        if component_problems:
            raise _startup_error(component_problems)
        document_bytes = _openapi.render(document)
        taken = {(spec[0], spec[1]) for spec, _meta in specs}
        _add_static_route(native, taken, self._openapi_url, document_bytes, "application/json")
        if self._docs_url is None:
            return
        page = _openapi.docs_html(
            openapi_url=self._openapi_url,
            docs_url=self._docs_url,
            title=self._api_info.title,
        ).encode()
        _add_static_route(native, taken, self._docs_url, page, "text/html; charset=utf-8")
        for path, body, content_type in _openapi.docs_assets(self._docs_url):
            _add_static_route(native, taken, path, body, content_type)


class _OverrideToken:
    """The context manager `app.override()` returns — `__exit__` reverts
    the substitution unless it was already reverted. A later `override()`
    of the same marker supersedes the entry, which revert leaves alone."""

    __slots__ = ("_map", "_original", "_replacement", "_reverted")

    def __init__(
        self, overrides: dict[object, object], original: object, replacement: object
    ) -> None:
        self._map = overrides
        self._original = original
        self._replacement = replacement
        self._reverted = False

    def revert(self) -> None:
        """Remove this token's override — a no-op once it has been
        reverted or superseded by a later `override()` of the same marker."""
        if not self._reverted:
            if self._map.get(self._original) is self._replacement:
                del self._map[self._original]
            self._reverted = True

    def __enter__(self) -> _OverrideToken:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.revert()


def _join_paths(prefix: str, path: str) -> str:
    """Join a mount prefix onto a declared route path (`"/api" + "/users"`)."""
    if not prefix:
        return path
    base = prefix.rstrip("/")
    return f"{base}{path}" if path.startswith("/") else f"{base}/{path}"


def _startup_error(problems: list[str]) -> StartupError:
    message = "application failed to start:" + "".join(f"\n  - {problem}" for problem in problems)
    return StartupError(message)


def _inspect_handler(
    method: str,
    path: str,
    handler: _Handler,
    problems: list[str],
    resolve_settings: Callable[[type[Settings]], Settings],
    graph: _di.Graph,
) -> tuple[_RouteSpec | None, _di.RouteDi | None]:
    """Turn one recorded declaration into a `_RouteSpec` plus the route's
    DI record — `(None, …)` appends problems instead. Provider
    parameters (`Annotated[T, provider]`/handle markers) leave the wire
    call spec: the registered callable becomes a wrapper that resolves
    them, so the route's spec, OpenAPI projection and path validation
    only ever see wire parameters (§12.1)."""
    label = f"{method} {path} ({getattr(handler, '__qualname__', '<handler>')})"
    try:
        signature = inspect.signature(handler)
    except (TypeError, ValueError) as exc:
        problems.append(f"{label}: cannot inspect the handler signature: {exc}")
        return None, None
    try:
        hints = typing.get_type_hints(handler, localns=_di.localns_of(handler), include_extras=True)
    except Exception as exc:  # noqa: BLE001 -- any hint-resolution failure is a startup problem
        problems.append(f"{label}: cannot resolve type hints: {exc}")
        return None, None

    # §12.1: provider-marker parameters resolve through the DI graph —
    # they never reach `param_spec`, so nothing about them lands on the
    # wire spec or the OpenAPI projection.
    route_di = _di.RouteDi()
    di_names: set[str] = set()
    for index, (name, parameter) in enumerate(signature.parameters.items()):
        if parameter.kind in (
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        ):
            continue
        hinted = hints.get(name)
        if hinted is None:
            continue
        picked = graph.marker_of(hinted)
        if picked is None:
            continue
        base, marker = picked
        where = f"{label} parameter `{name}`"
        # The marker identified this as a DI parameter — wire inspection
        # never sees it, even when the marker itself fails to resolve.
        di_names.add(name)
        key = graph.key_of(marker, base, where)
        if key is not None:
            positional = index if parameter.kind is inspect.Parameter.POSITIONAL_ONLY else None
            route_di.bindings.append((name, key, positional))

    path_names = frozenset(_path_param_names(path))
    builder = _typing.SpecBuilder()
    infos: list[_typing.ParamInfo] = []
    injected: dict[str, Settings] = {}
    try:
        for name, parameter in signature.parameters.items():
            if name in di_names:
                continue
            if parameter.kind in (
                inspect.Parameter.VAR_POSITIONAL,
                inspect.Parameter.VAR_KEYWORD,
            ):
                problems.append(
                    f"{label}: variadic parameter `{name}` is not supported; "
                    "handlers declare path, query, header, cookie, body or form parameters"
                )
                return None, None
            hint = hints.get(name, inspect.Parameter.empty)
            base_hint = (
                typing.get_args(hint)[0] if typing.get_origin(hint) is typing.Annotated else hint
            )
            if isinstance(base_hint, type) and issubclass(base_hint, Settings):
                # §20: a Settings-typed parameter is injected, not read
                # from the wire — it never reaches `call_args`.
                if parameter.kind is inspect.Parameter.POSITIONAL_ONLY:
                    problems.append(
                        f"{label}: settings parameter `{name}` cannot be positional-only"
                    )
                    return None, None
                if parameter.default is not inspect.Parameter.empty:
                    problems.append(
                        f"{label}: settings parameter `{name}` cannot declare a "
                        "default — the app injects the resolved instance"
                    )
                    return None, None
                if typing.get_origin(hint) is typing.Annotated:
                    problems.append(
                        f"{label}: settings parameter `{name}` takes no `Annotated` "
                        "source marker — a Settings type is injected, not extracted"
                    )
                    return None, None
                try:
                    injected[name] = resolve_settings(base_hint)
                except Exception as exc:  # noqa: BLE001 -- settings load failures are startup problems
                    problems.append(str(exc))
                    return None, None
                continue
            default = UNSET if parameter.default is inspect.Parameter.empty else parameter.default
            infos.append(
                _typing.param_spec(builder, name, hint, default, parameter.kind, path_names, label)
            )
        variants = _typing.response_spec(builder, hints.get("return", inspect.Signature.empty))
        response_descs: list[tuple[int | None, Descriptor | None]] = [
            (
                variant.status,
                compile_descriptor(variant.body, builder.callbacks)
                if variant.body is not None
                else None,
            )
            for variant in variants
        ]
    except ValueError as exc:
        # `AnnotationError` (a `ValueError`) and `compile_descriptor`'s
        # spec/compile `ValueError`s are all startup problems.
        problems.append(str(exc))
        return None, None

    path_infos = [info for info in infos if info.source == "path"]
    missing = [name for name in path_names if name not in {info.name for info in path_infos}]
    if missing:
        joined = ", ".join(f"`{name}`" for name in missing)
        problems.append(f"{label}: path parameter(s) {joined} are not bound by the handler")
        return None, None

    param_types = [
        (info.name, "int" if info.spec["kind"] == "int" else "str") for info in path_infos
    ]
    call_args: list[_CallArg] = [
        (
            info.name,
            info.source,
            info.kind,
            info.wire_name,
            info.descriptor,
            _missing_policy(info, signature),
        )
        for info in infos
    ]
    is_async = inspect.iscoroutinefunction(handler)
    if injected:
        handler = _inject_settings(handler, injected, is_async)
    if route_di.bindings:
        # The wrapper takes the request first (native `request` source),
        # resolves the DI parameters inside the handler's executor job,
        # then calls through — one Python crossing for resolve+handle.
        call_args.insert(0, _di.REQUEST_CALL_ARG)
        handler = graph.wrap(handler, route_di, is_async)
        graph.register_route(label, is_async, route_di)
    else:
        route_di = None
    return (
        method,
        path,
        param_types,
        call_args,
        handler,
        is_async,
        any(variant.status == 204 for variant in variants),
        response_descs,
    ), route_di


def _inject_settings(handler: _Handler, injected: dict[str, Settings], is_async: bool) -> _Handler:
    """Wrap `handler` so `Settings` parameters — removed from the call
    spec — arrive as keyword arguments the app resolved at startup."""
    if is_async:

        @functools.wraps(handler)
        async def async_injected(*args: Any, **kwargs: Any) -> Any:
            return await typing.cast(
                typing.Coroutine[Any, Any, Any],
                handler(*args, **injected, **kwargs),
            )

        return async_injected

    @functools.wraps(handler)
    def sync_injected(*args: Any, **kwargs: Any) -> Any:
        return handler(*args, **injected, **kwargs)

    return sync_injected


def _route_entry(
    spec: _RouteSpec,
    meta: _openapi.OperationMeta,
) -> _openapi.RouteEntry:
    """Project a `_RouteSpec` into the `RouteEntry` generation consumes —
    descriptors resolved to their spec dicts."""
    return _openapi.RouteEntry(
        method=spec[0],
        path=spec[1],
        call_args=tuple(
            (name, source, kind, wire, desc.spec if desc is not None else None, missing)
            for name, source, kind, wire, desc, missing in spec[3]
        ),
        responses=tuple(
            (status, desc.spec if desc is not None else None) for status, desc in spec[7]
        ),
        handler=spec[4],
        meta=meta,
    )


def _add_static_route(
    native: NativeApp,
    taken: set[tuple[str, str]],
    path: str,
    body: bytes,
    content_type: str,
) -> None:
    """Mount a `GET` serving a pre-built body. A declared route for the
    same `GET path` wins — skip the mount instead of a duplicate-route
    startup error."""
    if ("GET", path) in taken:
        return

    def handler() -> Response:
        return Response(body, content_type=content_type)

    native.add_route(("GET", path, [], [], handler, False, False, [(None, None)], []))


def _missing_policy(info: _typing.ParamInfo, signature: inspect.Signature) -> tuple[str, object]:
    """What an absent wire value means for `info` (§5.2): `required`
    collects a 422; `omit` leaves the argument out so the signature's own
    default applies; `literal`/`factory` pass a marker-declared default
    the signature cannot express."""
    if info.required:
        return ("required", None)
    if info.factory is not None:
        return ("factory", info.factory)
    if info.default is signature.parameters[info.name].default:
        # The signature's own default reached `info.default` — omitting
        # the kwarg applies exactly it (markers never overrode it).
        return ("omit", None)
    return ("literal", info.default)


def _path_param_names(path: str) -> list[str]:
    """The `{name}` and `{*name}` parameters of a route pattern, in order."""
    return [
        segment[1:-1].lstrip("*")
        for segment in path.split("/")
        if len(segment) > 2 and segment.startswith("{") and segment.endswith("}")
    ]
