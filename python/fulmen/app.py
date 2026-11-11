"""The `Fulmen` application object (DESIGN.md §5.1).

Route decorators record `(method, path, handler)` tuples. Nothing is
inspected or compiled at decoration time; `run()` (or a test server via
`Fulmen._compile_native()`) inspects each handler's signature and type
hints, hands the declarations to Rust, and serves.
"""

from __future__ import annotations

import inspect
import typing
from collections.abc import Callable, Iterable

from fulmen._fulmen import NativeApp, StartupError
from fulmen._fulmen import serve as _serve

_Handler = Callable[..., object]

_METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")

# A route declaration spec handed to `NativeApp.add_route`:
# (method, path, [(param-name, "int"|"str"), ...], [keyword-only-flag, ...],
#  handler, is-async). The path-param list is in handler signature order —
# the router returns extracted values in the same order.
_RouteSpec = tuple[str, str, list[tuple[str, str]], list[bool], _Handler, bool]


class Fulmen:
    """A Fulmen application: routes, then `run()` to serve."""

    __slots__ = ("_declared",)

    def __init__(self) -> None:
        self._declared: list[tuple[str, str, _Handler]] = []

    def route(
        self,
        path: str,
        methods: Iterable[str] = ("GET",),
    ) -> Callable[[_Handler], _Handler]:
        """Register a handler for `path` under each method in `methods`."""

        def decorator(handler: _Handler) -> _Handler:
            for method in methods:
                self._declared.append((method.upper(), path, handler))
            return handler

        return decorator

    def get(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `GET path`."""
        return self.route(path, methods=("GET",))

    def post(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `POST path`."""
        return self.route(path, methods=("POST",))

    def put(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PUT path`."""
        return self.route(path, methods=("PUT",))

    def patch(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `PATCH path`."""
        return self.route(path, methods=("PATCH",))

    def delete(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `DELETE path`."""
        return self.route(path, methods=("DELETE",))

    def head(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `HEAD path`."""
        return self.route(path, methods=("HEAD",))

    def options(self, path: str) -> Callable[[_Handler], _Handler]:
        """Register `handler` for `OPTIONS path`."""
        return self.route(path, methods=("OPTIONS",))

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

    def _compile_native(self) -> NativeApp:
        """Inspect every recorded handler and build the native declaration.

        Python-side problems (unknown parameter, bad annotation, missing
        path-param binding, uninspectable signature) are aggregated and
        raised as one `StartupError` in the same format Rust produces for
        core compile errors.
        """
        native = NativeApp()
        problems: list[str] = []
        for method, path, handler in self._declared:
            spec = _inspect_handler(method, path, handler, problems)
            if spec is not None:
                native.add_route(spec)
        if problems:
            raise _startup_error(problems)
        return native


def _startup_error(problems: list[str]) -> StartupError:
    message = "application failed to start:" + "".join(f"\n  - {problem}" for problem in problems)
    return StartupError(message)


def _inspect_handler(
    method: str,
    path: str,
    handler: _Handler,
    problems: list[str],
) -> _RouteSpec | None:
    """Turn one recorded declaration into a `_RouteSpec`, or append a
    problem and return `None`."""
    label = f"{method} {path} ({getattr(handler, '__qualname__', '<handler>')})"
    try:
        signature = inspect.signature(handler)
    except (TypeError, ValueError) as exc:
        problems.append(f"{label}: cannot inspect the handler signature: {exc}")
        return None
    try:
        hints = typing.get_type_hints(handler, include_extras=True)
    except Exception as exc:  # noqa: BLE001 -- any hint-resolution failure is a startup problem
        problems.append(f"{label}: cannot resolve type hints: {exc}")
        return None

    path_params = _path_param_names(path)
    param_set = frozenset(path_params)
    param_types: list[tuple[str, str]] = []
    kw_flags: list[bool] = []
    for name, parameter in signature.parameters.items():
        if parameter.kind in (
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        ):
            problems.append(
                f"{label}: variadic parameter `{name}` is not supported; "
                "handlers may only declare path parameters"
            )
            return None
        if name not in param_set:
            problems.append(
                f"{label}: parameter `{name}` is not a path parameter of `{path}`; "
                "query and body extraction are not supported in M1"
            )
            return None
        hint = hints.get(name)
        if hint is None:
            type_name = "str"
        elif hint is int:
            type_name = "int"
        elif hint is str:
            type_name = "str"
        else:
            problems.append(
                f"{label}: path parameter `{name}` must be annotated "
                f"`int` or `str`, got `{_type_name(hint)}`"
            )
            return None
        param_types.append((name, type_name))
        kw_flags.append(parameter.kind is inspect.Parameter.KEYWORD_ONLY)

    missing = [name for name in path_params if name not in signature.parameters]
    if missing:
        joined = ", ".join(f"`{name}`" for name in missing)
        problems.append(f"{label}: path parameter(s) {joined} are not bound by the handler")
        return None

    return (
        method,
        path,
        param_types,
        kw_flags,
        handler,
        inspect.iscoroutinefunction(handler),
    )


def _path_param_names(path: str) -> list[str]:
    """The `{name}` and `{*name}` parameters of a route pattern, in order."""
    return [
        segment[1:-1].lstrip("*")
        for segment in path.split("/")
        if len(segment) > 2 and segment.startswith("{") and segment.endswith("}")
    ]


def _type_name(hint: object) -> str:
    return getattr(hint, "__qualname__", None) or str(hint)
