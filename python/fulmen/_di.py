"""Dependency injection (DESIGN.md §12) — provider declarations, the
startup-validated dependency graph, and request-time resolution.

A *provider* is a callable (or constant) registered for injection into
handlers and other providers. Handlers inject it through
`Annotated[T, provider]`; providers depend on other providers the same
way, on a bare `T` when exactly one registered provider produces that
type, or on `Request` for the request object itself.

Scopes (§12.1): `APP` resolves once at startup (a synthetic startup
lifecycle hook), `REQUEST` once per request (cached on `request.state`),
`TRANSIENT` fresh at every injection site. Generator providers yield
their value and clean up in reverse resolution order — request-context
generators through a per-route `after_response` hook on a private scope,
application-context generators through a synthetic shutdown hook.

The graph is built at compile (`Fulmen._provider_graph` +
`Graph.validate`): cycles, missing providers, scope violations
(request-context values under application scope), async providers
reachable from sync handlers, and unused `app.override` targets all
aggregate into `StartupError` — nothing defers to the first request.

Resolution runs inside the handler's own executor job: the registered
callable is a wrapper that resolves the DI parameters and calls the
handler — no extra Python crossings, sync providers on the sync pool,
async ones on the asyncio loop, provider exceptions routing through the
route's `on_error` table like any handler failure.
"""

from __future__ import annotations

import contextlib
import enum
import functools
import inspect
import typing
from collections.abc import (
    AsyncGenerator,
    AsyncIterable,
    AsyncIterator,
    Callable,
    Generator,
    Iterable,
    Iterator,
)
from typing import Annotated, Any, get_args, get_origin, overload

from fulmen import plugins as _plugins
from fulmen.constraints import UNSET
from fulmen.request import Request

__all__ = ["Provider", "Scope", "provider"]

# `request.state` key holding the per-request resolution scope. Hooks may
# merge arbitrary keys into `state`; the name is deliberately unlikely
# to collide with application keys.
_STATE_KEY = "__fulmen_di__"

# The spec argument the DI wrapper binds the request object through —
# a `request`-sourced named argument whose wire name can't collide with
# a real handler parameter.
REQUEST_CALL_ARG: tuple[str, str, str, str, None, tuple[str, object]] = (
    "__fulmen_request__",
    "request",
    "normal",
    "__fulmen_request__",
    None,
    ("omit", None),
)

#: The private-scope `after_response` cleanup hook runs after every other
#: hook — user hooks may still read request-scoped provider values.
CLEANUP_PRIORITY = 65_535

_YIELD_ORIGINS = frozenset(
    {
        Iterator,
        AsyncIterator,
        Generator,
        AsyncGenerator,
        Iterable,
        AsyncIterable,
    }
)


def frame_locals(depth: int) -> dict[str, Any] | None:
    """A snapshot of the frame `depth` calls above this function's
    caller — used so `Annotated[T, local_name]` resolves when a
    declaration happens inside a function (tests, plugin `setup()`).
    The dict is copied: no frame is retained."""
    frame = inspect.currentframe()
    for _ in range(depth + 1):
        frame = frame.f_back if frame is not None else None
    return dict(frame.f_locals) if frame is not None else None


def stamp_localns(obj: Any) -> None:
    """Attach the declaring frame's locals to `obj` for hint resolution —
    a no-op on objects that cannot carry attributes (`Provider` captures
    its own `localns` at construction)."""
    localns = frame_locals(2)
    with contextlib.suppress(AttributeError):
        obj.__fulmen_localns__ = localns


def localns_of(obj: Any) -> dict[str, Any] | None:
    """The declaring frame's locals captured for `obj`, if any."""
    return getattr(obj, "localns", None) or getattr(obj, "__fulmen_localns__", None)


class Scope(enum.Enum):
    """A provider's lifetime and caching semantics (DESIGN.md §12.1)."""

    #: Once per application, resolved at startup.
    APP = "app"
    #: Once per request, cached within that request.
    REQUEST = "request"
    #: A fresh instance at every injection site.
    TRANSIENT = "transient"


class Provider[T]:
    """A provider declaration — the `Annotated` marker handlers use (§12.1).

    `@provider` produces these; `app.provide(...)` returns them for
    registration. `provider.value(obj)` wraps a constant (always
    application-scoped). A `Provider` is callable: it delegates to the
    wrapped function so tests can invoke it directly.
    """

    __slots__ = ("_const", "constant", "fn", "localns", "name", "provides", "scope")

    def __init__(
        self,
        fn: Callable[..., Any] | None,
        /,
        *,
        scope: Scope | str = Scope.REQUEST,
        provides: Any = UNSET,
        name: str | None = None,
        _const: bool = False,
        _value: Any = None,
        _localns: dict[str, Any] | None = None,
    ) -> None:
        if isinstance(scope, str):
            try:
                scope = Scope(scope.lower())
            except ValueError:
                known = ", ".join(s.value for s in Scope)
                raise ValueError(f"unknown provider scope {scope!r} (expected {known})") from None
        if not isinstance(scope, Scope):  # pyright: ignore[reportUnnecessaryIsInstance] -- runtime validation for untyped callers
            raise TypeError(f"provider scope must be a Scope or str, got {scope!r}")
        self.fn = fn
        self.scope = scope
        self.provides = provides
        self._const = _const
        self.constant = _value
        self.localns = _localns if _localns is not None else frame_locals(1)
        self.name = name or getattr(fn, "__qualname__", None) or type(fn).__qualname__

    @classmethod
    def value(
        cls,
        obj: T,
        *,
        provides: Any = UNSET,
        name: str | None = None,
        _localns: dict[str, Any] | None = None,
    ) -> Provider[T]:
        """A provider that resolves to `obj` — constants are app-scoped."""
        return cls(
            None,
            scope=Scope.APP,
            provides=provides if provides is not UNSET else type(obj),
            name=name or f"{type(obj).__qualname__}()",
            _const=True,
            _value=obj,
            _localns=_localns if _localns is not None else frame_locals(1),
        )

    @property
    def const(self) -> bool:
        """Whether this provider resolves to a fixed value."""
        return self._const

    def __call__(self, *args: Any, **kwargs: Any) -> T:
        if self._const:
            return self.constant
        if self.fn is None:
            raise TypeError("this Provider has no callable")
        return self.fn(*args, **kwargs)

    def __repr__(self) -> str:
        if self._const:
            return f"<fulmen.Provider value {self.constant!r}>"
        return f"<fulmen.Provider {self.name} scope=Scope.{self.scope.name}>"


@overload
def provider[T](fn: Callable[..., T], /) -> Provider[T]: ...


@overload
def provider[T](
    fn: None = None,
    /,
    *,
    scope: Scope | str = ...,
    provides: Any = ...,
    name: str | None = ...,
) -> Callable[[Callable[..., T]], Provider[T]]: ...


def provider(
    fn: Callable[..., Any] | None = None,
    /,
    *,
    scope: Scope | str = Scope.REQUEST,
    provides: Any = UNSET,
    name: str | None = None,
) -> Provider[Any] | Callable[[Callable[..., Any]], Provider[Any]]:
    """Declare a provider for dependency injection (DESIGN.md §12.1).

    ```python
    @provider(scope=Scope.APP)
    async def database() -> AsyncIterator[Database]:
        db = await Database.connect(...)
        yield db
        await db.close()
    ```

    Usable bare (`@provider` — request scope), with arguments, or as a
    direct call (`provider(fn, scope=…)`). `provides` overrides the type
    the provider registers under for bare-`T` lookups; by default the
    return annotation is used (`Iterator[T]`/`AsyncIterator[T]` unwrap
    to `T`). Generator/async-generator providers clean up after the
    response — or at shutdown when resolved in application scope.
    """
    # The declaring frame's locals — captured at the `provider(...)` call
    # so `Annotated[T, name]` resolves when the provider is defined inside
    # a function.
    localns = frame_locals(1)

    def decorate(f: Callable[..., Any]) -> Provider[Any]:
        return Provider[Any](f, scope=scope, provides=provides, name=name, _localns=localns)

    return decorate(fn) if fn is not None else decorate


def as_provider(
    decl: Any,
    *,
    scope: Scope | str | None = None,
    provides: Any = UNSET,
    name: str | None = None,
    _localns: dict[str, Any] | None = None,
) -> Provider[Any]:
    """Normalize one `app.provide(...)` argument to a `Provider`."""
    if isinstance(decl, Provider):
        if scope is not None or provides is not UNSET or name is not None:
            raise ValueError(
                "a Provider declaration already carries scope/provides/name — "
                "pass them to @provider instead"
            )
        return typing.cast(Provider[Any], decl)
    if callable(decl):
        return Provider[Any](
            decl,
            scope=Scope.REQUEST if scope is None else scope,
            provides=provides,
            name=name,
            _localns=_localns,
        )
    if scope is not None:
        raise ValueError("value providers are always application-scoped")
    return Provider[Any].value(decl, provides=provides, name=name, _localns=_localns)


def _peel(hint: Any) -> tuple[Any, list[Any]]:
    """`Annotated[T, m1, m2]` → `(T, [m1, m2])`; anything else → `(hint, [])`."""
    if get_origin(hint) is Annotated:
        args = get_args(hint)
        return args[0], list(args[1:])
    return hint, []


def _provides_of(fn: Callable[..., Any] | None, localns: dict[str, Any] | None = None) -> Any:
    """The type `fn` registers under: `-> Iterator[T]` unwraps to `T`,
    a class registers as itself, no annotation registers nothing."""
    if fn is None:
        return None
    if isinstance(fn, type):
        return fn
    try:
        declared = typing.get_type_hints(fn, localns=localns).get("return")
    except Exception:  # noqa: BLE001 -- a hint failure resurfaces at dep parsing with context
        return None
    if declared is None:
        return None
    base, _markers = _peel(declared)
    origin = get_origin(base)
    if origin in _YIELD_ORIGINS:
        args = get_args(base)
        return args[0] if args else None
    if base is None or base is type(None):
        return None
    return base


def _name_of(marker: Any) -> str:
    return getattr(marker, "__qualname__", None) or repr(marker)


def _type_name(hint: Any) -> str:
    return getattr(hint, "__qualname__", None) or str(hint)


def _provides_match(provides: Any, base: Any) -> bool:
    """Whether an entry providing `provides` satisfies a lookup for `base`."""
    try:
        if provides == base:
            return True
    except Exception:  # noqa: BLE001 -- arbitrary annotation objects compare unpredictably
        return False
    try:
        return isinstance(provides, type) and isinstance(base, type) and issubclass(provides, base)
    except TypeError:
        return False


class Dep(typing.NamedTuple):
    """One provider parameter, resolved at graph validation."""

    name: str
    kind: str  # "provider" | "request" | "type"
    key: Any  # "type" deps carry the wanted type here until resolved
    position: int | None  # signature index when positional-only


class Entry:
    """One normalized provider registration, keyed by its marker object."""

    __slots__ = (
        "const",
        "deps",
        "fn",
        "gen",
        "is_async",
        "label",
        "localns",
        "provides",
        "scope",
        "value",
    )

    def __init__(
        self,
        *,
        label: str,
        scope: Scope,
        fn: Callable[..., Any] | None,
        gen: str,
        is_async: bool,
        provides: Any,
        const: bool,
        value: Any,
        localns: dict[str, Any] | None,
    ) -> None:
        self.label = label
        self.scope = scope
        self.fn = fn
        self.gen = gen  # "none" | "sync" | "async"
        self.is_async = is_async
        self.provides = provides
        self.const = const
        self.value = value
        self.localns = localns
        self.deps: list[Dep] = []


class RouteDi:
    """One route's DI record: the DI parameter bindings plus the cleanup
    lane (`"sync"`/`"async"`/`None`) `Graph.validate` computes."""

    __slots__ = ("bindings", "cleanup_lane")

    def __init__(self) -> None:
        self.bindings: list[tuple[str, Any, int | None]] = []
        self.cleanup_lane: str | None = None


class _RequestScope:
    """Per-request resolution context, stored on `request.state`.

    `values` is the REQUEST-scope cache; `cleanups` collects generator
    teardown thunks in resolution order (the after-response hook runs
    them reversed)."""

    __slots__ = ("cleanups", "graph", "request", "values")

    def __init__(self, graph: Graph, request: Request) -> None:
        self.graph = graph
        self.request = request
        self.values: dict[Any, Any] = {}
        self.cleanups: list[Callable[[], Any]] = []

    def resolve(self, key: Any) -> Any:
        entry = self.graph.entries[key]
        if entry.scope is Scope.APP:
            return self.graph.app_values[key]
        if key in self.values:
            return self.values[key]
        value = self._produce(entry)
        if entry.scope is Scope.REQUEST:
            self.values[key] = value
        return value

    def _produce(self, entry: Entry) -> Any:
        if entry.const:
            return entry.value
        pos, kw = self._dep_args(entry)
        if entry.gen == "sync":
            generator = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.cleanups.append(generator.close)
            try:
                return next(generator)
            except StopIteration:
                self.cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        return entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]

    def _dep_args(self, entry: Entry) -> tuple[list[Any], dict[str, Any]]:
        pos: list[Any] = []
        kw: dict[str, Any] = {}
        for dep in entry.deps:
            value = self.request if dep.kind == "request" else self.resolve(dep.key)
            if dep.position is None:
                kw[dep.name] = value
            else:
                pos.insert(dep.position, value)
        return pos, kw

    async def aresolve(self, key: Any) -> Any:
        entry = self.graph.entries[key]
        if entry.scope is Scope.APP:
            return self.graph.app_values[key]
        if key in self.values:
            return self.values[key]
        value = await self._aproduce(entry)
        if entry.scope is Scope.REQUEST:
            self.values[key] = value
        return value

    async def _aproduce(self, entry: Entry) -> Any:
        if entry.const:
            return entry.value
        pos, kw = await self._adep_args(entry)
        if entry.gen == "async":
            agen = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.cleanups.append(agen.aclose)
            try:
                return await agen.__anext__()
            except StopAsyncIteration:
                self.cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        if entry.gen == "sync":
            generator = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.cleanups.append(generator.close)
            try:
                return next(generator)
            except StopIteration:
                self.cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        result = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
        if inspect.isawaitable(result):
            return await result
        return result

    async def _adep_args(self, entry: Entry) -> tuple[list[Any], dict[str, Any]]:
        pos: list[Any] = []
        kw: dict[str, Any] = {}
        for dep in entry.deps:
            value = self.request if dep.kind == "request" else await self.aresolve(dep.key)
            if dep.position is None:
                kw[dep.name] = value
            else:
                pos.insert(dep.position, value)
        return pos, kw


def _request_scope(graph: Graph, request: Request) -> _RequestScope:
    """The request's resolution scope, creating it on first use."""
    scope = request.state.get(_STATE_KEY)
    if not isinstance(scope, _RequestScope):
        scope = _RequestScope(graph, request)
        request.state[_STATE_KEY] = scope
    return scope


def _cleanup_request(request: Request, _response: object) -> None:
    """The private-scope `after_response` hook for routes whose DI
    closure can only produce synchronous generators."""
    scope = request.state.pop(_STATE_KEY, None)
    if isinstance(scope, _RequestScope):
        for close in reversed(scope.cleanups):
            close()


async def _acleanup_request(request: Request, _response: object) -> None:
    """The async variant — closes async generators with `aclose()`."""
    scope = request.state.pop(_STATE_KEY, None)
    if isinstance(scope, _RequestScope):
        for close in reversed(scope.cleanups):
            result = close()
            if inspect.isawaitable(result):
                await result


class Graph:
    """The provider registry and validated dependency graph for one
    compile (§12.2). `Fulmen._provider_graph` feeds it the declared
    providers and overrides; `_inspect_handler` feeds handler injection
    markers; `validate()` then checks the whole graph and prepares the
    startup/shutdown lifecycle hooks."""

    def __init__(self) -> None:
        self.entries: dict[Any, Entry] = {}
        self.provided: dict[Any, list[Any]] = {}
        self.plugin_providers: dict[str, list[Any]] = {}
        self.known_plugins: set[str] = set()
        self.override_map: dict[Any, Any] = {}
        self.problems: list[str] = []
        self.routes: list[tuple[str, bool, RouteDi]] = []
        self.app_values: dict[Any, Any] = {}
        self.app_cleanups: list[Callable[[], Any]] = []
        self._app_order: list[Any] = []
        self._app_ctx: set[Any] = set()
        self.startup_hook: Callable[[], Any] | None = None
        self.shutdown_hook: Callable[[], Any] | None = None
        self.startup_is_async = False
        self.shutdown_is_async = False

    # -- declarations ---------------------------------------------------

    def declare(self, decl: Any, owner: str | None = None) -> None:
        """Register one provider declaration (`app.provide`, or
        `ctx.add_provider` with `owner` = the plugin's name)."""
        try:
            fresh = decl not in self.entries
        except TypeError:
            self.problems.append(f"provider declaration {decl!r} is not hashable")
            return
        if fresh:
            self.entries[decl] = self._normalize(decl, None)
            self._register_provides(decl)
        if owner is not None:
            keys = self.plugin_providers.setdefault(owner, [])
            if decl not in keys:
                keys.append(decl)

    def _register_provides(self, key: Any) -> None:
        provides = self.entries[key].provides
        if provides is None:
            return
        try:
            keys = self.provided.setdefault(provides, [])
        except TypeError:
            return
        if key not in keys:
            keys.append(key)

    def _normalize(self, decl: Any, scope_hint: Scope | None) -> Entry:
        """`(decl, default-scope)` → a graph [`Entry`]: `Provider`s keep
        their own scope, plain callables take `scope_hint` or REQUEST,
        anything else is an application-scoped constant."""
        if isinstance(decl, Provider):
            if decl.const:
                return Entry(
                    label=decl.name,
                    scope=Scope.APP,
                    fn=None,
                    gen="none",
                    is_async=False,
                    provides=decl.provides if decl.provides is not UNSET else type(decl.constant),
                    const=True,
                    value=decl.constant,
                    localns=decl.localns,
                )
            return self._fn_entry(decl.fn, decl.scope, decl.provides, decl.name, decl.localns)
        if callable(decl):
            return self._fn_entry(
                decl, scope_hint or Scope.REQUEST, UNSET, _name_of(decl), localns_of(decl)
            )
        return Entry(
            label=f"{type(decl).__qualname__}()",
            scope=Scope.APP,
            fn=None,
            gen="none",
            is_async=False,
            provides=type(decl),
            const=True,
            value=decl,
            localns=None,
        )

    def _fn_entry(
        self,
        fn: Callable[..., Any] | None,
        scope: Scope,
        provides: Any,
        name: str | None,
        localns: dict[str, Any] | None,
    ) -> Entry:
        gen = (
            "async"
            if inspect.isasyncgenfunction(fn)
            else "sync"
            if inspect.isgeneratorfunction(fn)
            else "none"
        )
        return Entry(
            label=name or _name_of(fn),
            scope=scope,
            fn=fn,
            gen=gen,
            is_async=gen == "async" or inspect.iscoroutinefunction(fn),
            provides=provides if provides is not UNSET else _provides_of(fn, localns),
            const=False,
            value=None,
            localns=localns,
        )

    # -- marker resolution -------------------------------------------------

    def marker_of(self, hint: Any) -> tuple[Any, Any] | None:
        """`(base, marker)` when an annotation carries a provider marker.

        The marker is a `Provider`, a `PluginHandle`, an object already
        registered (`ctx.add_provider`/`app.provide`), or a plain
        function — which resolves at `key_of` time into either a
        registered provider or a "not registered" problem."""
        _base, markers = _peel(hint)
        if not markers:
            return None
        picked = self._pick_marker(markers)
        if picked is None:
            return None
        return _base, picked

    def _pick_marker(self, markers: list[Any]) -> Any:
        for marker in markers:
            if isinstance(marker, Provider | _plugins.PluginHandle):
                return typing.cast(Any, marker)
            try:
                if marker in self.entries:
                    return marker
            except TypeError:
                continue
            if inspect.isroutine(marker):
                return marker
        return None

    def key_of(self, marker: Any, base: Any, where: str) -> Any | None:
        """Marker → registry key, registering `Provider` markers on
        first use and appending a problem for anything unresolvable."""
        if isinstance(marker, _plugins.PluginHandle):
            return self._handle_key(typing.cast(_plugins.PluginHandle[Any], marker), base, where)
        if isinstance(marker, Provider):
            if marker not in self.entries:
                self.entries[marker] = self._normalize(marker, None)
                self._register_provides(marker)
            return typing.cast(Any, marker)
        try:
            if marker in self.entries:
                return marker
        except TypeError:
            pass
        if inspect.isroutine(marker):
            self.problems.append(
                f"{where}: `{_name_of(marker)}` is not a registered provider — "
                "declare it with @provider, app.provide() or ctx.add_provider()"
            )
        else:
            self.problems.append(f"{where}: `{marker!r}` is not a provider marker")
        return None

    def _handle_key(self, handle: _plugins.PluginHandle[Any], base: Any, where: str) -> Any | None:
        """`Annotated[T, plugin_handle]` → the provider that plugin
        declared for `T` (its only provider when types can't disambiguate)."""
        keys = self.plugin_providers.get(handle.name, [])
        if not keys:
            if handle.name in self.known_plugins:
                self.problems.append(f"{where}: plugin `{handle.name}` declares no providers")
            else:
                self.problems.append(
                    f"{where}: plugin `{handle.name}` is not used by this application"
                )
            return None
        matching = [key for key in keys if _provides_match(self.entries[key].provides, base)]
        if len(matching) == 1:
            return matching[0]
        if len(matching) > 1:
            self.problems.append(
                f"{where}: plugin `{handle.name}` declares {len(matching)} providers "
                f"for `{_type_name(base)}` — Annotated is ambiguous"
            )
            return None
        if len(keys) == 1:
            return keys[0]
        self.problems.append(
            f"{where}: plugin `{handle.name}` declares {len(keys)} providers; "
            f"none provides `{_type_name(base)}`"
        )
        return None

    def register_route(self, label: str, is_async: bool, route: RouteDi) -> None:
        """Record one route's DI usage for validation (scope/async rules
        and the generator-cleanup lane)."""
        self.routes.append((label, is_async, route))

    # -- handler wrappers ---------------------------------------------------

    def wrap(
        self,
        handler: Callable[..., Any],
        route: RouteDi,
        is_async: bool,
    ) -> Callable[..., Any]:
        """The callable registered with the native app: resolves the
        route's DI bindings, then calls `handler` — one Python crossing
        for the whole resolve+handle (§12.1)."""
        graph = self
        bindings = route.bindings

        if is_async:

            @functools.wraps(handler)
            async def async_wrapped(__fulmen_request__: Request, *args: Any, **kwargs: Any) -> Any:
                scope = _request_scope(graph, __fulmen_request__)
                positional = list(args)
                for name, key, index in bindings:
                    value = await scope.aresolve(key)
                    if index is None:
                        kwargs[name] = value
                    else:
                        positional.insert(index, value)
                return await handler(*positional, **kwargs)

            return async_wrapped

        @functools.wraps(handler)
        def wrapped(__fulmen_request__: Request, *args: Any, **kwargs: Any) -> Any:
            scope = _request_scope(graph, __fulmen_request__)
            positional = list(args)
            for name, key, index in bindings:
                value = scope.resolve(key)
                if index is None:
                    kwargs[name] = value
                else:
                    positional.insert(index, value)
            return handler(*positional, **kwargs)

        return wrapped

    def cleanup_hook(self, is_async: bool) -> Callable[..., Any]:
        """The `after_response` callable for a route's private cleanup
        scope — runs request-context generator teardown in reverse order."""
        return _acleanup_request if is_async else _cleanup_request

    # -- validation ------------------------------------------------------------

    def validate(self) -> list[str]:
        """Check the complete graph: overrides, signatures, cycles, scope
        rules, route async rules, cleanup lanes, then prepare the
        startup/shutdown hooks. Returns every problem found (§14.1)."""
        self._apply_overrides()
        # Provider parameters can surface new provider markers, which have
        # parameters of their own — inspect until the registry stops growing.
        inspected: set[int] = set()
        while True:
            pending = [entry for entry in self.entries.values() if id(entry) not in inspected]
            if not pending:
                break
            for entry in pending:
                inspected.add(id(entry))
                self._deps_of(entry)
        self._resolve_type_deps()
        self._app_ctx = self._app_context()
        self._check_cycles()
        self._check_scopes()
        self._check_routes()
        self._prepare_startup()
        return self.problems

    def _apply_overrides(self) -> None:
        for original, replacement in self.override_map.items():
            try:
                declared = original in self.entries
            except TypeError:
                declared = False
            if declared:
                self.entries[original] = self._normalize(replacement, self.entries[original].scope)
                self._register_provides(original)
            elif isinstance(original, Provider):
                self.entries[original] = self._normalize(replacement, original.scope)
                self._register_provides(original)
            else:
                self.problems.append(
                    f"app.override({_name_of(original)}, …): the first argument is not "
                    "a registered provider in this application"
                )

    def _deps_of(self, entry: Entry) -> None:
        """Inspect one provider's signature into [`Dep`] edges."""
        if entry.const or entry.fn is None:
            return
        fn = entry.fn
        try:
            signature = inspect.signature(fn)
        except (TypeError, ValueError) as exc:
            self.problems.append(f"provider `{entry.label}`: cannot inspect signature: {exc}")
            return
        try:
            hints = typing.get_type_hints(
                fn.__init__ if isinstance(fn, type) else fn,
                localns=entry.localns,
                include_extras=True,
            )
        except Exception as exc:  # noqa: BLE001 -- any hint-resolution failure is a startup problem
            self.problems.append(f"provider `{entry.label}`: cannot resolve type hints: {exc}")
            return
        deps: list[Dep] = []
        for index, (name, parameter) in enumerate(signature.parameters.items()):
            if name == "self":
                continue
            if parameter.kind in (
                inspect.Parameter.VAR_POSITIONAL,
                inspect.Parameter.VAR_KEYWORD,
            ):
                self.problems.append(
                    f"provider `{entry.label}`: variadic parameter `{name}` is not supported"
                )
                continue
            where = f"provider `{entry.label}` parameter `{name}`"
            hint = hints.get(name, inspect.Parameter.empty)
            base, markers = _peel(hint)
            positional = index if parameter.kind is inspect.Parameter.POSITIONAL_ONLY else None
            marker = self._pick_marker(markers)
            if marker is not None:
                key = self.key_of(marker, base, where)
                if key is not None:
                    deps.append(Dep(name, "provider", key, positional))
                continue
            if markers:
                joined = ", ".join(repr(m) for m in markers)
                self.problems.append(f"{where}: unsupported marker(s) {joined}")
                continue
            if base is Request:
                deps.append(Dep(name, "request", None, positional))
                continue
            if base is inspect.Parameter.empty:
                self.problems.append(
                    f"{where}: needs a provider marker or a provided-type annotation"
                )
                continue
            deps.append(Dep(name, "type", base, positional))
        entry.deps = deps

    def _resolve_type_deps(self) -> None:
        """Bare `settings: Settings` params resolve through the
        provides-registry; missing and ambiguous lookups are problems."""
        for entry in self.entries.values():
            resolved: list[Dep] = []
            for dep in entry.deps:
                if dep.kind != "type":
                    resolved.append(dep)
                    continue
                try:
                    keys = self.provided.get(dep.key)
                except TypeError:
                    keys = None
                where = f"provider `{entry.label}` parameter `{dep.name}`"
                if not keys:
                    self.problems.append(
                        f"{where}: no provider produces `{_type_name(dep.key)}` — "
                        "register one with @provider, app.provide() or ctx.add_provider()"
                    )
                elif len(keys) == 1:
                    resolved.append(Dep(dep.name, "provider", keys[0], dep.position))
                else:
                    names = ", ".join(f"`{self.entries[k].label}`" for k in keys)
                    self.problems.append(
                        f"{where}: `{_type_name(dep.key)}` is provided by {names} — "
                        "use Annotated to pick one"
                    )
            entry.deps = resolved

    def _app_context(self) -> set[Any]:
        """Entry keys that resolve in application context: APP providers
        plus the TRANSIENT providers reachable from them (each resolves
        fresh inside its consumer, in the consumer's context)."""
        ctx: set[Any] = set()
        queue = [key for key, entry in self.entries.items() if entry.scope is Scope.APP]
        while queue:
            key = queue.pop()
            if key in ctx:
                continue
            ctx.add(key)
            for dep in self.entries[key].deps:
                if dep.kind != "provider":
                    continue
                target = self.entries.get(dep.key)
                if target is not None and target.scope is not Scope.REQUEST:
                    queue.append(dep.key)
        return ctx

    def _check_cycles(self) -> None:
        """DFS over provider edges; report each cycle once (§12.2)."""
        state: dict[Any, int] = {}
        stack: list[Any] = []
        reported: set[frozenset[Any]] = set()

        def visit(key: Any) -> None:
            state[key] = 1
            stack.append(key)
            for dep in self.entries[key].deps:
                if dep.kind != "provider" or dep.key not in self.entries:
                    continue
                mark = state.get(dep.key, 0)
                if mark == 0:
                    visit(dep.key)
                elif mark == 1:
                    cycle = frozenset(stack[stack.index(dep.key) :])
                    if cycle not in reported:
                        reported.add(cycle)
                        path = [*stack[stack.index(dep.key) :], dep.key]
                        names = " → ".join(f"`{self.entries[k].label}`" for k in path)
                        self.problems.append(f"provider dependency cycle: {names}")
            stack.pop()
            state[key] = 2

        for key in self.entries:
            if state.get(key, 0) == 0:
                visit(key)

    def _check_scopes(self) -> None:
        """A request-context dependency (`Request`, or a REQUEST-scope
        provider) under an application-context consumer is a startup
        problem — the value cannot exist at startup (§12.2)."""
        for key, entry in self.entries.items():
            if key not in self._app_ctx:
                continue
            for dep in entry.deps:
                if dep.kind == "request":
                    self.problems.append(
                        f"provider `{entry.label}` ({entry.scope.value} scope) depends on "
                        "`request` — only request/transient-scoped providers may"
                    )
                elif dep.kind == "provider":
                    target = self.entries.get(dep.key)
                    if target is not None and target.scope is Scope.REQUEST:
                        self.problems.append(
                            f"provider `{entry.label}` ({entry.scope.value} scope) depends on "
                            f"request-scope provider `{target.label}`"
                        )

    def _request_closure(self, roots: list[Any]) -> set[Any]:
        """Providers a route resolves in request context: its DI roots,
        traversed without descending into APP providers (those resolved
        at startup — their closure runs on the app, not the request)."""
        seen: set[Any] = set()
        stack = list(roots)
        while stack:
            key = stack.pop()
            entry = self.entries.get(key)
            if entry is None or key in seen or entry.scope is Scope.APP:
                continue
            seen.add(key)
            stack.extend(dep.key for dep in entry.deps if dep.kind == "provider")
        return seen

    def _check_routes(self) -> None:
        """Per route: sync handlers cannot reach async providers in
        request context (§12.2), and the generator-cleanup lane is
        `"async"` when the closure contains an async generator."""
        for label, is_async, route in self.routes:
            closure = self._request_closure([key for _n, key, _i in route.bindings])
            if not is_async:
                for key in closure:
                    entry = self.entries[key]
                    if entry.is_async:
                        self.problems.append(
                            f"{label}: sync handler depends on async provider "
                            f"`{entry.label}` — use `async def` or make the provider sync"
                        )
            lane = None
            for key in closure:
                if self.entries[key].gen == "async":
                    lane = "async"
                    break
                if self.entries[key].gen == "sync":
                    lane = "sync"
            route.cleanup_lane = lane

    # -- startup / shutdown -------------------------------------------------

    def _prepare_startup(self) -> None:
        """APP providers (and constants) resolve once at startup — a
        synthetic lifecycle hook registered before plugin/app hooks;
        their generators' cleanup rides a shutdown hook that runs last."""
        self._app_order = [key for key, entry in self.entries.items() if entry.scope is Scope.APP]
        for key in self._app_order:
            entry = self.entries[key]
            if entry.const:
                self.app_values[key] = entry.value
        if not self._app_order:
            return
        self.startup_is_async = any(self.entries[k].is_async for k in self._app_ctx)
        self.shutdown_is_async = any(self.entries[k].gen == "async" for k in self._app_ctx)
        self.startup_hook = self._make_startup(self.startup_is_async)
        self.shutdown_hook = self._make_shutdown(self.shutdown_is_async)

    def _make_startup(self, is_async: bool) -> Callable[[], Any]:
        graph = self
        if is_async:

            async def async_startup() -> None:
                await graph._app_resolve_async()

            return async_startup

        def sync_startup() -> None:
            graph._app_resolve()

        return sync_startup

    def _make_shutdown(self, is_async: bool) -> Callable[[], Any]:
        graph = self
        if is_async:

            async def async_shutdown() -> None:
                for close in reversed(graph.app_cleanups):
                    result = close()
                    if inspect.isawaitable(result):
                        await result

            return async_shutdown

        def sync_shutdown() -> None:
            for close in reversed(graph.app_cleanups):
                close()

        return sync_shutdown

    def _app_resolve(self) -> None:
        """Resolve every APP provider (the startup hook body, sync lane)."""
        for key in self._app_order:
            self._app_value(key)

    def _app_value(self, key: Any) -> Any:
        if key in self.app_values:
            return self.app_values[key]
        entry = self.entries[key]
        value = self._produce_app(entry)
        self.app_values[key] = value
        return value

    def _produce_app(self, entry: Entry) -> Any:
        if entry.const:
            return entry.value
        pos, kw = self._app_dep_args(entry)
        if entry.gen == "sync":
            generator = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.app_cleanups.append(generator.close)
            try:
                return next(generator)
            except StopIteration:
                self.app_cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        return entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]

    def _app_dep_args(self, entry: Entry) -> tuple[list[Any], dict[str, Any]]:
        pos: list[Any] = []
        kw: dict[str, Any] = {}
        for dep in entry.deps:
            value = self._app_dep(dep)
            if dep.position is None:
                kw[dep.name] = value
            else:
                pos.insert(dep.position, value)
        return pos, kw

    def _app_dep(self, dep: Dep) -> Any:
        if dep.kind == "request":
            raise RuntimeError(
                "a provider resolving in application scope cannot depend on `request`"
            )
        target = self.entries[dep.key]
        if target.scope is Scope.REQUEST:
            raise RuntimeError(
                f"provider `{target.label}` is request-scoped and cannot resolve "
                "in application scope"
            )
        if target.scope is Scope.APP:
            return self._app_value(dep.key)
        return self._produce_app(target)

    async def _app_resolve_async(self) -> None:
        """Resolve every APP provider (the startup hook body, async lane —
        `asyncio.run` drives it through `run_lifecycle`)."""
        for key in self._app_order:
            await self._app_value_async(key)

    async def _app_value_async(self, key: Any) -> Any:
        if key in self.app_values:
            return self.app_values[key]
        entry = self.entries[key]
        value = await self._produce_app_async(entry)
        self.app_values[key] = value
        return value

    async def _produce_app_async(self, entry: Entry) -> Any:
        if entry.const:
            return entry.value
        pos, kw = await self._app_dep_args_async(entry)
        if entry.gen == "async":
            agen = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.app_cleanups.append(agen.aclose)
            try:
                return await agen.__anext__()
            except StopAsyncIteration:
                self.app_cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        if entry.gen == "sync":
            generator = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
            self.app_cleanups.append(generator.close)
            try:
                return next(generator)
            except StopIteration:
                self.app_cleanups.pop()
                raise RuntimeError(f"provider `{entry.label}` yielded no value") from None
        result = entry.fn(*pos, **kw)  # pyright: ignore[reportOptionalCall]
        if inspect.isawaitable(result):
            return await result
        return result

    async def _app_dep_args_async(self, entry: Entry) -> tuple[list[Any], dict[str, Any]]:
        pos: list[Any] = []
        kw: dict[str, Any] = {}
        for dep in entry.deps:
            value = await self._app_dep_async(dep)
            if dep.position is None:
                kw[dep.name] = value
            else:
                pos.insert(dep.position, value)
        return pos, kw

    async def _app_dep_async(self, dep: Dep) -> Any:
        if dep.kind == "request":
            raise RuntimeError(
                "a provider resolving in application scope cannot depend on `request`"
            )
        target = self.entries[dep.key]
        if target.scope is Scope.REQUEST:
            raise RuntimeError(
                f"provider `{target.label}` is request-scoped and cannot resolve "
                "in application scope"
            )
        if target.scope is Scope.APP:
            return await self._app_value_async(dep.key)
        return await self._produce_app_async(target)
