# Spec format — the annotation → schema IR contract

`_typing.py` turns Python annotations into spec dicts; `fulmen-schema`'s
`TypeDesc::compile` (via `compile_descriptor` in `fulmen-py`) turns spec
dicts into validators. This file is the contract between the two. When
either side changes, change this document in the same commit.

Every spec is a JSON-shaped dict. `kind` is always present and selects the
node type; the remaining keys depend on the kind.

## 1. Scalar kinds

| `kind` | Python source | Extra keys |
| --- | --- | --- |
| `any` | `Any`, `object` | — |
| `none` | `None`, `NoneType` | — |
| `bool` | `bool` | — |
| `int` | `int` | `range`, `multiple_of` |
| `float` | `float` | `range`, `multiple_of` |
| `str` | `str`, `Email`, `Url` → `email`/`url` | `len`, `pattern` |
| `bytes` | `bytes` | `len` |
| `datetime`/`date`/`time`/`timedelta` | `datetime.*` | — |
| `decimal` | `Decimal` | `range`, `multiple_of` (bounds as `{"decimal": "..."}`) |
| `uuid` | `uuid.UUID` | — |
| `ipv4`/`ipv6` | `ipaddress.IPv4Address`/`IPv6Address` | — |
| `raw` | `Raw`, `UploadFile`, `Request` (wire view) | — |
| `typevar` | `TypeVar` | `name` — resolved through the enclosing `generic`'s scope |

## 2. Container and composite kinds

| `kind` | Python source | Extra keys |
| --- | --- | --- |
| `list` | `list[T]` | `item`, `len`, `unique` |
| `set`/`frozenset` | `set[T]`/`frozenset[T]` | `item`, `len` |
| `tuple` | `tuple[T, ...]` | `item`, `len` |
| `tuple_fixed` | `tuple[A, B, ...]` | `items` (list of nodes) |
| `dict` | `dict[K, V]` | `key`, `value` |
| `optional` | `T \| None` | `inner` — constraints pass through to `inner` |
| `union` | `T \| U` | `variants`, optional `tag` (discriminator field) |
| `literal` | `Literal[...]` | `members` (scalar values) |
| `newtype` | `NewType` | `name`, `inner` — constraints pass through to `inner` |
| `enum` | `Enum`/`IntEnum`/`StrEnum` | `class`, `flavor` (`int`/`str`/`plain`), `members` (`{name, value}`) |
| `record` | `Model`, dataclass, `TypedDict` | `class`, `flavor` (`model`/`dataclass`/`typed_dict`), `fields`, `extra` (`ignore`/`forbid`/`keep`), `rename`, `type_params` |
| `generic` | `Model[T]`/generic dataclass | `origin` (record node), `args` |

A record `fields` entry is `{name, type, alias, missing, meta}`.
`missing` is one of the policies in §3.

## 3. Missing policies

Everywhere a field or call argument can be absent, `missing` says what
to do:

| Value | Meaning |
| --- | --- |
| `"required"` | Absence is a validation error (collected into the 422 `errors` array). |
| `"omit"` | Leave the argument/field out — the Python signature or `cls(**kwargs)` default applies. |
| `{"literal": v}` | Pass the scalar `v` (`None`, bool, int, float, str, bytes). |
| `{"factory": id}` | Invoke callbacks table entry `id` per request/field — used for non-scalar defaults so `default=[1]` and `default_factory=list` never share a mutable value. |

`Default(...)`/`Field(default=...)` on a signature parameter maps to
`literal` (marker-declared); a signature `= default` maps to `omit`;
`Field(default_factory=...)` and non-scalar defaults map to `factory`.

## 4. Response variants

`response_spec()` returns a list of `ResponseVariant` describing the
handler's declared response set — one entry per union member:

| Field | Meaning |
| --- | --- |
| `status` | `int` status code, or `None` for a dynamic `Response` (status chosen at runtime). |
| `body` | A spec node for the declared body type, or `None` for `NoContent`/bare `Response`. |

Examples: `-> int` → `[(200, int-node)]`; `-> None` → `[(204, None)]`;
`-> Point | None` → `[(200, record), (204, None)]`; `-> Response` →
`[(None, None)]`; `-> NotFound[dict]` → `[(404, dict-node)]`.
`app.py` compiles each `body` into a `Descriptor` and hands the list to
`NativeApp.add_route` as `response_descs`.

## 5. The native route tuple

`NativeApp.add_route` takes
`(method, path, param_types, call_args, handler, is_async, none_ok,
response_descs, layers)`:

- `param_types`: `[(name, "int"|"str")]` — the router's match-time path
  types (`{name}` / `{*name}` catch-all).
- `call_args`: `[(name, source, kind, wire_name, Descriptor | None,
  missing)]` — `source` is `path`/`query`/`header`/`cookie`/`body`/
  `form`/`raw`/`request`/`file`; `kind` is `pos_only`/`normal`/`kw_only`;
  `missing` is `(kind, object)` where kind is the §3 policy name and
  `object` is the literal value or factory callable.
- `none_ok`: whether the handler may return `None`.
- `response_descs`: `[(status | None, Descriptor | None)]` per §4.
- `layers`: `[int]` — the group-scope ids this route descends through
  (mounted `Fulmen(prefix=…)` apps, outermost first; `[]` for direct
  routes). `NativeApp.add_scope()` allocates ids; hook and `on_error`
  registrations carry a scope — `0` is the app itself — so a route's
  resolved hook chains and error table come from the app scope plus its
  `layers` (DESIGN.md §8, §5.5).

## 6. `meta` and callbacks

`node["meta"]` carries non-semantic annotation payload:
`alias` (wire name), `description`, `example`, `deprecated`, `default`
(`Default(...)` mid-tree), `after` (list of `AfterValidator` callback ids),
`transforms` (`strip`/`lower`/`upper`).

`SpecBuilder` allocates integer callback ids (`cb_id`) in build order;
`compile_descriptor(spec, callbacks)` resolves `meta.after` entries and
`{"factory": id}` missing policies against that table. Ids are per-spec —
a descriptor compiled without its callbacks table cannot resolve them.
