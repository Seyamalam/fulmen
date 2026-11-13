"""The plugin system (DESIGN.md §13).

A *plugin* is any object satisfying the [`Plugin`] protocol: a unique
`name`, a `version`, optional `requires`/`conflicts` name lists, and a
`setup(ctx)` method. `app.use(plugin, config)` records the plugin and
returns a [`PluginHandle`] immediately; `setup()` itself runs later, at
compile time, in the resolved dependency order (§6.4.2) — so `use()`
order is *not* execution order.

[`PluginContext`] is the only extension surface a plugin sees (§13.1):
it stages every contribution — lifecycle hooks, routes and mounted
groups, `on_error` handlers, provider declarations, OpenAPI components,
CLI commands and `fulmen check` rules — and the merge into the app's
native declaration happens when `setup()` returns, ahead of the app's
own hook registrations so plugin hooks precede app hooks in each phase
(§8). The context closes when `setup()` returns; registering anything
afterwards raises `RuntimeError` (§14.8).

Configuration today is a plain per-plugin mapping (`use(plugin, config)`
or keyword arguments); `Settings` sections (§20, M3.22) will bridge into
the same `ctx.config` slot — plugins already read only their own slice.
"""

from __future__ import annotations

import dataclasses
import inspect
import re
from collections.abc import Callable, Iterable, Mapping, Sequence
from types import MappingProxyType
from typing import TYPE_CHECKING, Any, Protocol, TypeVar, cast, runtime_checkable

from fulmen import openapi as _openapi
from fulmen._fulmen import resolve_plugin_order

if TYPE_CHECKING:
    from fulmen.app import Fulmen

__all__ = [
    "CheckFn",
    "CheckRule",
    "CliHandler",
    "Plugin",
    "PluginContext",
    "PluginHandle",
]

_Handler = Callable[..., object]

#: A `fulmen <command>` handler contributed by a plugin: `(app, argv)`
#: → exit code. `app` is the loaded `Fulmen`, `argv` the arguments after
#: `fulmen <command> <module:app>`.
CliHandler = Callable[["Fulmen", Sequence[str]], int]

#: A `fulmen check` rule body: `(app)` → human-readable findings.
CheckFn = Callable[["Fulmen"], Iterable[str]]

# §13.3: `scope = "app"` forbids `group.use()`; anything else applies to
# its group and children.
_PLUGIN_SCOPES = frozenset({"app", "any"})

# OpenAPI 3.1 `components` object keys (`x-*` extension keys also pass).
_COMPONENT_SECTIONS = frozenset(
    {
        "schemas",
        "responses",
        "parameters",
        "examples",
        "requestBodies",
        "headers",
        "securitySchemes",
        "links",
        "callbacks",
        "pathItems",
    }
)

# §24.1: plugin check rules namespace their code so they cannot collide
# with the built-in `FULNNN` set.
_CHECK_CODE = re.compile(r"FUL-[A-Z0-9][A-Z0-9_]*-\d{3}\Z")
_CLI_COMMAND = re.compile(r"[a-z][a-z0-9-]*\Z")


@runtime_checkable
class Plugin(Protocol):
    """The plugin contract (DESIGN.md §13.1).

    `requires` names plugins that must be set up first; `conflicts`
    names plugins that cannot be used together. `scope = "app"` forbids
    registration on a `Fulmen(prefix=…)` group (§13.3).
    """

    name: str
    version: str
    requires: Sequence[str]
    conflicts: Sequence[str]

    def setup(self, ctx: PluginContext) -> object:
        """Register the plugin's contributions; the return value becomes
        `PluginHandle.state` once the app compiles."""
        ...


PluginT = TypeVar("PluginT", bound="Plugin")


@dataclasses.dataclass(frozen=True)
class CheckRule:
    """A `fulmen check` rule contributed by a plugin (§24.1).

    `code` is `FUL-<NAME>-<NNN>` — the plugin-namespaced form. `check`
    is called with the `Fulmen` app and yields finding strings;
    `fixable` marks rules `fulmen fix` may repair. The `check` command
    itself lands with the CLI milestone; this is the registration seam
    plugins record against now."""

    code: str
    description: str
    check: CheckFn
    fixable: bool = False


class PluginHandle[PluginT: Plugin]:
    """What `app.use()` returns (§5.6, §13.1).

    Attributes the handle doesn't define fall through to the plugin
    object, so class-level surfaces used at decoration time —
    `auth.Claims` in `Annotated[auth.Claims, auth]` — work before the
    app compiles. `state` is what `setup()` returned; it exists only
    after the first compile (`run()`, `serve_for_test`, `openapi()`).
    """

    __slots__ = ("_config", "_plugin", "_settled", "_state")

    def __init__(self, plugin: PluginT, config: Mapping[str, Any]) -> None:
        self._plugin = plugin
        self._config = config
        self._state: object = None
        self._settled = False

    @property
    def plugin(self) -> PluginT:
        """The registered plugin object."""
        return self._plugin

    @property
    def name(self) -> str:
        """`plugin.name`."""
        return self._plugin.name

    @property
    def version(self) -> str:
        """`plugin.version`."""
        return self._plugin.version

    @property
    def config(self) -> Mapping[str, Any]:
        """The plugin's configuration slice (`use(plugin, config)`)."""
        return self._config

    @property
    def state(self) -> object:
        """Whatever `setup()` returned — available after the first compile."""
        if not self._settled:
            raise RuntimeError(
                f"plugin `{self.name}` has not been set up — its state exists once the app compiles"
            )
        return self._state

    def settle(self, state: object) -> None:
        """Record `setup()`'s return value — the app calls this at
        compile; plugins have no reason to call it."""
        self._state = state
        self._settled = True

    def __getattr__(self, name: str) -> Any:
        # Class-level plugin surfaces (a `Claims` type, constants) must
        # resolve at decoration time, before `setup()` has run.
        if name == "_plugin":
            raise AttributeError(name)
        return getattr(self._plugin, name)

    def __repr__(self) -> str:
        return f"<fulmen.PluginHandle {self.name} {self.version}>"


class PluginContext:
    """The extension surface handed to `Plugin.setup()` (§13.1).

    Registration methods mirror `Fulmen`'s: contributions are staged on
    the context and merged when `setup()` returns — plugin hooks land in
    each phase ahead of app-level hooks (§8), routes and mounted groups
    join under the app the plugin was used on, and the OpenAPI, CLI,
    check-rule and provider collections aggregate across the plugin set.

    The context is single-use: it closes when `setup()` returns, and any
    registration after that raises `RuntimeError` (§14.8). Registering
    *native* middleware arrives with the plugin SDK (M3.14/M3.15).
    """

    __slots__ = (
        "_app",
        "_check_rules",
        "_cli",
        "_closed",
        "_components",
        "_config",
        "_error_handlers",
        "_hooks",
        "_lifecycle",
        "_mounted",
        "_plugin_name",
        "_providers",
        "_routes",
    )

    def __init__(self, *, app: Fulmen, plugin_name: str, config: Mapping[str, Any]) -> None:
        self._app = app
        self._plugin_name = plugin_name
        self._config = MappingProxyType(dict(config))
        self._closed = False
        self._routes: list[tuple[str, str, _Handler, _openapi.OperationMeta]] = []
        self._hooks: list[tuple[str, _Handler, bool, str, int]] = []
        self._error_handlers: list[tuple[type[BaseException], _Handler, bool]] = []
        self._lifecycle: list[tuple[str, _Handler, bool]] = []
        self._mounted: list[Fulmen] = []
        self._providers: list[object] = []
        self._components: dict[str, dict[str, Any]] = {}
        self._cli: dict[str, CliHandler] = {}
        self._check_rules: list[CheckRule] = []

    @property
    def app(self) -> Fulmen:
        """The application (or route group) the plugin was `use()`d on."""
        return self._app

    @property
    def plugin_name(self) -> str:
        """The registering plugin's `name`."""
        return self._plugin_name

    @property
    def config(self) -> Mapping[str, Any]:
        """The plugin's configuration slice — `use(plugin, config)` plus
        keyword arguments. When `Settings` lands (M3.22), the app's
        `[tool.fulmen.<name>]` section feeds this same mapping."""
        return self._config

    @property
    def providers(self) -> list[object]:
        """Provider declarations for typed injection (§12, §13.1).

        The DI task (M3.10) consumes this collection; until then it is
        the documented seam: plugins append provider declarations —
        `add_provider` returns its argument for decorator use."""
        return self._providers

    @property
    def staged(self) -> StagedContributions:
        """Everything the plugin registered so far — the app's compile
        machinery reads this; plugins have no reason to call it."""
        return StagedContributions(
            owner=self._app,
            hooks=self._hooks,
            error_handlers=self._error_handlers,
            lifecycle=self._lifecycle,
            routes=self._routes,
            mounted=self._mounted,
            providers=self._providers,
            components=self._components,
            cli=self._cli,
            check_rules=self._check_rules,
        )

    # -- lifecycle hooks (§8) -------------------------------------------------

    def add_hook(
        self,
        phase: str,
        fn: _Handler | None = None,
        *,
        priority: int | None = None,
    ) -> Callable[[_Handler], _Handler] | _Handler:
        """Register `fn` in lifecycle `phase`, like `Fulmen.add_hook`.

        Plugin hooks precede app-level hooks in the phase's `(priority,
        registration order)` sort (§8); `priority` defaults to
        `PRIORITY_USER` and security plugins pass a band below it.
        """
        from fulmen.app import HOOK_PHASES, PRIORITY_USER

        self._ensure_open()
        if phase not in HOOK_PHASES:
            names = ", ".join(HOOK_PHASES)
            raise ValueError(f"unknown hook phase {phase!r} (expected one of: {names})")
        band = PRIORITY_USER if priority is None else priority

        def decorator(hook: _Handler) -> _Handler:
            self._ensure_open()
            short = getattr(hook, "__name__", None) or getattr(hook, "__qualname__", "<hook>")
            name = f"{self._plugin_name}:{short}"
            self._hooks.append((phase, hook, inspect.iscoroutinefunction(hook), name, band))
            return hook

        return decorator(fn) if fn is not None else decorator

    def on_request(self, fn: _Handler | None = None):
        """`ctx.on_request` — before any routing work (§8)."""
        return self.add_hook("on_request", fn)

    def before_route(self, fn: _Handler | None = None):
        """`ctx.before_route` — once the route is known."""
        return self.add_hook("before_route", fn)

    def guard(self, fn: _Handler | None = None):
        """`ctx.guard` — allow/deny checks (rate limit, auth, CSRF)."""
        return self.add_hook("guard", fn)

    def resolve(self, fn: _Handler | None = None):
        """`ctx.resolve` — dependency resolution (§8)."""
        return self.add_hook("resolve", fn)

    def before_handle(self, fn: _Handler | None = None):
        """`ctx.before_handle` — immediately before the handler."""
        return self.add_hook("before_handle", fn)

    def after_handle(self, fn: _Handler | None = None):
        """`ctx.after_handle` — after the handler; may replace the response."""
        return self.add_hook("after_handle", fn)

    def on_response(self, fn: _Handler | None = None):
        """`ctx.on_response` — post-encode header mutation over any response."""
        return self.add_hook("on_response", fn)

    def after_response(self, fn: _Handler | None = None):
        """`ctx.after_response` — fire-and-forget access log/metrics."""
        return self.add_hook("after_response", fn)

    def on_startup(self, fn: _Handler | None = None):
        """`ctx.on_startup` — run after a successful compile, before bind (§5.7)."""
        return self._lifecycle_decorator("startup", fn)

    def on_shutdown(self, fn: _Handler | None = None):
        """`ctx.on_shutdown` — run after the server stops (§5.7)."""
        return self._lifecycle_decorator("shutdown", fn)

    def _lifecycle_decorator(
        self, kind: str, fn: _Handler | None
    ) -> Callable[[_Handler], _Handler] | _Handler:
        def decorator(hook: _Handler) -> _Handler:
            self._ensure_open()
            self._lifecycle.append((kind, hook, inspect.iscoroutinefunction(hook)))
            return hook

        return decorator(fn) if fn is not None else decorator

    # -- routes, groups and error handlers (§5.1, §5.5, §17) -------------------

    def route(
        self,
        path: str,
        methods: Iterable[str] = ("GET",),
        **options: Any,
    ) -> Callable[[_Handler], _Handler]:
        """Register a route, exactly like `Fulmen.route` (§5.1)."""
        self._ensure_open()
        meta = _openapi.operation_meta(options)

        def decorator(handler: _Handler) -> _Handler:
            self._ensure_open()
            from fulmen import _di

            _di.stamp_localns(handler)
            for method in methods:
                self._routes.append((method.upper(), path, handler, meta))
            return handler

        return decorator

    def get(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `GET path`."""
        return self.route(path, methods=("GET",), **options)

    def post(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `POST path`."""
        return self.route(path, methods=("POST",), **options)

    def put(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PUT path`."""
        return self.route(path, methods=("PUT",), **options)

    def patch(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PATCH path`."""
        return self.route(path, methods=("PATCH",), **options)

    def delete(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `DELETE path`."""
        return self.route(path, methods=("DELETE",), **options)

    def head(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `HEAD path`."""
        return self.route(path, methods=("HEAD",), **options)

    def options(self, path: str, **options: Any) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `OPTIONS path`."""
        return self.route(path, methods=("OPTIONS",), **options)

    def mount(self, app: Fulmen) -> Fulmen:
        """Mount a `Fulmen(prefix=…)` group as a child of the app the
        plugin was used on — the group's hooks and `on_error` handlers
        scope to it exactly like `Fulmen.mount` (§5.5)."""
        from fulmen.app import Fulmen

        self._ensure_open()
        # Runtime validation for untyped callers (pyright sees the
        # annotation as proof enough).
        if not isinstance(app, Fulmen):  # pyright: ignore[reportUnnecessaryIsInstance]
            raise TypeError(f"mount expects a Fulmen application, got {type(app).__qualname__}")
        self._mounted.append(app)
        return app

    def on_error(self, exc_class: type[BaseException]) -> Callable[[_Handler], _Handler]:
        """Register an exception handler, like `Fulmen.on_error` (§17)."""
        self._ensure_open()
        # Runtime validation for untyped callers (pyright sees the
        # annotation as proof enough).
        if not (isinstance(exc_class, type) and issubclass(exc_class, BaseException)):  # pyright: ignore[reportUnnecessaryIsInstance]
            raise TypeError(f"on_error expects an exception class, got {exc_class!r}")

        def decorator(handler: _Handler) -> _Handler:
            self._ensure_open()
            self._error_handlers.append((exc_class, handler, inspect.iscoroutinefunction(handler)))
            return handler

        return decorator

    # -- providers seam (§12, M3.10) -------------------------------------------

    def add_provider(self, provider: object) -> object:
        """Append a provider declaration to `ctx.providers`; returns it
        for decorator use (`@ctx.add_provider`).

        The declaration's type hints resolve against `setup()`'s locals
        (`Annotated[T, name]` works for providers defined inside setup)."""
        from fulmen import _di

        self._ensure_open()
        _di.stamp_localns(provider)
        self._providers.append(provider)
        return provider

    # -- OpenAPI components (§13.1) ----------------------------------------------

    def add_openapi_component(self, section: str, name: str, component: Mapping[str, Any]) -> None:
        """Contribute `components/<section>/<name>` to the app's OpenAPI
        document — security schemes, shared schemas, reusable parameters.

        `section` is an OpenAPI 3.1 components key (`securitySchemes`,
        `schemas`, `headers`, …) or an `x-*` extension key. Declaring the
        same `(section, name)` twice — here or across plugins — is a
        startup error, as is colliding with a generated component."""
        self._ensure_open()
        if section not in _COMPONENT_SECTIONS and not section.startswith("x-"):
            raise ValueError(
                f"unknown OpenAPI components section {section!r} "
                f"(expected one of: {', '.join(sorted(_COMPONENT_SECTIONS))}, or `x-*`)"
            )
        entries = self._components.setdefault(section, {})
        if name in entries:
            raise ValueError(
                f"plugin `{self._plugin_name}` redeclares OpenAPI component `{section}.{name}`"
            )
        entries[name] = dict(component)

    # -- CLI commands (§24) ------------------------------------------------------

    def cli_command(
        self, name: str, handler: CliHandler | None = None
    ) -> Callable[[CliHandler], CliHandler] | CliHandler:
        """Register `fulmen <name> <module:app> [args]` → `handler(app, argv)`.

        `name` is a lowercase command word; `handler` returns the exit
        code. Usable directly or as a decorator."""

        def decorator(fn: CliHandler) -> CliHandler:
            self._ensure_open()
            self._cli[name] = fn
            return fn

        self._ensure_open()
        if not _CLI_COMMAND.fullmatch(name):
            raise ValueError(
                f"invalid plugin command name {name!r} (expected a lowercase word like `jwt-keys`)"
            )
        from fulmen.cli import BUILTIN_COMMANDS

        if name in BUILTIN_COMMANDS:
            raise ValueError(f"plugin command name {name!r} collides with a built-in command")
        if name in self._cli:
            raise ValueError(f"plugin `{self._plugin_name}` redeclares CLI command `{name}`")
        return decorator(handler) if handler is not None else decorator

    # -- check rules (§24.1) ----------------------------------------------------

    def check_rule(
        self, code: str, description: str = "", *, fixable: bool = False
    ) -> Callable[[CheckFn], CheckFn]:
        """Register a `fulmen check` rule as a decorator (§24.1).

        `code` is the plugin-namespaced `FUL-<NAME>-<NNN>` form; the
        decorated function takes `(app)` and yields finding strings."""
        self._ensure_open()
        if not _CHECK_CODE.fullmatch(code):
            raise ValueError(
                f"invalid check-rule code {code!r} "
                "(expected `FUL-<NAME>-<NNN>`, e.g. `FUL-JWT-001`)"
            )

        def decorator(fn: CheckFn) -> CheckFn:
            self._ensure_open()
            text = description or inspect.getdoc(fn) or ""
            self._check_rules.append(
                CheckRule(code=code, description=text, check=fn, fixable=fixable)
            )
            return fn

        return decorator

    # -- internals ---------------------------------------------------------------

    def _ensure_open(self) -> None:
        """§14.8: contributions register inside `setup()` only."""
        if self._closed:
            raise RuntimeError(
                f"plugin `{self._plugin_name}`'s context is closed — "
                "contributions are registered inside setup() only"
            )

    def close(self) -> None:
        """Seal the context — the app calls this when `setup()` returns."""
        self._closed = True


@dataclasses.dataclass(frozen=True)
class StagedContributions:
    """Snapshot of one plugin's `PluginContext` registrations —
    package-internal; consumed by `Fulmen` compile machinery and the
    plugin plan, never by plugins themselves."""

    owner: Fulmen
    hooks: list[tuple[str, _Handler, bool, str, int]]
    error_handlers: list[tuple[type[BaseException], _Handler, bool]]
    lifecycle: list[tuple[str, _Handler, bool]]
    routes: list[tuple[str, str, _Handler, _openapi.OperationMeta]]
    mounted: list[Fulmen]
    providers: list[object]
    components: dict[str, dict[str, Any]]
    cli: dict[str, CliHandler]
    check_rules: list[CheckRule]


# -- plan machinery (used by `Fulmen._prepare_plugins`) ---------------------------


@dataclasses.dataclass(frozen=True)
class PluginMeta:
    """The validated contract fields of one registered plugin."""

    name: str
    version: str
    requires: tuple[str, ...]
    conflicts: tuple[str, ...]
    app_only: bool


@dataclasses.dataclass
class PluginUse:
    """One `use()` call awaiting resolution — package-internal."""

    owner: Fulmen
    plugin: Any
    handle: PluginHandle[Any]
    meta: PluginMeta


@dataclasses.dataclass
class PluginPlan:
    """Everything one plugin-resolution pass produced (§6.4.2) —
    package-internal.

    `entries` is the emitted `(use, ctx)` list in setup order — the
    order hooks replay in, so plugin hooks precede app hooks (§8).
    `extra_mounts` maps `id(owner app)` to the groups plugins mounted,
    which `Fulmen._walk` treats as extra children. The remaining
    collections aggregate cross-plugin registries; `problems` collects
    every startup error found (§14.1)."""

    entries: list[tuple[PluginUse, PluginContext]] = dataclasses.field(
        default_factory=list[tuple[PluginUse, PluginContext]]
    )
    problems: list[str] = dataclasses.field(default_factory=list[str])
    extra_mounts: dict[int, list[Fulmen]] = dataclasses.field(
        default_factory=dict[int, list["Fulmen"]]
    )
    components: dict[str, dict[str, Any]] = dataclasses.field(
        default_factory=dict[str, dict[str, Any]]
    )
    cli_commands: dict[str, CliHandler] = dataclasses.field(default_factory=dict[str, CliHandler])
    check_rules: list[CheckRule] = dataclasses.field(default_factory=list[CheckRule])
    providers: list[object] = dataclasses.field(default_factory=list[object])

    def route_decls(self) -> dict[int, list[tuple[str, str, _Handler, _openapi.OperationMeta]]]:
        """Plugin-declared routes keyed by `id(owner app)`, in setup order."""
        out: dict[int, list[tuple[str, str, _Handler, _openapi.OperationMeta]]] = {}
        for _use, ctx in self.entries:
            staged = ctx.staged
            if staged.routes:
                out.setdefault(id(staged.owner), []).extend(staged.routes)
        return out

    def finish(self) -> None:
        """Aggregate the cross-plugin registries; same-name contributions
        from different plugins are startup problems."""
        cli_owners: dict[str, str] = {}
        component_owners: dict[tuple[str, str], str] = {}
        rule_owners: dict[str, str] = {}
        for use, ctx in self.entries:
            staged = ctx.staged
            for section, entries in staged.components.items():
                target = self.components.setdefault(section, {})
                for name, component in entries.items():
                    key = (section, name)
                    if key in component_owners:
                        self.problems.append(
                            f"plugins `{component_owners[key]}` and `{use.meta.name}` both "
                            f"declare OpenAPI component `{section}.{name}`"
                        )
                    else:
                        component_owners[key] = use.meta.name
                        target[name] = component
            for name, handler in staged.cli.items():
                if name in cli_owners:
                    self.problems.append(
                        f"plugins `{cli_owners[name]}` and `{use.meta.name}` both "
                        f"register CLI command `{name}`"
                    )
                else:
                    cli_owners[name] = use.meta.name
                    self.cli_commands[name] = handler
            for rule in staged.check_rules:
                if rule.code in rule_owners:
                    self.problems.append(
                        f"plugins `{rule_owners[rule.code]}` and `{use.meta.name}` both "
                        f"register check rule `{rule.code}`"
                    )
                else:
                    rule_owners[rule.code] = use.meta.name
                    self.check_rules.append(rule)
            self.providers.extend(staged.providers)


def plugin_meta(plugin: object) -> PluginMeta:
    """Read and validate the §13.1 contract fields (`use()` calls this
    eagerly so malformed plugins fail at registration, not compile)."""
    label = type(plugin).__qualname__
    name = getattr(plugin, "name", None)
    if not isinstance(name, str) or not name:
        raise TypeError(f"plugin {label} must declare a non-empty `name: str`")
    version = getattr(plugin, "version", None)
    if not isinstance(version, str) or not version:
        raise TypeError(f"plugin `{name}` must declare a non-empty `version: str`")
    if not callable(getattr(plugin, "setup", None)):
        raise TypeError(f"plugin `{name}` must define `setup(ctx)`")
    requires = _name_tuple(getattr(plugin, "requires", ()), name, "requires")
    conflicts = _name_tuple(getattr(plugin, "conflicts", ()), name, "conflicts")
    scope = getattr(plugin, "scope", "any")
    if scope not in _PLUGIN_SCOPES:
        raise ValueError(
            f'plugin `{name}` declares unknown scope {scope!r} (expected "app" or "any")'
        )
    return PluginMeta(
        name=name,
        version=version,
        requires=requires,
        conflicts=conflicts,
        app_only=scope == "app",
    )


def _name_tuple(value: object, name: str, field_name: str) -> tuple[str, ...]:
    """Normalize a `requires`/`conflicts` sequence to a validated tuple."""
    if value is None:
        return ()
    if isinstance(value, str) or not isinstance(value, Iterable):
        raise TypeError(f"plugin `{name}`: `{field_name}` must be a sequence of plugin names")
    out: list[str] = []
    for item in cast(Iterable[object], value):
        if not isinstance(item, str) or not item:
            raise TypeError(f"plugin `{name}`: `{field_name}` must be a sequence of plugin names")
        out.append(item)
    return tuple(out)


def resolve_uses(uses: Sequence[PluginUse]) -> tuple[list[int], list[str]]:
    """`(order, problems)` — the native resolver's answer for the
    registered set (§13.2)."""
    decls = [(use.meta.name, list(use.meta.requires), list(use.meta.conflicts)) for use in uses]
    return resolve_plugin_order(decls)


def order_integrity(entries: Sequence[tuple[PluginUse, PluginContext]]) -> list[str]:
    """Verify the emitted setup order honours every `requires` edge.

    A plugin used inside a plugin-mounted group always runs *after* the
    plugins that mounted it, so a `requires` pointing at one is a real
    ordering violation worth reporting by name."""
    positions: dict[str, int] = {}
    emitted = [use for use, _ctx in entries]
    for pos, use in enumerate(emitted):
        positions.setdefault(use.meta.name, pos)
    problems: list[str] = []
    for use in emitted:
        mine = positions[use.meta.name]
        for required in use.meta.requires:
            theirs = positions.get(required)
            if theirs is not None and theirs > mine:
                problems.append(
                    f"plugin `{use.meta.name}` requires `{required}`, which runs later — "
                    "it was registered inside a plugin-mounted group"
                )
    return problems


def merge_components(
    document: dict[str, Any], components: Mapping[str, Mapping[str, Any]]
) -> list[str]:
    """Fold plugin-declared OpenAPI components into `document`.

    Collisions with generated components (`components/schemas/<Model>`)
    are returned as problem strings — the caller turns them into the
    same aggregated `StartupError` as every other declaration problem."""
    problems: list[str] = []
    if not components:
        return problems
    target_all = document.setdefault("components", {})
    for section in sorted(components):
        target = target_all.setdefault(section, {})
        for name in sorted(components[section]):
            if name in target:
                problems.append(
                    f"plugin-declared OpenAPI component `{section}.{name}` "
                    "collides with a generated component"
                )
            else:
                target[name] = components[section][name]
    return problems
