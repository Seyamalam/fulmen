"""M2 integration tests: typed parameters, models, responses, error routing.

Everything runs against a real server on an ephemeral port with a stdlib
urllib client — the parameter pipeline is extracted and validated in Rust
before the handler is ever called.
"""

from __future__ import annotations

import json
import logging
import pathlib
from collections.abc import Callable
from typing import Annotated, Literal

import pytest
from conftest import fetch
from fulmen import (
    Body,
    Cookie,
    Created,
    Field,
    Form,
    Fulmen,
    Header,
    Len,
    Model,
    NoContent,
    Query,
    Range,
    Redirect,
    Response,
    Stream,
)
from fulmen.errors import (
    BadRequest,
    HTTPError,
    NotFound,
    Unauthorized,
    ValidationError,
)
from fulmen.responses import FileResponse, HTMLResponse, JSONResponse, TextResponse


def _json(body: dict | list | str) -> bytes:
    return json.dumps(body).encode()


JSON_CT = {"content-type": "application/json"}
FORM_CT = {"content-type": "application/x-www-form-urlencoded"}


# ---- models -----------------------------------------------------------------


class User(Model):
    id: int
    name: Annotated[str, Len(min=1)] = "anon"
    tags: list[str] = Field(default_factory=list)


def test_model_construct_and_dump() -> None:
    user = User(id=7, name="ada")
    assert user.id == 7
    assert user.name == "ada"
    assert user.tags == []
    user2 = User(id=7, name="ada")
    user2.tags.append("x")
    assert user.tags == []  # default_factory is per-instance
    assert user == user2 or user.tags != user2.tags
    assert user.model_dump() == {"id": 7, "name": "ada", "tags": []}


def test_model_init_validates() -> None:
    with pytest.raises(ValidationError):
        User(id=7, name="")
    with pytest.raises(ValidationError):
        User(name="ada")  # `id` is required


def test_model_lax_constructs_from_text() -> None:
    # lax mode coerces wire-shaped input ("7" → 7) — used by form/query
    # adapters and `strict=False` models.
    class Lax(Model, strict=False):
        id: int

    assert Lax(id="7").id == 7


def test_model_setattr_validates() -> None:
    user = User(id=1)
    user.name = "grace"
    assert user.name == "grace"
    with pytest.raises(ValidationError):
        user.name = ""


def test_frozen_model_rejects_setattr() -> None:
    class Frozen(Model, frozen=True):
        id: int

    f = Frozen(id=1)
    with pytest.raises(TypeError, match="frozen"):
        f.id = 2  # type: ignore[misc]
    assert hash(f) == hash(Frozen(id=1))


def test_model_extra_forbid() -> None:
    class Strict(Model, extra="forbid"):
        id: int

    with pytest.raises(ValidationError):
        Strict(id=1, extra_key=2)


def test_model_validate() -> None:
    assert User.model_validate({"id": 3}).id == 3


# ---- path parameters ---------------------------------------------------------


def test_annotated_path_param_validates(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/items/{name}")
    def item(name: Annotated[str, Len(min=3)]) -> str:
        return name

    base = serve(app)
    assert fetch(f"{base}/items/abc").status == 200
    res = fetch(f"{base}/items/a")
    assert res.status == 422
    assert json.loads(res.body)["errors"][0]["loc"] == "/path/name"


# ---- query parameters --------------------------------------------------------


def test_query_typed_and_default(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/q")
    def q(page: Annotated[int, Query(ge=1)] = 1, q2: str = "x") -> dict:
        return {"page": page, "q2": q2}

    base = serve(app)
    assert json.loads(fetch(f"{base}/q").body) == {"page": 1, "q2": "x"}
    assert json.loads(fetch(f"{base}/q?page=5&q2=y").body) == {"page": 5, "q2": "y"}
    res = fetch(f"{base}/q?page=0")
    assert res.status == 422


def test_marker_default_is_literal(serve: Callable[[Fulmen], str]) -> None:
    # `Query(default=...)` with no signature default: the marker's value is
    # passed literally — the signature has no default to fall back on.
    app = Fulmen()

    @app.get("/lim")
    def lim(limit: Annotated[int, Query(default=7)]) -> dict:
        return {"limit": limit}

    base = serve(app)
    assert json.loads(fetch(f"{base}/lim").body) == {"limit": 7}
    assert json.loads(fetch(f"{base}/lim?limit=2").body) == {"limit": 2}


def test_field_factory_default_is_per_request(serve: Callable[[Fulmen], str]) -> None:
    # `Field(default_factory=...)` as the signature default: the factory runs
    # per request, so a handler mutating the value leaks nothing across calls.
    app = Fulmen()

    @app.get("/ft")
    def ft(
        tag: Annotated[list[str], Query()] = Field(default_factory=list),  # noqa: B008 - the marker API
    ) -> dict:
        tag.append("got")
        return {"tag": tag}

    base = serve(app)
    assert json.loads(fetch(f"{base}/ft").body) == {"tag": ["got"]}
    assert json.loads(fetch(f"{base}/ft").body) == {"tag": ["got"]}
    assert json.loads(fetch(f"{base}/ft?tag=a").body) == {"tag": ["a", "got"]}


def test_query_list_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/tags")
    def tags(tag: Query[list[str]]) -> dict:
        return {"tag": tag}

    base = serve(app)
    assert json.loads(fetch(f"{base}/tags?tag=a&tag=b").body) == {"tag": ["a", "b"]}
    assert fetch(f"{base}/tags").status == 422


def test_query_alias_and_optional(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/search")
    def search(
        page_size: Annotated[int, Query(alias="pageSize")] = 20,
        filter: Query[str | None] = None,
    ) -> dict:
        return {"size": page_size, "filter": filter}

    base = serve(app)
    res = fetch(f"{base}/search?pageSize=50")
    assert json.loads(res.body) == {"size": 50, "filter": None}


def test_query_literal_and_enum(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/mode")
    def mode(m: Annotated[Literal["a", "b"], Query()] = "a") -> str:
        return m

    base = serve(app)
    assert fetch(f"{base}/mode?m=b").body == b"b"
    assert fetch(f"{base}/mode?m=z").status == 422


# ---- header parameters -------------------------------------------------------


def test_header_wire_name_and_case(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/h")
    def h(x_token: Header[str]) -> str:
        return x_token

    base = serve(app)
    # `x_token` derives the wire name `x-token`; headers match case-insensitively.
    res = fetch(f"{base}/h", headers={"X-TOKEN": "tok"})
    assert res.status == 200
    assert res.body == b"tok"


def test_header_alias_and_missing(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/auth")
    def auth(
        key: Annotated[str, Header(alias="X-Api-Key")], agent: Header[str | None] = None
    ) -> dict:
        return {"key": key, "agent": agent}

    base = serve(app)
    res = fetch(f"{base}/auth", headers={"x-api-key": "k1"})
    assert json.loads(res.body) == {"key": "k1", "agent": None}
    res = fetch(f"{base}/auth")
    assert res.status == 422
    assert json.loads(res.body)["errors"][0]["loc"] == "/header/X-Api-Key"


# ---- cookie parameters -------------------------------------------------------


def test_cookie_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/c")
    def c(session: Cookie[str], theme: Cookie[str] = "light") -> dict:
        return {"session": session, "theme": theme}

    base = serve(app)
    res = fetch(f"{base}/c", headers={"Cookie": "session=abc; theme=dark"})
    assert json.loads(res.body) == {"session": "abc", "theme": "dark"}
    assert fetch(f"{base}/c").status == 422


# ---- body parameters ---------------------------------------------------------


def test_model_body(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/users")
    def create(user: User) -> dict:
        return {"id": user.id, "name": user.name, "tags": user.tags}

    base = serve(app)
    res = fetch(f"{base}/users", "POST", _json({"id": 1, "name": "ada"}), headers=JSON_CT)
    assert res.status == 200
    assert json.loads(res.body) == {"id": 1, "name": "ada", "tags": []}
    res = fetch(f"{base}/users", "POST", _json({"id": 1, "name": ""}), headers=JSON_CT)
    assert res.status == 422
    assert json.loads(res.body)["errors"][0]["loc"] == "/body/name"


def test_body_marker_and_dict(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/echo")
    def echo(payload: Body[dict]) -> dict:
        return payload

    res = fetch(f"{serve(app)}/echo", "POST", _json({"x": [1, 2]}), headers=JSON_CT)
    assert res.status == 200
    assert json.loads(res.body) == {"x": [1, 2]}


def test_malformed_json_is_400(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/users")
    def create(user: User) -> dict:
        return {"id": user.id}

    res = fetch(f"{serve(app)}/users", "POST", b"{nope", headers=JSON_CT)
    assert res.status == 400
    assert res.header("Content-Type") == "application/problem+json"
    assert json.loads(res.body)["title"] == "Bad Request"


def test_unsupported_content_type_is_415(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/users")
    def create(user: User) -> dict:
        return {"id": user.id}

    res = fetch(f"{serve(app)}/users", "POST", b"<xml/>", headers={"content-type": "text/xml"})
    assert res.status == 415
    assert res.header("Content-Type") == "application/problem+json"


def test_no_body_param_ignores_body(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/ping")
    def ping() -> str:
        return "pong"

    res = fetch(f"{serve(app)}/ping", "POST", b"\x00\xff not json", headers=JSON_CT)
    assert res.status == 200
    assert res.body == b"pong"


def test_raw_body_bytes(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/raw")
    def raw(payload: Body[bytes]) -> str:
        return f"{len(payload)}"

    res = fetch(
        f"{serve(app)}/raw",
        "POST",
        b"\x01\x02\x03",
        headers={"content-type": "application/octet-stream"},
    )
    assert res.body == b"3"


# ---- form parameters ----------------------------------------------------------


def test_form_fields(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/login")
    def login(username: Form[str], age: Form[int] = 0) -> dict:
        return {"u": username, "a": age}

    base = serve(app)
    res = fetch(f"{base}/login", "POST", b"username=ada&age=36", headers=FORM_CT)
    assert json.loads(res.body) == {"u": "ada", "a": 36}
    assert fetch(f"{base}/login", "POST", b"age=3", headers=FORM_CT).status == 422
    # A JSON body to a form route supplies no form fields.
    res = fetch(f"{base}/login", "POST", _json({"username": "ada"}), headers=JSON_CT)
    assert res.status == 422
    assert json.loads(res.body)["errors"][0]["loc"] == "/form/username"


# ---- mixed sources in one handler --------------------------------------------


def test_all_sources_one_handler(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/all/{uid}")
    def all_(
        uid: int,
        verbose: bool = False,
        x_kind: Header[str] = "?",
        session: Cookie[str | None] = None,
        item: Body[dict | None] = None,
    ) -> dict:
        return {
            "uid": uid,
            "verbose": verbose,
            "kind": x_kind,
            "session": session,
            "item": item,
        }

    res = fetch(
        f"{serve(app)}/all/9?verbose=true",
        "POST",
        _json({"n": 1}),
        headers={"content-type": "application/json", "X-Kind": "demo", "Cookie": "session=s1"},
    )
    assert res.status == 200
    assert json.loads(res.body) == {
        "uid": 9,
        "verbose": True,
        "kind": "demo",
        "session": "s1",
        "item": {"n": 1},
    }


# ---- response classes ---------------------------------------------------------


def test_typed_response_classes(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/j")
    def j() -> JSONResponse[dict]:
        return JSONResponse({"k": 1}, status=202, headers={"X-A": "b"})

    @app.get("/t")
    def t() -> TextResponse:
        return TextResponse("plain")

    @app.get("/h")
    def h() -> HTMLResponse:
        return HTMLResponse("<b>x</b>")

    base = serve(app)
    res = fetch(f"{base}/j")
    assert (res.status, res.header("Content-Type")) == (202, "application/json")
    assert res.header("X-A") == "b"
    assert fetch(f"{base}/t").header("Content-Type") == "text/plain; charset=utf-8"
    assert fetch(f"{base}/h").header("Content-Type") == "text/html; charset=utf-8"


def test_created_accepted_no_content(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/new")
    def new() -> Created[dict]:
        return Created({"id": 5}, headers={"Location": "/users/5"})

    @app.post("/job")
    def job() -> Response:
        from fulmen.responses import Accepted

        return Accepted({"queued": True})

    @app.delete("/gone")
    def gone() -> NoContent:
        return NoContent()

    base = serve(app)
    res = fetch(f"{base}/new", "POST", b"{}")
    assert res.status == 201
    assert res.header("Location") == "/users/5"
    assert json.loads(res.body) == {"id": 5}
    assert fetch(f"{base}/job", "POST", b"{}").status == 202
    res = fetch(f"{base}/gone", "DELETE")
    assert res.status == 204
    assert res.body == b""


def test_redirect(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/old")
    def old() -> Redirect:
        return Redirect("/new")

    res = fetch(f"{serve(app)}/old", follow_redirects=False)
    assert res.status == 307
    assert res.header("Location") == "/new"


def test_file_response(serve: Callable[[Fulmen], str], tmp_path: pathlib.Path) -> None:
    path = tmp_path / "report.txt"
    path.write_text("the report")
    app = Fulmen()

    @app.get("/report")
    def report() -> FileResponse:
        return FileResponse(path)

    res = fetch(f"{serve(app)}/report")
    assert res.status == 200
    assert res.body == b"the report"
    assert res.header("Content-Type") == "text/plain"
    assert res.header("Content-Disposition") == 'attachment; filename="report.txt"'


def test_stream_buffered(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/s")
    def s() -> Stream:
        return Stream([b"ab", "cd", b"ef"])

    res = fetch(f"{serve(app)}/s")
    assert res.status == 200
    assert res.body == b"abcdef"


def test_union_return_none_is_204(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/maybe/{i}")
    def maybe(i: int) -> dict | None:
        return {"i": i} if i else None

    base = serve(app)
    assert fetch(f"{base}/maybe/1").status == 200
    assert fetch(f"{base}/maybe/0").status == 204


# ---- HTTPError ----------------------------------------------------------------


def test_http_error_problem_shape(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/nf")
    def nf() -> str:
        raise NotFound(detail="no such thing")

    res = fetch(f"{serve(app)}/nf")
    assert res.status == 404
    assert res.header("Content-Type") == "application/problem+json"
    body = json.loads(res.body)
    assert body == {
        "type": "about:blank",
        "title": "Not Found",
        "status": 404,
        "detail": "no such thing",
    }


def test_http_error_status_and_headers(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/teapot")
    def teapot() -> str:
        raise HTTPError(418, "short and stout")

    @app.get("/auth")
    def auth() -> str:
        raise Unauthorized(headers={"WWW-Authenticate": "Bearer"})

    base = serve(app)
    res = fetch(f"{base}/teapot")
    assert res.status == 418
    assert json.loads(res.body)["detail"] == "short and stout"
    res = fetch(f"{base}/auth")
    assert res.status == 401
    assert res.header("WWW-Authenticate") == "Bearer"


def test_http_error_errors_payload(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/multi")
    def multi() -> str:
        raise BadRequest(errors=[{"loc": "/body/x", "code": "int", "message": "nope"}])

    res = fetch(f"{serve(app)}/multi")
    assert res.status == 400
    assert json.loads(res.body)["errors"][0]["code"] == "int"


# ---- on_error -----------------------------------------------------------------


class PaymentFailedError(Exception):
    pass


class DbDownError(Exception):
    pass


def test_on_error_custom_exception(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/pay")
    def pay() -> str:
        raise PaymentFailedError("card declined")

    @app.on_error(PaymentFailedError)
    def on_payment(exc: PaymentFailedError) -> Response:
        return Response({"reason": str(exc)}, status=402)

    res = fetch(f"{serve(app)}/pay")
    assert res.status == 402
    assert json.loads(res.body) == {"reason": "card declined"}


def test_on_error_mro_closest_wins(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    class MidError(PaymentFailedError):
        pass

    class LeafError(MidError):
        pass

    @app.get("/x")
    def x() -> str:
        raise LeafError("deep")

    @app.on_error(PaymentFailedError)
    def on_parent(exc: PaymentFailedError) -> str:
        return f"parent:{exc}"

    @app.on_error(MidError)
    def on_mid(exc: MidError) -> str:
        return f"mid:{exc}"

    res = fetch(f"{serve(app)}/x")
    assert res.body == b"mid:deep"


def test_on_error_overrides_problem_rendering(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/nf")
    def nf() -> str:
        raise NotFound("custom override")

    @app.on_error(NotFound)
    def on_nf(exc: NotFound) -> Response:
        return Response("handled differently", status=404, content_type="text/plain")

    res = fetch(f"{serve(app)}/nf")
    assert res.status == 404
    assert res.header("Content-Type") == "text/plain"


def test_error_handler_failure_is_500(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/db")
    def db() -> str:
        raise DbDownError("down")

    @app.on_error(DbDownError)
    def on_db(exc: DbDownError) -> str:
        raise RuntimeError("error handler exploded") from exc

    res = fetch(f"{serve(app)}/db")
    assert res.status == 500


def test_on_error_async_handler(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/a")
    async def a() -> str:
        raise DbDownError("async down")

    @app.on_error(DbDownError)
    async def on_db(exc: DbDownError) -> dict:
        return {"async": str(exc)}

    res = fetch(f"{serve(app)}/a")
    assert res.status == 200
    assert json.loads(res.body) == {"async": "async down"}


def test_unregistered_exception_is_500_and_leaks_nothing(
    serve: Callable[[Fulmen], str], caplog: pytest.LogCaptureFixture
) -> None:
    app = Fulmen()

    @app.get("/oops")
    def oops() -> str:
        raise KeyError("sekrit-xyzzy")

    with caplog.at_level(logging.ERROR, logger="fulmen"):
        res = fetch(f"{serve(app)}/oops")
    assert res.status == 500
    assert b"sekrit-xyzzy" not in res.body
    assert "KeyError" in caplog.text


# ---- the 4xx/5xx split ----------------------------------------------------------


def test_problem_shape_consistency(serve: Callable[[Fulmen], str]) -> None:
    # Every native error path emits RFC 9457 `application/problem+json`
    # with the same {type, title, status} envelope (§17).
    app = Fulmen()

    @app.get("/v/{i}")
    def v(i: Annotated[int, Range(ge=10)]) -> str:
        return "ok"

    @app.post("/u")
    def u(user: User) -> dict:
        return {"id": user.id}

    base = serve(app)
    for res in (
        fetch(f"{base}/v/1"),  # path str-routed validation → 422
        fetch(f"{base}/u", "POST", b"{", headers=JSON_CT),  # malformed → 400
        fetch(f"{base}/u", "POST", b"{}", headers=JSON_CT),  # missing field → 422
        fetch(f"{base}/nowhere"),  # no route → 404
    ):
        assert res.header("Content-Type") == "application/problem+json"
        body = json.loads(res.body)
        assert {"type", "title", "status"} <= body.keys()
        assert body["status"] == res.status
    assert json.loads(fetch(f"{base}/v/1").body)["type"] == (
        "https://fulmen.dev/problems/validation"
    )
