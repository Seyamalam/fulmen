"""The `Fulmen` application object (DESIGN.md §5.1, §5.2, §13.3).

Route decorators record `(method, path, handler, meta)` tuples; `on_error`
registrations record `(exc_class, handler)` pairs. Nothing is inspected
or compiled at decoration time; `run()` (or a test server via
`Fulmen._compile_native()`) inspects each handler's signature and type
hints — `_typing.py` turns every parameter into its source/wire-name/
descriptor/missing spec — hands the declarations to Rust, and serves.

The same frozen spec trees feed `Fulmen.openapi()` — the OpenAPI 3.1
document (§11.3, §23.2, `fulmen.openapi`) — which `_compile_native`
renders once and mounts as `GET openapi_url` plus a self-hosted Swagger
UI under `docs_url` when configured.
"""

from __future__ import annotations

import inspect
import typing
from collections.abc import Callable, Iterable, Mapping
from typing import TYPE_CHECKING, Any, Unpack

from fulmen import _typing
from fulmen import openapi as _openapi
from fulmen._fulmen import NativeApp, StartupError, compile_descriptor
from fulmen._fulmen import serve as _serve
from fulmen.constraints import UNSET
from fulmen.responses import Response

if TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

_Handler = Callable[..., object]

_METHODS = ("GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS")

# One `call_args` element of a route spec (docs/SPEC-FORMAT.md §3):
# (parameter name, source, signature kind, wire name, Descriptor|None,
#  (missing-kind, missing-object)). `missing-kind` is "required" (absence
# is a 422), "omit" (the signature default applies), "literal" (pass the
# object) or "factory" (call it per request).
_CallArg = tuple[str, str, str, str, "Descriptor | None", tuple[str, object]]

# A route declaration spec handed to `NativeApp.add_route`:
# (method, path, [(path-name, "int"|"str"), ...], call_args, handler,
#  is-async, none-ok, [(status|None, Descriptor|None), ...]). The
# path-param list is in handler signature order — the router returns
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
    """

    __slots__ = (
        "_api_info",
        "_debug",
        "_declared",
        "_docs_url",
        "_error_handlers",
        "_openapi_url",
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
    ) -> None:
        for name, url in (("openapi_url", openapi_url), ("docs_url", docs_url)):
            if url is not None and (not url.startswith("/") or '"' in url):
                raise ValueError(
                    f"{name} must be None or an absolute path starting with '/', got {url!r}"
                )
        self._debug = debug
        self._declared: list[tuple[str, str, _Handler, _openapi.OperationMeta]] = []
        self._error_handlers: list[tuple[type[BaseException], _Handler, bool]] = []
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
        meta = _openapi.operation_meta(options)

        def decorator(handler: _Handler) -> _Handler:
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
            self._error_handlers.append((exc_class, handler, inspect.iscoroutinefunction(handler)))
            return handler

        return decorator

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

    def openapi(self) -> dict[str, Any]:
        """The OpenAPI 3.1 document for the declared routes (§11.3, §23.2).

        Inspects handlers exactly like `_compile_native` — a broken app
        raises `StartupError` here too. The returned dict is freshly
        built each call; serving renders it once at startup instead.
        """
        specs, problems = self._route_specs()
        if problems:
            raise _startup_error(problems)
        return _openapi.generate(self._api_info, [_route_entry(spec, meta) for spec, meta in specs])

    def _compile_native(self) -> NativeApp:
        """Inspect every recorded handler and build the native declaration.

        Python-side problems (unknown parameter, bad annotation, missing
        path-param binding, uninspectable signature) are aggregated and
        raised as one `StartupError` in the same format Rust produces for
        core compile errors.
        """
        native = NativeApp(debug=self._debug)
        specs, problems = self._route_specs()
        for spec, _meta in specs:
            native.add_route(spec)
        for exc_class, handler, is_async in self._error_handlers:
            native.add_error_handler(exc_class, handler, is_async)
        if problems:
            raise _startup_error(problems)
        self._serve_docs(native, specs)
        return native

    def _route_specs(self) -> tuple[list[tuple[_RouteSpec, _openapi.OperationMeta]], list[str]]:
        """Every declaration inspected into its `_RouteSpec` + doc meta,
        plus the aggregated problem list."""
        specs: list[tuple[_RouteSpec, _openapi.OperationMeta]] = []
        problems: list[str] = []
        for method, path, handler, meta in self._declared:
            spec = _inspect_handler(method, path, handler, problems)
            if spec is not None:
                specs.append((spec, meta))
        return specs, problems

    def _serve_docs(
        self,
        native: NativeApp,
        specs: list[tuple[_RouteSpec, _openapi.OperationMeta]],
    ) -> None:
        """Mount `GET openapi_url`, `GET docs_url` and the UI assets, each
        serving a body rendered once here — the spec trees are frozen at
        compile time, so there is nothing left to compute per request.
        `openapi_url=None` registers nothing."""
        if self._openapi_url is None:
            return
        entries = [_route_entry(spec, meta) for spec, meta in specs]
        document = _openapi.render(_openapi.generate(self._api_info, entries))
        taken = {(method, path) for method, path, _handler, _meta in self._declared}
        _add_static_route(native, taken, self._openapi_url, document, "application/json")
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

    path_names = frozenset(_path_param_names(path))
    builder = _typing.SpecBuilder()
    infos: list[_typing.ParamInfo] = []
    try:
        for name, parameter in signature.parameters.items():
            if parameter.kind in (
                inspect.Parameter.VAR_POSITIONAL,
                inspect.Parameter.VAR_KEYWORD,
            ):
                problems.append(
                    f"{label}: variadic parameter `{name}` is not supported; "
                    "handlers declare path, query, header, cookie, body or form parameters"
                )
                return None
            hint = hints.get(name, inspect.Parameter.empty)
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
        return None

    path_infos = [info for info in infos if info.source == "path"]
    missing = [name for name in path_names if name not in {info.name for info in path_infos}]
    if missing:
        joined = ", ".join(f"`{name}`" for name in missing)
        problems.append(f"{label}: path parameter(s) {joined} are not bound by the handler")
        return None

    param_types = [
        (info.name, "int" if info.spec["kind"] == "int" else "str") for info in path_infos
    ]
    call_args = [
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
    return (
        method,
        path,
        param_types,
        call_args,
        handler,
        inspect.iscoroutinefunction(handler),
        any(variant.status == 204 for variant in variants),
        response_descs,
    )


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

    native.add_route(("GET", path, [], [], handler, False, False, [(None, None)]))


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
