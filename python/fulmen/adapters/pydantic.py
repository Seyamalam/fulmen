"""Pydantic v2 adapter (DESIGN.md §11.8, §15).

A `pydantic.BaseModel` subclass in a handler signature or return
annotation becomes a Fulmen `record` spec node: `model_fields` is read
once at startup and its field info — annotations, aliases, `ge`/`le`/
`min_length`/… metadata, defaults and `default_factory`, `model_config` —
is translated into the same spec tree a `Model` produces. Validation,
materialization, response encoding and OpenAPI generation then run
through the native descriptor machinery; `pydantic-core` is never
invoked on the request path.

Detection is duck-typed (`model_fields` + `__pydantic_serializer__`), so
importing this module never imports `pydantic`; the adapter only needs
the package when a user actually holds a `BaseModel` subclass — which
already implies it is installed. `PydanticValidator` is the explicit
handle (DESIGN.md §15.1 `Validator` protocol) and raises a
``pip install fulmen[pydantic]``-style error when the extra is missing.

Materialization: the `record` materializer creates instances with
`cls.__new__` + `object.__setattr__` (no `__init__`), which leaves a
`BaseModel` without its internals (`__pydantic_fields_set__`,
`__pydantic_extra__`, `__pydantic_private__`). A per-record `meta.after`
finalize callback — the same hook `AfterValidator` uses — restores them
in the exact order `BaseModel.model_construct` uses, then runs
`model_post_init` and `model_validator(mode="after")` hooks.

Documented deviations from real pydantic validation (all surfaced at
startup or via `AnnotationError` — never silently at request time):

* `BeforeValidator`/`field_validator(mode="before")` run *after* native
  validation (the spec IR has no pre-hook); a function depending on the
  raw wire shape may misbehave. `wrap`/`plain` validators,
  `model_validator(mode="before"|"wrap")`, `WrapValidator`,
  `PlainValidator`, `SkipValidation`, `InstanceOf`, `Tag`,
  `AliasChoices`, `AliasPath`, `decimal_places`/`max_digits` and
  `allow_inf_nan=True` raise `AnnotationError` at spec build.
* `populate_by_name`/`validate_by_name` are ignored — the wire accepts
  the declared alias only.
* `serialization_alias` is ignored — encoded output uses the validation
  alias (like `model_dump(by_alias=True)` with the input alias).
* `model_fields_set` reports every materialized field (Fulmen injects
  literal defaults natively), so `model_dump(exclude_unset=True)`
  includes defaults.
* `computed_field`s and `field_serializer`/`model_serializer`s are not
  part of the spec — they do not appear in responses; `model_dump()` on
  the materialized instance still runs pydantic's own serializer.
* Recursive models raise `AnnotationError` (the schema IR has no
  reference type yet) — same as a recursive `Model`.
"""

from __future__ import annotations

import copy
import functools
import inspect
import pathlib
import sys
import types as pytypes
import typing
from typing import Annotated, Any, get_args, get_origin

from fulmen._fulmen import compile_descriptor
from fulmen._typing import AnnotationError, SpecBuilder
from fulmen.constraints import (
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

if typing.TYPE_CHECKING:
    from fulmen._fulmen import Descriptor

# Marker classes `Annotated` conversion passes through untouched — a
# user may already write Fulmen markers inside a pydantic model.
_FULMEN_MARKERS = (
    FieldSpec,
    Alias,
    Default,
    Description,
    Example,
    AfterValidator,
    Discriminator,
    Len,
    Range,
    Pattern,
    MultipleOf,
    Unique,
    Strip,
    Lower,
    Upper,
    Deprecated,
)
_BARE_MARKERS = (Strip, Lower, Upper, Deprecated)  # legal as bare classes too


def _is_fulmen_marker(item: Any) -> bool:
    return isinstance(item, _FULMEN_MARKERS) or item in _BARE_MARKERS


# metadata attr → spec constraint key. `annotated_types`, pydantic's
# `StringConstraints` and its internal `_PydanticGeneralMetadata` all use
# these attribute names, so constraints are read by attribute, not by
# class identity — that keeps the mapping version-tolerant.
_CONS_ATTRS = (
    "ge",
    "gt",
    "le",
    "lt",
    "multiple_of",
    "min_length",
    "max_length",
    "min_items",
    "max_items",
    "pattern",
)
_TRANSFORM_ATTRS = (
    ("strip_whitespace", "strip"),
    ("to_lower", "lower"),
    ("to_upper", "upper"),
)
# Metadata attributes that exist but cannot be expressed in the spec IR —
# a set value is an `AnnotationError`, never a silent divergence.
_UNSUPPORTED_ATTRS = ("decimal_places", "max_digits")

# pydantic metadata class names → how the adapter treats them. Anything
# else whose module is `pydantic.*`/`annotated_types` and carries no
# constraint attributes is an `AnnotationError`; user-defined metadata
# (other modules) is ignored, matching pydantic's own tolerance.
_PYDANTIC_METADATA = {
    "AfterValidator": "after",
    "BeforeValidator": "before",
    "WrapValidator": "error",
    "PlainValidator": "error",
    "SkipValidation": "error",
    "InstanceOf": "error",
    "ValidateAs": "error",
    "Tag": "error",
    "Discriminator": "discriminator",
    "Strict": "noop",
    "FailFast": "error",
    "PydanticUseDefault": "error",
    "PydanticDescriptorProxy": "error",
    "ImportString": "error",
}
_ALIAS_CLASSES = {"AliasChoices", "AliasPath"}
_UNSUPPORTED_TYPE_NAMES = {
    "SecretStr",
    "SecretBytes",
    "Secret",
    "Json",
    "PaymentCardNumber",
    "ByteSize",
    "EncodedStr",
    "EncodedBytes",
    "Base64Str",
    "Base64Bytes",
    "NameEmail",
}
_URL_TYPES = {
    "AnyUrl",
    "AnyHttpUrl",
    "HttpUrl",
    "FileUrl",
    "FtpPath",
    "FtpUrl",
    "PostgresDsn",
    "CockroachDsn",
    "AmqpDsn",
    "RedisDsn",
    "MongoDsn",
    "KafkaDsn",
    "NatsDsn",
    "MySQLDsn",
    "MariaDBDsn",
    "ClickHouseDsn",
    "SnowflakeDsn",
}
_EMAIL_TYPES = {"EmailStr"}
_STRICT_TYPES = {
    "StrictStr": "str",
    "StrictInt": "int",
    "StrictFloat": "float",
    "StrictBool": "bool",
}
_PATH_TYPES = {"Path", "FilePath", "DirectoryPath", "NewPath"}


def _require_pydantic() -> Any:
    try:
        import pydantic
    except ImportError as e:
        raise ImportError(
            "pydantic support is an optional dependency — "
            "install it with `pip install fulmen[pydantic]`"
        ) from e
    return pydantic


def is_pydantic_model(tp: Any) -> bool:
    """`True` for a `pydantic.BaseModel` subclass — duck-typed, so no
    `pydantic` import is needed to rule a type out."""
    return (
        isinstance(tp, type)
        and isinstance(getattr(tp, "model_fields", None), dict)
        and hasattr(tp, "model_config")
        and hasattr(tp, "__pydantic_serializer__")
    )


def _is_undefined(value: Any) -> bool:
    return type(value).__name__ in ("PydanticUndefined", "PydanticPlaceholder")


def _one_arg(fn: Any, where: str) -> Any:
    """Check a `(value)`-shape validator callable — `(value, info)`-style
    signatures (a second *required* parameter) are rejected because there
    is no `ValidationInfo` to pass."""
    try:
        inspect.signature(fn).bind(object())
    except TypeError:
        raise AnnotationError(
            f"{where}: validator {getattr(fn, '__qualname__', fn)!r} requires a "
            "ValidationInfo/context parameter — not supported by the adapter"
        ) from None
    except (ValueError, AttributeError):
        pass  # builtins/partials without introspectable signatures — call optimistically
    return fn


def _pydantic_type_kind(builder: SpecBuilder, tp: Any, where: str) -> dict[str, Any] | None:
    """Map pydantic-specific field types (`AnyUrl`, `EmailStr`, `Path`…)
    to spec nodes, or raise for ones with no native equivalent."""
    name = getattr(tp, "__name__", type(tp).__name__)
    module = getattr(tp, "__module__", "") or ""
    if not module.startswith("pydantic"):
        if name in _PATH_TYPES and isinstance(tp, type) and issubclass(tp, pathlib.PurePath):
            # A `str` on the wire that materializes as a real Path.
            return {"kind": "str", "meta": {"after": [builder.cb_id(pathlib.Path)]}}
        return None
    if name in _URL_TYPES:
        return {"kind": "url"}
    if name in _EMAIL_TYPES:
        return {"kind": "email"}
    if name in _STRICT_TYPES:
        return {"kind": _STRICT_TYPES[name]}
    if name in _UNSUPPORTED_TYPE_NAMES:
        raise AnnotationError(f"{where}: pydantic `{name}` is not supported by the adapter")
    if name in _PATH_TYPES and isinstance(tp, type) and issubclass(tp, pathlib.PurePath):
        return {"kind": "str", "meta": {"after": [builder.cb_id(pathlib.Path)]}}
    return None


def _convert_metadata_item(
    builder: SpecBuilder,
    item: Any,
    where: str,
    cons: dict[str, Any],
    after: list[int],
    node: dict[str, Any],
) -> None:
    """Fold one `FieldInfo.metadata` entry into `cons`/`after`/`node`."""
    if _is_field_info(item):
        builder.apply_markers(node, _convert_field_info(item, where), where)
        return
    if _is_fulmen_marker(item):
        builder.apply_markers(node, [item], where)
        return
    name = type(item).__name__
    module = type(item).__module__ or ""
    treatment = _PYDANTIC_METADATA.get(name)
    if treatment == "error":
        raise AnnotationError(f"{where}: pydantic `{name}` is not supported by the adapter")
    if treatment == "noop":
        return
    if treatment == "discriminator":
        tag = getattr(item, "discriminator", None)
        if not isinstance(tag, str):
            raise AnnotationError(f"{where}: pydantic Discriminator needs a field name")
        builder.apply_markers(node, [Discriminator(tag)], where)
        return
    if treatment in ("after", "before"):
        # `before` runs post-validation — a documented deviation.
        after.append(builder.cb_id(_one_arg(getattr(item, "func", None), where)))
        return
    consumed = False
    for attr in _CONS_ATTRS:
        value = getattr(item, attr, None)
        if value is not None:
            if attr == "pattern" and not isinstance(value, str):
                # `Field(pattern=re.compile(...))` — the wire needs the
                # source text, same as the `Annotated` path unwraps it.
                value = getattr(value, "pattern", value)
            cons[attr] = value
            consumed = True
    for attr, transform in _TRANSFORM_ATTRS:
        if getattr(item, attr, False):
            cons.setdefault("transforms", {})[transform] = True
            consumed = True
    for attr in _UNSUPPORTED_ATTRS:
        if getattr(item, attr, None) is not None:
            raise AnnotationError(f"{where}: pydantic `{attr}` cannot be expressed natively")
    if getattr(item, "allow_inf_nan", False):
        raise AnnotationError(f"{where}: allow_inf_nan=True cannot be expressed natively")
    if consumed:
        return
    if module.startswith(("annotated_types", "pydantic")):
        raise AnnotationError(f"{where}: unsupported pydantic metadata {item!r}")
    # User-defined Annotated metadata (not a validation contract): ignore.


def _is_field_info(item: Any) -> bool:
    """`pydantic.fields.FieldInfo` — appears inside nested `Annotated`
    metadata (`list[Annotated[int, Field(ge=2)]]`)."""
    return (
        type(item).__name__ == "FieldInfo"
        and hasattr(item, "metadata")
        and hasattr(item, "is_required")
    )


def _convert_field_info(info: Any, where: str) -> list[Any]:
    """A nested `FieldInfo` → markers from its own metadata + doc attrs."""
    out: list[Any] = []
    for sub in info.metadata:
        out.extend(_convert_annotated_marker(sub, where))
    if info.description is not None:
        out.append(Description(info.description))
    examples = getattr(info, "examples", None)
    if examples:
        out.append(Example(examples[0] if isinstance(examples, (list, tuple)) else examples))
    if info.deprecated:
        out.append(Deprecated())
    if info.discriminator is not None:
        out.append(Discriminator(info.discriminator))
    return out


def _convert_annotated_marker(marker: Any, where: str) -> list[Any]:
    """One nested-`Annotated` metadata item → Fulmen marker objects."""
    if _is_fulmen_marker(marker):
        return [marker]
    if _is_field_info(marker):
        return _convert_field_info(marker, where)
    name = type(marker).__name__
    module = type(marker).__module__ or ""
    treatment = _PYDANTIC_METADATA.get(name)
    if treatment == "error":
        raise AnnotationError(f"{where}: pydantic `{name}` is not supported by the adapter")
    if treatment == "noop":
        return []
    if treatment == "discriminator":
        tag = getattr(marker, "discriminator", None)
        if not isinstance(tag, str):
            raise AnnotationError(f"{where}: pydantic Discriminator needs a field name")
        return [Discriminator(tag)]
    if treatment in ("after", "before"):
        return [AfterValidator(_one_arg(getattr(marker, "func", None), where))]
    out: list[Any] = []
    rng = {
        k: getattr(marker, k, None)
        for k in ("ge", "gt", "le", "lt")
        if getattr(marker, k, None) is not None
    }
    if rng:
        out.append(Range(**rng))
    multiple_of = getattr(marker, "multiple_of", None)
    if multiple_of is not None:
        out.append(MultipleOf(multiple_of))
    min_len = getattr(marker, "min_length", None)
    if min_len is None:
        min_len = getattr(marker, "min_items", None)
    max_len = getattr(marker, "max_length", None)
    if max_len is None:
        max_len = getattr(marker, "max_items", None)
    if min_len is not None or max_len is not None:
        out.append(Len(min=min_len, max=max_len))
    pat = getattr(marker, "pattern", None)
    if pat is not None:
        out.append(Pattern(pat if isinstance(pat, str) else pat.pattern))
    if getattr(marker, "strip_whitespace", False):
        out.append(Strip())
    if getattr(marker, "to_lower", False):
        out.append(Lower())
    if getattr(marker, "to_upper", False):
        out.append(Upper())
    for attr in _UNSUPPORTED_ATTRS:
        if getattr(marker, attr, None) is not None:
            raise AnnotationError(f"{where}: pydantic `{attr}` cannot be expressed natively")
    if getattr(marker, "allow_inf_nan", False):
        raise AnnotationError(f"{where}: allow_inf_nan=True cannot be expressed natively")
    if out:
        return out
    if module.startswith(("annotated_types", "pydantic")):
        raise AnnotationError(f"{where}: unsupported pydantic metadata {marker!r}")
    return []


def _rewrite_hint(hint: Any, where: str) -> Any:
    """Re-express a pydantic-resolved annotation with Fulmen markers.

    Pydantic strips the outer `Annotated` into `FieldInfo.metadata` but
    leaves `Annotated[...]` wrappers inside nested positions
    (`list[Annotated[int, Ge(0)]]`); each wrapper's `annotated_types` /
    pydantic metadata becomes our `Range`/`Len`/`Pattern`/… markers so
    `SpecBuilder.type_spec` handles the whole tree.
    """
    base, markers = hint, []
    if get_origin(hint) is Annotated:
        args = get_args(hint)
        base, markers = args[0], list(args[1:])
    if markers:
        converted: list[Any] = []
        for marker in markers:
            converted.extend(_convert_annotated_marker(marker, where))
        rewritten = _rewrite_hint(base, where)
        # A noop-only marker list (`Annotated[int, Strict()]`) leaves nothing
        # to wrap — `Annotated[int]` alone is a TypeError.
        return Annotated[rewritten, *converted] if converted else rewritten
    origin = get_origin(hint)
    if origin is None or origin is typing.Literal:
        return hint
    args = get_args(hint)
    new_args = tuple(_rewrite_hint(a, where) for a in args)
    if new_args == args:
        return hint
    if origin in (typing.Union, pytypes.UnionType):
        return functools.reduce(lambda a, b: a | b, new_args)
    if hasattr(hint, "copy_with"):
        return hint.copy_with(new_args)
    return origin[new_args]


def _missing_policy(builder: SpecBuilder, field: Any) -> Any:
    if field.is_required():
        return "required"
    factory = getattr(field, "default_factory", None)
    if factory is not None:
        return {"factory": builder.cb_id(factory)}
    default = field.default
    if _is_undefined(default):
        return "required"
    if default is None or isinstance(default, bool | int | float | str | bytes):
        return {"literal": default}
    # Non-scalar defaults get a per-instance copy — never a shared mutable.
    return {"factory": builder.cb_id(functools.partial(copy.deepcopy, default))}


def _wire_alias(cls: type[Any], name: str, field: Any, where: str) -> str | None:
    validation = getattr(field, "validation_alias", None)
    if validation is not None:
        if isinstance(validation, str):
            return validation
        if type(validation).__name__ in _ALIAS_CLASSES:
            raise AnnotationError(
                f"{where}: {type(validation).__name__} is not supported — use a plain alias string"
            )
    if isinstance(field.alias, str):
        return field.alias
    config: dict[str, Any] = dict(getattr(cls, "model_config", None) or {})
    generator: Any = config.get("alias_generator")
    if generator is None:
        return None
    if callable(generator):
        produced = generator(name)
        return produced if isinstance(produced, str) else None
    for attr in ("validation_alias", "alias"):
        fn = getattr(generator, attr, None)
        if callable(fn):
            produced = fn(name)
            if isinstance(produced, str):
                return produced
    return None


def _field_spec(
    builder: SpecBuilder,
    cls: type[Any],
    name: str,
    field: Any,
    decorators: dict[str, list[Any]],
    config: dict[str, Any],
) -> dict[str, Any]:
    where = f"field {cls.__qualname__}.{name}"
    special = _pydantic_type_kind(builder, field.annotation, where)
    hint = field.annotation if special is not None else _rewrite_hint(field.annotation, where)
    node = special or builder.type_spec(hint, where)
    cons: dict[str, Any] = {}
    after: list[int] = []
    for item in field.metadata:
        _convert_metadata_item(builder, item, where, cons, after, node)
    for dec in decorators.get(name, []):
        if dec.info.mode in ("wrap", "plain"):
            raise AnnotationError(
                f"{where}: field_validator(mode={dec.info.mode!r}) is not supported"
            )
        # `before` runs post-validation — a documented deviation.
        after.append(builder.cb_id(_one_arg(dec.func, where)))
    builder.apply_constraints(node, cons, where)
    # model_config str_* flags apply to every str field.
    inner = node
    while inner["kind"] in ("optional", "newtype") and "inner" in inner:
        inner = inner["inner"]
    if inner["kind"] == "str":
        if config.get("str_strip_whitespace"):
            node.setdefault("meta", {}).setdefault("transforms", {})["strip"] = True
        if config.get("str_to_lower"):
            node.setdefault("meta", {}).setdefault("transforms", {})["lower"] = True
        if config.get("str_to_upper"):
            node.setdefault("meta", {}).setdefault("transforms", {})["upper"] = True
        for src, dst in (("str_min_length", "min"), ("str_max_length", "max")):
            if config.get(src) is not None and dst not in inner.get("len", {}):
                inner.setdefault("len", {})[dst] = config[src]
    if field.discriminator is not None:
        builder.apply_markers(node, [Discriminator(field.discriminator)], where)
    meta: dict[str, Any] = {}
    if field.description is not None:
        meta["description"] = field.description
    examples = getattr(field, "examples", None)
    if examples:
        meta["example"] = examples[0] if isinstance(examples, (list, tuple)) else examples
    if field.deprecated:
        meta["deprecated"] = True
    if isinstance(field.title, str):
        meta["title"] = field.title
    if after:
        node.setdefault("meta", {}).setdefault("after", []).extend(after)
    return {
        "name": name,
        "type": node,
        "alias": _wire_alias(cls, name, field, where),
        "missing": _missing_policy(builder, field),
        "meta": meta or None,
    }


def _make_finalize(
    cls: type[Any],
    field_names: frozenset[str],
    model_validators: list[Any],
    keep_extras: bool,
) -> Any:
    """The record-level `meta.after` hook: restore `BaseModel` internals
    on the `__new__`-materialized instance, in `model_construct` order.

    Guarded — the materializer can run record `meta.after` hooks more
    than once, so `__pydantic_fields_set__` doubles as the finalized flag.
    """

    def finalize(obj: Any) -> Any:
        if getattr(obj, "__pydantic_fields_set__", None) is not None:
            return obj
        data = obj.__dict__
        extras = {k: v for k, v in data.items() if k not in field_names}
        for key in extras:
            del data[key]
        object.__setattr__(obj, "__pydantic_fields_set__", set(data))
        object.__setattr__(obj, "__pydantic_extra__", extras if keep_extras else None)
        object.__setattr__(obj, "__pydantic_private__", None)
        post_init = getattr(cls, "model_post_init", None)
        if post_init is not None:
            obj.model_post_init(None)
        for dec in model_validators:
            result = dec.func(obj)
            if result is not None:
                obj = result
        return obj

    return finalize


def _decorators(cls: type[Any]) -> tuple[dict[str, list[Any]], list[Any]]:
    """`cls.__pydantic_decorators__` → (per-field validators, after-model
    validators). `model_serializer`/`field_serializer`/`computed_field`
    are ignored (they live outside the spec — documented deviation)."""
    per_field: dict[str, list[Any]] = {}
    model_after: list[Any] = []
    info = getattr(cls, "__pydantic_decorators__", None)
    if info is None:
        return per_field, model_after
    for dec in info.field_validators.values():
        for fname in dec.info.fields:
            per_field.setdefault(fname, []).append(dec)
    for dec in info.model_validators.values():
        if dec.info.mode != "after":
            raise AnnotationError(
                f"{cls.__qualname__}: model_validator(mode={dec.info.mode!r}) "
                "is not supported by the adapter"
            )
        model_after.append(dec)
    return per_field, model_after


def _ensure_resolvable(cls: type[Any]) -> None:
    """Bind `cls` at its `module:qualname` so the descriptor's class
    resolution (`import module; getattr(module, qualname)`) can find it.

    Pydantic creates generic specializations (`Page[int]`) and
    `model_rebuild` copies dynamically — they are real classes but are
    not bound in their module namespace. Function-local classes
    (`<locals>` in the qualname) cannot be made resolvable; materializing
    those raises at request time, the same limitation a local `Model`
    class has.
    """
    module = sys.modules.get(cls.__module__)
    parts = cls.__qualname__.split(".")
    if module is None or "<locals>" in parts:
        return
    parent: Any = module
    for part in parts[:-1]:
        parent = getattr(parent, part, None)
        if parent is None:
            return
    if getattr(parent, parts[-1], None) is not cls:
        setattr(parent, parts[-1], cls)


def model_spec(builder: SpecBuilder, cls: type[Any], where: str) -> dict[str, Any]:
    """A `BaseModel` subclass → a `record` spec node (flavor `model`)."""
    with builder.record_build(cls) as recursive:
        if recursive:
            raise AnnotationError(
                f"recursive pydantic model {cls.__qualname__} is not supported "
                "(the schema IR has no reference type yet)"
            )
        if getattr(cls, "__pydantic_root_model__", False):
            return _root_model_spec(builder, cls, where)
        _ensure_resolvable(cls)
        config: dict[str, Any] = dict(getattr(cls, "model_config", None) or {})
        per_field, model_validators = _decorators(cls)
        fields: list[dict[str, Any]] = []
        for name, field in cls.model_fields.items():
            fields.append(_field_spec(builder, cls, name, field, per_field, config))
        extra = config.get("extra", "ignore")
        extra_policy = {"ignore": "ignore", "forbid": "forbid", "allow": "keep"}.get(extra)
        if extra_policy is None:
            raise AnnotationError(f"{where}: model_config extra={extra!r} is not supported")
        meta: dict[str, Any] = {
            "after": [
                builder.cb_id(
                    _make_finalize(
                        cls, frozenset(cls.model_fields), model_validators, extra == "allow"
                    )
                )
            ]
        }
        # `cls.__doc__` (not inspect.getdoc — that inherits) → pydantic's
        # own "docstring becomes schema description" convention.
        if cls.__doc__:
            meta["description"] = inspect.cleandoc(cls.__doc__)
        type_params = [p.__name__ for p in getattr(cls, "__parameters__", ())]
        return {
            "kind": "record",
            "class": {"name": cls.__qualname__, "module": cls.__module__},
            "flavor": "model",
            "fields": fields,
            "extra": extra_policy,
            "rename": None,
            "type_params": type_params,
            "meta": meta,
        }


def _root_model_spec(builder: SpecBuilder, cls: type[Any], where: str) -> dict[str, Any]:
    """`RootModel[T]` → the root field's node plus a wrap-after hook
    (`model_construct` materializes a real `RootModel` instance)."""
    field = cls.model_fields["root"]
    node = builder.type_spec(_rewrite_hint(field.annotation, f"{where}.root"), f"{where}.root")
    cons: dict[str, Any] = {}
    after: list[int] = []
    for item in field.metadata:
        _convert_metadata_item(builder, item, where, cons, after, node)
    builder.apply_constraints(node, cons, where)

    def wrap_root(value: Any) -> Any:
        return cls.model_construct(root=value)

    after.append(builder.cb_id(wrap_root))
    node.setdefault("meta", {}).setdefault("after", []).extend(after)
    return node


def model_spec_if_pydantic(builder: SpecBuilder, hint: Any, where: str) -> dict[str, Any] | None:
    """`_base_spec` hook: `hint` → its spec node, `None` when `hint` is
    not a `BaseModel` subclass or a mapped pydantic field type."""
    special = _pydantic_type_kind(builder, hint, where)
    if special is not None:
        return special
    if is_pydantic_model(hint):
        return model_spec(builder, hint, where)
    return None


def generic_spec_if_pydantic(
    builder: SpecBuilder, origin: Any, args: tuple[Any, ...], where: str
) -> dict[str, Any] | None:
    """`_alias_spec` hook: `P[T]` (unbound typevars left) → a `generic`
    node like a generic `Model` produces; a concrete `P[int]` is already
    a real class and is handled by `model_spec_if_pydantic`."""
    if not is_pydantic_model(origin):
        return None
    return {
        "kind": "generic",
        "origin": model_spec(builder, origin, where),
        "args": [builder.type_spec(a, where) for a in args],
    }


class PydanticValidator:
    """The `Validator`-protocol handle for pydantic models (DESIGN §15.1).

    The `Fulmen` app detects `BaseModel` subclasses automatically — this
    class exists for explicit use (`supports`, `compile`, `json_schema`)
    and to fail loudly with the extra's install hint when `pydantic` is
    absent.
    """

    @property
    def conformance_types(self) -> tuple[Any, Any]:
        """(supported, unsupported) probe pair for the testkit's
        `Validator` suite (DESIGN.md §15.3)."""
        pydantic = _require_pydantic()
        model = pydantic.create_model("_ConformanceProbe", value=(int, ...))
        return (model, int)

    def __init__(self) -> None:
        _require_pydantic()

    @staticmethod
    def supports(tp: Any) -> bool:
        return is_pydantic_model(tp)

    @staticmethod
    def spec(tp: type[Any]) -> dict[str, Any]:
        """The spec tree `type_spec(tp)` produces for `tp`."""
        if not is_pydantic_model(tp):
            raise AnnotationError(f"{tp!r} is not a pydantic BaseModel subclass")
        return model_spec(SpecBuilder(), tp, "annotation")

    def compile(self, tp: type[Any]) -> Descriptor:
        """Compile `tp` into a native `Descriptor` — pydantic's own
        validator is never used."""
        if not is_pydantic_model(tp):
            raise AnnotationError(f"{tp!r} is not a pydantic BaseModel subclass")
        builder = SpecBuilder()
        return compile_descriptor(model_spec(builder, tp, "annotation"), builder.callbacks)

    def json_schema(self, tp: type[Any]) -> dict[str, Any]:
        """The spec-derived JSON Schema for `tp` (what OpenAPI emits)."""
        from fulmen.openapi import _SchemaGen  # pyright: ignore[reportPrivateUsage] -- package-internal reuse

        gen = _SchemaGen()
        schema = gen.schema(self.spec(tp))
        if gen.schemas:
            schema = {**schema, "components": {"schemas": gen.schemas}}
        return schema


__all__ = [
    "PydanticValidator",
    "generic_spec_if_pydantic",
    "is_pydantic_model",
    "model_spec",
    "model_spec_if_pydantic",
]
