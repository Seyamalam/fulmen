"""Declarative settings (DESIGN.md §20, §14.7).

A `Settings` subclass declares typed fields; values resolve from
layered sources and are validated by the schema engine at startup.
Missing or invalid settings stop the boot with every problem listed in
one `StartupError`.

```python
class AppSettings(Settings, env_prefix="APP_"):
    database_url: Secret[str]
    debug: bool = False
    allowed_origins: list[str] = []

app = Fulmen(settings=AppSettings)
```

Source precedence, lowest to highest (§20): field defaults,
`fulmen.toml` (`[tool.fulmen]` or the class's `toml_section`), the
`env_file` dot-env file (opt-in — never loaded unless configured, and
ignored in production mode with a warning), environment variables
(`env_prefix + FIELD`), and constructor arguments — the programmatic
tier DESIGN lists as "CLI flags" (explicit call-site values win).

`Secret[T]` wraps a value so `str`/`repr`/f-strings/log interpolation
print `**********`; `.get_secret_value()` is the only way out.
"""

from __future__ import annotations

import dataclasses
import json
import os
import re
import tomllib
import types as pytypes
import typing
import warnings
from collections.abc import Mapping
from pathlib import Path
from typing import Annotated, Any, ClassVar, dataclass_transform

from fulmen import _typing
from fulmen._fulmen import Descriptor, StartupError
from fulmen.constraints import UNSET, Field
from fulmen.errors import ValidationError

_TOML_AUTO: Any = object()

# Spec kinds whose env/dotenv strings are JSON-decoded before validation
# (scalars and stringly kinds go through lax coercion instead).
_JSON_KINDS = frozenset(
    {"list", "set", "frozenset", "tuple", "tuple_fixed", "dict", "record", "generic"}
)

_ENV_NAME_RE = re.compile(r"[^0-9A-Za-z]+")

_DOTENV_ESCAPES = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\"}


class Secret[T]:
    """A setting value that never prints (§14.7).

    `str()`, `repr()` and `format()` all render `**********`; logs and
    error pages never see the wrapped value. `get_secret_value()` is
    the only way to read it back.
    """

    __slots__ = ("_value",)

    def __init__(self, value: T) -> None:
        self._value = value

    def get_secret_value(self) -> T:
        """The wrapped value — the only accessor."""
        return self._value

    def __str__(self) -> str:
        return "**********"

    def __repr__(self) -> str:
        return "Secret('**********')"

    def __format__(self, spec: str) -> str:
        return format("**********", spec)

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, Secret):
            return False
        return self._value == typing.cast(Any, other).get_secret_value()

    def __hash__(self) -> int:
        return hash(self._value)

    def __bool__(self) -> bool:
        return bool(self._value)


@dataclasses.dataclass(frozen=True)
class _Field:
    """One resolved settings field, compiled once per class."""

    name: str
    key: str  # input key the record validator accepts (alias or name)
    env_key: str  # uppercased/sanitized `key` — env prefix attaches here
    secret: bool
    wants_json: bool  # env/dotenv strings JSON-decode before validation


def _env_key(key: str) -> str:
    return _ENV_NAME_RE.sub("_", key).upper()


def _peel(hint: Any) -> tuple[Any, list[Any]]:
    if typing.get_origin(hint) is Annotated:
        args = typing.get_args(hint)
        return args[0], list(args[1:])
    return hint, []


def _unsecret(hint: Any) -> tuple[Any, bool]:
    """`Secret[T]` → `(T, True)`; `Optional[Secret[T]]` → `(T | None, True)`."""
    if hint is Secret:
        return Any, True
    if typing.get_origin(hint) is Secret:
        args = typing.get_args(hint)
        if len(args) != 1:
            raise _typing.AnnotationError(f"Secret takes exactly one type argument, got {hint!r}")
        return args[0], True
    if typing.get_origin(hint) in (typing.Union, pytypes.UnionType):
        args = typing.get_args(hint)
        members = [a for a in args if a is not type(None)]
        if len(members) == 1 and len(args) == 2:
            inner, secret = _unsecret(members[0])
            return (inner | None, True) if secret else (hint, False)
    return hint, False


def _contains_secret(hint: Any) -> bool:
    if hint is Secret or typing.get_origin(hint) is Secret:
        return True
    return any(_contains_secret(arg) for arg in typing.get_args(hint))


def _node_kinds(node: dict[str, Any]) -> set[str]:
    """Every spec kind under `node` — `optional`/`union`/`newtype` unwrap."""
    kinds = {str(node["kind"])}
    children = (
        node.get("inner"),
        node.get("item"),
        *node.get("variants", ()),
        *node.get("items", ()),
    )
    for child in children:
        if isinstance(child, dict):
            kinds |= _node_kinds(typing.cast("dict[str, Any]", child))
    return kinds


def _split_section(section: str) -> tuple[str, ...]:
    return tuple(part for part in section.split(".") if part)


def _derive_section(env_prefix: str, explicit: str | None) -> tuple[str, ...]:
    """The `[tool.fulmen]` subsection a settings class reads.

    `toml_section` wins; otherwise it derives from `env_prefix`:
    `"FULMEN_"` → the `[tool.fulmen]` table itself, `"FULMEN_JWT_"`
    → `[tool.fulmen.jwt]` (the plugin convention, §20), anything else
    → `[tool.fulmen.<prefix-lowercased>]`.
    """
    if explicit is not None:
        return _split_section(explicit)
    prefix = env_prefix.rstrip("_")
    if prefix.startswith("FULMEN_"):
        prefix = prefix[len("FULMEN_") :]
    elif prefix == "FULMEN":
        prefix = ""
    return (prefix.lower(),) if prefix else ()


def _compile_settings(cls: type[Settings]) -> Descriptor:
    """Resolve `cls`'s annotations into a record descriptor + field map.

    Secret annotations are rewritten to their inner type for the schema
    engine (the wrapper is applied after validation); everything else —
    defaults, `Field()`, `Alias`, constraints — compiles exactly like a
    `Model` field.
    """
    builder = _typing.SpecBuilder()
    hints = typing.get_type_hints(cls, include_extras=True)
    rewritten: dict[str, Any] = {}
    secrets: set[str] = set()
    defaults: dict[str, Any] = {}
    for name, hint in hints.items():
        if name.startswith("_") or typing.get_origin(hint) is typing.ClassVar:
            continue
        base, markers = _peel(hint)
        inner, secret = _unsecret(base)
        if not secret and _contains_secret(inner):
            raise _typing.AnnotationError(
                f"settings field `{name}`: Secret[T] is only supported as the "
                "whole field type (optionally inside Optional)"
            )
        if secret:
            secrets.add(name)
        rewritten[name] = Annotated[inner, *markers] if markers else inner
        declared = getattr(cls, name, UNSET)
        if declared is not UNSET:
            defaults[name] = declared

    # The record machinery reads annotations + class-attr defaults off a
    # class; a synthetic proxy carries the Secret-rewritten hints while
    # keeping the real module/qualname for diagnostics.
    proxy = type(
        cls.__name__,
        (),
        {
            "__annotations__": rewritten,
            "__module__": cls.__module__,
            "__qualname__": cls.__qualname__,
            **defaults,
        },
    )
    spec = builder.record_spec(proxy)
    spec["class"] = {"name": cls.__qualname__, "module": cls.__module__}
    descriptor = _typing.compile_descriptor(spec, builder.callbacks)

    fields: dict[str, _Field] = {}
    for f in spec["fields"]:
        key = f["alias"] or f["name"]
        fields[f["name"]] = _Field(
            name=f["name"],
            key=key,
            env_key=_env_key(key),
            secret=f["name"] in secrets,
            wants_json=bool(_node_kinds(f["type"]) & _JSON_KINDS),
        )
    cls.__settings_fields__ = fields
    cls.__settings_descriptor__ = descriptor
    return descriptor


def _settings_descriptor(cls: type[Settings]) -> Descriptor:
    """`cls`'s compiled descriptor, building lazily after a forward ref."""
    descriptor = cls.__settings_descriptor__
    if descriptor is None:
        descriptor = _compile_settings(cls)
    return descriptor


def _is_production(environ: Mapping[str, str]) -> bool:
    return environ.get("FULMEN_ENV", "").lower() == "production"


def _as_table(value: Any) -> dict[str, Any]:
    """`value` as a TOML table, or `{}` when it isn't a mapping."""
    if isinstance(value, dict):
        return typing.cast("dict[str, Any]", value)
    return {}


def _load_toml_table(
    toml_file: str | Path | None, sections: tuple[str, ...], problems: list[str], label: str
) -> dict[str, Any]:
    """`fulmen.toml`'s `[tool.fulmen.<section>]` table, or `{}`."""
    if toml_file is None:
        return {}
    path = Path(toml_file)
    if not path.is_file():
        return {}
    try:
        with path.open("rb") as handle:
            document: dict[str, Any] = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        problems.append(f"settings `{label}`: cannot read {path}: {exc}")
        return {}
    node = _as_table(document.get("tool", {}))
    node = _as_table(node.get("fulmen", {}))
    for part in sections:
        node = _as_table(node.get(part, {}))
    return node


def _load_dotenv(
    env_file: str | Path | None, environ: Mapping[str, str], label: str
) -> dict[str, str]:
    """Parse the configured dot-env file — opt-in only, off in production."""
    if env_file is None:
        return {}
    if _is_production(environ):
        warnings.warn(
            f"settings `{label}`: env_file {env_file!r} is ignored in production mode "
            "(FULMEN_ENV=production)",
            stacklevel=4,
        )
        return {}
    path = Path(env_file)
    if not path.is_file():
        return {}
    try:
        text = path.read_text()
    except OSError:
        return {}
    return _parse_dotenv(text)


def _parse_dotenv(text: str) -> dict[str, str]:
    """Minimal dot-env syntax: `KEY=value`, quotes, `#` comments, `export`."""
    out: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[7:].lstrip()
        key, sep, value = line.partition("=")
        key = key.strip()
        if not key:
            continue
        value = value.strip() if sep else ""
        if value.startswith('"') and value.endswith('"') and len(value) >= 2:
            value = _unescape_double_quoted(value[1:-1])
        elif value.startswith("'") and value.endswith("'") and len(value) >= 2:
            value = value[1:-1]
        else:
            value = value.split(" #", 1)[0].rstrip()
        out[key] = value
    return out


def _unescape_double_quoted(value: str) -> str:
    """`\\n`/`\\t`/`\\r`/`\\"`/`\\\\` escapes, single left-to-right pass."""
    out: list[str] = []
    i = 0
    while i < len(value):
        if value[i] == "\\" and i + 1 < len(value) and value[i + 1] in _DOTENV_ESCAPES:
            out.append(_DOTENV_ESCAPES[value[i + 1]])
            i += 2
        else:
            out.append(value[i])
            i += 1
    return "".join(out)


def _toml_label(toml_file: Any, sections: tuple[str, ...]) -> str:
    return f"{'.'.join(('tool', 'fulmen', *sections))} in {toml_file}"


def _coerce(f: _Field, raw: str) -> Any:
    """Env/dotenv strings: JSON-decode containers, pass scalars to lax."""
    if f.wants_json:
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return raw
    return raw


class Settings:
    """Base class for declarative application settings (DESIGN.md §20).

    ```python
    class AppSettings(Settings, env_prefix="APP_", env_file=".env"):
        database_url: Secret[str]
        debug: bool = False
        allowed_origins: list[str] = []
    ```

    Class arguments:

    - `env_prefix`: env-var prefix, default `"FULMEN_"`. Field
      `database_url` resolves `APP_DATABASE_URL` here.
    - `env_file`: opt-in dot-env path (never loaded unless set; ignored
      in production mode with a warning).
    - `toml_file`: TOML path, default `./fulmen.toml` when present;
      `None` disables the TOML source.
    - `toml_section`: `[tool.fulmen]` subsection (e.g. `"jwt"` reads
      `[tool.fulmen.jwt]`); derived from `env_prefix` when unset.

    Instance keyword arguments override every source (`_env_prefix`,
    `_env_file`, `_toml_file`, `_toml_section`, `_environ` tune the
    sources themselves). Fields validate through the schema engine;
    failures raise `StartupError` listing every problem. The source
    each field resolved from is recorded in `__field_sources__`.
    """

    __settings_env_prefix__: ClassVar[str] = "FULMEN_"
    __settings_env_file__: ClassVar[str | Path | None] = None
    __settings_toml_file__: ClassVar[str | Path | None] = "fulmen.toml"
    __settings_toml_sections__: ClassVar[tuple[str, ...]] = ()
    __settings_fields__: ClassVar[dict[str, _Field]] = {}
    __settings_descriptor__: ClassVar[Descriptor | None] = None
    __field_sources__: dict[str, str]

    @dataclass_transform(kw_only_default=True, field_specifiers=(Field,))
    def __init_subclass__(
        cls,
        *,
        env_prefix: str = "FULMEN_",
        env_file: str | Path | None = None,
        toml_file: str | Path | None = _TOML_AUTO,
        toml_section: str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init_subclass__(**kwargs)
        cls.__settings_env_prefix__ = env_prefix
        cls.__settings_env_file__ = env_file
        cls.__settings_toml_file__ = "fulmen.toml" if toml_file is _TOML_AUTO else toml_file
        cls.__settings_toml_sections__ = _derive_section(env_prefix, toml_section)
        cls.__settings_fields__ = {}
        cls.__settings_descriptor__ = None
        try:
            _compile_settings(cls)
        except NameError:
            # Forward ref to a later name: compile lazily on first use.
            cls.__settings_descriptor__ = None

    def __init__(
        self,
        /,
        _env_prefix: object = UNSET,
        _env_file: object = UNSET,
        _toml_file: object = UNSET,
        _toml_section: object = UNSET,
        _environ: Mapping[str, str] | None = None,
        **values: Any,
    ) -> None:
        cls = type(self)
        descriptor = _settings_descriptor(cls)
        fields = cls.__settings_fields__
        label = cls.__qualname__
        environ = os.environ if _environ is None else _environ
        env_prefix = cls.__settings_env_prefix__ if _env_prefix is UNSET else _env_prefix
        env_file = cls.__settings_env_file__ if _env_file is UNSET else _env_file
        toml_file = cls.__settings_toml_file__ if _toml_file is UNSET else _toml_file
        sections = (
            cls.__settings_toml_sections__
            if _toml_section is UNSET
            else (() if _toml_section is None else _split_section(typing.cast(str, _toml_section)))
        )
        if not isinstance(env_prefix, str):
            raise TypeError(f"settings `{label}`: env_prefix must be a str, got {env_prefix!r}")

        problems: list[str] = []
        merged: dict[str, Any] = {}
        sources: dict[str, str] = {}
        env_name = {f.name: f"{env_prefix}{f.env_key}" for f in fields.values()}

        table = _load_toml_table(typing.cast(Any, toml_file), sections, problems, label)
        for name, f in fields.items():
            if f.key in table:
                merged[f.key] = table[f.key]
                sources[name] = f"toml:{_toml_label(toml_file, sections)}"

        dotenv = _load_dotenv(typing.cast(Any, env_file), environ, label)
        for name, f in fields.items():
            if (name_ := env_name[name]) in dotenv:
                merged[f.key] = _coerce(f, dotenv[name_])
                sources[name] = f"dotenv:{name_}"

        for name, f in fields.items():
            if (name_ := env_name[name]) in environ:
                merged[f.key] = _coerce(f, environ[name_])
                sources[name] = f"env:{name_}"

        by_key = {f.key: f for f in fields.values()} | {f.name: f for f in fields.values()}
        for key, value in values.items():
            field = by_key.get(key)
            if field is None:
                problems.append(f"settings `{label}`: unknown argument `{key}`")
                continue
            merged[field.key] = value.get_secret_value() if isinstance(value, Secret) else value
            sources[field.name] = "init"

        try:
            descriptor.construct(self, merged, lax=True)
        except ValidationError as exc:
            problems.extend(
                _settings_problems(label, exc, fields, env_name, sources, env_file, toml_file)
            )
        if problems:
            message = f"settings `{label}` failed to load:" + "".join(
                f"\n  - {problem}" for problem in problems
            )
            error = StartupError(message)
            error.problems = tuple(problems)  # pyright: ignore[reportAttributeAccessIssue] - carries the aggregated problems
            raise error

        for name, f in fields.items():
            value = getattr(self, name, UNSET)
            if f.secret and value is not None and not isinstance(value, Secret):
                setattr(self, name, Secret(value))
            sources.setdefault(name, "default")
        self.__field_sources__ = sources

    def __eq__(self, other: object) -> bool:
        if type(other) is not type(self):
            return False
        return all(
            getattr(self, name, UNSET) == getattr(other, name, UNSET)
            for name in type(self).__settings_fields__
        )

    def __repr__(self) -> str:
        parts = ", ".join(
            f"{name}={value!r}"
            for name in type(self).__settings_fields__
            if (value := getattr(self, name, UNSET)) is not UNSET
        )
        return f"{type(self).__qualname__}({parts})"

    __hash__: ClassVar[None] = None  # pyright: ignore[reportIncompatibleMethodOverride] - mutable settings are unhashable


def _settings_problems(
    label: str,
    exc: ValidationError,
    fields: dict[str, _Field],
    env_name: dict[str, str],
    sources: dict[str, str],
    env_file: Any,
    toml_file: Any,
) -> list[str]:
    """`ValidationError.entries` → one problem line per settings field."""
    by_key = {f.key: f for f in fields.values()} | {f.name: f for f in fields.values()}
    lines: list[str] = []
    for entry in exc.entries:
        # `loc` arrives as a validation path (`/body/database_url`) — the
        # field name is its last segment.
        loc = str(entry.get("loc", "?")).rsplit("/", 1)[-1]
        message = str(entry.get("message", "invalid"))
        field = by_key.get(loc)
        if field is None:
            lines.append(f"settings `{label}`: {message} ({entry.get('loc', '?')})")
            continue
        source = sources.get(field.name)
        if source is not None:
            lines.append(f"settings `{label}` field `{loc}` ({source}): {message}")
            continue
        checked = ["init kwargs", f"env {env_name[field.name]}"]
        if env_file is not None:
            checked.append(f"dotenv {env_file}")
        if toml_file is not None:
            checked.append(str(toml_file))
        checked.append("default")
        lines.append(f"settings `{label}` field `{loc}`: {message} (checked: {', '.join(checked)})")
    return lines
