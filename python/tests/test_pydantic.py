"""M2.17 pydantic adapter tests (DESIGN.md §11.8).

A `pydantic.BaseModel` subclass in a handler signature becomes a native
`record` spec — validation runs in Rust, pydantic's own validator is
never invoked, and the handler receives a fully initialized instance
(`model_dump`, `model_fields_set`, `repr`, extra/private internals all
work). Everything pydantic-specific skips cleanly without the extra.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Callable
from typing import Annotated, Literal

import pytest
from conftest import fetch
from fulmen import Body, Fulmen
from fulmen._typing import AnnotationError, SpecBuilder
from fulmen.adapters.pydantic import PydanticValidator, is_pydantic_model
from fulmen.errors import ValidationError

pydantic = pytest.importorskip("pydantic", reason="pydantic extra not installed")

from pydantic import (  # noqa: E402
    AliasChoices,
    BaseModel,
    BeforeValidator,
    ConfigDict,
    Field,
    RootModel,
    SecretStr,
    field_validator,
    model_validator,
)
from pydantic.functional_validators import AfterValidator as PydAfterValidator  # noqa: E402


def _json(body: dict | list | str | int) -> bytes:
    return json.dumps(body).encode()


JSON_CT = {"content-type": "application/json"}


# ---- module-level models (record classes resolve by module+qualname) ---------


class Geo(BaseModel):
    """A nested model."""

    lat: float
    lon: float


class Order(BaseModel):
    """The ~10-field body record."""

    order_id: Annotated[int, Field(ge=1)]
    customer: Annotated[str, Field(min_length=1, max_length=50)]
    email: str
    age: Annotated[int, Field(ge=0, le=150)] = 18
    price: float = 0.0
    quantity: Annotated[int, Field(gt=0)] = 1
    sku: Annotated[str, Field(pattern="^[A-Z]+")] = "SKU"
    tags: list[str] = Field(default_factory=list)
    geo: Geo | None = None
    express: bool = False


class Aliased(BaseModel):
    model_config = ConfigDict(extra="forbid")

    user_name: str = Field(alias="userName")
    real_age: int = Field(validation_alias="yearsOld")


class WithValidators(BaseModel):
    name: str
    seen: bool = False

    @field_validator("name")
    @classmethod
    def upper(cls, v: str) -> str:
        return v.upper()

    @model_validator(mode="after")
    def mark(self) -> WithValidators:
        object.__setattr__(self, "seen", True)
        return self


class WithAfterMeta(BaseModel):
    x: Annotated[int, PydAfterValidator(lambda v: v * 2)]


class WithBeforeMeta(BaseModel):
    x: Annotated[int, BeforeValidator(lambda v: v + 1)]


class WithExtraAllow(BaseModel):
    model_config = ConfigDict(extra="allow")

    a: int


class WithPrivate(BaseModel):
    a: int
    _priv: int = 3


class IntRoot(RootModel[int]):
    pass


class Page[T](BaseModel):
    items: list[T]


class CatModel(BaseModel):
    kind: Literal["cat"] = "cat"
    lives: int = 9


class DogModel(BaseModel):
    kind: Literal["dog"] = "dog"


class HasPet(BaseModel):
    pet: Annotated[CatModel | DogModel, Field(discriminator="kind")]


class WithUnsupported(BaseModel):
    x: SecretStr


class WithAliasChoices(BaseModel):
    x: int = Field(validation_alias=AliasChoices("x", "y"))


class WithWrapValidator(BaseModel):
    x: int

    @field_validator("x", mode="wrap")
    @classmethod
    def wrap(cls, v: int, handler: object) -> int:
        return v  # pragma: no cover


class WithBeforeModelValidator(BaseModel):
    x: int

    @model_validator(mode="before")
    @classmethod
    def pre(cls, data: object) -> object:
        return data  # pragma: no cover


class NestedCons(BaseModel):
    xs: list[Annotated[int, Field(ge=2)]]


class SelfRef(BaseModel):
    kid: SelfRef | None = None


# ---- detection & graceful missing-extra --------------------------------------


def test_is_pydantic_model() -> None:
    assert is_pydantic_model(Order)
    assert is_pydantic_model(IntRoot)
    assert not is_pydantic_model(dict)
    assert not is_pydantic_model(int)
    assert not is_pydantic_model(None)


def test_missing_pydantic_extra_message(monkeypatch: pytest.MonkeyPatch) -> None:
    # `PydanticValidator()` is the explicit handle: without the package it
    # must fail loudly with the install hint.
    monkeypatch.setitem(sys.modules, "pydantic", None)
    with pytest.raises(ImportError, match="fulmen\\[pydantic\\]"):
        PydanticValidator()


def test_pydantic_validator_protocol() -> None:
    adapter = PydanticValidator()
    assert adapter.supports(Order)
    assert not adapter.supports(dict)
    spec = adapter.spec(Order)
    assert spec["kind"] == "record"
    desc = adapter.compile(Order)
    obj = desc.validate({"order_id": 1, "customer": "ada", "email": "a@b.co"})
    assert isinstance(obj, Order)
    schema = adapter.json_schema(Order)
    assert "components" in schema or "$ref" in schema


# ---- spec translation ---------------------------------------------------------


def test_spec_fields_and_defaults() -> None:
    spec = PydanticValidator.spec(Order)
    by_name = {f["name"]: f for f in spec["fields"]}
    assert spec["extra"] == "ignore"
    assert by_name["order_id"]["type"]["range"] == {"ge": 1}
    assert by_name["customer"]["type"]["len"] == {"min": 1, "max": 50}
    assert by_name["age"]["missing"] == {"literal": 18}
    assert by_name["quantity"]["type"]["range"] == {"gt": 0}
    assert by_name["sku"]["type"]["pattern"] == "^[A-Z]+"
    assert by_name["tags"]["missing"] == {"factory": 0}
    assert by_name["geo"]["type"]["kind"] == "optional"
    assert by_name["geo"]["type"]["inner"]["kind"] == "record"
    assert spec["meta"]["description"] == "The ~10-field body record."


def test_spec_alias() -> None:
    spec = PydanticValidator.spec(Aliased)
    by_name = {f["name"]: f for f in spec["fields"]}
    assert by_name["user_name"]["alias"] == "userName"
    assert by_name["real_age"]["alias"] == "yearsOld"


def test_validate_materializes_real_instance() -> None:
    desc = PydanticValidator().compile(Order)
    obj = desc.validate(
        {
            "order_id": 7,
            "customer": "ada",
            "email": "a@b.co",
            "geo": {"lat": 1.5, "lon": 2.5},
            "express": True,
        }
    )
    assert isinstance(obj, Order)
    assert obj.order_id == 7
    assert obj.age == 18  # default applied
    assert obj.tags == []  # default_factory per-instance
    assert isinstance(obj.geo, Geo)
    assert obj.geo.lat == 1.5
    # pydantic internals are intact — dump/repr/eq/fields_set all work.
    assert obj.model_dump()["geo"] == {"lat": 1.5, "lon": 2.5}
    assert repr(obj).startswith("Order(")
    assert obj.model_fields_set == set(Order.model_fields)
    obj2 = desc.validate({"order_id": 7, "customer": "ada", "email": "a@b.co"})
    obj2.tags.append("x")
    assert obj.tags == []


def test_validate_rejects_bad_input() -> None:
    desc = PydanticValidator().compile(Order)
    with pytest.raises(ValidationError) as exc:
        desc.validate({"order_id": 0, "customer": "", "email": "a@b.co", "price": "x"})
    codes = {e["code"] for e in exc.value.entries}
    assert {"ge", "min_len", "type"} <= codes
    with pytest.raises(ValidationError):
        desc.validate({"customer": "ada", "email": "a@b.co"})  # order_id missing


def test_validate_alias_wire_names() -> None:
    desc = PydanticValidator().compile(Aliased)
    obj = desc.validate({"userName": "ada", "yearsOld": 5})
    assert obj.user_name == "ada"
    assert obj.real_age == 5
    with pytest.raises(ValidationError):
        desc.validate({"user_name": "ada", "yearsOld": 5})  # canonical name rejected
    with pytest.raises(ValidationError):
        desc.validate({"userName": "ada", "yearsOld": 5, "zzz": 1})  # extra=forbid


def test_field_and_model_validators_run() -> None:
    desc = PydanticValidator().compile(WithValidators)
    obj = desc.validate({"name": "ada"})
    assert obj.name == "ADA"  # field_validator(mode="after")
    assert obj.seen is True  # model_validator(mode="after") ran once


def test_after_validator_metadata() -> None:
    desc = PydanticValidator().compile(WithAfterMeta)
    assert desc.validate({"x": 3}).x == 6


def test_before_validator_runs_post_validation() -> None:
    # Deviation: `BeforeValidator` runs after native validation (there is
    # no pre-hook in the spec IR).
    desc = PydanticValidator().compile(WithBeforeMeta)
    assert desc.validate({"x": 3}).x == 4


def test_extra_allow_collects_pydantic_extra() -> None:
    desc = PydanticValidator().compile(WithExtraAllow)
    obj = desc.validate({"a": 1, "b": 2})
    assert obj.a == 1
    assert obj.model_extra == {"b": 2}
    assert obj.b == 2  # extras resolve through __getattr__
    assert obj.model_dump() == {"a": 1, "b": 2}


def test_private_attrs_initialized() -> None:
    desc = PydanticValidator().compile(WithPrivate)
    assert desc.validate({"a": 1})._priv == 3


def test_root_model() -> None:
    desc = PydanticValidator().compile(IntRoot)
    obj = desc.validate(5)
    assert isinstance(obj, IntRoot)
    assert obj.root == 5
    assert obj.model_dump() == 5
    with pytest.raises(ValidationError):
        desc.validate("x")


def test_generic_model() -> None:
    builder = SpecBuilder()
    spec = builder.type_spec(Page[int])
    assert spec["kind"] == "record"
    from fulmen._fulmen import compile_descriptor

    desc = compile_descriptor(spec, builder.callbacks)
    obj = desc.validate({"items": [1, 2]})
    assert isinstance(obj, Page)
    assert obj.items == [1, 2]


def test_discriminated_union() -> None:
    desc = PydanticValidator().compile(HasPet)
    obj = desc.validate({"pet": {"kind": "dog"}})
    assert isinstance(obj.pet, DogModel)
    with pytest.raises(ValidationError):
        desc.validate({"pet": {"kind": "fish"}})


def test_unsupported_types_raise_at_build() -> None:
    adapter = PydanticValidator()
    with pytest.raises(AnnotationError, match="SecretStr"):
        adapter.spec(WithUnsupported)
    with pytest.raises(AnnotationError, match="AliasChoices"):
        adapter.spec(WithAliasChoices)
    with pytest.raises(AnnotationError, match="wrap"):
        adapter.spec(WithWrapValidator)
    with pytest.raises(AnnotationError, match="before"):
        adapter.spec(WithBeforeModelValidator)


def test_nested_annotated_metadata() -> None:
    desc = PydanticValidator().compile(NestedCons)
    assert desc.validate({"xs": [2, 3]}).xs == [2, 3]
    with pytest.raises(ValidationError):
        desc.validate({"xs": [1]})


def test_recursive_model_errors() -> None:
    SelfRef.model_rebuild()
    with pytest.raises(AnnotationError, match="recursive"):
        PydanticValidator.spec(SelfRef)


# ---- routes -------------------------------------------------------------------


def test_route_body_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/orders")
    def create(order: Order) -> dict:
        return {"id": order.order_id, "customer": order.customer, "express": order.express}

    base = serve(app)
    res = fetch(
        f"{base}/orders",
        "POST",
        _json({"order_id": 1, "customer": "ada", "email": "a@b.co", "express": True}),
        headers=JSON_CT,
    )
    assert res.status == 200
    assert json.loads(res.body) == {"id": 1, "customer": "ada", "express": True}


def test_route_body_param_is_typed_instance(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/orders2")
    def create(order: Order) -> dict:
        assert isinstance(order, Order)  # a real pydantic instance in the handler
        return {"dump": order.model_dump()["order_id"]}

    base = serve(app)
    res = fetch(
        f"{base}/orders2",
        "POST",
        _json({"order_id": 9, "customer": "ada", "email": "a@b.co"}),
        headers=JSON_CT,
    )
    assert res.status == 200
    assert json.loads(res.body) == {"dump": 9}


def test_route_body_422_never_enters_python(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()
    calls: list[str] = []

    @app.post("/orders3")
    def create(order: Order) -> dict:
        calls.append("handler")
        return {"ok": True}

    base = serve(app)
    res = fetch(
        f"{base}/orders3",
        "POST",
        _json({"order_id": 0, "customer": "", "email": "a@b.co"}),
        headers=JSON_CT,
    )
    assert res.status == 422
    assert res.header("Content-Type") == "application/problem+json"
    body = json.loads(res.body)
    assert {e["code"] for e in body["errors"]} >= {"ge", "min_len"}
    assert calls == []  # rejected before the handler ran


def test_route_alias_body(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/alias")
    def create(user: Aliased) -> dict:
        return {"name": user.user_name, "age": user.real_age}

    base = serve(app)
    res = fetch(
        f"{base}/alias",
        "POST",
        _json({"userName": "ada", "yearsOld": 30}),
        headers=JSON_CT,
    )
    assert res.status == 200
    assert json.loads(res.body) == {"name": "ada", "age": 30}
    res = fetch(
        f"{base}/alias",
        "POST",
        _json({"user_name": "ada", "yearsOld": 30}),
        headers=JSON_CT,
    )
    assert res.status == 422


def test_route_body_marker_explicit(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/marked")
    def create(order: Body[Order]) -> dict:
        return {"id": order.order_id}

    base = serve(app)
    res = fetch(
        f"{base}/marked",
        "POST",
        _json({"order_id": 3, "customer": "ada", "email": "a@b.co"}),
        headers=JSON_CT,
    )
    assert res.status == 200
    assert json.loads(res.body) == {"id": 3}


def test_route_returns_pydantic_model(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/order/{oid}")
    def read(oid: int) -> Order:
        return Order(order_id=oid, customer="ada", email="a@b.co", geo=Geo(lat=1.0, lon=2.0))

    base = serve(app)
    res = fetch(f"{base}/order/4")
    assert res.status == 200
    body = json.loads(res.body)
    assert body["order_id"] == 4
    assert body["geo"] == {"lat": 1.0, "lon": 2.0}
    assert body["age"] == 18


def test_route_model_validate_classmethod(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/revalidate")
    def revalidate(order: Order) -> dict:
        # The instance also passes pydantic's own validation afterwards.
        again = Order.model_validate(order.model_dump())
        return {"same": again.order_id == order.order_id}

    base = serve(app)
    res = fetch(
        f"{base}/revalidate",
        "POST",
        _json({"order_id": 2, "customer": "ada", "email": "a@b.co"}),
        headers=JSON_CT,
    )
    assert res.status == 200
    assert json.loads(res.body) == {"same": True}


def test_route_root_model_body(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/root")
    def create(root: IntRoot) -> dict:
        return {"root": root.root + 1}

    base = serve(app)
    res = fetch(f"{base}/root", "POST", _json(41), headers=JSON_CT)
    assert res.status == 200
    assert json.loads(res.body) == {"root": 42}


def test_route_discriminated_union_body(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/pet")
    def create(owner: HasPet) -> dict:
        return {"pet": type(owner.pet).__name__}

    base = serve(app)
    res = fetch(f"{base}/pet", "POST", _json({"pet": {"kind": "cat"}}), headers=JSON_CT)
    assert res.status == 200
    assert json.loads(res.body) == {"pet": "CatModel"}


# ---- OpenAPI --------------------------------------------------------------------


def test_openapi_components_and_paths(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/orders")
    def create(order: Order) -> Order:
        return order

    base = serve(app)
    res = fetch(f"{base}/openapi.json")
    assert res.status == 200
    doc = json.loads(res.body)
    assert doc["openapi"] == "3.1.0"
    op = doc["paths"]["/orders"]["post"]
    ref = op["requestBody"]["content"]["application/json"]["schema"]["$ref"]
    assert ref == "#/components/schemas/Order"
    order = doc["components"]["schemas"]["Order"]
    assert order["type"] == "object"
    props = order["properties"]
    assert set(props) == set(Order.model_fields)
    assert props["order_id"] == {"type": "integer", "minimum": 1}
    assert props["customer"] == {"type": "string", "minLength": 1, "maxLength": 50}
    assert props["sku"] == {"type": "string", "pattern": "^[A-Z]+", "default": "SKU"}
    assert props["geo"]["anyOf"][0]["$ref"] == "#/components/schemas/Geo"
    assert "order_id" in order["required"]
    assert "customer" in order["required"]
    assert "age" not in order["required"]  # has a default
    assert doc["components"]["schemas"]["Geo"]["properties"]["lat"] == {"type": "number"}
    assert order["description"] == "The ~10-field body record."
    # response $ref also resolves
    assert op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"] == ref


def test_openapi_alias_and_forbid(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/alias")
    def create(user: Aliased) -> dict:
        return {}

    base = serve(app)
    doc = json.loads(fetch(f"{base}/openapi.json").body)
    aliased = doc["components"]["schemas"]["Aliased"]
    assert set(aliased["properties"]) == {"userName", "yearsOld"}
    assert aliased["additionalProperties"] is False


def test_startup_error_on_unsupported_model() -> None:
    app = Fulmen()

    @app.post("/bad")
    def create(x: WithUnsupported) -> dict:
        return {}  # pragma: no cover

    from fulmen import StartupError

    with pytest.raises(StartupError, match="SecretStr"):
        app._compile_native()
