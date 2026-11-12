"""M2.11 OpenAPI 3.1 generation + self-hosted docs (DESIGN.md §11.3, §23.2).

`Fulmen.openapi()` renders the declared routes' frozen spec trees into
an OpenAPI 3.1 / JSON Schema 2020-12 document; `_compile_native` mounts
it at `GET /openapi.json` next to a vendored Swagger UI under `/docs`.
"""

from __future__ import annotations

import datetime
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
    Response,
    StartupError,
)
from fulmen import openapi as _openapi


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
