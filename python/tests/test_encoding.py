"""M2.9 integration tests: descriptor-driven response encoding + debug
output validation (DESIGN.md §10.2, §11.6, §13, §14.2).

A declared return annotation compiles into `response_descs`; the Rust
encoder walks the returned object under that descriptor instead of the
generic dict/list walk. Unannotated routes and `Any` keep the generic
path. `Fulmen(debug=True)` (or `serve_for_test(debug=…)`) additionally
validates every result against the picked variant's validator.
"""

from __future__ import annotations

import dataclasses
import datetime
import enum
import ipaddress
import json
import uuid
from collections.abc import Callable
from decimal import Decimal
from typing import Annotated, Any, Literal, NewType, TypedDict

from conftest import fetch
from fulmen import Alias, Fulmen, Model, Range
from fulmen._fulmen import serve_for_test
from fulmen.responses import Created, Response

UTC = datetime.UTC
USER_ID = uuid.UUID("12345678-1234-5678-1234-567812345678")


# ---- fixtures ----------------------------------------------------------------


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


class Level(enum.IntEnum):
    LOW = 1
    HIGH = 9


class User(Model):
    id: int
    name: str
    user_id: uuid.UUID
    balance: Decimal
    color: Color


class CamelUser(Model, rename="camel"):
    user_name: str
    home_town: str


class AliasedUser(Model):
    user_name: Annotated[str, Alias("userName")]


class Nested(Model):
    user: User
    tags: list[str]


class Kept(Model, extra="keep"):
    id: int


class OptionalFields(Model):
    name: str
    nickname: str | None = None


@dataclasses.dataclass
class Point:
    x: int
    y: int


@dataclasses.dataclass(slots=True)
class Slotted:
    x: int
    y: int


class MovieTD(TypedDict):
    title: str
    year: int


class Page[T](Model):
    items: list[T]
    total: int


class Blob(Model):
    payload: bytes


UserId = NewType("UserId", int)


def _user(name: str = "ada") -> User:
    return User(
        id=1,
        name=name,
        user_id=USER_ID,
        balance=Decimal("1.50"),
        color=Color.RED,
    )


def _json(res) -> Any:
    return json.loads(res.body)


# ---- scalar encodes ----------------------------------------------------------


def test_scalar_types(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/int")
    def i() -> int:
        return 42

    @app.get("/float")
    def f() -> float:
        return 1.5

    @app.get("/float-from-int")
    def fi() -> float:
        return 3

    @app.get("/bool")
    def b() -> bool:
        return True

    base = serve(app)
    res = fetch(f"{base}/int")
    assert res.status == 200
    assert res.header("content-type") == "application/json"
    assert res.body == b"42"
    assert fetch(f"{base}/float").body == b"1.5"
    assert fetch(f"{base}/float-from-int").body == b"3.0"
    assert fetch(f"{base}/bool").body == b"true"


def test_str_stays_text_and_bytes_stay_octets(serve: Callable[[Fulmen], str]) -> None:
    # §10.2's runtime-type channels survive the contract: a declared
    # `-> str` is text/plain, `-> bytes` is octet-stream — not JSON.
    app = Fulmen()

    @app.get("/s")
    def s() -> str:
        return "hello"

    @app.get("/b")
    def b() -> bytes:
        return b"\x00\x01hi"

    base = serve(app)
    res = fetch(f"{base}/s")
    assert res.status == 200
    assert res.header("content-type") == "text/plain; charset=utf-8"
    assert res.body == b"hello"
    res = fetch(f"{base}/b")
    assert res.header("content-type") == "application/octet-stream"
    assert res.body == b"\x00\x01hi"


def test_decimal_encodes_as_string(serve: Callable[[Fulmen], str]) -> None:
    # §11.6: Decimal travels as a string so arbitrary precision survives
    # the wire — `str()` keeps the value's own digits.
    app = Fulmen()

    @app.get("/d")
    def d() -> Decimal:
        return Decimal("1.50")

    @app.get("/big")
    def big() -> Decimal:
        return Decimal("12345678901234567890.123")

    base = serve(app)
    assert fetch(f"{base}/d").body == b'"1.50"'
    assert fetch(f"{base}/big").body == b'"12345678901234567890.123"'


def test_temporal_types(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/dt")
    def dt() -> datetime.datetime:
        return datetime.datetime(2024, 1, 2, 3, 4, 5, tzinfo=UTC)

    @app.get("/dt-naive")
    def dtn() -> datetime.datetime:
        return datetime.datetime(2024, 1, 2, 3, 4, 5)

    @app.get("/d")
    def d() -> datetime.date:
        return datetime.date(2024, 2, 29)

    @app.get("/t")
    def t() -> datetime.time:
        return datetime.time(3, 4, 5, 6000)

    @app.get("/td")
    def td() -> datetime.timedelta:
        return datetime.timedelta(days=3, hours=4, minutes=5, seconds=6)

    @app.get("/td-neg")
    def tdn() -> datetime.timedelta:
        return datetime.timedelta(seconds=-1)

    base = serve(app)
    assert fetch(f"{base}/dt").body == b'"2024-01-02T03:04:05+00:00"'
    assert fetch(f"{base}/dt-naive").body == b'"2024-01-02T03:04:05"'
    assert fetch(f"{base}/d").body == b'"2024-02-29"'
    assert fetch(f"{base}/t").body == b'"03:04:05.006000"'
    assert fetch(f"{base}/td").body == b'"P3DT4H5M6S"'
    assert fetch(f"{base}/td-neg").body == b'"-P0DT0H0M1S"'


def test_uuid_and_ip(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/u")
    def u() -> uuid.UUID:
        return USER_ID

    @app.get("/v4")
    def v4() -> ipaddress.IPv4Address:
        return ipaddress.IPv4Address("10.0.0.1")

    @app.get("/v6")
    def v6() -> ipaddress.IPv6Address:
        return ipaddress.IPv6Address("::1")

    base = serve(app)
    assert fetch(f"{base}/u").body == b'"12345678-1234-5678-1234-567812345678"'
    assert fetch(f"{base}/v4").body == b'"10.0.0.1"'
    assert fetch(f"{base}/v6").body == b'"::1"'


def test_enum_encodes_member_value(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/c")
    def c() -> Color:
        return Color.BLUE

    @app.get("/l")
    def lev() -> Level:
        return Level.HIGH

    base = serve(app)
    assert fetch(f"{base}/c").body == b'"blue"'
    assert fetch(f"{base}/l").body == b"9"


def test_none_and_literal(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/none")
    def n() -> None:
        return None

    @app.get("/lit")
    def lit() -> Literal["a", "b"]:
        return "b"

    base = serve(app)
    res = fetch(f"{base}/none")
    assert res.status == 204
    assert res.body == b""
    assert fetch(f"{base}/lit").body == b'"b"'


def test_newtype_and_any(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/uid")
    def uid() -> UserId:
        return UserId(7)

    @app.get("/any")
    def a() -> Any:
        return {"x": [1, "two", None]}

    base = serve(app)
    assert fetch(f"{base}/uid").body == b"7"
    assert _json(fetch(f"{base}/any")) == {"x": [1, "two", None]}


# ---- containers and records ---------------------------------------------------


def test_containers(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/list")
    def lst() -> list[int]:
        return [1, 2, 3]

    @app.get("/set")
    def s() -> set[int]:
        return {1, 2}

    @app.get("/frozen")
    def fs() -> frozenset[str]:
        return frozenset({"a"})

    @app.get("/tuple-var")
    def tv() -> tuple[int, ...]:
        return (1, 2)

    @app.get("/tuple-fixed")
    def tf() -> tuple[int, str]:
        return (1, "x")

    @app.get("/dict")
    def d() -> dict[str, int]:
        return {"a": 1}

    @app.get("/int-keys")
    def ik() -> dict[int, str]:
        return {1: "a", 2: "b"}

    base = serve(app)
    assert fetch(f"{base}/list").body == b"[1,2,3]"
    assert sorted(_json(fetch(f"{base}/set"))) == [1, 2]
    assert fetch(f"{base}/frozen").body == b'["a"]'
    assert fetch(f"{base}/tuple-var").body == b"[1,2]"
    assert fetch(f"{base}/tuple-fixed").body == b'[1,"x"]'
    assert fetch(f"{base}/dict").body == b'{"a":1}'
    assert _json(fetch(f"{base}/int-keys")) == {"1": "a", "2": "b"}


def test_model_encodes_by_field_walk(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/u")
    def u() -> User:
        return _user()

    base = serve(app)
    res = fetch(f"{base}/u")
    assert res.status == 200
    assert res.header("content-type") == "application/json"
    assert _json(res) == {
        "id": 1,
        "name": "ada",
        "user_id": "12345678-1234-5678-1234-567812345678",
        "balance": "1.5",
        "color": "red",
    }


def test_model_rename_and_alias(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/camel")
    def c() -> CamelUser:
        # Model kwargs are validated wire input — camel-renamed fields
        # construct under their wire names.
        return CamelUser(userName="ada", homeTown="paris")

    @app.get("/aliased")
    def a() -> AliasedUser:
        return AliasedUser(userName="ada")

    base = serve(app)
    assert _json(fetch(f"{base}/camel")) == {"userName": "ada", "homeTown": "paris"}
    assert _json(fetch(f"{base}/aliased")) == {"userName": "ada"}


def test_model_extras_keep_and_optional(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/kept")
    def k() -> Kept:
        return Kept(id=1, extra_stuff="yes")  # type: ignore[call-arg]

    @app.get("/opt")
    def o() -> OptionalFields:
        return OptionalFields(name="ada")

    base = serve(app)
    body = _json(fetch(f"{base}/kept"))
    assert body["id"] == 1
    assert body["extra_stuff"] == "yes"
    assert _json(fetch(f"{base}/opt")) == {"name": "ada", "nickname": None}


def test_nested_model_and_generic_model(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/nested")
    def n() -> Nested:
        return Nested(user=_user(), tags=["a", "b"])

    @app.get("/page")
    def p() -> Page[Point]:
        return Page[Point](items=[Point(1, 2), Point(3, 4)], total=2)

    base = serve(app)
    body = _json(fetch(f"{base}/nested"))
    assert body["user"]["name"] == "ada"
    assert body["tags"] == ["a", "b"]
    assert _json(fetch(f"{base}/page")) == {
        "items": [{"x": 1, "y": 2}, {"x": 3, "y": 4}],
        "total": 2,
    }


def test_dataclass_and_slotted_dataclass(serve: Callable[[Fulmen], str]) -> None:
    # Field walk, not __dict__ assumptions: a slotted dataclass has no
    # __dict__, the descriptor's declared fields still encode.
    app = Fulmen()

    @app.get("/p")
    def p() -> Point:
        return Point(1, 2)

    @app.get("/s")
    def s() -> Slotted:
        return Slotted(3, 4)

    base = serve(app)
    assert _json(fetch(f"{base}/p")) == {"x": 1, "y": 2}
    assert _json(fetch(f"{base}/s")) == {"x": 3, "y": 4}


def test_typed_dict(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/m")
    def m() -> MovieTD:
        return {"title": "spirited", "year": 2001}

    base = serve(app)
    assert _json(fetch(f"{base}/m")) == {"title": "spirited", "year": 2001}


def test_bytes_inside_json_are_base64(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/d")
    def d() -> dict[str, bytes]:
        return {"payload": b"hi"}

    @app.get("/m")
    def m() -> Blob:
        return Blob(payload=b"hey")

    base = serve(app)
    assert _json(fetch(f"{base}/d")) == {"payload": "aGk="}
    assert _json(fetch(f"{base}/m")) == {"payload": "aGV5"}


# ---- unions and status variants -----------------------------------------------


def test_same_status_union_picks_by_shape(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/u")
    def u() -> Point | User:
        return _user()

    @app.get("/p")
    def p() -> Point | User:
        return Point(1, 2)

    base = serve(app)
    assert _json(fetch(f"{base}/u"))["name"] == "ada"
    assert _json(fetch(f"{base}/p")) == {"x": 1, "y": 2}


def test_scalar_union_picks_by_shape(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/n")
    def n() -> int | str:
        return 5

    @app.get("/s")
    def s() -> int | str:
        return "hi"

    base = serve(app)
    assert fetch(f"{base}/n").body == b"5"
    res = fetch(f"{base}/s")
    # `int | str` — the str variant's §10.2 channel wins by shape.
    assert res.header("content-type") == "text/plain; charset=utf-8"
    assert res.body == b"hi"


def test_created_variant_by_status(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/u")
    def u() -> User | Created[Point]:
        return _user()

    @app.get("/c")
    def c() -> User | Created[Point]:
        return Created(Point(9, 9))

    @app.get("/bare")
    def bare() -> Created[User]:
        # A bare body fulfils the single declared variant — `-> Created[T]`
        # answers 201 even without the `Created(...)` wrapper.
        return _user()

    base = serve(app)
    res = fetch(f"{base}/u")
    assert res.status == 200
    assert _json(res)["name"] == "ada"
    res = fetch(f"{base}/c")
    assert res.status == 201
    assert _json(res) == {"x": 9, "y": 9}
    res = fetch(f"{base}/bare")
    assert res.status == 201
    assert _json(res)["name"] == "ada"


def test_response_object_picks_body_desc_by_status(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/c")
    def c() -> User | Created[Point]:
        # The body's descriptor is selected by the *actual* status.
        return Response(Point(7, 7), status=201)

    @app.get("/u")
    def u() -> User | Created[Point]:
        return Response(_user(), status=200)

    @app.get("/other")
    def o() -> User | Point | Created[dict]:
        # Status 202 has no declared variant and no single or unique-200
        # descriptor to fall back on — the body encodes generically (a
        # debug-mode warning is logged).
        return Response({"ad-hoc": True}, status=202)

    @app.get("/plain")
    def pl() -> dict[str, int]:
        # One descriptor total: it covers whatever status the Response chose.
        return Response({"e": 1}, status=404)

    base = serve(app)
    res = fetch(f"{base}/c")
    assert res.status == 201
    assert _json(res) == {"x": 7, "y": 7}
    res = fetch(f"{base}/u")
    assert res.status == 200
    assert _json(res)["name"] == "ada"
    res = fetch(f"{base}/other")
    assert res.status == 202
    assert _json(res) == {"ad-hoc": True}
    res = fetch(f"{base}/plain")
    assert res.status == 404
    assert _json(res) == {"e": 1}


def test_unannotated_and_dynamic_response_keep_generic(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/free")
    def free():
        return {"a": 1}

    @app.get("/dyn")
    def dyn() -> Response:
        return Response({"b": 2}, status=202, content_type="application/json")

    base = serve(app)
    res = fetch(f"{base}/free")
    assert res.status == 200
    assert _json(res) == {"a": 1}
    res = fetch(f"{base}/dyn")
    assert res.status == 202
    assert _json(res) == {"b": 2}


# ---- contract violations -------------------------------------------------------


def test_shape_violation_is_500_without_debug(serve: Callable[[Fulmen], str]) -> None:
    # Always-on structural checking: a body that fits no declared variant
    # rides the ordinary internal-error path — 500, details in the log.
    app = Fulmen()

    @app.get("/bad")
    def bad() -> int:
        return "nope"  # type: ignore[return-value]

    @app.get("/bad-str")
    def bad_str() -> str:
        return 5  # type: ignore[return-value]

    @app.get("/bad-bool")
    def bad_bool() -> int:
        return True  # type: ignore[return-value]

    @app.get("/bad-model")
    def bad_model() -> User:
        return {"wrong": 1}  # type: ignore[return-value]

    base = serve(app)
    for path in ("/bad", "/bad-str", "/bad-bool", "/bad-model"):
        res = fetch(f"{base}{path}")
        assert res.status == 500, path
        assert res.header("content-type") == "application/problem+json"
        assert _json(res)["title"] == "Internal Server Error"


def test_debug_output_violation_reports_errors(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen(debug=True)

    @app.get("/bad")
    def bad() -> User:
        return {"name": "ada"}  # type: ignore[return-value] -- missing required fields

    @app.get("/constrained")
    def constrained() -> Annotated[int, Range(le=10)]:
        return 42

    @app.get("/bad-str")
    def bad_str() -> str:
        return 5  # type: ignore[return-value]

    @app.get("/ok")
    def ok() -> User:
        return _user()

    base = serve(app)

    res = fetch(f"{base}/bad")
    assert res.status == 500
    assert res.header("content-type") == "application/problem+json"
    body = _json(res)
    assert body["title"] == "Response validation failed"
    assert body["errors"], body
    assert any(e["code"] == "type" or e["code"] == "missing" for e in body["errors"])

    res = fetch(f"{base}/constrained")
    assert res.status == 500
    body = _json(res)
    assert body["errors"][0]["code"] == "le"
    assert body["errors"][0]["loc"].startswith("/response")

    res = fetch(f"{base}/bad-str")
    assert res.status == 500
    assert _json(res)["title"] == "Response validation failed"

    # A conforming result still answers 200 under debug.
    res = fetch(f"{base}/ok")
    assert res.status == 200
    assert _json(res)["name"] == "ada"


def test_debug_shape_errors_carry_loc(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen(debug=True)

    @app.get("/bad")
    def bad() -> list[int]:
        return [1, "x", 3]  # type: ignore[list-item]

    base = serve(app)
    res = fetch(f"{base}/bad")
    assert res.status == 500
    body = _json(res)
    assert body["errors"]
    assert body["errors"][0]["loc"] == "/response/1"


def test_serve_flag_enables_debug_without_app_flag() -> None:
    # `serve_for_test(debug=True)` / `run(debug=…)` activate output
    # validation for an app built without `Fulmen(debug=True)`.
    app = Fulmen()

    @app.get("/bad")
    def bad() -> int:
        return "nope"  # type: ignore[return-value]

    server = serve_for_test(app._compile_native(), debug=True)
    try:
        res = fetch(f"http://{server.addr()}/bad")
        assert res.status == 500
        assert _json(res)["title"] == "Response validation failed"
    finally:
        server.stop()


def test_debug_logs_handler_identity(serve: Callable[[Fulmen], str], caplog) -> None:
    app = Fulmen(debug=True)

    @app.get("/bad")
    def bad() -> int:
        return "nope"  # type: ignore[return-value]

    base = serve(app)
    with caplog.at_level("ERROR", logger="fulmen"):
        assert fetch(f"{base}/bad").status == 500
    assert any("GET /bad" in record.getMessage() for record in caplog.records)


def test_debug_mode_passes_valid_results(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen(debug=True)

    @app.get("/u")
    def u() -> User:
        return _user()

    @app.get("/c")
    def c() -> User | Created[Point]:
        # Typed status variants validate against the *actual* status's
        # descriptor — 201 selects the Point variant, not User.
        return Created(Point(1, 1))

    @app.get("/wrong-variant")
    def wv() -> User | Created[Point]:
        # A User at 201 is a contract violation of the Point variant.
        return Created(_user())  # type: ignore[arg-type]

    base = serve(app)
    assert fetch(f"{base}/u").status == 200
    res = fetch(f"{base}/c")
    assert res.status == 201
    assert _json(res) == {"x": 1, "y": 1}
    assert fetch(f"{base}/wrong-variant").status == 500
