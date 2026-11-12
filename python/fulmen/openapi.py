"""OpenAPI 3.1 generation from the schema IR (DESIGN.md §11.3, §23.2).

The spec trees `_typing.py` builds (docs/SPEC-FORMAT.md) already describe
every parameter and response body — this module renders them into an
OpenAPI 3.1 document. That means the JSON Schema 2020-12 dialect: `type`
arrays for nullable (never 3.0's `nullable: true`), numeric
`exclusiveMinimum`/`exclusiveMaximum`, `const`/`enum`, `prefixItems`,
`oneOf` + `discriminator` for `Discriminator("field")` unions, `anyOf`
otherwise.

Generation runs once at compile time (`Fulmen._compile_native`): the IR
is frozen by then, so `GET /openapi.json` serves a pre-rendered document
through a synthetic route — zero per-request cost, and nothing is
registered when `openapi_url=None`.

The docs UI is Swagger UI, served from the app itself (ROADMAP M2.11:
"self-hosted assets"). The assets are `swagger-ui-dist` (Apache-2.0)
pinned below and vendored under `fulmen/static` gzip-compressed — the
repo file-size guardrail is 512 KiB — and decompressed once at compile
time, so clients never need `Accept-Encoding` support.
"""

from __future__ import annotations

import base64
import dataclasses
import gzip
import html
import inspect
import json
from collections.abc import Callable, Iterable, Mapping, Sequence
from http import HTTPStatus
from pathlib import Path
from typing import Any, TypedDict, cast

#: The vendored Swagger UI bundle version (`swagger-ui-dist`, Apache-2.0,
#: https://github.com/swagger-api/swagger-ui). Bump by replacing the
#: `.gz` files under `fulmen/static` and this constant.
SWAGGER_UI_VERSION = "5.34.1"

_STATIC = Path(__file__).with_name("static")

_OPENAPI_VERSION = "3.1.0"

#: Where a call-arg source can land in `parameters`. `body`/`form`/`file`
#: become `requestBody`; `request` is framework-injected and invisible.
_PARAMETER_SOURCES = frozenset({"path", "query", "header", "cookie"})

_HTTP_METHOD_ORDER = ("get", "put", "post", "delete", "options", "head", "patch", "trace")

_FORMATS = {
    "datetime": "date-time",
    "date": "date",
    "time": "time",
    "timedelta": "duration",
    "uuid": "uuid",
    "email": "email",
    "url": "uri",
    "ipv4": "ipv4",
    "ipv6": "ipv6",
}


@dataclasses.dataclass(frozen=True)
class OperationMeta:
    """Per-route documentation options collected by the decorators."""

    tags: tuple[str, ...] = ()
    summary: str | None = None
    description: str | None = None
    operation_id: str | None = None
    deprecated: bool = False
    include_in_schema: bool = True


class RouteOptions(TypedDict, total=False):
    """The keyword arguments every route decorator accepts."""

    tags: Iterable[str]
    summary: str
    description: str
    operation_id: str
    deprecated: bool
    include_in_schema: bool


def operation_meta(options: Mapping[str, object]) -> OperationMeta:
    """Validate decorator `**options` into an [`OperationMeta`]."""
    unknown = set(options) - RouteOptions.__annotations__.keys()
    if unknown:
        joined = ", ".join(sorted(unknown))
        raise TypeError(f"unknown route option(s): {joined}")
    tag_list: list[str] = []
    tags = options.get("tags")
    if tags is not None:
        if isinstance(tags, str) or not isinstance(tags, Iterable):
            raise TypeError("route option `tags` must be an iterable of str")
        for tag in cast(Iterable[Any], tags):
            if not isinstance(tag, str):
                raise TypeError("route option `tags` must be an iterable of str")
            tag_list.append(tag)
    return OperationMeta(
        tags=tuple(tag_list),
        summary=_opt_str(options, "summary"),
        description=_opt_str(options, "description"),
        operation_id=_opt_str(options, "operation_id"),
        deprecated=bool(options.get("deprecated", False)),
        include_in_schema=bool(options.get("include_in_schema", True)),
    )


def _opt_str(options: Mapping[str, object], key: str) -> str | None:
    value = options.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        raise TypeError(f"route option `{key}` must be a str")
    return value


@dataclasses.dataclass(frozen=True)
class ApiInfo:
    """The `info`/`servers`/`tags` app-level document metadata."""

    title: str
    version: str
    description: str | None = None
    contact: Mapping[str, Any] | None = None
    license: Mapping[str, Any] | None = None
    servers: Sequence[str | Mapping[str, Any]] = ()
    tags: Sequence[str | Mapping[str, Any]] = ()


# One call_args entry with the descriptor resolved to its spec dict:
# (parameter name, source, signature kind, wire name, spec|None,
#  (missing-kind, missing-object)) — SPEC-FORMAT.md §5.
CallArgSpec = tuple[str, str, str, str, dict[str, Any] | None, tuple[str, object]]


@dataclasses.dataclass(frozen=True)
class RouteEntry:
    """Everything generation needs from one inspected route."""

    method: str
    path: str
    call_args: tuple[CallArgSpec, ...]
    responses: tuple[tuple[int | None, dict[str, Any] | None], ...]
    handler: Callable[..., object]
    meta: OperationMeta


def generate(info: ApiInfo, routes: Sequence[RouteEntry]) -> dict[str, Any]:
    """Build the OpenAPI 3.1 document for `info` + `routes`."""
    gen = _SchemaGen()
    used_ids: set[str] = set()
    paths: dict[str, dict[str, Any]] = {}
    for entry in routes:
        if not entry.meta.include_in_schema:
            continue
        paths.setdefault(_openapi_path(entry.path), {})[entry.method.lower()] = _operation(
            entry, gen, used_ids
        )
    info_obj: dict[str, Any] = {"title": info.title, "version": info.version}
    if info.description is not None:
        info_obj["description"] = info.description
    if info.contact is not None:
        info_obj["contact"] = dict(info.contact)
    if info.license is not None:
        info_obj["license"] = dict(info.license)
    document: dict[str, Any] = {
        "openapi": _OPENAPI_VERSION,
        "info": info_obj,
        "paths": {
            path: {m: ops[m] for m in _HTTP_METHOD_ORDER if m in ops}
            for path, ops in sorted(paths.items())
        },
    }
    if info.servers:
        document["servers"] = [{"url": s} if isinstance(s, str) else dict(s) for s in info.servers]
    if info.tags:
        document["tags"] = [{"name": t} if isinstance(t, str) else dict(t) for t in info.tags]
    if gen.schemas:
        document["components"] = {
            "schemas": {name: gen.schemas[name] for name in sorted(gen.schemas)}
        }
    return document


def render(document: Mapping[str, Any]) -> bytes:
    """The document's wire form — compact, deterministic, UTF-8."""
    return json.dumps(document, ensure_ascii=False, separators=(",", ":")).encode()


def _openapi_path(path: str) -> str:
    """`/files/{*path}` → `/files/{path}` — OpenAPI has no catch-all marker."""
    return path.replace("{*", "{")


def _operation(entry: RouteEntry, gen: _SchemaGen, used_ids: set[str]) -> dict[str, Any]:
    op: dict[str, Any] = {}
    if entry.meta.summary:
        op["summary"] = entry.meta.summary
    description = entry.meta.description or inspect.getdoc(entry.handler)
    if description:
        op["description"] = description
    op["operationId"] = _operation_id(entry, used_ids)
    if entry.meta.tags:
        op["tags"] = list(entry.meta.tags)
    if entry.meta.deprecated:
        op["deprecated"] = True
    defaults = _sig_defaults(entry.handler)
    parameters = _parameters(entry, gen, defaults)
    if parameters:
        op["parameters"] = parameters
    request_body = _request_body(entry, gen, defaults)
    if request_body is not None:
        op["requestBody"] = request_body
    op["responses"] = _responses(entry, gen)
    return op


def _operation_id(entry: RouteEntry, used_ids: set[str]) -> str:
    base = entry.meta.operation_id or getattr(entry.handler, "__name__", None) or "handler"
    candidate = base
    if candidate in used_ids:
        candidate = f"{base}_{entry.method.lower()}"
    counter = 2
    while candidate in used_ids:
        candidate = f"{base}_{entry.method.lower()}_{counter}"
        counter += 1
    used_ids.add(candidate)
    return candidate


def _sig_defaults(handler: Callable[..., object]) -> Mapping[str, object]:
    """A handler's own signature defaults — the effective defaults of
    `omit`-policy params, which `call_args` deliberately does not carry."""
    try:
        parameters = inspect.signature(handler).parameters
    except (TypeError, ValueError):
        return {}
    return {
        name: p.default
        for name, p in parameters.items()
        if p.default is not inspect.Parameter.empty
    }


def _parameters(
    entry: RouteEntry,
    gen: _SchemaGen,
    defaults: Mapping[str, object],
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for name, source, _kind, wire_name, node, missing in entry.call_args:
        if source not in _PARAMETER_SOURCES:
            continue
        schema = gen.schema(node)
        parameter: dict[str, Any] = {
            "name": wire_name,
            "in": source,
            "required": source == "path" or missing[0] == "required",
            "schema": schema,
        }
        if missing[0] == "literal":
            schema["default"] = _jsonable(missing[1])
        elif missing[0] == "omit" and name in defaults:
            schema["default"] = _jsonable(defaults[name])
        meta = node.get("meta") if node is not None else None
        if meta:
            # Parameter-level homes read better than schema siblings here.
            if meta.get("description"):
                parameter["description"] = meta["description"]
                schema.pop("description", None)
            if meta.get("deprecated"):
                parameter["deprecated"] = True
                schema.pop("deprecated", None)
            if "example" in meta:
                parameter["example"] = _jsonable(meta["example"])
                schema.pop("examples", None)
        out.append(parameter)
    return out


def _request_body(
    entry: RouteEntry,
    gen: _SchemaGen,
    defaults: Mapping[str, object],
) -> dict[str, Any] | None:
    body = [a for a in entry.call_args if a[1] == "body"]
    form = [a for a in entry.call_args if a[1] == "form"]
    files = [a for a in entry.call_args if a[1] == "file"]
    if not (body or form or files):
        return None
    content: dict[str, Any] = {}
    if body:
        content[_body_media(body)] = {"schema": _body_schema(body, gen, defaults)}
    if form:
        content["application/x-www-form-urlencoded"] = {
            "schema": _object_schema(form, gen, defaults)
        }
    if files:
        content["multipart/form-data"] = {"schema": _object_schema(files, gen, defaults)}
    request_body: dict[str, Any] = {"content": content}
    if any(a[5][0] == "required" for a in (*body, *form, *files)):
        request_body["required"] = True
    return request_body


def _body_media(body: Sequence[CallArgSpec]) -> str:
    """A lone raw-bytes body is octet-stream; everything else is JSON."""
    if len(body) == 1 and (body[0][4] or {}).get("kind") in ("bytes", "raw"):
        return "application/octet-stream"
    return "application/json"


def _body_schema(
    body: Sequence[CallArgSpec],
    gen: _SchemaGen,
    defaults: Mapping[str, object],
) -> dict[str, Any]:
    if len(body) == 1:
        return gen.schema(body[0][4])
    return _object_schema(body, gen, defaults)


def _object_schema(
    args: Sequence[CallArgSpec],
    gen: _SchemaGen,
    defaults: Mapping[str, object],
) -> dict[str, Any]:
    """Several same-source params → one object schema keyed by wire name."""
    properties: dict[str, Any] = {}
    required: list[str] = []
    for name, _source, _kind, wire_name, node, missing in args:
        schema = gen.schema(node)
        if missing[0] == "literal":
            schema["default"] = _jsonable(missing[1])
        elif missing[0] == "omit" and name in defaults:
            schema["default"] = _jsonable(defaults[name])
        if missing[0] == "required":
            required.append(wire_name)
        properties[wire_name] = schema
    out: dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        out["required"] = required
    return out


def _responses(entry: RouteEntry, gen: _SchemaGen) -> dict[str, Any]:
    # `response_spec` splits union returns into one variant per member —
    # `-> User | None` → `200` + `204`, `-> Cat | Dog` → two `200`s —
    # so same-status nodes merge back into one response.
    grouped: dict[str, tuple[int | None, list[dict[str, Any] | None]]] = {}
    for status, node in entry.responses:
        key = "default" if status is None else str(status)
        grouped.setdefault(key, (status, []))[1].append(node)
    responses: dict[str, Any] = {}
    for key, (status, nodes) in grouped.items():
        response: dict[str, Any] = {"description": _phrase(status)}
        content = _merged_content([n for n in nodes if n is not None], gen)
        if content:
            response["content"] = content
        responses[key] = response
    if not responses:
        responses["200"] = {"description": "Successful Response"}
    return responses


def _phrase(status: int | None) -> str:
    if status is None:
        return "Response"
    try:
        return HTTPStatus(status).phrase
    except ValueError:
        return "Response"


def _response_media(node: Mapping[str, Any]) -> str:
    """Match the §10.2 return-conversion table."""
    kind = node.get("kind")
    if kind == "str":
        return "text/plain"
    if kind in ("bytes", "raw"):
        return "application/octet-stream"
    return "application/json"


def _merged_content(nodes: list[dict[str, Any]], gen: _SchemaGen) -> dict[str, Any]:
    """Same-status variants → `content`, grouped per media type. A group
    of records sharing exactly one single-valued `Literal` field (the
    `Discriminator` pattern — dispatch consumed the marker) renders
    `oneOf` + `discriminator`; anything else dedupes into `anyOf`."""
    media_groups: dict[str, list[dict[str, Any]]] = {}
    for node in nodes:
        media_groups.setdefault(_response_media(node), []).append(node)
    content: dict[str, Any] = {}
    for media, group in media_groups.items():
        if len(group) == 1:
            content[media] = {"schema": gen.schema(group[0])}
            continue
        schema = _discriminate(group, gen)
        if schema is None:
            schemas = [gen.schema(n) for n in group]
            unique: list[dict[str, Any]] = []
            for s in schemas:
                if s not in unique:
                    unique.append(s)
            schema = unique[0] if len(unique) == 1 else {"anyOf": unique}
        content[media] = {"schema": schema}
    return content


def _discriminate(nodes: list[dict[str, Any]], gen: _SchemaGen) -> dict[str, Any] | None:
    """`oneOf` + `discriminator` when every variant is a record carrying
    the same single-valued `Literal` field under one wire name with
    distinct values; `None` otherwise."""
    if len(nodes) < 2 or any(n.get("kind") not in ("record", "generic") for n in nodes):
        return None
    tag_values: dict[str, list[Any]] = {}
    for node in nodes:
        record = node if node["kind"] == "record" else gen.specialize(node)
        for raw in _seq(record.get("fields")):
            field = _obj(raw)
            ftype = _obj(field.get("type"))
            members = _seq(ftype.get("members"))
            if ftype.get("kind") == "literal" and len(members) == 1:
                wire = _wire_key(field, record.get("rename"))
                tag_values.setdefault(wire, []).append(_jsonable(members[0]))
    candidates = {
        wire: values
        for wire, values in tag_values.items()
        if len(values) == len(nodes) and len(set(values)) == len(nodes)
    }
    if len(candidates) != 1:
        return None
    wire, values = next(iter(candidates.items()))
    refs = [f"#/components/schemas/{gen.ref_name(node)}" for node in nodes]
    return {
        "oneOf": [{"$ref": ref} for ref in refs],
        "discriminator": {
            "propertyName": wire,
            "mapping": {str(value): ref for value, ref in zip(values, refs, strict=True)},
        },
    }


# ---- spec node → JSON Schema -------------------------------------------------


def _jsonable(value: object) -> Any:
    """A spec literal → a JSON-representable value (`bytes` → base64)."""
    if value is None or isinstance(value, bool | int | float | str):
        return value
    if isinstance(value, bytes):
        return base64.b64encode(value).decode("ascii")
    return str(value)


def _jtype(value: object) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    return "string"


def _obj(value: object) -> dict[str, Any]:
    """A spec dict member — the dict itself, or `{}` for `None`/missing."""
    return cast("dict[str, Any]", value) if isinstance(value, dict) else {}


def _seq(value: object) -> Sequence[Any]:
    """A spec sequence member — itself, or `[]` for `None`/missing."""
    return cast("Sequence[Any]", value) if isinstance(value, Sequence) else []


def _num(bound: object) -> Any:
    """A range/multipleOf bound → a JSON Schema number."""
    if isinstance(bound, dict):
        return float(cast("dict[str, Any]", bound)["decimal"])
    return bound


def _rename(rule: str | None, name: str) -> str:
    """`fulmen_schema::RenameRule::apply` mirrored exactly."""
    if rule == "kebab":
        return name.replace("_", "-")
    if rule in ("camel", "pascal"):
        out: list[str] = []
        up = rule == "pascal"
        for c in name:
            if c == "_":
                up = True
            elif up:
                out.append(c.upper())
                up = False
            else:
                out.append(c)
        return "".join(out)
    return name


def _wire_key(field: Mapping[str, Any], rename: str | None) -> str:
    """The input key a record field accepts: `alias`, then `rename`,
    then `name` — mirrors `FieldDesc::wire_key`."""
    if field.get("alias"):
        return str(field["alias"])
    return _rename(rename, str(field["name"]))


def _canon(value: object) -> Any:
    """Canonicalize a spec node for dedupe keys: drop callback ids
    (`meta.after`, `{"factory": n}`) — volatile across builds and
    invisible in the rendered schema."""
    if isinstance(value, dict):
        node = cast("dict[str, Any]", value)
        if set(node) == {"factory"}:
            return {"factory": "*"}
        return {k: _canon(v) for k, v in sorted(node.items()) if k != "after"}
    if isinstance(value, list | tuple):
        return [_canon(v) for v in cast("Sequence[Any]", value)]
    if isinstance(value, bytes):
        return {"bytes": _jsonable(value)}
    return value


def _component_name(class_name: str) -> str:
    """A class qualname → a legal `#/components/schemas/` key
    (`^[a-zA-Z0-9.\\-_]+$`; anything else becomes `_`)."""
    cleaned = "".join(c if c.isalnum() or c in ".-_" else "_" for c in class_name)
    return cleaned or "Model"


def _node_label(node: Mapping[str, Any]) -> str:
    """A short component-name fragment for a generic argument."""
    if node.get("kind") == "record":
        return str(node.get("class", {}).get("name", "record"))
    if node.get("kind") == "generic":
        return str(node.get("origin", {}).get("class", {}).get("name", "generic"))
    return str(node.get("kind", "any"))


class _SchemaGen:
    """Spec node → JSON Schema, plus `components/schemas` dedupe."""

    def __init__(self) -> None:
        self.schemas: dict[str, dict[str, Any]] = {}
        self._by_key: dict[str, str] = {}

    # -- records and components ------------------------------------------------

    def ref_name(self, node: dict[str, Any]) -> str:
        """Register `node` (record, generic or enum) in
        `components/schemas` and return its component name."""
        if node["kind"] == "generic":
            node = self.specialize(node)
        key = json.dumps(_canon(node), sort_keys=True)
        if key in self._by_key:
            return self._by_key[key]
        base = _component_name(node["class"]["name"])
        name = base
        counter = 2
        while name in self.schemas:
            name = f"{base}_{counter}"
            counter += 1
        self._by_key[key] = name
        self.schemas[name] = {}  # placeholder before recursion
        self.schemas[name] = (
            self._enum_schema(node) if node["kind"] == "enum" else self._record_schema(node)
        )
        return name

    def specialize(self, node: dict[str, Any]) -> dict[str, Any]:
        """A `generic` node → its record with `typevar`s bound by `args`."""
        origin = _obj(node.get("origin"))
        mapping = dict(zip(_seq(origin.get("type_params")), _seq(node.get("args")), strict=True))
        fields: list[dict[str, Any]] = []
        for raw in _seq(origin.get("fields")):
            field = _obj(raw)
            fields.append({**field, "type": self._subst_any(field.get("type"), mapping)})
        args = "_".join(_node_label(_obj(arg)) for arg in _seq(node.get("args")))
        name = f"{origin['class']['name']}_{args}" if args else origin["class"]["name"]
        return {
            **origin,
            "class": {"name": name, "module": origin["class"]["module"]},
            "fields": fields,
            "type_params": [],
        }

    def _subst_any(self, value: object, mapping: Mapping[str, Any]) -> object:
        if isinstance(value, dict):
            node = cast("dict[str, Any]", value)
            if node.get("kind") == "typevar":
                return mapping.get(cast("str", node.get("name")), value)
            return {k: self._subst_any(v, mapping) for k, v in node.items()}
        if isinstance(value, list):
            return [self._subst_any(v, mapping) for v in cast("Sequence[Any]", value)]
        return value

    def _record_schema(self, node: dict[str, Any]) -> dict[str, Any]:
        out: dict[str, Any] = {"type": "object"}
        node_meta = _obj(node.get("meta"))
        if node_meta.get("description"):
            out["description"] = node_meta["description"]
        properties: dict[str, Any] = {}
        required: list[str] = []
        rename: str | None = node.get("rename")
        for raw in _seq(node.get("fields")):
            field = _obj(raw)
            wire = _wire_key(field, rename)
            schema = self.schema(field.get("type"))
            meta = _obj(field.get("meta"))
            if meta.get("description"):
                schema["description"] = meta["description"]
            if "example" in meta:
                schema["examples"] = [_jsonable(meta["example"])]
            if meta.get("deprecated"):
                schema["deprecated"] = True
            missing = field.get("missing")
            if missing == "required":
                required.append(wire)
            elif isinstance(missing, dict) and "literal" in missing:
                schema["default"] = _jsonable(cast("dict[str, Any]", missing)["literal"])
            properties[wire] = schema
        if properties:
            out["properties"] = properties
        if required:
            out["required"] = required
        if node.get("extra") == "forbid":
            out["additionalProperties"] = False
        return out

    def _enum_schema(self, node: dict[str, Any]) -> dict[str, Any]:
        enum_members = _seq(node.get("members"))
        out: dict[str, Any] = {"enum": [_jsonable(_obj(m).get("value")) for m in enum_members]}
        types = {_jtype(_obj(m).get("value")) for m in enum_members}
        if len(types) == 1 and types != {"null"}:
            out["type"] = next(iter(types))
        return out

    # -- every node kind --------------------------------------------------------

    # One arm per spec `kind` — the flat match IS the SPEC-FORMAT table.
    def schema(self, node: dict[str, Any] | None) -> dict[str, Any]:
        if node is None:
            return {}
        kind = node.get("kind")
        match kind:
            case "any" | "typevar":
                out: dict[str, Any] = {}
            case "none":
                out = {"type": "null"}
            case "bool":
                out = {"type": "boolean"}
            case "int" | "float" | "decimal":
                out = _SCALARS[kind].copy()
                self._numeric(out, node)
            case "str":
                out = {"type": "string"}
                self._len(out, node, "minLength", "maxLength")
                if node.get("pattern") is not None:
                    out["pattern"] = node["pattern"]
            case "bytes":
                out = {"type": "string", "format": "byte"}
                self._len(out, node, "minLength", "maxLength")
            case "raw":
                out = {"type": "string", "format": "binary"}
            case (
                "datetime"
                | "date"
                | "time"
                | "timedelta"
                | "uuid"
                | "email"
                | "url"
                | "ipv4"
                | "ipv6"
            ):
                out = {"type": "string", "format": _FORMATS[kind]}
            case "list" | "set" | "frozenset" | "tuple":
                out = {"type": "array", "items": self.schema(node["item"])}
                self._len(out, node, "minItems", "maxItems")
                if kind in ("set", "frozenset") or node.get("unique"):
                    out["uniqueItems"] = True
            case "tuple_fixed":
                items = [self.schema(item) for item in _seq(node.get("items"))]
                out = {
                    "type": "array",
                    "prefixItems": items,
                    "items": False,
                    "minItems": len(items),
                    "maxItems": len(items),
                }
            case "dict":
                out = {"type": "object"}
                key = _obj(node.get("key"))
                if key.get("kind") not in (None, "str", "any"):
                    out["propertyNames"] = self.schema(key)
                value_schema = self.schema(node["value"])
                if value_schema:
                    out["additionalProperties"] = value_schema
                self._len(out, node, "minProperties", "maxProperties")
            case "literal":
                members = [_jsonable(m) for m in _seq(node.get("members"))]
                out = {"const": members[0]} if len(members) == 1 else {"enum": members}
            case "enum":
                out = {"$ref": f"#/components/schemas/{self.ref_name(node)}"}
            case "optional":
                out = self._nullable(self.schema(node["inner"]))
            case "union":
                variant_nodes = _seq(node.get("variants"))
                variants = [self.schema(v) for v in variant_nodes]
                if node.get("tag") is not None:
                    prop, mapping = self._discriminator(variant_nodes, node["tag"])
                    disc: dict[str, Any] = {"propertyName": prop}
                    if mapping:
                        disc["mapping"] = mapping
                    out = {"oneOf": variants, "discriminator": disc}
                elif len(variants) == 1:
                    out = variants[0]
                else:
                    out = {"anyOf": variants}
            case "newtype":
                out = {**self.schema(node["inner"]), "title": node.get("name") or "NewType"}
            case "record" | "generic":
                out = {"$ref": f"#/components/schemas/{self.ref_name(node)}"}
            case _:
                out = {}
        meta = node.get("meta")
        if meta:
            if meta.get("description"):
                out["description"] = meta["description"]
            if "example" in meta:
                out["examples"] = [_jsonable(meta["example"])]
            if meta.get("deprecated"):
                out["deprecated"] = True
            if "default" in meta:
                out["default"] = _jsonable(meta["default"])
        return out

    def _nullable(self, schema: dict[str, Any]) -> dict[str, Any]:
        """OpenAPI 3.1 nullable: `type` arrays, `anyOf` for `$ref`/composites."""
        if "$ref" in schema:
            return {"anyOf": [schema, {"type": "null"}]}
        t = schema.get("type")
        if isinstance(t, str):
            return {**schema, "type": [t, "null"]}
        if isinstance(t, list):
            return schema if "null" in t else {**schema, "type": [*t, "null"]}
        if "anyOf" in schema:
            return {**schema, "anyOf": [*schema["anyOf"], {"type": "null"}]}
        return {"anyOf": [schema, {"type": "null"}]}

    def _discriminator(
        self, variants: Sequence[dict[str, Any]], tag: str
    ) -> tuple[str, dict[str, str]]:
        """The discriminated-union `propertyName` (the field's *wire* name)
        plus a `value → $ref` mapping wherever the tag field is a
        `Literal` — OpenAPI resolves values against component names
        implicitly, and ours never match them."""
        prop = tag
        mapping: dict[str, str] = {}
        for variant in variants:
            if variant.get("kind") not in ("record", "generic"):
                continue
            record = variant if variant["kind"] == "record" else self.specialize(variant)
            field = next(
                (f for f in _seq(record.get("fields")) if _obj(f).get("name") == tag),
                None,
            )
            if field is None:
                continue
            prop = _wire_key(_obj(field), record.get("rename"))
            ftype = _obj(_obj(field).get("type"))
            if ftype.get("kind") == "literal":
                members = _seq(ftype.get("members"))
                if len(members) == 1:
                    mapping[str(_jsonable(members[0]))] = (
                        f"#/components/schemas/{self.ref_name(variant)}"
                    )
        return prop, mapping

    def _numeric(self, out: dict[str, Any], node: Mapping[str, Any]) -> None:
        rng = _obj(node.get("range"))
        for src, dst in (
            ("ge", "minimum"),
            ("gt", "exclusiveMinimum"),
            ("le", "maximum"),
            ("lt", "exclusiveMaximum"),
        ):
            if rng.get(src) is not None:
                out[dst] = _num(rng[src])
        if node.get("multiple_of") is not None:
            out["multipleOf"] = _num(node["multiple_of"])

    def _len(self, out: dict[str, Any], node: Mapping[str, Any], lo: str, hi: str) -> None:
        length = _obj(node.get("len"))
        if length.get("min") is not None:
            out[lo] = length["min"]
        if length.get("max") is not None:
            out[hi] = length["max"]


_SCALARS: dict[str, dict[str, Any]] = {
    "int": {"type": "integer"},
    "float": {"type": "number"},
    # The validator accepts numeric JSON or decimal text; the encoder
    # emits text (§11.6). `format` carries the intent.
    "decimal": {"type": ["number", "string"], "format": "decimal"},
}


# ---- the docs UI --------------------------------------------------------------


def docs_html(*, openapi_url: str, docs_url: str, title: str) -> str:
    """The Swagger UI host page — plain HTML wired to the vendored assets."""
    safe_title = html.escape(f"{title} — API docs")
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{safe_title}</title>
<link rel="stylesheet" href="{docs_url}/swagger-ui.css">
</head>
<body>
<div id="swagger-ui"></div>
<script src="{docs_url}/swagger-ui-bundle.js"></script>
<script>
window.ui = SwaggerUIBundle({{
  url: "{openapi_url}",
  dom_id: "#swagger-ui",
  deepLinking: true,
}});
</script>
</body>
</html>
"""


def _gunzip(name: str) -> bytes:
    return gzip.decompress((_STATIC / name).read_bytes())


def docs_assets(docs_url: str) -> list[tuple[str, bytes, str]]:
    """`(path, body, content-type)` for each self-hosted docs asset."""
    return [
        (
            f"{docs_url}/swagger-ui.css",
            _gunzip("swagger-ui.css.gz"),
            "text/css; charset=utf-8",
        ),
        (
            f"{docs_url}/swagger-ui-bundle.js",
            _gunzip("swagger-ui-bundle.js.gz"),
            "text/javascript; charset=utf-8",
        ),
    ]
