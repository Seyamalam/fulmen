"""Settings tests (DESIGN.md §20, milestone M3.22).

`Secret` masking, source precedence (defaults < `fulmen.toml` < `.env` <
environment < constructor kwargs), `.env` opt-in/production guard,
aggregated `StartupError` problems, `__field_sources__` provenance, and
`Fulmen(settings=…)` injection into handlers.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Annotated

import pytest
from fulmen import Alias, Field, Fulmen, Header, Model, Secret, Settings, StartupError
from fulmen._typing import AnnotationError
from fulmen.testing import TestClient

# Every test class uses a unique env prefix so real process env vars
# (and cross-test monkeypatch leakage) can never satisfy a field.


# Settings classes used as handler annotations live at module level —
# `get_type_hints` resolves annotations in module globals.


class _InjectSettings(Settings, env_prefix="TS_APP_"):
    greeting: str = "hello"
    debug: bool = False


class _InstanceSettings(Settings, env_prefix="TS_INST_"):
    greeting: str = "hello"


class _FailSettings(Settings, env_prefix="TS_FAIL_"):
    required: str


class _MarkerSettings(Settings, env_prefix="TS_MR_"):
    x: int = 0


class _RootSettings(Settings, env_prefix="TS_MROOT_"):
    who: str = "root"


class _ApiSettings(Settings, env_prefix="TS_MAPI_"):
    who: str = "api"


class _OtherSettings(Settings, env_prefix="TS_OTH_"):
    value: int = 7


class _AsyncSettings(Settings, env_prefix="TS_ASYNC_"):
    value: int = 11


class _FwdRefSettings(Settings, env_prefix="TS_FWD_"):
    inner: "_FwdInner"  # noqa: UP037 -- deliberate string forward reference


class _FwdInner(Model):
    value: int = 5


def test_secret_never_leaks(caplog: pytest.LogCaptureFixture) -> None:
    """`str`/`repr`/`format`/f-strings/logging all render `**********`."""
    secret = Secret("hunter2")
    assert str(secret) == "**********"
    assert repr(secret) == "Secret('**********')"
    assert f"{secret}" == "**********"
    assert f"{secret!r}" == "Secret('**********')"
    assert f"{secret:>15}" == "     **********"
    assert f"{secret!s}" == "**********"
    assert f"the password is {secret} and {secret!r}" == (
        "the password is ********** and Secret('**********')"
    )
    assert "hunter2" not in str(secret)
    assert "hunter2" not in repr(secret)
    assert "hunter2" not in f"{secret}"

    with caplog.at_level(logging.INFO):
        logging.getLogger("test").info("connecting with %s", secret)
        logging.getLogger("test").info("connecting with %r", secret)
        logging.getLogger("test").info("connecting with %s", f"pw={secret}")
    assert "hunter2" not in caplog.text

    class S(Settings, env_prefix="TS_LEAK_"):
        database_url: Secret[str]

    settings = S(database_url="postgres://u:p@h/db", _environ={})
    assert "postgres://u:p@h/db" not in repr(settings)
    assert repr(settings.database_url) == "Secret('**********')"


def test_secret_accessors() -> None:
    """`get_secret_value` is the only way out; eq/hash/bool work."""
    secret = Secret("hunter2")
    assert secret.get_secret_value() == "hunter2"
    assert secret == Secret("hunter2")
    assert secret != Secret("other")
    assert secret != "hunter2"  # noqa: S105 - not a real credential, a masking check
    assert hash(secret) == hash(Secret("hunter2"))
    assert Secret("") == Secret("")
    assert not Secret("")


def test_defaults_apply() -> None:
    """Unset fields fall back to declared defaults (§20, lowest tier)."""

    class S(Settings, env_prefix="TS_DEF_"):
        debug: bool = False
        retries: int = 3
        origins: list[str] = []  # noqa: RUF012 - the documented idiom; defaults copy per instance

    settings = S(_environ={})
    assert settings.debug is False
    assert settings.retries == 3
    assert settings.origins == []
    assert settings.__field_sources__ == {
        "debug": "default",
        "retries": "default",
        "origins": "default",
    }
    # Mutable defaults never share state between instances.
    other = S(_environ={})
    settings.origins.append("x")
    assert other.origins == []


def test_env_vars_with_prefix() -> None:
    """`env_prefix + FIELD_NAME` resolves, lax-coerced from strings."""

    class S(Settings, env_prefix="TS_ENV_"):
        debug: bool = False
        retries: int = 3
        origins: list[str] = []  # noqa: RUF012 - the documented idiom; defaults copy per instance

    settings = S(
        _environ={"TS_ENV_DEBUG": "true", "TS_ENV_RETRIES": "9", "TS_ENV_ORIGINS": '["a","b"]'}
    )
    assert settings.debug is True
    assert settings.retries == 9
    assert settings.origins == ["a", "b"]
    assert settings.__field_sources__["debug"] == "env:TS_ENV_DEBUG"


def test_init_kwargs_beat_env() -> None:
    """Constructor arguments — the §20 'CLI flags' tier — win."""

    class S(Settings, env_prefix="TS_INIT_"):
        retries: int = 3

    settings = S(retries=1, _environ={"TS_INIT_RETRIES": "9"})
    assert settings.retries == 1
    assert settings.__field_sources__["retries"] == "init"


def test_env_beats_dotenv_beats_toml(tmp_path: Path) -> None:
    """The middle tiers of §20 precedence resolve in order."""
    env_file = tmp_path / ".env"
    env_file.write_text("FULMEN_DEBUG=false\nFULMEN_PORT=1111\n")
    toml_file = tmp_path / "fulmen.toml"
    toml_file.write_text('[tool.fulmen]\ndebug = true\nport = 2222\nhost = "t"\n')

    class S(Settings, env_prefix="FULMEN_", env_file=env_file, toml_file=toml_file):
        debug: bool = True
        port: int = 0
        host: str = "default"

    # No env var: .env beats toml for debug, toml feeds host.
    settings = S(_environ={})
    assert settings.debug is False
    assert settings.port == 1111
    assert settings.host == "t"
    assert settings.__field_sources__["debug"] == "dotenv:FULMEN_DEBUG"
    assert settings.__field_sources__["host"].startswith("toml:")

    # Env var beats .env.
    settings = S(_environ={"FULMEN_DEBUG": "true"})
    assert settings.debug is True
    assert settings.__field_sources__["debug"] == "env:FULMEN_DEBUG"


def test_dotenv_opt_in_only(tmp_path: Path) -> None:
    """An `.env` file is never loaded unless `env_file` is configured."""
    env_file = tmp_path / ".env"
    env_file.write_text("TS_OPT_DEBUG=true\n")

    class NoDotenv(Settings, env_prefix="TS_OPT_"):
        debug: bool = False

    # Even with a matching var in the file, nothing resolves from it.
    settings = NoDotenv(_environ={}, _toml_file=None)
    assert settings.debug is False
    assert settings.__field_sources__["debug"] == "default"

    class WithDotenv(Settings, env_prefix="TS_OPT_", env_file=env_file):
        debug: bool = False

    settings = WithDotenv(_environ={}, _toml_file=None)
    assert settings.debug is True


def test_dotenv_ignored_in_production(tmp_path: Path) -> None:
    """`FULMEN_ENV=production` suppresses `env_file` with a warning."""
    env_file = tmp_path / ".env"
    env_file.write_text("TS_PROD_DEBUG=true\n")

    class S(Settings, env_prefix="TS_PROD_", env_file=env_file):
        debug: bool = False

    with pytest.warns(UserWarning, match="production"):
        settings = S(_environ={"FULMEN_ENV": "production"}, _toml_file=None)
    assert settings.debug is False


def test_dotenv_syntax(tmp_path: Path) -> None:
    """Quotes, `export`, comments and empty values parse like dotenv."""
    env_file = tmp_path / ".env"
    env_file.write_text(
        "# comment\n"
        "TS_SYN_PLAIN=hello\n"
        'TS_SYN_QUOTED="two words"\n'
        "TS_SYN_SINGLE='three'\n"
        "export TS_SYN_EXPORTED=yes\n"
        "TS_SYN_TRAILING=v # trailing comment\n"
        'TS_SYN_ESCAPED="a\\nb"\n'
        "TS_SYN_EMPTY=\n"
    )

    class S(Settings, env_prefix="TS_SYN_", env_file=env_file):
        plain: str = ""
        quoted: str = ""
        single: str = ""
        exported: str = ""
        trailing: str = ""
        escaped: str = ""
        empty: str = "x"

    s = S(_environ={}, _toml_file=None)
    assert s.plain == "hello"
    assert s.quoted == "two words"
    assert s.single == "three"
    assert s.exported == "yes"
    assert s.trailing == "v"
    assert s.escaped == "a\nb"
    assert s.empty == ""


def test_toml_section_derivation(tmp_path: Path) -> None:
    """`env_prefix` picks the `[tool.fulmen.<section>]` table to read."""
    toml_file = tmp_path / "fulmen.toml"
    toml_file.write_text(
        '[tool.fulmen]\nlevel = "root"\n'
        '[tool.fulmen.jwt]\nlevel = "jwt"\n'
        '[tool.fulmen.custom]\nlevel = "custom"\n'
    )

    class Root(Settings, env_prefix="FULMEN_", toml_file=toml_file):
        level: str = "none"

    class Jwt(Settings, env_prefix="FULMEN_JWT_", toml_file=toml_file):
        level: str = "none"

    class Custom(Settings, env_prefix="TS_X_", toml_file=toml_file, toml_section="custom"):
        level: str = "none"

    assert Root(_environ={}).level == "root"
    assert Jwt(_environ={}).level == "jwt"
    assert Custom(_environ={}).level == "custom"


def test_toml_disabled(tmp_path: Path) -> None:
    """`toml_file=None` removes the TOML source entirely."""

    class S(Settings, env_prefix="TS_OFF_", toml_file=None):
        x: int = 0

    settings = S(_environ={})
    assert settings.x == 0


def test_missing_required_aggregates() -> None:
    """Every missing field lands in one `StartupError` problem list."""

    class S(Settings, env_prefix="TS_REQ_"):
        required_one: int
        required_two: str
        fine: bool = True

    with pytest.raises(StartupError) as raised:
        S(_environ={})
    message = str(raised.value)
    assert "required_one" in message
    assert "required_two" in message
    assert "fine" not in message
    problems = getattr(raised.value, "problems", ())
    assert len(problems) == 2
    assert any("TS_REQ_REQUIRED_ONE" in p for p in problems)


def test_invalid_env_value_aggregates_with_source() -> None:
    """A bad env value names the variable it came from."""

    class S(Settings, env_prefix="TS_BAD_"):
        retries: int = 0
        also_missing: str

    with pytest.raises(StartupError) as raised:
        S(_environ={"TS_BAD_RETRIES": "not-an-int"})
    problems = getattr(raised.value, "problems", ())
    assert len(problems) == 2
    assert any("env:TS_BAD_RETRIES" in p for p in problems)
    assert any("also_missing" in p for p in problems)


def test_unknown_init_kwarg_is_a_problem() -> None:
    class S(Settings, env_prefix="TS_UNK_"):
        x: int = 0

    with pytest.raises(StartupError, match="unknown argument `nope`"):
        S(nope=1, _environ={})


def test_field_constraints_and_alias() -> None:
    """`Field()`/`Alias`/constraints apply to settings exactly like models."""

    class S(Settings, env_prefix="TS_FLD_"):
        retries: int = Field(default=3, description="retry count")
        timeout: int = 10

    settings = S(_environ={"TS_FLD_RETRIES": "5"})
    assert settings.retries == 5

    class Bounded(Settings, env_prefix="TS_BND_"):
        workers: int = Field(default=2, ge=1, le=4)

    assert Bounded(_environ={"TS_BND_WORKERS": "4"}).workers == 4
    with pytest.raises(StartupError):
        Bounded(_environ={"TS_BND_WORKERS": "99"})

    class Aliased(Settings, env_prefix="TS_AL_"):
        workers: Annotated[int, Alias("workers-count")] = 1

    # The env name derives from the wire name (the alias), not the field name.
    assert Aliased(_environ={"TS_AL_WORKERS_COUNT": "3"}).workers == 3
    assert Aliased(workers=4, _environ={}).workers == 4


def test_settings_equality_and_repr() -> None:
    class S(Settings, env_prefix="TS_EQ_"):
        x: int = 1

    assert S(_environ={}) == S(_environ={})
    assert S(x=2, _environ={}) != S(_environ={})
    assert S(_environ={}) != 42
    assert "x=1" in repr(S(_environ={}))


def test_nested_secret_rejected() -> None:
    """`Secret` is only valid as the whole field type."""
    with pytest.raises(AnnotationError, match="whole field"):

        class _Bad(Settings, env_prefix="TS_NEST_"):
            secrets: list[Secret[str]]


def test_app_settings_injection() -> None:
    """`Fulmen(settings=…)` resolves once and injects into handlers."""
    app = Fulmen(settings=_InjectSettings, openapi_url=None, docs_url=None)

    @app.get("/hi")
    def hi(settings: _InjectSettings) -> dict:
        return {"greeting": settings.greeting}

    with TestClient(app) as client:
        assert client.get("/hi").json() == {"greeting": "hello"}
        assert isinstance(app.settings, _InjectSettings)


def test_app_settings_instance() -> None:
    """A pre-built instance is used as-is and shared with handlers."""
    instance = _InstanceSettings(greeting="bonjour", _environ={})
    app = Fulmen(settings=instance, openapi_url=None, docs_url=None)

    @app.get("/hi")
    def hi(settings: _InstanceSettings) -> str:
        return settings.greeting

    with TestClient(app) as client:
        assert client.get("/hi").text == "bonjour"
        assert app.settings is instance


def test_app_settings_resolve_at_startup() -> None:
    """A missing required setting fails compilation before serving."""
    app = Fulmen(settings=_FailSettings, openapi_url=None, docs_url=None)

    @app.get("/x")
    def x() -> str:
        return "x"

    with pytest.raises(StartupError, match="required"):
        app._compile_native()


def test_settings_param_rejects_markers_and_defaults() -> None:
    """A settings parameter is injected — sources/defaults are nonsense."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/bad")
    def bad(settings: Annotated[_MarkerSettings, Header()]) -> str:
        return "x"

    @app.get("/bad2")
    def bad2(settings: _MarkerSettings = _MarkerSettings(_environ={})) -> str:  # noqa: B008 - intentional injection misuse
        return "x"

    with pytest.raises(StartupError):
        app._compile_native()


def test_mounted_app_settings() -> None:
    """A mounted app with its own settings injects them; the root's apply elsewhere."""
    root = Fulmen(settings=_RootSettings, openapi_url=None, docs_url=None)
    api = Fulmen(settings=_ApiSettings, prefix="/api", openapi_url=None, docs_url=None)

    @root.get("/r")
    def r(settings: _RootSettings) -> str:
        return settings.who

    @api.get("/a")
    def a(settings: _ApiSettings) -> str:
        return settings.who

    root.mount(api)
    with TestClient(root) as client:
        assert client.get("/r").text == "root"
        assert client.get("/api/a").text == "api"


def test_handler_without_app_settings_gets_own_class() -> None:
    """Injecting a Settings class the app doesn't own resolves it standalone."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/o")
    def o(settings: _OtherSettings) -> int:
        return settings.value

    with TestClient(app) as client:
        assert client.get("/o").text == "7"


def test_settings_survive_async_handlers() -> None:
    """Async handlers get the same injection."""
    app = Fulmen(settings=_AsyncSettings, openapi_url=None, docs_url=None)

    @app.get("/a")
    async def a(settings: _AsyncSettings) -> int:
        return settings.value

    with TestClient(app) as client:
        assert client.get("/a").text == "11"


# -- Secret[T] hint shapes -------------------------------------------------------


def test_bare_secret_annotation() -> None:
    """A bare `Secret` (no type argument) masks like `Secret[Any]`."""

    class S(Settings, env_prefix="TS_BARESEC_"):
        token: Secret = Secret("default")

    settings = S(_environ={"TS_BARESEC_TOKEN": "abc123"})
    assert settings.token.get_secret_value() == "abc123"
    assert "abc123" not in repr(settings.token)


def test_secret_requires_one_argument() -> None:
    """`Secret` is single-parameter — the typing machinery itself rejects
    `Secret[int, str]` before `_unsecret` sees it."""
    with pytest.raises(TypeError, match="Too many arguments"):

        class _Bad(Settings, env_prefix="TS_SECN_"):
            key: Secret[int, str]


def test_optional_secret_unwraps() -> None:
    class S(Settings, env_prefix="TS_OPTSEC_"):
        key: Secret[int] | None = None

    assert S(_environ={}).key is None
    assert S(_environ={"TS_OPTSEC_KEY": "7"}).key.get_secret_value() == 7


def test_secret_inside_a_wider_union_is_rejected() -> None:
    """`Secret[T] | U` is a nested secret — the same `whole field`
    rejection as `list[Secret[T]]`."""

    with pytest.raises(AnnotationError, match="whole field"):

        class _Bad(Settings, env_prefix="TS_UNSEC_"):
            key: Secret[int] | str = "x"


def test_plain_optional_field_is_not_secret() -> None:
    class S(Settings, env_prefix="TS_PLAINOPT_"):
        plain: int | None = None

    settings = S(_environ={})
    assert settings.plain is None
    assert S(_environ={"TS_PLAINOPT_PLAIN": "3"}).plain == 3


# -- loading edges ----------------------------------------------------------------


def test_forward_ref_settings_compile_lazily() -> None:
    """A `Settings` class declared before its field type (module level)
    compiles lazily on first use — `_FwdRefSettings` above references
    `_FwdInner`, defined after it."""
    settings = _FwdRefSettings(inner=_FwdInner(value=9), _environ={})
    assert settings.inner.value == 9


def test_unreadable_toml_is_a_problem(tmp_path) -> None:
    bad = tmp_path / "fulmen.toml"
    bad.write_text("not = [valid")

    class S(Settings, env_prefix="TS_BADTOML_", toml_file=bad):
        x: int = 0

    with pytest.raises(StartupError, match="cannot read"):
        S(_environ={})


def test_missing_env_file_is_silently_empty(tmp_path) -> None:
    class S(Settings, env_prefix="TS_NOENV_"):
        x: int = 3

    settings = S(_env_file=tmp_path / "absent.env", _environ={})
    assert settings.x == 3


def test_unreadable_env_file_is_silently_empty(tmp_path) -> None:
    env_file = tmp_path / "unreadable.env"
    env_file.write_text("X=1")
    env_file.chmod(0)

    class S(Settings, env_prefix="TS_UNREAD_"):
        x: int = 4

    try:
        settings = S(_env_file=env_file, _environ={})
    finally:
        env_file.chmod(0o644)
    assert settings.x == 4


def test_dotenv_line_without_key_is_skipped(tmp_path) -> None:
    env_file = tmp_path / ".env"
    env_file.write_text("=orphaned\nTS_DOT_X=8\n")

    class S(Settings, env_prefix="TS_DOT_", env_file=env_file):
        x: int = 0

    assert S(_environ={}, _toml_file=None).x == 8


def test_non_json_env_container_falls_back_to_raw() -> None:
    """JSON-looking fields (`dict`/`list`) accept a non-JSON raw string
    and let validation complain — the raw survives `_coerce`."""

    class S(Settings, env_prefix="TS_JSON_"):
        mapping: dict[str, int]

    with pytest.raises(StartupError):
        S(_environ={"TS_JSON_MAPPING": "not-json"})
    settings = S(_environ={"TS_JSON_MAPPING": '{"a": 1}'})
    assert settings.mapping == {"a": 1}


def test_env_prefix_must_be_a_str() -> None:
    class S(Settings, env_prefix="TS_EPR_"):
        x: int = 0

    with pytest.raises(TypeError, match="env_prefix must be a str"):
        S(_env_prefix=42, _environ={})


def test_unmapped_error_loc_names_the_settings() -> None:
    """A validation entry whose `loc` resolves to a *nested* field (not a
    declared settings field) still reports with its full location."""

    class S(Settings, env_prefix="TS_UNMAP_"):
        inner: _FwdInner

    with pytest.raises(StartupError) as excinfo:
        S(inner={"value": "not-an-int"}, _environ={})
    assert "failed to load" in str(excinfo.value)
    assert "inner/value" in str(excinfo.value)


def test_error_message_lists_checked_sources(tmp_path) -> None:
    """Field problems name every source checked — env, dotenv, toml."""
    env_file = tmp_path / ".env"
    env_file.write_text("")
    toml_file = tmp_path / "fulmen.toml"
    toml_file.write_text("[tool.fulmen]\n")

    class S(Settings, env_prefix="TS_SRC_", env_file=env_file, toml_file=toml_file):
        required: str

    with pytest.raises(StartupError) as raised:
        S(_environ={})
    message = str(raised.value)
    assert "dotenv" in message
    assert "fulmen.toml" in message


# -- settings-parameter compile checks (§20) ---------------------------------------


def test_settings_param_cannot_be_positional_only() -> None:
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/bad")
    def bad(settings: _MarkerSettings, /) -> str:
        return "x"

    with pytest.raises(StartupError, match="cannot be positional-only"):
        app._compile_native()


def test_settings_load_failure_is_a_startup_problem() -> None:
    """A foreign `Settings` class that fails construction surfaces as an
    aggregated startup problem, not a stray exception."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/bad")
    def bad(settings: _FailSettings) -> str:
        return "x"

    with pytest.raises(StartupError, match="required"):
        app._compile_native()


def test_same_foreign_settings_class_shares_the_cache() -> None:
    """Two routes declaring the same foreign `Settings` class share one
    resolved instance (the `_settings_cache` hit path)."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/a")
    def a(settings: _OtherSettings) -> int:
        return id(settings)

    @app.get("/b")
    def b(settings: _OtherSettings) -> int:
        return id(settings)

    with TestClient(app) as client:
        assert client.get("/a").json() == client.get("/b").json()


def test_injected_settings_wrapper_runs_handler() -> None:
    """The injected-settings wrapper is a plain closure — calling the
    compiled spec's handler exercises it off the dispatch path."""
    app = Fulmen(openapi_url=None, docs_url=None)

    @app.get("/sync")
    def sync_handler(q: str, settings: _OtherSettings) -> str:
        return f"{q}:{settings.value}"

    @app.get("/async")
    async def async_handler(settings: _OtherSettings) -> int:
        return settings.value

    specs, problems = app._route_specs({})
    assert problems == []
    sync_spec = next(spec for spec, _m, _l, _d in specs if spec[1] == "/sync")
    async_spec = next(spec for spec, _m, _l, _d in specs if spec[1] == "/async")
    wrapped = sync_spec[4]
    assert wrapped(q="hi") == "hi:7"

    import asyncio

    assert asyncio.run(async_spec[4]()) == 7


def test_toml_non_table_sections_collapse_to_empty(tmp_path) -> None:
    """A `[tool.fulmen]` leaf that isn't a table reads as no settings."""
    toml_file = tmp_path / "fulmen.toml"
    toml_file.write_text('tool = "scalar"\n')

    class S(Settings, env_prefix="TS_NT_", toml_file=toml_file):
        x: int = 1

    assert S(_environ={}).x == 1


def test_error_message_without_toml_omits_the_toml_source(tmp_path) -> None:
    """With `toml_file=None` the checked-sources line skips toml."""

    class S(Settings, env_prefix="TS_NOTOML_", toml_file=None):
        required: str

    with pytest.raises(StartupError) as raised:
        S(_environ={}, _toml_file=None)
    message = str(raised.value)
    assert "checked:" in message
    assert "fulmen.toml" not in message
