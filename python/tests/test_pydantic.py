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
import typing
from collections.abc import Callable
from typing import Annotated, ClassVar, Literal

import pytest
from conftest import fetch
from fulmen import Body, Fulmen, Len
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


# ---- adapter edge coverage -----------------------------------------------------
# ---- adapter edge coverage ------------------------------------------------------
#
# Models here are module-level: materializing a `desc.validate` result resolves
# the class by `module:qualname`, which cannot reach `<locals>` classes.


class _OpaqueCallable:
    """A callable `inspect.signature` can't introspect."""

    @property
    def __signature__(self) -> object:
        raise ValueError("no signature")

    def __call__(self, v: int) -> int:
        return v + 1


class OpaqueAfterMeta(BaseModel):
    x: Annotated[int, PydAfterValidator(_OpaqueCallable())]


from pydantic.types import Strict as _PydStrict  # noqa: E402


class StrictMetaField(BaseModel):
    x: Annotated[int, _PydStrict()]


class NestedStrictMeta(BaseModel):
    xs: list[Annotated[int, _PydStrict()]]


class FulmenMarkerMeta(BaseModel):
    x: Annotated[list[int], Len(min=1)]


class NestedFulmenMarkerMeta(BaseModel):
    xs: list[Annotated[str, Len(min=2)]]


class _FakeDiscriminator:
    """Duck-typed `pydantic.Discriminator` — the adapter reads by name."""

    discriminator = "kind"


_FakeDiscriminator.__name__ = "Discriminator"


class _FakeDiscriminatorFn:
    discriminator = staticmethod(lambda v: "kind")


_FakeDiscriminatorFn.__name__ = "Discriminator"


class TaggedUnionHolder(BaseModel):
    pet: Annotated[CatModel | DogModel, _FakeDiscriminator()]


class BadTaggedUnion(BaseModel):
    pet: Annotated[CatModel | DogModel, _FakeDiscriminatorFn()]


class StringConsField(BaseModel):
    x: Annotated[
        str,
        pydantic.StringConstraints(strip_whitespace=True, to_lower=True, min_length=1),
    ]


class NestedStringConsField(BaseModel):
    xs: list[
        Annotated[
            str,
            pydantic.StringConstraints(strip_whitespace=True, to_lower=True),
        ]
    ]


class UnionMemberAnnotated(BaseModel):
    x: Annotated[str, pydantic.StringConstraints(to_lower=True)] | int


class DictArgAnnotated(BaseModel):
    m: dict[str, Annotated[str, pydantic.StringConstraints(to_lower=True)]]


class NestedAfterMeta(BaseModel):
    xs: list[Annotated[int, PydAfterValidator(lambda v: v + 1)]]


class NoneReturningValidator(BaseModel):
    x: int

    @model_validator(mode="after")
    def check(self) -> None:
        return None


class ConstrainedRoot(RootModel[int]):
    root: int = Field(gt=0)


def test_one_arg_rejects_info_signature() -> None:
    """A `(value, info)`-shape validator can't run — no ValidationInfo."""

    def with_info(v: int, info: object) -> int:
        return v

    class M(BaseModel):
        x: Annotated[int, PydAfterValidator(with_info)]

    with pytest.raises(AnnotationError, match="ValidationInfo"):
        PydanticValidator().compile(M)


def test_one_arg_passes_uninspectable_callables() -> None:
    """Callables whose signature raises are called optimistically."""
    desc = PydanticValidator().compile(OpaqueAfterMeta)
    assert desc.validate({"x": 1}).x == 2


def test_pydantic_field_type_kinds() -> None:
    """`AnyUrl`/`Strict*`/path types map to spec kinds."""
    import pathlib

    class M(BaseModel):
        url: pydantic.AnyUrl
        strict_i: pydantic.StrictInt
        strict_s: pydantic.StrictStr
        strict_f: pydantic.StrictFloat
        strict_b: pydantic.StrictBool
        local: pathlib.Path

    spec = PydanticValidator.spec(M)
    fields = {f["name"]: f["type"] for f in spec["fields"]}
    assert fields["url"]["kind"] == "url"
    assert fields["strict_i"]["kind"] == "int"
    assert fields["strict_s"]["kind"] == "str"
    assert fields["strict_f"]["kind"] == "float"
    assert fields["strict_b"]["kind"] == "bool"
    assert fields["local"]["kind"] == "str"


def test_top_level_pydantic_type_annotation() -> None:
    """A mapped pydantic type as a bare annotation hits the special hook."""
    builder = SpecBuilder()
    assert builder.type_spec(pydantic.EmailStr, "t")["kind"] == "email"
    assert builder.type_spec(pydantic.AnyUrl, "t")["kind"] == "url"


def test_file_path_field_rejects_unsupported_metadata() -> None:
    """`FilePath` carries a `PathType` metadata item the IR can't express —
    the adapter raises rather than silently dropping the exists-check."""

    class M(BaseModel):
        path: pydantic.FilePath

    with pytest.raises(AnnotationError, match="unsupported pydantic metadata"):
        PydanticValidator.spec(M)


def test_metadata_error_treatments() -> None:
    """`WrapValidator`/`PlainValidator`/`SkipValidation` raise at spec build."""
    from pydantic.functional_validators import PlainValidator, WrapValidator

    class W(BaseModel):
        x: Annotated[int, WrapValidator(lambda v, h: v)]

    with pytest.raises(AnnotationError, match="WrapValidator"):
        PydanticValidator.spec(W)

    class P(BaseModel):
        x: Annotated[int, PlainValidator(lambda v: v)]

    with pytest.raises(AnnotationError, match="PlainValidator"):
        PydanticValidator.spec(P)

    class S(BaseModel):
        x: Annotated[int, pydantic.SkipValidation()]

    with pytest.raises(AnnotationError, match="SkipValidation"):
        PydanticValidator.spec(S)


def test_nested_wrap_validator_raises() -> None:
    from pydantic.functional_validators import WrapValidator

    class M(BaseModel):
        xs: list[Annotated[int, WrapValidator(lambda v, h: v)]]

    with pytest.raises(AnnotationError, match="WrapValidator"):
        PydanticValidator.spec(M)


def test_strict_metadata_is_a_noop() -> None:
    desc = PydanticValidator().compile(StrictMetaField)
    assert desc.validate({"x": 1}).x == 1


def test_nested_strict_noop() -> None:
    desc = PydanticValidator().compile(NestedStrictMeta)
    assert desc.validate({"xs": [1]}).xs == [1]


def test_fulmen_markers_inside_pydantic_annotated() -> None:
    """Fulmen markers mixed into `Annotated` pass through `apply_markers`."""
    desc = PydanticValidator().compile(FulmenMarkerMeta)
    assert desc.validate({"x": [1]}).x == [1]
    with pytest.raises(ValidationError):
        desc.validate({"x": []})


def test_nested_fulmen_marker_in_annotated() -> None:
    """Fulmen `Len` inside a nested `Annotated` is kept verbatim."""
    spec = PydanticValidator.spec(NestedFulmenMarkerMeta)
    item = spec["fields"][0]["type"]["item"]
    assert item["len"] == {"min": 2}


def test_discriminator_metadata_marker() -> None:
    """A `Discriminator`-named metadata item with a str tag applies."""
    desc = PydanticValidator().compile(TaggedUnionHolder)
    obj = desc.validate({"pet": {"kind": "dog"}})
    assert isinstance(obj.pet, DogModel)


def test_discriminator_callable_tag_rejected() -> None:
    """A callable `discriminator` (pydantic's own shape) can't name a field."""
    with pytest.raises(AnnotationError, match="needs a field name"):
        PydanticValidator.spec(BadTaggedUnion)


def test_string_constraints_transforms() -> None:
    """`StringConstraints` folds strip/lower into the spec."""
    desc = PydanticValidator().compile(StringConsField)
    assert desc.validate({"x": "  AB "}).x == "ab"


def test_unsupported_metadata_attrs_raise() -> None:
    """`decimal_places`/`allow_inf_nan` can't map to the IR."""
    from decimal import Decimal

    class D(BaseModel):
        x: Decimal = Field(decimal_places=2)

    with pytest.raises(AnnotationError, match="decimal_places"):
        PydanticValidator.spec(D)

    class F(BaseModel):
        x: float = Field(allow_inf_nan=True)

    with pytest.raises(AnnotationError, match="allow_inf_nan"):
        PydanticValidator.spec(F)

    class F2(BaseModel):
        x: float = Field(allow_inf_nan=False)

    # Even a False flag is an unconsumed pydantic marker — still an error.
    with pytest.raises(AnnotationError, match="unsupported pydantic metadata"):
        PydanticValidator.spec(F2)


def test_unknown_pydantic_metadata_raises() -> None:
    import annotated_types

    class M(BaseModel):
        x: Annotated[int, annotated_types.Predicate(lambda v: True)]

    with pytest.raises(AnnotationError, match="unsupported pydantic metadata"):
        PydanticValidator.spec(M)


def test_nested_field_info_metadata() -> None:
    """`Field(...)` inside a nested `Annotated` converts doc attrs + ge."""

    class M(BaseModel):
        xs: list[Annotated[int, Field(ge=2, description="an item", examples=[3], deprecated=True)]]

    spec = PydanticValidator.spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["meta"]["description"] == "an item"
    assert item["meta"]["example"] == 3
    assert item["meta"]["deprecated"] is True
    assert item["range"]["ge"] == 2


def test_nested_discriminator_field_info() -> None:
    """`Field(discriminator=...)` nested inside a container sets the tag."""

    class M(BaseModel):
        pets: list[Annotated[CatModel | DogModel, Field(discriminator="kind")]]

    spec = PydanticValidator.spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["tag"] == "kind"


def test_nested_annotated_marker_conversions() -> None:
    """Nested `annotated_types` metadata become Fulmen markers."""
    import annotated_types

    class M(BaseModel):
        xs: list[
            Annotated[
                int,
                annotated_types.Gt(0),
                annotated_types.Le(9),
                annotated_types.MultipleOf(3),
            ]
        ]

    spec = PydanticValidator.spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["range"] == {"gt": 0, "le": 9}
    assert item["multiple_of"] == 3


def test_nested_min_max_len_and_pattern() -> None:
    class M(BaseModel):
        xs: list[
            Annotated[
                str,
                pydantic.StringConstraints(min_length=2, pattern="^a"),
            ]
        ]

    spec = PydanticValidator.spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["len"] == {"min": 2}
    assert item["pattern"] == "^a"


def test_nested_transforms() -> None:
    spec = PydanticValidator.spec(NestedStringConsField)
    item = spec["fields"][0]["type"]["item"]
    assert item["meta"]["transforms"] == {"strip": True, "lower": True}


def test_nested_unsupported_attrs_raise() -> None:
    from decimal import Decimal

    class M(BaseModel):
        xs: list[Annotated[Decimal, Field(decimal_places=2)]]

    with pytest.raises(AnnotationError, match="decimal_places"):
        PydanticValidator.spec(M)

    class F(BaseModel):
        xs: list[Annotated[float, Field(allow_inf_nan=True)]]

    with pytest.raises(AnnotationError, match="allow_inf_nan"):
        PydanticValidator.spec(F)


def test_nested_unknown_marker_raises() -> None:
    import annotated_types

    class M(BaseModel):
        xs: list[Annotated[int, annotated_types.Predicate(lambda v: True)]]

    with pytest.raises(AnnotationError, match="unsupported pydantic metadata"):
        PydanticValidator.spec(M)


def test_nested_after_validator_marker() -> None:
    desc = PydanticValidator().compile(NestedAfterMeta)
    assert desc.validate({"xs": [1]}).xs == [2]


def test_rewrite_hint_rebuilds_unions() -> None:
    """A union whose members rewrite (Annotated inside) is reassembled."""
    desc = PydanticValidator().compile(UnionMemberAnnotated)
    assert desc.validate({"x": "AB"}).x == "ab"
    assert desc.validate({"x": 5}).x == 5


def test_rewrite_hint_copy_with_containers() -> None:
    """`dict[str, Annotated[...]]` rewrites args through `copy_with`."""
    desc = PydanticValidator().compile(DictArgAnnotated)
    assert desc.validate({"m": {"a": "AB"}}).m == {"a": "ab"}


def test_undefined_default_is_required() -> None:
    """A duck-typed `PydanticUndefined` default resolves to `required`."""
    from fulmen.adapters.pydantic import model_spec

    class PydanticUndefined:
        pass

    class M(BaseModel):
        x: int = Field(default=PydanticUndefined())

    spec = model_spec(SpecBuilder(), M, "annotation")
    assert spec["fields"][0]["missing"] == "required"


def test_non_scalar_default_gets_deepcopy_factory() -> None:
    class M(BaseModel):
        xs: list[int] = [1, 2]

    spec = PydanticValidator.spec(M)
    assert "factory" in spec["fields"][0]["missing"]


def test_serialization_alias_with_alias() -> None:
    """`alias` + a distinct `serialization_alias` — the wire name is the
    validation alias (pydantic mirrors `alias` into it)."""

    class M(BaseModel):
        x: int = Field(alias="wire", serialization_alias="out")

    spec = PydanticValidator.spec(M)
    assert spec["fields"][0]["alias"] == "wire"


def test_alias_generator_callable_and_object() -> None:
    """`alias_generator` accepts a callable or an `AliasGenerator`."""

    class C(BaseModel):
        model_config = ConfigDict(alias_generator=str.upper)
        user_name: str

    spec = PydanticValidator.spec(C)
    assert spec["fields"][0]["alias"] == "USER_NAME"

    from pydantic.aliases import AliasGenerator

    class G(BaseModel):
        model_config = ConfigDict(alias_generator=AliasGenerator(validation_alias=str.lower))
        UserName: str

    spec = PydanticValidator.spec(G)
    assert spec["fields"][0]["alias"] == "username"


def test_model_config_str_flags() -> None:
    class M(BaseModel):
        model_config = ConfigDict(
            str_strip_whitespace=True,
            str_to_lower=False,
            str_to_upper=True,
            str_min_length=1,
            str_max_length=9,
        )
        x: str

    spec = PydanticValidator.spec(M)
    node = spec["fields"][0]["type"]
    assert node["meta"]["transforms"] == {"strip": True, "upper": True}
    assert node["len"] == {"min": 1, "max": 9}


def test_field_doc_metadata() -> None:
    class M(BaseModel):
        x: int = Field(description="the x", examples=[7], deprecated=True, title="X")

    spec = PydanticValidator.spec(M)
    meta = spec["fields"][0]["meta"]
    assert meta["description"] == "the x"
    assert meta["example"] == 7
    assert meta["deprecated"] is True
    assert meta["title"] == "X"


def test_model_validator_returning_none_keeps_obj() -> None:
    desc = PydanticValidator().compile(NoneReturningValidator)
    assert desc.validate({"x": 1}).x == 1


def test_decorators_absent_on_ducktyped_model() -> None:
    """A duck-typed model without `__pydantic_decorators__` specs fine."""
    from fulmen.adapters.pydantic import model_spec

    class Fake:
        model_fields: ClassVar[dict] = {}
        model_config: ClassVar[dict] = {}
        __pydantic_serializer__ = object()

    spec = model_spec(SpecBuilder(), Fake, "annotation")
    assert spec["kind"] == "record"
    assert spec["fields"] == []


def test_function_local_model_specs() -> None:
    """`<locals>` qualnames skip the resolvable-binding step."""

    class LocalModel(BaseModel):
        x: int

    spec = PydanticValidator.spec(LocalModel)
    assert spec["kind"] == "record"


def test_unresolvable_parent_qualname() -> None:
    """A qualname whose intermediate name isn't bound just skips."""
    ghost = type("Ghost", (BaseModel,), {"__annotations__": {"x": int}})
    ghost.__qualname__ = "NoSuchOuter.Ghost"
    spec = PydanticValidator.spec(ghost)
    assert spec["class"]["name"] == "NoSuchOuter.Ghost"


def test_unsupported_extra_config_raises() -> None:
    class M(BaseModel):
        x: int

    original = M.model_config
    M.model_config = {"extra": "unlisted-policy"}
    try:
        with pytest.raises(AnnotationError, match="extra="):
            PydanticValidator.spec(M)
    finally:
        M.model_config = original


def test_root_model_with_constraints() -> None:
    desc = PydanticValidator().compile(ConstrainedRoot)
    assert desc.validate(3).root == 3
    with pytest.raises(ValidationError):
        desc.validate(-1)


def test_spec_and_compile_reject_non_models() -> None:
    adapter = PydanticValidator()
    with pytest.raises(AnnotationError, match="not a pydantic"):
        adapter.spec(int)
    with pytest.raises(AnnotationError, match="not a pydantic"):
        adapter.compile(int)


def test_json_schema_includes_components() -> None:
    schema = PydanticValidator().json_schema(HasPet)
    assert "components" in schema
    assert "CatModel" in schema["components"]["schemas"]


class _NestedTagFn:
    discriminator = staticmethod(lambda v: "kind")


_NestedTagFn.__name__ = "Discriminator"


class NestedBadTag(BaseModel):
    pets: list[Annotated[CatModel | DogModel, _NestedTagFn()]]


def test_nested_discriminator_callable_tag_rejected() -> None:
    with pytest.raises(AnnotationError, match="needs a field name"):
        PydanticValidator.spec(NestedBadTag)


def test_nested_min_items_and_to_upper() -> None:
    """`min_items`-only and `to_upper` marker attributes convert."""

    class MinItems:
        min_items = 2

    class ToUpper:
        to_upper = True

    MinItems.__module__ = "annotated_types._fake"
    ToUpper.__module__ = "annotated_types._fake"

    class M(BaseModel):
        xs: list[Annotated[list[int], MinItems()]]
        ys: list[Annotated[str, ToUpper()]]

    spec = PydanticValidator.spec(M)
    fields = {f["name"]: f["type"]["item"] for f in spec["fields"]}
    assert fields["xs"]["len"] == {"min": 2}
    assert fields["ys"]["meta"]["transforms"] == {"upper": True}


def test_nested_user_marker_is_ignored() -> None:
    """A marker from a user module with no constraint attrs is dropped —
    the `Annotated` unwraps to the bare base type."""

    class ArbitraryMeta:
        pass

    class M(BaseModel):
        xs: list[Annotated[int, ArbitraryMeta()]]

    spec = PydanticValidator.spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["kind"] == "int"


def test_deeply_nested_qualname_rebinds() -> None:
    """A multi-part qualname walks `Outer.Inner` and rebinds the class —
    the path pydantic uses for relocated generic specializations."""
    import sys
    from types import SimpleNamespace

    inner = SimpleNamespace()
    inner.Cls = None
    outer = SimpleNamespace()
    outer.Inner = inner
    sys.modules[__name__].Outer = outer
    try:
        moved = type(
            "Cls",
            (BaseModel,),
            {"__module__": __name__, "__annotations__": {"x": int}},
        )
        moved.__qualname__ = "Outer.Inner.Cls"
        spec = PydanticValidator.spec(moved)
        assert spec["kind"] == "record"
        assert inner.Cls is moved  # `_ensure_resolvable` bound it
    finally:
        del sys.modules[__name__].Outer


def test_alias_choices_validation_alias_is_an_error() -> None:
    """`AliasChoices`/`AliasPath` cannot be expressed as a wire name."""

    class Aliased(BaseModel):
        x: int = Field(validation_alias=AliasChoices("a", "b"))

    with pytest.raises(AnnotationError, match="AliasChoices"):
        PydanticValidator.spec(Aliased)


def test_alias_generator_object_attributes() -> None:
    """A non-callable `AliasGenerator` resolves `validation_alias`/`alias`
    methods instead of being called directly."""
    from pydantic.aliases import AliasGenerator

    class Gen(BaseModel):
        model_config = ConfigDict(
            alias_generator=AliasGenerator(validation_alias=lambda n: f"v_{n}")
        )
        user_name: str = ""

    spec = PydanticValidator.spec(Gen)
    by_name = {f["name"]: f for f in spec["fields"]}
    assert by_name["user_name"]["alias"] == "v_user_name"


def test_compiled_regex_pattern_metadata() -> None:
    """A compiled `re.Pattern` in `Field(pattern=...)` is unwrapped to
    its source string."""
    import re

    class Rx(BaseModel):
        x: Annotated[str, Field(pattern=re.compile("^a+$"))] = ""

    spec = PydanticValidator.spec(Rx)
    assert spec["fields"][0]["type"]["pattern"] == "^a+$"


def test_discriminator_marker_in_metadata() -> None:
    """A `pydantic.Discriminator` marker converts to the fulmen marker."""

    class UnionHolder(BaseModel):
        pet: Annotated[CatModel | DogModel, pydantic.Discriminator("kind")]

    spec = PydanticValidator.spec(UnionHolder)
    node = spec["fields"][0]["type"]
    assert node["tag"] == "kind"


def test_unknown_metadata_object_is_ignored() -> None:
    """Non-pydantic Annotated metadata is documentation, not a contract —
    `_convert_metadata_item` falls through and ignores it."""

    class Opaque:
        pass

    class Holder(BaseModel):
        x: Annotated[int, Opaque()] = 0

    spec = PydanticValidator.spec(Holder)
    assert spec["fields"][0]["type"]["kind"] == "int"


def test_str_config_constraints_fold_into_len() -> None:
    """`ConfigDict(str_min_length=...)` lands on the str node's len cons."""

    class Cfg(BaseModel):
        model_config = ConfigDict(str_min_length=2, str_max_length=4)
        x: str = "ab"

    spec = PydanticValidator.spec(Cfg)
    node = spec["fields"][0]["type"]
    assert node["len"] == {"min": 2, "max": 4}


def test_json_schema_without_components() -> None:
    """A `RootModel` spec is the inner type — `json_schema` emits no
    `components` block when nothing was collected."""
    assert PydanticValidator().json_schema(RootModel[int]) == {"type": "integer"}


def test_str_config_transforms_fold_into_meta() -> None:
    """`ConfigDict(str_to_lower/str_strip_whitespace)` lands as
    transform flags on the str node's meta."""

    class CfgT(BaseModel):
        model_config = ConfigDict(str_to_lower=True, str_strip_whitespace=True)
        x: str = "a"

    spec = PydanticValidator.spec(CfgT)
    transforms = spec["fields"][0]["type"]["meta"]["transforms"]
    assert transforms == {"strip": True, "lower": True}


def test_typing_generic_rebuilds_via_copy_with() -> None:
    """`typing.List[...]` generic aliases rebuild through `copy_with`
    after marker rewriting (unlike builtin `list[...]`)."""

    class Lst(BaseModel):
        xs: typing.List[pydantic.StrictInt] = Field(  # noqa: UP006 -- `typing.List` exercises the `copy_with` rebuild path
            default_factory=list
        )

    spec = PydanticValidator.spec(Lst)
    assert spec["fields"][0]["type"] == {"kind": "list", "item": {"kind": "int"}}


def test_nested_compiled_pattern_unwraps_source() -> None:
    """`Field(pattern=re.compile(...))` nested inside a container's
    `Annotated` unwraps to the regex source text."""
    import re

    class RxList(BaseModel):
        xs: list[Annotated[str, Field(pattern=re.compile("^b+$"))]] = Field(default_factory=list)

    spec = PydanticValidator.spec(RxList)
    assert spec["fields"][0]["type"]["item"]["pattern"] == "^b+$"
