"""Annotation → spec-tree conversion (DESIGN.md §11.1-§11.3).

Everything in this file runs once per declaration — at `Model` class
creation and inside `Fulmen._compile_native` — and produces a *spec tree*:
nested `dict`s/`list`s/`str`s/`int`s/`float`s/`bool`s/`None` with no Python
objects (docs/SPEC-FORMAT.md is the contract). `fulmen._fulmen.
compile_descriptor(spec, callbacks)` turns the tree into a compiled
`Descriptor`. Python callables the spec references (`default_factory`,
`AfterValidator` hooks) ride in a per-declaration `callbacks` list whose
indices are the spec's `{"factory": id}` / `meta.after` ids.
"""

from __future__ import annotations

import copy
import dataclasses
import datetime
import enum
import functools
import inspect
import ipaddress
import types as pytypes
import typing
import uuid
from collections.abc import Callable
from decimal import Decimal
from typing import Annotated, Any, get_args, get_origin, get_type_hints

from fulmen._fulmen import compile_descriptor
from fulmen.constraints import (
    UNSET,
    AfterValidator,
    Alias,
    Default,
    Deprecated,
    Description,
    Discriminator,
    Example,
    FieldSpec,
    Len,
    Lower,
    MultipleOf,
    Pattern,
    Range,
    Strip,
    Unique,
    Upper,
)
from fulmen.params import Marker, Request, UploadFile
from fulmen.types import Email, Raw, Url

if typing.TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

_NoneType = type(None)


class AnnotationError(ValueError):
    """An annotation cannot be expressed in the spec IR — a startup problem."""


# Kinds that default to `query` source when the name is not a path param.
_QUERY_KINDS = {
    "bool",
    "int",
    "float",
    "str",
    "bytes",
    "decimal",
    "uuid",
    "email",
    "url",
    "ipv4",
    "ipv6",
    "datetime",
    "date",
    "time",
    "timedelta",
    "enum",
    "literal",
    "newtype",
    "any",
}

_LEN_KINDS = {"str", "bytes", "list", "set", "frozenset", "tuple", "tuple_fixed", "dict"}
_RANGE_KINDS = {"int", "float", "decimal"}

# Marker classes used bare (`Annotated[str, Strip]`) or as instances
# (`Annotated[str, Strip()]`): `marker is cls or isinstance(marker, cls)`.
_TRANSFORMS: dict[type[Any], str] = {Strip: "strip", Lower: "lower", Upper: "upper"}


def _is_deprecated(marker: Any) -> bool:
    return marker is Deprecated or isinstance(marker, Deprecated)


def _is_transform(marker: Any) -> bool:
    return isinstance(marker, Strip | Lower | Upper) or marker in (Strip, Lower, Upper)


def _is_constraint(marker: Any) -> bool:
    return isinstance(marker, Len | Range | Pattern | MultipleOf | Unique) or _is_transform(marker)


def _peel_annotated(hint: Any) -> tuple[Any, list[Any]]:
    """`Annotated[T, m1, m2]` → `(T, [m1, m2])`; anything else → `(hint, [])`."""
    if get_origin(hint) is Annotated:
        args = get_args(hint)
        return args[0], list(args[1:])
    return hint, []


def _bound_spec(value: Any) -> Any:
    """`int|float` → itself; `Decimal` → `{"decimal": text}` for the spec."""
    if isinstance(value, bool):
        raise AnnotationError(f"bound must be a number, got {value!r}")
    if isinstance(value, int | float):
        return value
    if isinstance(value, Decimal):
        return {"decimal": str(value)}
    raise AnnotationError(f"bound must be int, float or Decimal, got {value!r}")


def _literal_spec(value: Any) -> Any:
    """`Literal[...]` member → its spec literal (`None|bool|int|float|str|bytes`)."""
    if value is None or isinstance(value, bool | int | float | str | bytes):
        return value
    raise AnnotationError(f"unsupported literal member {value!r}")


def _type_name(hint: Any) -> str:
    return getattr(hint, "__qualname__", None) or str(hint)


class SpecBuilder:
    """Builds spec nodes for one declaration unit, collecting the Python
    callables (`default_factory`, `AfterValidator`) the spec refers to."""

    def __init__(self) -> None:
        self.callbacks: list[Callable[..., Any]] = []
        self._building: frozenset[type[Any]] = frozenset()

    def cb_id(self, fn: Callable[..., Any]) -> int:
        self.callbacks.append(fn)
        return len(self.callbacks) - 1

    # ---- the annotation → node core ---------------------------------------

    def type_spec(self, hint: Any, where: str = "annotation") -> dict[str, Any]:
        """Convert one resolved annotation into a spec node."""
        base, markers = _peel_annotated(hint)
        node = self._base_spec(base, where)
        if markers:
            self.apply_markers(node, markers, where)
        return node

    def _base_spec(self, hint: Any, where: str) -> dict[str, Any]:
        if hint is Any or hint is object:
            return {"kind": "any"}
        if hint is None or hint is _NoneType:
            return {"kind": "none"}
        if hint is bool:
            return {"kind": "bool"}
        if hint is int:
            return {"kind": "int"}
        if hint is float:
            return {"kind": "float"}
        if hint is str:
            return {"kind": "str"}
        if hint is bytes:
            return {"kind": "bytes"}
        if hint is datetime.datetime:
            return {"kind": "datetime"}
        if hint is datetime.date:
            return {"kind": "date"}
        if hint is datetime.time:
            return {"kind": "time"}
        if hint is datetime.timedelta:
            return {"kind": "timedelta"}
        if hint is Decimal:
            return {"kind": "decimal"}
        if hint is uuid.UUID:
            return {"kind": "uuid"}
        if hint is ipaddress.IPv4Address:
            return {"kind": "ipv4"}
        if hint is ipaddress.IPv6Address:
            return {"kind": "ipv6"}
        if hint is Email:
            return {"kind": "email"}
        if hint is Url:
            return {"kind": "url"}
        if hint is Raw:
            return {"kind": "raw"}
        if hint is UploadFile:
            # The `file` source materializes the object; the wire is raw bytes.
            return {"kind": "raw"}
        if hint is Request:
            raise AnnotationError(
                "Request is injected by the framework, not a schema type — "
                "declare `request: Request` on the handler"
            )
        if isinstance(hint, typing.TypeVar):
            return {"kind": "typevar", "name": hint.__name__}
        if isinstance(hint, enum.EnumMeta):
            return self._enum_spec(hint)
        if getattr(hint, "__fulmen_model__", False):
            return self.record_spec(hint)
        if dataclasses.is_dataclass(hint) and isinstance(hint, type):
            return self._dataclass_spec(hint)
        if typing.is_typeddict(hint):
            return self._typed_dict_spec(hint)
        supertype = getattr(hint, "__supertype__", None)
        if supertype is not None:
            return {
                "kind": "newtype",
                "name": getattr(hint, "__qualname__", "NewType").split(".")[-1],
                "inner": self.type_spec(supertype, where),
            }

        origin = get_origin(hint)
        if origin is not None:
            return self._alias_spec(origin, get_args(hint), hint, where)

        if hint is list:
            return {"kind": "list", "item": {"kind": "any"}}
        if hint is set:
            return {"kind": "set", "item": {"kind": "any"}}
        if hint is frozenset:
            return {"kind": "frozenset", "item": {"kind": "any"}}
        if hint is tuple:
            return {"kind": "tuple", "item": {"kind": "any"}}
        if hint is dict:
            return {"kind": "dict", "key": {"kind": "str"}, "value": {"kind": "any"}}
        raise AnnotationError(f"unsupported {where} {_type_name(hint)}")

    def _alias_spec(
        self, origin: Any, args: tuple[Any, ...], hint: Any, where: str
    ) -> dict[str, Any]:
        if origin is Annotated:
            base, markers = _peel_annotated(hint)
            node = self._base_spec(base, where)
            self.apply_markers(node, markers, where)
            return node
        if origin in (typing.Union, pytypes.UnionType):
            none = args.index(_NoneType) if _NoneType in args else -1
            rest = [a for i, a in enumerate(args) if i != none]
            if not rest:
                return {"kind": "none"}
            inner = (
                {"kind": "union", "variants": [self.type_spec(a, where) for a in rest]}
                if len(rest) > 1
                else self.type_spec(rest[0], where)
            )
            if none >= 0:
                return {"kind": "optional", "inner": inner}
            return inner
        if origin is typing.Literal:
            return {"kind": "literal", "members": [_literal_spec(a) for a in args]}
        if origin is list:
            item = args[0] if args else Any
            return {"kind": "list", "item": self.type_spec(item, where)}
        if origin is set:
            item = args[0] if args else Any
            return {"kind": "set", "item": self.type_spec(item, where)}
        if origin is frozenset:
            item = args[0] if args else Any
            return {"kind": "frozenset", "item": self.type_spec(item, where)}
        if origin is tuple:
            if len(args) == 2 and args[1] is Ellipsis:
                return {"kind": "tuple", "item": self.type_spec(args[0], where)}
            return {"kind": "tuple_fixed", "items": [self.type_spec(a, where) for a in args]}
        if origin is dict:
            key, value = args if len(args) == 2 else (str, Any)
            if key is Any:
                key = str  # `dict`/`dict[Any, V]` wire keys are strings.
            return {
                "kind": "dict",
                "key": self.type_spec(key, where),
                "value": self.type_spec(value, where),
            }
        if origin is typing.ClassVar:
            raise AnnotationError(f"ClassVar is not a schema type in {where}")
        # `Page[int]` — a generic Model/dataclass.
        if getattr(origin, "__fulmen_model__", False):
            return {
                "kind": "generic",
                "origin": self.record_spec(origin),
                "args": [self.type_spec(a, where) for a in args],
            }
        if isinstance(origin, type) and dataclasses.is_dataclass(origin):
            return {
                "kind": "generic",
                "origin": self._dataclass_spec(origin),
                "args": [self.type_spec(a, where) for a in args],
            }
        raise AnnotationError(f"unsupported {where} {_type_name(hint)}")

    def _enum_spec(self, cls: enum.EnumMeta) -> dict[str, Any]:
        flavor = "int" if issubclass(cls, int) else ("str" if issubclass(cls, str) else "plain")
        members = [
            {"name": name, "value": _literal_spec(member.value)}
            for name, member in cls.__members__.items()
        ]
        return {
            "kind": "enum",
            "class": {"name": cls.__qualname__, "module": cls.__module__},
            "flavor": flavor,
            "members": members,
        }

    # ---- records -----------------------------------------------------------

    def record_spec(self, cls: type[Any]) -> dict[str, Any]:
        """`Model` subclasses: the class's declared fields → a `record` node."""
        if cls in self._building:
            raise AnnotationError(
                f"recursive model {cls.__qualname__} is not supported "
                "(the schema IR has no reference type yet)"
            )
        self._building = self._building | {cls}
        try:
            hints = get_type_hints(cls, include_extras=True)
            fields: list[dict[str, Any]] = []
            for name, hint in hints.items():
                if name.startswith("_") or get_origin(hint) is typing.ClassVar:
                    continue
                fields.append(self._model_field(cls, name, hint))
            type_params = [p.__name__ for p in getattr(cls, "__parameters__", ())]
            return {
                "kind": "record",
                "class": {"name": cls.__qualname__, "module": cls.__module__},
                "flavor": "model",
                "fields": fields,
                "extra": getattr(cls, "__model_extra__", "ignore"),
                "rename": getattr(cls, "__model_rename__", None),
                "type_params": type_params,
            }
        finally:
            self._building = self._building - {cls}

    def _model_field(self, cls: type[Any], name: str, hint: Any) -> dict[str, Any]:
        where = f"field {cls.__qualname__}.{name}"
        base, markers = _peel_annotated(hint)
        node = self.type_spec(base, where)
        meta: dict[str, Any] = {}
        alias: str | None = None
        declared: Any = getattr(cls, name, UNSET)
        default: Any = UNSET
        factory: Callable[[], Any] | None = None
        cons: dict[str, Any] = {}
        for marker in markers:
            if isinstance(marker, FieldSpec):
                _merge_field_spec(marker, meta, cons)
                if marker.alias:
                    alias = marker.alias
                if marker.default is not UNSET:
                    default = marker.default
                if marker.default_factory is not None:
                    factory = marker.default_factory
            elif isinstance(marker, Alias):
                alias = marker.name
            elif isinstance(marker, Default):
                default = marker.value
            elif isinstance(marker, AfterValidator):
                node.setdefault("meta", {}).setdefault("after", []).append(self.cb_id(marker.fn))
            elif isinstance(marker, Description):
                meta["description"] = marker.text
            elif isinstance(marker, Example):
                meta["example"] = _literal_spec(marker.value)
            elif _is_deprecated(marker):
                meta["deprecated"] = True
            elif _is_constraint(marker):
                self._collect_constraint(cons, marker, where)
            elif isinstance(marker, Discriminator):
                raise AnnotationError(f"Discriminator belongs on a union type, not on {where}")
            elif isinstance(marker, Marker):
                raise AnnotationError(
                    f"{type(marker).__name__}() is a parameter marker — it cannot "
                    f"appear inside the model {where}"
                )
            else:
                raise AnnotationError(f"unsupported Annotated marker {marker!r} on {where}")
        self.apply_constraints(node, cons, where)
        if declared is not UNSET:
            if isinstance(declared, FieldSpec):
                _merge_field_spec(declared, meta, cons)
                self.apply_constraints(node, cons, where)
                if declared.alias:
                    alias = declared.alias
                if declared.default_factory is not None:
                    factory = declared.default_factory
                elif declared.default is not UNSET:
                    default = declared.default
            else:
                default = declared
        return {
            "name": name,
            "type": node,
            "alias": alias,
            "missing": self._missing(default, factory, node),
            "meta": meta or None,
        }

    def _missing(
        self,
        default: Any,
        factory: Callable[[], Any] | None,
        node: dict[str, Any],
    ) -> Any:
        if factory is not None:
            return {"factory": self.cb_id(factory)}
        node_default = node.get("meta", {}).get("default", UNSET)
        if default is UNSET and node_default is not UNSET:
            default = node_default
        if default is UNSET:
            return "required"
        if default is None or isinstance(default, bool | int | float | str | bytes):
            return {"literal": default}
        # Non-scalar defaults (a list, a model, a set): a per-instance copy
        # avoids the shared-mutable-default bug.
        return {"factory": self.cb_id(functools.partial(copy.deepcopy, default))}

    def _collect_constraint(self, cons: dict[str, Any], marker: Any, where: str) -> None:
        if isinstance(marker, Len):
            if marker.min is not None:
                cons["min_length"] = marker.min
            if marker.max is not None:
                cons["max_length"] = marker.max
        elif isinstance(marker, Range):
            cons.update(
                {
                    key: value
                    for key, value in (
                        ("ge", marker.ge),
                        ("gt", marker.gt),
                        ("le", marker.le),
                        ("lt", marker.lt),
                    )
                    if value is not None
                }
            )
        elif isinstance(marker, Pattern):
            cons["pattern"] = marker.pattern
        elif isinstance(marker, MultipleOf):
            cons["multiple_of"] = marker.value
        elif isinstance(marker, Unique):
            cons["unique"] = marker.enabled
        elif _is_transform(marker):
            name = (marker if isinstance(marker, type) else type(marker)).__name__.lower()
            cons.setdefault("transforms", {})[name] = True
        else:
            raise AnnotationError(f"unsupported constraint {marker!r} on {where}")

    def apply_markers(self, node: dict[str, Any], markers: list[Any], where: str) -> None:
        """Markers inside `Annotated[...]` — mid-tree only node-level
        markers are legal (param/field markers are partitioned first)."""
        cons: dict[str, Any] = {}
        for marker in markers:
            if isinstance(marker, Alias):
                node.setdefault("meta", {})["alias"] = marker.name
            elif isinstance(marker, Default):
                node.setdefault("meta", {})["default"] = _literal_spec(marker.value)
            elif isinstance(marker, Description):
                node.setdefault("meta", {})["description"] = marker.text
            elif isinstance(marker, Example):
                node.setdefault("meta", {})["example"] = _literal_spec(marker.value)
            elif _is_deprecated(marker):
                node.setdefault("meta", {})["deprecated"] = True
            elif isinstance(marker, AfterValidator):
                node.setdefault("meta", {}).setdefault("after", []).append(self.cb_id(marker.fn))
            elif isinstance(marker, Discriminator):
                if node["kind"] != "union":
                    raise AnnotationError(
                        f"Discriminator applies to a union type, not `{node['kind']}` in {where}"
                    )
                node["tag"] = marker.field
            elif _is_constraint(marker):
                self._collect_constraint(cons, marker, where)
            else:
                raise AnnotationError(f"unsupported Annotated marker {marker!r} in {where}")
        self.apply_constraints(node, cons, where)

    @staticmethod
    def _unwrap(node: dict[str, Any]) -> dict[str, Any]:
        """Constraints pass through `optional`/`newtype` to the inner node."""
        while node["kind"] in ("optional", "newtype") and "inner" in node:
            node = node["inner"]
        return node

    def apply_constraints(self, node: dict[str, Any], cons: dict[str, Any], where: str) -> None:
        inner = self._unwrap(node)
        kind = inner["kind"]
        if any(cons.get(k) is not None for k in ("ge", "gt", "le", "lt", "multiple_of")):
            if kind not in _RANGE_KINDS:
                raise AnnotationError(
                    f"Range/MultipleOf applies to int, float or Decimal — got `{kind}` in {where}"
                )
            rng = inner.setdefault("range", {})
            for key in ("ge", "gt", "le", "lt"):
                if cons.get(key) is not None:
                    rng[key] = _bound_spec(cons[key])
            if cons.get("multiple_of") is not None:
                inner["multiple_of"] = _bound_spec(cons["multiple_of"])
        if any(
            cons.get(k) is not None for k in ("min_length", "max_length", "min_items", "max_items")
        ):
            if kind not in _LEN_KINDS:
                raise AnnotationError(
                    f"Len applies to str/bytes/containers — got `{kind}` in {where}"
                )
            ln = inner.setdefault("len", {})
            for src, dst in (
                ("min_length", "min"),
                ("max_length", "max"),
                ("min_items", "min"),
                ("max_items", "max"),
            ):
                if cons.get(src) is not None:
                    ln[dst] = cons[src]
        if cons.get("pattern") is not None:
            if kind != "str":
                raise AnnotationError(f"Pattern applies to str — got `{kind}` in {where}")
            inner["pattern"] = cons["pattern"]
        if cons.get("unique"):
            if kind != "list":
                raise AnnotationError(f"Unique applies to list — got `{kind}` in {where}")
            inner["unique"] = True
        transforms = cons.get("transforms")
        if transforms:
            node.setdefault("meta", {}).setdefault("transforms", {}).update(transforms)

    # ---- dataclass / TypedDict records --------------------------------------

    def _record_field_meta(
        self,
        cls: type[Any],
        name: str,
        hint: Any,
        where: str,
        allow_default: bool,
    ) -> tuple[dict[str, Any], dict[str, Any]]:
        """`(node, meta)` for a dataclass/TypedDict field: marker handling
        without `Field`/`Alias`/`Default` (those are class-declared)."""
        base, markers = _peel_annotated(hint)
        node = self.type_spec(base, where)
        meta: dict[str, Any] = {}
        cons: dict[str, Any] = {}
        for marker in markers:
            if isinstance(marker, Description):
                meta["description"] = marker.text
            elif isinstance(marker, Example):
                meta["example"] = _literal_spec(marker.value)
            elif _is_deprecated(marker):
                meta["deprecated"] = True
            elif isinstance(marker, AfterValidator):
                node.setdefault("meta", {}).setdefault("after", []).append(self.cb_id(marker.fn))
            elif isinstance(marker, Default) and allow_default:
                node.setdefault("meta", {})["default"] = _literal_spec(marker.value)
            elif _is_constraint(marker):
                self._collect_constraint(cons, marker, where)
            else:
                raise AnnotationError(f"unsupported Annotated marker {marker!r} on {where}")
        self.apply_constraints(node, cons, where)
        del cls, name
        return node, meta

    def _dataclass_spec(self, cls: type[Any]) -> dict[str, Any]:
        if cls in self._building:
            raise AnnotationError(f"recursive dataclass {cls.__qualname__} is not supported")
        self._building = self._building | {cls}
        try:
            hints = get_type_hints(cls, include_extras=True)
            fields: list[dict[str, Any]] = []
            for f in dataclasses.fields(cls):
                if f.name not in hints:
                    continue
                where = f"field {cls.__qualname__}.{f.name}"
                node, meta = self._record_field_meta(
                    cls, f.name, hints[f.name], where, allow_default=True
                )
                if not f.init:
                    missing: Any = "omit"
                elif f.default_factory is not dataclasses.MISSING:  # type: ignore[comparison-overlap]
                    missing = "omit"  # `cls(**kwargs)` calls the factory itself.
                elif f.default is not dataclasses.MISSING:
                    missing = self._missing(f.default, None, node)
                else:
                    missing = "required"
                fields.append(
                    {
                        "name": f.name,
                        "type": node,
                        "alias": None,
                        "missing": missing,
                        "meta": meta or None,
                    }
                )
            type_params = [p.__name__ for p in getattr(cls, "__parameters__", ())]
            return {
                "kind": "record",
                "class": {"name": cls.__qualname__, "module": cls.__module__},
                "flavor": "dataclass",
                "fields": fields,
                "extra": "ignore",
                "rename": None,
                "type_params": type_params,
            }
        finally:
            self._building = self._building - {cls}

    def _typed_dict_spec(self, cls: Any) -> dict[str, Any]:
        if cls in self._building:
            raise AnnotationError(f"recursive TypedDict {cls.__qualname__} is not supported")
        self._building = self._building | {cls}
        try:
            hints = get_type_hints(cls, include_extras=True)
            # `__required_keys__` misreports under `from __future__ import
            # annotations` (PEP 563 leaves the qualifier a string, so CPython
            # marks every key required) — read the qualifier off the resolved
            # hint and fall back to `__total__` for unqualified fields.
            total = getattr(cls, "__total__", True)
            fields: list[dict[str, Any]] = []
            for name, hint in hints.items():
                base, markers = _peel_annotated(hint)
                qualifier = get_origin(base)
                if qualifier in (typing.Required, typing.NotRequired):
                    base = get_args(base)[0]
                    # Keep the Annotated markers for _record_field_meta's pass.
                    hint = Annotated[base, *markers] if markers else base
                missing = (
                    "omit"
                    if qualifier is typing.NotRequired
                    or (qualifier is not typing.Required and not total)
                    else "required"
                )
                where = f"field {cls.__qualname__}.{name}"
                node, meta = self._record_field_meta(cls, name, hint, where, allow_default=True)
                fields.append(
                    {
                        "name": name,
                        "type": node,
                        "alias": None,
                        "missing": missing,
                        "meta": meta or None,
                    }
                )
            return {
                "kind": "record",
                "class": {"name": cls.__qualname__, "module": cls.__module__},
                "flavor": "typed_dict",
                "fields": fields,
                "extra": "forbid",
                "rename": None,
                "type_params": [],
            }
        finally:
            self._building = self._building - {cls}


def _merge_field_spec(spec: FieldSpec, meta: dict[str, Any], cons: dict[str, Any]) -> None:
    if spec.description is not None:
        meta["description"] = spec.description
    if spec.example is not UNSET:
        meta["example"] = spec.example
    if spec.deprecated:
        meta["deprecated"] = True
    cons.update(
        {
            key: value
            for key, value in (
                ("ge", spec.ge),
                ("gt", spec.gt),
                ("le", spec.le),
                ("lt", spec.lt),
                ("min_length", spec.min_length),
                ("max_length", spec.max_length),
                ("min_items", spec.min_items),
                ("max_items", spec.max_items),
                ("pattern", spec.pattern),
                ("multiple_of", spec.multiple_of),
            )
            if value is not None
        }
    )
    if spec.unique:
        cons["unique"] = True


# ---- handler parameters -------------------------------------------------------


@dataclasses.dataclass
class ParamInfo:
    """One handler parameter's route-spec entry."""

    name: str
    source: str
    kind: str  # "pos_only" | "normal" | "kw_only"
    required: bool
    wire_name: str
    spec: dict[str, Any]
    descriptor: Descriptor | None
    default: Any = UNSET
    factory: Callable[[], Any] | None = None


def param_spec(
    builder: SpecBuilder,
    name: str,
    hint: Any,
    default: Any,
    pkind: object,
    path_names: frozenset[str],
    where: str,
) -> ParamInfo:
    """Convert one `inspect.Parameter` into its source/type spec."""
    base, markers = _peel_annotated(hint if hint is not inspect.Parameter.empty else Any)
    sig_default = default
    if isinstance(sig_default, Marker | FieldSpec):
        markers = [*markers, sig_default]
        sig_default = UNSET

    source_marker: Marker | None = None
    default_value: Any = UNSET
    factory: Callable[[], Any] | None = None
    alias: str | None = None
    meta: dict[str, Any] = {}
    cons: dict[str, Any] = {}
    for marker in markers:
        if isinstance(marker, Marker):
            if source_marker is not None:
                raise AnnotationError(
                    f"{where}: parameter `{name}` declares two source markers "
                    f"({type(source_marker).__name__} and {type(marker).__name__})"
                )
            source_marker = marker
            _merge_marker(marker, meta, cons)
            if marker.alias:
                alias = marker.alias
            if marker.default is not UNSET:
                default_value = marker.default
        elif isinstance(marker, FieldSpec):
            _merge_field_spec(marker, meta, cons)
            if marker.alias:
                alias = marker.alias
            if marker.default is not UNSET:
                default_value = marker.default
            if marker.default_factory is not None:
                factory = marker.default_factory
        elif isinstance(marker, Alias):
            alias = marker.name
        elif isinstance(marker, Default):
            default_value = marker.value
        elif isinstance(marker, Description):
            meta["description"] = marker.text
        elif isinstance(marker, Example):
            meta["example"] = _literal_spec(marker.value)
        elif _is_deprecated(marker):
            meta["deprecated"] = True
        elif isinstance(marker, AfterValidator | Discriminator) or _is_constraint(marker):
            pass  # applied onto the node below
        else:
            raise AnnotationError(f"{where}: unsupported Annotated marker {marker!r} on `{name}`")

    node_markers = [
        m
        for m in markers
        if not isinstance(m, Marker | FieldSpec | Alias | Default | Description | Example)
        and not _is_deprecated(m)
    ]
    if base is Request:
        source = "request"
        node: dict[str, Any] = {"kind": "raw"}
    elif base is UploadFile:
        source = "file"
        node = {"kind": "raw"}
    else:
        node = builder.type_spec(base, f"parameter `{name}` of {where}")
        source = (
            "path"
            if source_marker is None and name in path_names
            else (source_marker.source if source_marker is not None else _default_source(node))
        )
    if node_markers:
        builder.apply_markers(node, node_markers, where)
    builder.apply_constraints(node, cons, f"parameter `{name}` of {where}")
    if alias is not None:
        node.setdefault("meta", {})["alias"] = alias
    if meta:
        node.setdefault("meta", {}).update(
            {k: v for k, v in meta.items() if k not in node.get("meta", {})}
        )

    kind = "normal"
    if pkind is inspect.Parameter.POSITIONAL_ONLY:
        kind = "pos_only"
    elif pkind is inspect.Parameter.KEYWORD_ONLY:
        kind = "kw_only"

    if sig_default is not UNSET and default_value is UNSET:
        default_value = sig_default
    required = default_value is UNSET and factory is None
    if source == "path":
        if name not in path_names:
            raise AnnotationError(
                f"{where}: `{name}` is marked Path but does not appear in the route"
            )
        if node["kind"] not in ("int", "str"):
            raise AnnotationError(
                f"{where}: path parameter `{name}` must be annotated `int` or `str`, "
                f"got `{node['kind']}` (wider path types land with the extraction task)"
            )
        if not required:
            raise AnnotationError(f"{where}: path parameter `{name}` cannot have a default")

    if default_value is not UNSET and not (
        default_value is None or isinstance(default_value, bool | int | float | str | bytes)
    ):
        # Non-scalar defaults get a per-request copy, like model fields.
        factory = factory or functools.partial(copy.deepcopy, default_value)
        default_value = UNSET

    wire = alias or (name.replace("_", "-") if source == "header" else name)
    descriptor = compile_descriptor(node, builder.callbacks)
    return ParamInfo(
        name=name,
        source=source,
        kind=kind,
        required=required,
        wire_name=wire,
        spec=node,
        descriptor=descriptor,
        default=default_value,
        factory=factory,
    )


def _merge_marker(marker: Marker, meta: dict[str, Any], cons: dict[str, Any]) -> None:
    for key in (
        "ge",
        "gt",
        "le",
        "lt",
        "min_length",
        "max_length",
        "min_items",
        "max_items",
        "pattern",
        "multiple_of",
    ):
        value = getattr(marker, key)
        if value is not None:
            cons[key] = value
    if marker.unique:
        cons["unique"] = True
    if marker.description is not None:
        meta["description"] = marker.description
    if marker.example is not UNSET:
        meta["example"] = marker.example
    if marker.deprecated:
        meta["deprecated"] = True


def _default_source(node: dict[str, Any]) -> str:
    """No marker and not a path name: scalars → query, records/containers → body."""
    kind = node["kind"]
    if kind in _QUERY_KINDS:
        return "query"
    if kind == "optional":
        return _default_source(node["inner"])
    if kind == "union":
        return _default_source(node["variants"][0])
    return "body"


# ---- handler return annotations -------------------------------------------------


@dataclasses.dataclass
class ResponseVariant:
    """One member of a handler's declared response set."""

    status: int | None  # None = the dynamic `Response` escape
    body: dict[str, Any] | None


def response_spec(builder: SpecBuilder, hint: Any) -> list[ResponseVariant]:
    """`-> X` → the declared response set (docs/SPEC-FORMAT.md §4)."""
    if hint is inspect.Signature.empty:
        return []
    variants: list[ResponseVariant] = []
    _response_variants(builder, hint, variants)
    return variants


def _response_variants(builder: SpecBuilder, hint: Any, out: list[ResponseVariant]) -> None:
    base, _markers = _peel_annotated(hint)
    if base is None or base is _NoneType:
        out.append(ResponseVariant(204, None))
        return
    origin, args = get_origin(base), get_args(base)
    if origin in (typing.Union, pytypes.UnionType):
        for member in args:
            _response_variants(builder, member, out)
        return
    out.append(_one_response_variant(builder, base, origin, args))


def _one_response_variant(
    builder: SpecBuilder,
    base: Any,
    origin: Any,
    args: tuple[Any, ...],
) -> ResponseVariant:
    from fulmen.errors import HTTPError
    from fulmen.responses import Response

    for cls, cls_args in ((base, args), (origin, args)):
        if cls is None:
            continue
        if isinstance(cls, type) and issubclass(cls, HTTPError):
            body = builder.type_spec(cls_args[0], "response") if cls_args else None
            return ResponseVariant(cls.status_code, body)
        if cls is Response:
            return ResponseVariant(None, None)
        if isinstance(cls, type) and issubclass(cls, Response):
            status: int | None = getattr(cls, "status_code", None)
            body = builder.type_spec(cls_args[0], "response") if cls_args else None
            return ResponseVariant(status, body)
    return ResponseVariant(200, builder.type_spec(base, "response"))


def compile_model(cls: type[Any], rename: str | None = None) -> Descriptor:
    """Build `cls`'s record spec and compile it — Model's class descriptor."""
    builder = SpecBuilder()
    spec = builder.record_spec(cls)
    if rename is not None:
        spec["rename"] = rename
    descriptor = compile_descriptor(spec, builder.callbacks)
    cls.__descriptor__ = descriptor
    cls.__spec__ = spec
    cls.__field_set__ = frozenset(descriptor.field_names)
    return descriptor


def model_descriptor(cls: type[Any]) -> Descriptor:
    """`cls`'s descriptor, building it lazily when forward refs delayed it."""
    descriptor: Descriptor | None = getattr(cls, "__descriptor__", None)
    if descriptor is None:
        descriptor = compile_model(cls)
    return descriptor
