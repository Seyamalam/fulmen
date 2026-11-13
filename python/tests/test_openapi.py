"""M2.11 OpenAPI 3.1 generation + self-hosted docs (DESIGN.md §11.3, §23.2).

`Fulmen.openapi()` renders the declared routes' frozen spec trees into
an OpenAPI 3.1 / JSON Schema 2020-12 document; `_compile_native` mounts
it at `GET /openapi.json` next to a vendored Swagger UI under `/docs`.
"""

from __future__ import annotations

import datetime
import enum
import gzip
import json
import uuid
from collections.abc import Callable, Iterator
from decimal import Decimal
from enum import IntEnum, StrEnum
from pathlib import Path
from typing import Annotated, Any, Literal

import pytest
from conftest import fetch
from fulmen import (
    Body,
    Cookie,
    Created,
    Default,
    Deprecated,
    Description,
    Discriminator,
    Email,
    Example,
    Field,
    Form,
    Fulmen,
    Header,
    Len,
    Model,
    MultipleOf,
    NoContent,
    Pattern,
    Query,
    Range,
    Raw,
    Response,
    StartupError,
    UploadFile,
)
from fulmen import openapi as _openapi
from fulmen._typing import SpecBuilder


class Size(StrEnum):
    """A str-flavored enum used as a query parameter and a record field."""

    SMALL = "s"
    LARGE = "l"


class Level(IntEnum):
    LOW = 1
    HIGH = 2


class Address(Model):
    street: str
    zip_code: Annotated[str, Field(description="Postal code", example="90210")]


class BodyPart(Model):
    n: int


class WeirdResponse(Response):
    status_code = 799


class MixedEnum(enum.Enum):
    NONE = None
    FLAG = True
    RATE = 1.5
    TAG = "x"


class MixedHolder(Model):
    m: MixedEnum


class KebabModel(Model, rename="kebab"):
    field_one: int
    field_two: str


class BlobModel(Model):
    payload: bytes = b"k"


class PageModel[T](Model):
    items: list[T]


class TagStrA(Model):
    kind: str
    a: int


class TagStrB(Model):
    kind: str


class TagLitA(Model):
    kind: Literal["a"] = "a"


class TagNoB(Model):
    other: str


class OptDecModel(Model):
    dec: Decimal | None
    anything: Any | None


class MetaDefaultModel(Model):
    x: Annotated[int, Default(5)]
    xs: list[Annotated[int, Default(7)]] = Field(default_factory=list)


class EmptyModel(Model):
    pass


DupOne = type("Dup", (Model,), {"__annotations__": {"a": int}})
DupTwo = type("Dup", (Model,), {"__annotations__": {"b": str}})


class User(Model):
    id: int
    name: Annotated[str, Len(min=1, max=50), Description("Display name"), Example("Ada")]
    email: Email
    address: Address
    tags: list[str] = Field(default_factory=list)
    legacy: Annotated[str, Deprecated()] = ""


class Cat(Model):
    kind: Literal["cat"] = "cat"
    lives: int = 9


class Dog(Model):
    kind: Literal["dog"] = "dog"
    barks: bool = True


Pet = Annotated[Cat | Dog, Discriminator("kind")]


def build_rich_app() -> Fulmen:
    app = Fulmen(
        title="Pet Zoo",
        version="2.1.0",
        description="A small zoo API.",
        contact={"name": "Zookeepers", "email": "hi@zoo.dev"},
        license={"name": "MIT"},
        servers=["https://api.zoo.dev", {"url": "https://staging.zoo.dev", "description": "stg"}],
        tags=[{"name": "pets", "description": "pet ops"}, "users"],
    )

    @app.get("/users/{uid}", tags=["users"], summary="Fetch one user")
    def get_user(
        uid: int,
        verbose: Annotated[bool, Query(description="Chatty output")] = False,
        api_token: Annotated[str | None, Header(alias="x-api-token", deprecated=True)] = None,
        session: Annotated[str | None, Cookie()] = None,
        size: Annotated[Size, Query(example="s")] = Size.SMALL,
        level: Level = Level.LOW,
    ) -> User | None:
        """Get a user, or 404-shaped `None`."""
        return None

    @app.post("/users", operation_id="createUser")
    def create_user(user: User) -> Created[User]:
        return Created(
            User(id=1, name="a", email="a@b.c", address=Address(street="s", zip_code="z"))
        )

    @app.post("/pets", tags=["pets"])
    def make_pet(pet: Pet) -> Pet:
        return Cat()

    @app.get("/pets/{pet_id}", tags=["pets"], deprecated=True)
    def get_pet(pet_id: int) -> Pet:
        return Cat()

    @app.post("/login")
    def login(
        username: Annotated[str, Form()],
        password: Annotated[str, Form(min_length=8)],
    ) -> dict[str, str]:
        return {}

    @app.post("/files/{*rest}")
    def upload(rest: str, data: Annotated[bytes, Body()]) -> NoContent:
        return NoContent()

    @app.get("/items/{item_id}")
    def read_item(
        item_id: str,
        qty: Annotated[int, Query(), Range(gt=0, lt=10), MultipleOf(2)] = 2,
        code: Annotated[str, Query(), Pattern(r"^[a-z]+$"), Len(max=12)] = "abc",
        *,
        scores: Annotated[list[int], Query(), Len(min=1, max=3), Field(default_factory=list)],
    ) -> Cat | int | Response:
        """Either a cat, a count, or a dynamic response."""
        return 1

    @app.get("/hidden", include_in_schema=False)
    def hidden() -> str:
        return "shh"

    @app.delete("/users/{uid}")
    def delete_user(uid: int) -> None:
        return None

    @app.get("/doc")
    def docstring_route() -> str:
        """Docstring summary line.

        More detail follows."""
        return "x"

    return app


def test_openapi_document() -> None:
    doc = build_rich_app().openapi()

    assert doc["openapi"] == "3.1.0"
    assert doc["info"] == {
        "title": "Pet Zoo",
        "version": "2.1.0",
        "description": "A small zoo API.",
        "contact": {"name": "Zookeepers", "email": "hi@zoo.dev"},
        "license": {"name": "MIT"},
    }
    assert doc["servers"] == [
        {"url": "https://api.zoo.dev"},
        {"url": "https://staging.zoo.dev", "description": "stg"},
    ]
    assert doc["tags"] == [{"name": "pets", "description": "pet ops"}, {"name": "users"}]

    assert set(doc["paths"]) == {
        "/users/{uid}",
        "/users",
        "/pets",
        "/pets/{pet_id}",
        "/login",
        "/files/{rest}",
        "/items/{item_id}",
        "/doc",
    }
    assert "/hidden" not in doc["paths"]

    get_user = doc["paths"]["/users/{uid}"]["get"]
    assert get_user["summary"] == "Fetch one user"
    assert get_user["description"] == "Get a user, or 404-shaped `None`."
    assert get_user["operationId"] == "get_user"
    assert get_user["tags"] == ["users"]
    assert get_user["parameters"] == [
        {"name": "uid", "in": "path", "required": True, "schema": {"type": "integer"}},
        {
            "name": "verbose",
            "in": "query",
            "required": False,
            "schema": {"type": "boolean", "default": False},
            "description": "Chatty output",
        },
        {
            "name": "x-api-token",
            "in": "header",
            "required": False,
            "schema": {"type": ["string", "null"], "default": None},
            "deprecated": True,
        },
        {
            "name": "session",
            "in": "cookie",
            "required": False,
            "schema": {"type": ["string", "null"], "default": None},
        },
        {
            "name": "size",
            "in": "query",
            "required": False,
            "schema": {"$ref": "#/components/schemas/Size", "default": "s"},
            "example": "s",
        },
        {
            "name": "level",
            "in": "query",
            "required": False,
            "schema": {"$ref": "#/components/schemas/Level", "default": 1},
        },
    ]
    assert get_user["responses"] == {
        "200": {
            "description": "OK",
            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}},
        },
        "204": {"description": "No Content"},
    }

    create = doc["paths"]["/users"]["post"]
    assert create["operationId"] == "createUser"
    assert create["requestBody"] == {
        "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}},
        "required": True,
    }
    assert create["responses"]["201"]["content"]["application/json"]["schema"] == {
        "$ref": "#/components/schemas/User"
    }

    pet_op = doc["paths"]["/pets"]["post"]
    assert pet_op["tags"] == ["pets"]
    pet_union = {
        "oneOf": [
            {"$ref": "#/components/schemas/Cat"},
            {"$ref": "#/components/schemas/Dog"},
        ],
        "discriminator": {
            "propertyName": "kind",
            "mapping": {"cat": "#/components/schemas/Cat", "dog": "#/components/schemas/Dog"},
        },
    }
    assert pet_op["requestBody"]["content"]["application/json"]["schema"] == pet_union
    assert pet_op["responses"]["200"]["content"]["application/json"]["schema"] == pet_union
    assert doc["paths"]["/pets/{pet_id}"]["get"]["deprecated"] is True

    login = doc["paths"]["/login"]["post"]
    assert login["requestBody"] == {
        "content": {
            "application/x-www-form-urlencoded": {
                "schema": {
                    "type": "object",
                    "properties": {
                        "username": {"type": "string"},
                        "password": {"type": "string", "minLength": 8},
                    },
                    "required": ["username", "password"],
                }
            }
        },
        "required": True,
    }

    upload = doc["paths"]["/files/{rest}"]["post"]
    assert upload["parameters"] == [
        {"name": "rest", "in": "path", "required": True, "schema": {"type": "string"}}
    ]
    assert upload["requestBody"]["content"] == {
        "application/octet-stream": {"schema": {"type": "string", "format": "byte"}}
    }
    assert upload["responses"] == {"204": {"description": "No Content"}}

    item = doc["paths"]["/items/{item_id}"]["get"]
    assert item["parameters"][1:] == [
        {
            "name": "qty",
            "in": "query",
            "required": False,
            "schema": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "exclusiveMaximum": 10,
                "multipleOf": 2,
                "default": 2,
            },
        },
        {
            "name": "code",
            "in": "query",
            "required": False,
            "schema": {
                "type": "string",
                "maxLength": 12,
                "pattern": "^[a-z]+$",
                "default": "abc",
            },
        },
        {
            "name": "scores",
            "in": "query",
            "required": False,
            "schema": {
                "type": "array",
                "items": {"type": "integer"},
                "minItems": 1,
                "maxItems": 3,
            },
        },
    ]
    assert item["responses"] == {
        "200": {
            "description": "OK",
            "content": {
                "application/json": {
                    "schema": {"anyOf": [{"$ref": "#/components/schemas/Cat"}, {"type": "integer"}]}
                }
            },
        },
        "default": {"description": "Response"},
    }

    delete = doc["paths"]["/users/{uid}"]["delete"]
    assert delete["responses"] == {"204": {"description": "No Content"}}

    doc_route = doc["paths"]["/doc"]["get"]
    assert doc_route["description"] == "Docstring summary line.\n\nMore detail follows."

    schemas = doc["components"]["schemas"]
    assert schemas["Size"] == {"enum": ["s", "l"], "type": "string"}
    assert schemas["Level"] == {"enum": [1, 2], "type": "integer"}
    assert schemas["Address"] == {
        "type": "object",
        "properties": {
            "street": {"type": "string"},
            "zip_code": {"type": "string", "description": "Postal code", "examples": ["90210"]},
        },
        "required": ["street", "zip_code"],
    }
    assert schemas["User"] == {
        "type": "object",
        "properties": {
            "id": {"type": "integer"},
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 50,
                "description": "Display name",
                "examples": ["Ada"],
            },
            "email": {"type": "string", "format": "email"},
            "address": {"$ref": "#/components/schemas/Address"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "legacy": {"type": "string", "deprecated": True, "default": ""},
        },
        "required": ["id", "name", "email", "address"],
    }
    assert schemas["Cat"] == {
        "type": "object",
        "properties": {
            "kind": {"const": "cat", "default": "cat"},
            "lives": {"type": "integer", "default": 9},
        },
    }
    assert schemas["Dog"] == {
        "type": "object",
        "properties": {
            "kind": {"const": "dog", "default": "dog"},
            "barks": {"type": "boolean", "default": True},
        },
    }


def test_deterministic_render() -> None:
    app = build_rich_app()
    assert _openapi.render(app.openapi()) == _openapi.render(app.openapi())
    # Path keys sorted, method keys in HTTP order, components sorted.
    doc = app.openapi()
    assert list(doc["paths"]) == sorted(doc["paths"])
    assert list(doc["paths"]["/users/{uid}"]) == ["get", "delete"]
    assert list(doc["components"]["schemas"]) == sorted(doc["components"]["schemas"])


def _walk(node: Any) -> Any:
    if isinstance(node, dict):
        yield node
        for value in node.values():
            yield from _walk(value)
    elif isinstance(node, list):
        for value in node:
            yield from _walk(value)


def test_openapi_31_dialect() -> None:
    doc = build_rich_app().openapi()
    for node in _walk(doc):
        assert "nullable" not in node
        for key in ("exclusiveMinimum", "exclusiveMaximum"):
            if key in node:
                assert isinstance(node[key], int | float)


class Shapes(Model):
    """Container + literal field shapes inside a request/response model."""

    data: dict[str, int]
    pair: tuple[int, str]
    uniq: set[int]
    role: Literal["admin", "member"]
    one: Literal["only"]
    free: Any
    maybe: int | None
    price: Decimal


def test_scalar_schema_shapes() -> None:
    app = Fulmen()

    @app.get("/s")
    def scalars(
        when: Annotated[datetime.datetime, Query()],
        day: Annotated[datetime.date, Query()],
        uid: Annotated[uuid.UUID, Query()],
        blob: Annotated[bytes, Query()],
        free: Any = None,
    ) -> str:
        return "x"

    @app.post("/shapes")
    def shapes(body: Shapes) -> Shapes:
        return body

    doc = app.openapi()
    params = doc["paths"]["/s"]["get"]["parameters"]
    schemas = {p["name"]: p["schema"] for p in params}
    assert schemas["when"] == {"type": "string", "format": "date-time"}
    assert schemas["day"] == {"type": "string", "format": "date"}
    assert schemas["uid"] == {"type": "string", "format": "uuid"}
    assert schemas["blob"] == {"type": "string", "format": "byte"}
    assert schemas["free"] == {"default": None}

    props = doc["components"]["schemas"]["Shapes"]["properties"]
    assert props["data"] == {"type": "object", "additionalProperties": {"type": "integer"}}
    assert props["pair"] == {
        "type": "array",
        "prefixItems": [{"type": "integer"}, {"type": "string"}],
        "items": False,
        "minItems": 2,
        "maxItems": 2,
    }
    assert props["uniq"] == {"type": "array", "items": {"type": "integer"}, "uniqueItems": True}
    assert props["role"] == {"enum": ["admin", "member"]}
    assert props["one"] == {"const": "only"}
    assert props["free"] == {}
    assert props["maybe"] == {"type": ["integer", "null"]}
    assert props["price"] == {"type": ["number", "string"], "format": "decimal"}


def test_served_document_and_docs(serve: Callable[..., str]) -> None:
    app = build_rich_app()
    base = serve(app)

    res = fetch(f"{base}/openapi.json")
    assert res.status == 200
    assert res.header("Content-Type") == "application/json"
    assert json.loads(res.body) == app.openapi()
    # The convenience routes don't document themselves.
    assert "/openapi.json" not in json.loads(res.body)["paths"]

    res = fetch(f"{base}/docs")
    assert res.status == 200
    assert res.header("Content-Type") == "text/html; charset=utf-8"
    page = res.body.decode()
    assert 'url: "/openapi.json"' in page
    assert "swagger-ui-bundle.js" in page
    assert "Pet Zoo" in page

    css = fetch(f"{base}/docs/swagger-ui.css")
    assert css.status == 200
    assert css.header("Content-Type") == "text/css; charset=utf-8"
    assert len(css.body) > 100_000  # served decompressed

    js = fetch(f"{base}/docs/swagger-ui-bundle.js")
    assert js.status == 200
    assert js.header("Content-Type") == "text/javascript; charset=utf-8"
    assert b"SwaggerUIBundle" in js.body


def test_served_document_structure(serve: Callable[..., str]) -> None:
    """The served document is a structurally valid OpenAPI 3.1 object.

    The M2 exit criterion asks for a served-document check, not just
    `app.openapi()`: every path/method declares responses, every `in`
    is legal, `required` marks only path params, every local `$ref`
    resolves inside the document, and component schemas are objects.
    """
    app = build_rich_app()
    base = serve(app)
    res = fetch(f"{base}/openapi.json")
    assert res.status == 200
    doc = json.loads(res.body)

    assert doc["openapi"] == "3.1.0"
    assert {"title", "version"} <= set(doc["info"])
    components = doc.get("components", {}).get("schemas", {})

    def refs(node: Any) -> Iterator[str]:
        if isinstance(node, dict):
            for key, value in node.items():
                if key == "$ref":
                    yield value
                else:
                    yield from refs(value)
        elif isinstance(node, list):
            for item in node:
                yield from refs(item)

    for path, item in doc["paths"].items():
        assert path.startswith("/")
        for method, op in item.items():
            assert method in {"get", "post", "put", "patch", "delete", "head", "options"}
            assert op["responses"], f"{method} {path} has no responses"
            for status, resp in op["responses"].items():
                assert status == "default" or status.isdigit()
                assert "description" in resp or "content" in resp
            for param in op.get("parameters", []):
                assert param["in"] in {"path", "query", "header", "cookie"}
                assert "name" in param
                assert "schema" in param
                if param["in"] == "path":
                    assert param["required"] is True
            body = op.get("requestBody")
            if body is not None:
                assert "content" in body
    for name, schema in components.items():
        assert name, f"empty component name {schema}"
        assert "type" in schema, f"component {name} malformed"
        if schema.get("type") == "object":
            required = schema.get("required", [])
            assert set(required) <= set(schema.get("properties", {}))
    for ref in refs(doc):
        assert ref.startswith("#/components/schemas/")
        name = ref.removeprefix("#/components/schemas/")
        assert name in components, f"dangling $ref {ref}"


def test_disabled_openapi(serve: Callable[..., str]) -> None:
    app = Fulmen(openapi_url=None)

    @app.get("/x")
    def x() -> int:
        return 1

    base = serve(app)
    for path in ("/openapi.json", "/docs", "/docs/swagger-ui.css", "/docs/swagger-ui-bundle.js"):
        assert fetch(f"{base}{path}").status == 404
    assert fetch(f"{base}/x").status == 200


def test_docs_only_disabled(serve: Callable[..., str]) -> None:
    app = Fulmen(docs_url=None)

    @app.get("/x")
    def x() -> int:
        return 1

    base = serve(app)
    assert fetch(f"{base}/openapi.json").status == 200
    assert fetch(f"{base}/docs").status == 404
    assert fetch(f"{base}/docs/swagger-ui.css").status == 404


def test_custom_urls(serve: Callable[..., str]) -> None:
    app = Fulmen(openapi_url="/spec.json", docs_url="/api-docs")

    @app.get("/x")
    def x() -> int:
        return 1

    base = serve(app)
    assert fetch(f"{base}/spec.json").status == 200
    assert fetch(f"{base}/openapi.json").status == 404
    page = fetch(f"{base}/api-docs")
    assert page.status == 200
    assert b'url: "/spec.json"' in page.body
    assert fetch(f"{base}/api-docs/swagger-ui-bundle.js").status == 200


def test_declared_route_wins(serve: Callable[..., str]) -> None:
    app = Fulmen()

    @app.get("/openapi.json")
    def mine() -> dict[str, bool]:
        return {"mine": True}

    base = serve(app)
    res = fetch(f"{base}/openapi.json")
    assert res.status == 200
    assert json.loads(res.body) == {"mine": True}
    # ...and the user's route documents itself.
    assert "/openapi.json" in app.openapi()["paths"]


def test_constructor_url_validation() -> None:
    with pytest.raises(ValueError, match="openapi_url"):
        Fulmen(openapi_url="openapi.json")
    with pytest.raises(ValueError, match="docs_url"):
        Fulmen(docs_url='"/evil')


def test_openapi_broken_app() -> None:
    app = Fulmen()

    @app.get("/bad/{missing}")
    def bad(other: int) -> int:
        return other

    with pytest.raises(StartupError, match="not bound"):
        app.openapi()


def test_route_option_validation() -> None:
    app = Fulmen()
    with pytest.raises(TypeError, match="unknown route option"):
        app.get("/x", bogus=1)(lambda: 1)
    with pytest.raises(TypeError, match="tags"):
        app.get("/x", tags="not-a-list")(lambda: 1)


def test_vendored_assets_exist_and_decompress() -> None:
    static = Path(_openapi.__file__).with_name("static")
    for name in ("swagger-ui.css.gz", "swagger-ui-bundle.js.gz"):
        data = gzip.decompress((static / name).read_bytes())
        assert len(data) > 10_000


class Page[T](Model):
    items: list[T]
    total: int


class CamelCase(Model, rename="camel"):
    long_name: str
    wire_key: int


def test_edge_cases() -> None:
    app = Fulmen()

    @app.get("/pages")
    def pages() -> Page[User]:
        return Page(items=[], total=0)

    @app.get("/camel")
    def camel() -> CamelCase:
        return CamelCase(long_name="x", wire_key=1)

    @app.get("/maybe")
    def maybe() -> Cat | Dog | None:
        return None

    @app.get("/limits")
    def limits(
        price: Annotated[Decimal, Query(), Range(ge=Decimal("1.5"))],
        exact: Annotated[Decimal, Query(), MultipleOf(Decimal("0.5"))] = Decimal("0.5"),
        raw: Annotated[bytes, Query()] = b"\x00\xff",
        which: Annotated[int | str | None, Query()] = None,
    ) -> str:
        return "x"

    @app.get("/bare")
    def bare():
        return "x"

    def dup() -> int:
        return 1

    app.get("/a")(dup)
    app.get("/b")(dup)

    doc = app.openapi()
    schemas = doc["components"]["schemas"]

    page = schemas["Page_User"]
    assert page["type"] == "object"
    assert page["properties"]["items"] == {
        "type": "array",
        "items": {"$ref": "#/components/schemas/User"},
    }
    assert schemas["User"]  # nested ref materialized

    assert schemas["CamelCase"]["properties"] == {
        "longName": {"type": "string"},
        "wireKey": {"type": "integer"},
    }

    # `Cat | Dog | None` → `200` (discriminated pair) + `204`.
    maybe_responses = doc["paths"]["/maybe"]["get"]["responses"]
    assert maybe_responses["200"]["content"]["application/json"]["schema"] == {
        "oneOf": [
            {"$ref": "#/components/schemas/Cat"},
            {"$ref": "#/components/schemas/Dog"},
        ],
        "discriminator": {
            "propertyName": "kind",
            "mapping": {"cat": "#/components/schemas/Cat", "dog": "#/components/schemas/Dog"},
        },
    }
    assert maybe_responses["204"] == {"description": "No Content"}

    params = {p["name"]: p["schema"] for p in doc["paths"]["/limits"]["get"]["parameters"]}
    assert params["price"] == {
        "type": ["number", "string"],
        "format": "decimal",
        "minimum": 1.5,
    }
    # `Decimal` isn't a spec scalar → the default rides a factory, no `default` key.
    assert params["exact"] == {
        "type": ["number", "string"],
        "format": "decimal",
        "multipleOf": 0.5,
    }
    assert params["raw"] == {"type": "string", "format": "byte", "default": "AP8="}
    assert params["which"] == {
        "anyOf": [{"type": "integer"}, {"type": "string"}, {"type": "null"}],
        "default": None,
    }

    # No return annotation → no declared responses → a bare 200.
    assert doc["paths"]["/bare"]["get"]["responses"] == {
        "200": {"description": "Successful Response"}
    }

    # Same handler name under two routes → unique operationIds.
    assert doc["paths"]["/a"]["get"]["operationId"] != doc["paths"]["/b"]["get"]["operationId"]


def test_route_option_type_errors() -> None:
    app = Fulmen()
    with pytest.raises(TypeError, match="summary"):
        app.get("/x", summary=5)(lambda: 1)
    with pytest.raises(TypeError, match="description"):
        app.get("/x", description=5)(lambda: 1)
    with pytest.raises(TypeError, match="operation_id"):
        app.get("/x", operation_id=5)(lambda: 1)
    with pytest.raises(TypeError, match="tags"):
        app.get("/x", tags=5)(lambda: 1)
    with pytest.raises(TypeError, match="tags"):
        app.get("/x", tags=[1, 2])(lambda: 1)


# -- generation edges --------------------------------------------------------------


def test_operation_id_dedup_counts_past_two() -> None:
    """A third same-method collision on one base id takes the
    `..._2` while-loop arm."""
    app = Fulmen(openapi_url=None)

    @app.get("/a", operation_id="dup")
    def a() -> str:
        return "a"

    @app.get("/b", operation_id="dup")
    def b() -> str:
        return "b"

    @app.get("/c", operation_id="dup")
    def c() -> str:
        return "c"

    doc = app.openapi()
    ids = [doc["paths"][path]["get"]["operationId"] for path in ("/a", "/b", "/c")]
    assert ids == ["dup", "dup_get", "dup_get_2"]


def test_uploadfile_param_is_multipart() -> None:
    app = Fulmen(openapi_url=None)

    @app.post("/up")
    def up(f: UploadFile) -> str:
        return "ok"

    doc = app.openapi()
    content = doc["paths"]["/up"]["post"]["requestBody"]["content"]
    assert "multipart/form-data" in content


def test_multiple_body_params_form_an_object() -> None:
    """Two `Body` params merge into one object schema keyed by wire name;
    a literal default and a required member exercise `_object_schema`."""
    app = Fulmen(openapi_url=None)

    @app.post("/pair")
    def pair(
        first: Annotated[BodyPart, Body()],
        second: Annotated[int, Body()] = 5,
    ) -> str:
        return "ok"

    doc = app.openapi()
    body = doc["paths"]["/pair"]["post"]["requestBody"]
    schema = body["content"]["application/json"]["schema"]
    assert schema["type"] == "object"
    assert set(schema["properties"]) == {"first", "second"}
    assert schema["properties"]["second"]["default"] == 5
    assert schema["required"] == ["first"]
    assert body["required"] is True


def test_form_defaults_land_in_schema() -> None:
    """A defaulted `Form` member documents `default` in the object schema."""
    app = Fulmen(openapi_url=None)

    @app.post("/f")
    def f(name: Annotated[str, Form()], count: Annotated[int, Form()] = 3) -> str:
        return "ok"

    doc = app.openapi()
    schema = doc["paths"]["/f"]["post"]["requestBody"]["content"][
        "application/x-www-form-urlencoded"
    ]["schema"]
    assert schema["properties"]["count"]["default"] == 3
    assert schema["required"] == ["name"]


def test_unusual_response_status_has_generic_phrase() -> None:
    """A status outside `HTTPStatus` still renders — summary falls back."""
    app = Fulmen(openapi_url=None)

    @app.get("/w")
    def w() -> WeirdResponse:
        return WeirdResponse()

    doc = app.openapi()
    responses = doc["paths"]["/w"]["get"]["responses"]
    assert "799" in responses


def test_plain_enum_member_types() -> None:
    """Enum members of different JSON types feed `_jtype`'s branches."""
    app = Fulmen(openapi_url=None)

    @app.get("/h")
    def h() -> MixedHolder:
        return MixedHolder(m=MixedEnum.TAG)

    doc = app.openapi()
    schema = doc["components"]["schemas"]["MixedHolder"]["properties"]["m"]
    # enum members collapse into the component or inline — either way it renders
    assert "enum" in schema or "$ref" in schema


def test_kebab_renamed_fields_wire_names() -> None:
    app = Fulmen(openapi_url=None)

    @app.get("/k")
    def k() -> KebabModel:
        return KebabModel(field_one=1, field_two="x")

    doc = app.openapi()
    schema = doc["components"]["schemas"]["KebabModel"]
    assert set(schema["properties"]) == {"field-one", "field-two"}


def test_bytes_field_default_is_canonical() -> None:
    """A `bytes` literal default must survive the `ref_name` dedupe key."""
    app = Fulmen(openapi_url=None)

    @app.get("/b")
    def b() -> BlobModel:
        return BlobModel()

    doc = app.openapi()
    schema = doc["components"]["schemas"]["BlobModel"]["properties"]["payload"]
    assert schema["type"] == "string"


def test_generic_model_component_label() -> None:
    """`Page[int]` specializes the generic record — component name carries
    the argument label."""
    app = Fulmen(openapi_url=None)

    @app.get("/p")
    def p() -> PageModel[int]:
        return PageModel(items=[1])

    @app.get("/pp")
    def pp() -> PageModel[PageModel[int]]:
        return PageModel(items=[PageModel(items=[2])])

    doc = app.openapi()
    names = list(doc["components"]["schemas"])
    assert any("Page" in n for n in names)


def test_same_named_models_dedupe_component_names() -> None:
    """Two record nodes mapping to one component name get `Name_2`.
    (`type(...)` gives both classes the same qualname — a `Model`
    subclass' spec captures `__qualname__` at class creation.)"""
    app = Fulmen(openapi_url=None)

    @app.get("/f")
    def f() -> DupOne:
        return DupOne(a=1)

    @app.get("/s")
    def s() -> DupTwo:
        return DupTwo(b="x")

    doc = app.openapi()
    names = sorted(doc["components"]["schemas"])
    assert names == ["Dup", "Dup_2"]


def test_none_return_and_none_kind() -> None:
    """`-> None` produces a `none` spec node → `{"type": "null"}` schema."""
    app = Fulmen(openapi_url=None)

    @app.get("/n")
    def n() -> None:
        return None

    doc = app.openapi()
    assert doc["paths"]["/n"]["get"]["responses"] == {"204": {"description": "No Content"}}


def test_raw_body_is_binary() -> None:
    app = Fulmen(openapi_url=None)

    @app.post("/raw")
    def raw(body: Raw) -> str:
        return "ok"

    doc = app.openapi()
    content = doc["paths"]["/raw"]["post"]["requestBody"]["content"]
    schema = next(iter(content.values()))["schema"]
    assert schema.get("format") == "binary"


def test_tagged_union_with_nonliteral_tag() -> None:
    """A tag field that's a plain `str` yields the discriminator property
    without a value mapping."""

    app = Fulmen(openapi_url=None)

    @app.get("/u")
    def u() -> Annotated[TagStrA | TagStrB, Discriminator("kind")]:
        return TagStrA(kind="a", a=1)

    doc = app.openapi()
    resp = doc["paths"]["/u"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
    assert "oneOf" in resp or "anyOf" in resp


def test_tagged_union_skips_non_record_and_missing_tag() -> None:
    """`Discriminator` on a union with a non-record variant or a record
    missing the tag field ignores those variants for the mapping."""

    app = Fulmen(openapi_url=None)

    @app.get("/u1")
    def u1() -> Annotated[TagLitA | int, Discriminator("kind")]:
        return 1

    @app.get("/u2")
    def u2() -> Annotated[TagLitA | TagNoB, Discriminator("kind")]:
        return TagLitA()

    doc = app.openapi()
    assert "/u1" in doc["paths"]
    assert "/u2" in doc["paths"]


def test_optional_decimal_and_any_nullable_shapes() -> None:
    """`Optional[Decimal]` widens a type-list schema; `Optional[Any]`
    falls back to `anyOf`-with-null."""

    app = Fulmen(openapi_url=None)

    @app.get("/m")
    def m() -> OptDecModel:
        return OptDecModel(dec=None, anything=None)

    doc = app.openapi()
    props = doc["components"]["schemas"]["OptDecModel"]["properties"]
    dec = props["dec"]
    assert "anyOf" in dec or "null" in json.dumps(dec)


def test_schema_gen_direct_edges() -> None:
    """`schema(None)` and the unknown-kind fallback are cheap direct checks."""
    gen = _openapi._SchemaGen()
    assert gen.schema(None) == {}
    assert gen.schema({"kind": "totally-unknown"}) == {}


def test_meta_default_renders_in_schema() -> None:
    """`Annotated[int, Default(5)]` mid-tree lands `default` on the
    item schema; a field-level `Default` sets `missing` instead."""
    app = Fulmen(openapi_url=None)

    @app.get("/m")
    def m() -> MetaDefaultModel:
        return MetaDefaultModel()

    doc = app.openapi()
    props = doc["components"]["schemas"]["MetaDefaultModel"]["properties"]
    assert props["x"]["default"] == 5
    assert props["xs"]["items"]["default"] == 7


def test_empty_model_has_no_properties_key() -> None:
    app = Fulmen(openapi_url=None)

    @app.get("/e")
    def e() -> EmptyModel:
        return EmptyModel()

    doc = app.openapi()
    schema = doc["components"]["schemas"]["EmptyModel"]
    assert schema == {"type": "object"} or "properties" not in schema


def test_jsonable_fallback_str() -> None:
    """Non-scalar spec literals stringify (bytes base64 elsewhere)."""
    assert _openapi._jsonable(Decimal("1.5")) == "1.5"
    assert _openapi._jsonable(b"\x01") == "AQ=="


def test_operation_id_fallback_to_handler_name() -> None:
    app = Fulmen(openapi_url=None)

    @app.get("/x")
    def some_handler() -> str:
        return "x"

    doc = app.openapi()
    assert doc["paths"]["/x"]["get"]["operationId"] == "some_handler"


def test_sig_defaults_returns_empty_for_uninspectable() -> None:
    """`inspect.signature` failures yield `{}` — defaults just don't
    render for handlers built on exotic callables."""
    assert _openapi._sig_defaults(iter) == {}
    assert _openapi._sig_defaults(42) == {}


def test_any_response_renders_empty_schema() -> None:
    """`-> Any` produces the `{}` schema — the `any`/`typevar` case."""
    app = Fulmen(openapi_url=None)

    @app.get("/a")
    def a() -> Any:
        return 1

    doc = app.openapi()
    schema = doc["paths"]["/a"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
    assert schema == {}


def test_identical_variant_schemas_dedupe() -> None:
    """`list[int] | tuple[int, ...]` produces two identical variant
    schemas — `_merged_content` collapses them into one."""
    app = Fulmen(openapi_url=None)

    @app.get("/u")
    def u() -> list[int] | tuple[int, ...]:
        return [1]

    doc = app.openapi()
    schema = doc["paths"]["/u"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
    assert schema == {"type": "array", "items": {"type": "integer"}}


class NullFieldModel(Model):
    x: None


class LitCat(Model):
    kind: Literal["cat"]
    lives: int


class LitDog(Model):
    kind: Literal["dog"]
    fetch: bool


def test_none_field_schema_is_null() -> None:
    """A `None` annotation compiles to `{"type": "null"}` in the schema."""
    app = Fulmen(openapi_url=None)

    @app.get("/n")
    def n() -> NullFieldModel:
        return NullFieldModel(x=None)

    doc = app.openapi()
    ref = doc["paths"]["/n"]["get"]["responses"]["200"]["content"]["application/json"]["schema"][
        "$ref"
    ]
    name = ref.rsplit("/", 1)[-1]
    assert doc["components"]["schemas"][name]["properties"]["x"] == {"type": "null"}


def test_generic_dict_arg_substitutes_into_list_items() -> None:
    """`Page[dict[str, int]]` — `_subst_any` recurses into dict-valued
    node subtrees while binding the typevar."""
    app = Fulmen(openapi_url=None)

    @app.get("/g")
    def g() -> PageModel[dict[str, int]]:
        return PageModel(items=[{"a": 1}])

    doc = app.openapi()
    ref = doc["paths"]["/g"]["get"]["responses"]["200"]["content"]["application/json"]["schema"][
        "$ref"
    ]
    name = ref.rsplit("/", 1)[-1]
    items = doc["components"]["schemas"][name]["properties"]["items"]
    assert items["items"] == {
        "type": "object",
        "additionalProperties": {"type": "integer"},
    }


def test_param_literal_and_unbacked_omit_defaults() -> None:
    """A `Default` marker lands in the param schema directly; an
    `Optional` param with no handler default stays omit-without-default."""
    app = Fulmen(openapi_url=None)

    @app.get("/p")
    def p(
        a: Annotated[int, Query(), Default(3)],
        b: Annotated[int | None, Query()],
    ) -> int:
        return a

    doc = app.openapi()
    params = doc["paths"]["/p"]["get"]["parameters"]
    schemas = {param["name"]: param["schema"] for param in params}
    assert schemas["a"]["default"] == 3
    assert "default" not in schemas["b"]


def test_tagged_union_emits_discriminator_mapping() -> None:
    """Variants sharing a single-member `Literal` tag field produce a
    `discriminator` with a per-variant schema mapping."""
    app = Fulmen(openapi_url=None)

    @app.get("/pet")
    def pet() -> LitCat | LitDog:
        return LitCat(kind="cat", lives=9)

    doc = app.openapi()
    schema = doc["paths"]["/pet"]["get"]["responses"]["200"]["content"]["application/json"][
        "schema"
    ]
    assert schema["discriminator"]["propertyName"] == "kind"
    assert set(schema["discriminator"]["mapping"]) == {"cat", "dog"}


class TupPage[T](Model):
    pair: tuple[T, str]


class PlainPet(Model):
    lives: int


class StrKindPet(Model):
    kind: str


class MultiKindPet(Model):
    kind: Literal["a", "b"]


def test_generic_list_values_substitute() -> None:
    """`tuple[T, str]` inside a generic Model — `_subst_any` recurses
    into list-valued node entries too."""
    app = Fulmen(openapi_url=None)

    @app.get("/t")
    def t() -> TupPage[int]:
        return TupPage(pair=(1, "a"))

    doc = app.openapi()
    ref = doc["paths"]["/t"]["get"]["responses"]["200"]["content"]["application/json"]["schema"][
        "$ref"
    ]
    name = ref.rsplit("/", 1)[-1]
    pair = doc["components"]["schemas"][name]["properties"]["pair"]
    assert pair["prefixItems"] == [{"type": "integer"}, {"type": "string"}]


def test_form_object_param_defaults() -> None:
    """Form params merge into one object schema — a `Default` marker
    emits `default`, a signature default echoes it, and an all-optional
    form emits no `required` key."""
    app = Fulmen(openapi_url=None)

    @app.post("/f")
    def f(
        a: Annotated[int, Form(), Default(3)],
        b: Annotated[int | None, Form()] = None,
    ) -> int:
        return a

    doc = app.openapi()
    schema = doc["paths"]["/f"]["post"]["requestBody"]["content"][
        "application/x-www-form-urlencoded"
    ]["schema"]
    assert schema["properties"]["a"]["default"] == 3
    assert schema["properties"]["b"]["default"] is None
    assert "required" not in schema


def test_discriminator_skips_unusable_variants() -> None:
    """Defensive IR handling: native descriptor validation rejects
    malformed tagged unions upstream, but `_SchemaGen` still skips
    non-record variants, variants lacking the tag field, non-literal tag
    fields and multi-member `Literal` tags when building the mapping."""
    gen = _openapi._SchemaGen()
    variants = [
        SpecBuilder().record_spec(LitCat),
        SpecBuilder().record_spec(PlainPet),
        SpecBuilder().record_spec(StrKindPet),
        SpecBuilder().record_spec(MultiKindPet),
        {"kind": "int"},
    ]
    schema = gen.schema({"kind": "union", "tag": "kind", "variants": variants})
    disc = schema["discriminator"]
    assert disc["propertyName"] == "kind"
    assert set(disc["mapping"]) == {"cat"}


def test_discriminator_with_no_usable_variant_has_no_mapping() -> None:
    """A tagged union whose variants carry no single-member literal tag
    emits `propertyName` but no `mapping`."""
    gen = _openapi._SchemaGen()
    variants = [
        SpecBuilder().record_spec(PlainPet),
        SpecBuilder().record_spec(StrKindPet),
    ]
    schema = gen.schema({"kind": "union", "tag": "kind", "variants": variants})
    assert schema["discriminator"] == {"propertyName": "kind"}
