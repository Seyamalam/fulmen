"""Unit coverage for `_typing`'s annotation → spec machinery.

These exercise `SpecBuilder.type_spec` / `param_spec` / `response_spec`
directly — the spec dicts are the contract `app.py` hands to the Rust
bridge, so checking shapes here is cheaper than booting a TestServer for
every annotation flavor.
"""

from __future__ import annotations

import dataclasses
import datetime
import inspect
import ipaddress
import typing
import uuid
from decimal import Decimal
from typing import Annotated, Any, Literal, NewType, Optional, TypedDict, Union

import pytest
from fulmen import _typing
from fulmen._typing import (
    AnnotationError,
    ParamInfo,
    ResponseVariant,
    SpecBuilder,
    param_spec,
    response_spec,
)
from fulmen.constraints import (
    UNSET,
    AfterValidator,
    Alias,
    Default,
    Deprecated,
    Description,
    Discriminator,
    Example,
    Field,
    Len,
    MultipleOf,
    Pattern,
    Range,
    Strip,
    Unique,
)
from fulmen.errors import HTTPError, NotFound, ValidationError, problem_for
from fulmen.model import Model
from fulmen.params import (
    Body,
    Cookie,
    Header,
    Path,
    Query,
    Request,
    UploadFile,
)
from fulmen.responses import JSONResponse, Response
from fulmen.types import Email, Raw, Url

EMPTY_PATH: frozenset[str] = frozenset()


def p(name: str, hint: Any, *, path: frozenset[str] = EMPTY_PATH, **kw: Any) -> ParamInfo:
    """`param_spec` shorthand: default + kind are keyword-only test sugar."""
    return param_spec(
        SpecBuilder(),
        name,
        hint,
        kw.pop("default", UNSET),
        kw.pop("pkind", inspect.Parameter.POSITIONAL_OR_KEYWORD),
        path,
        kw.pop("where", "route GET /x"),
    )


# ---- scalar kinds --------------------------------------------------------------


@pytest.mark.parametrize(
    ("hint", "kind"),
    [
        (bool, "bool"),
        (int, "int"),
        (float, "float"),
        (str, "str"),
        (bytes, "bytes"),
        (datetime.datetime, "datetime"),
        (datetime.date, "date"),
        (datetime.time, "time"),
        (datetime.timedelta, "timedelta"),
        (Decimal, "decimal"),
        (uuid.UUID, "uuid"),
        (ipaddress.IPv4Address, "ipv4"),
        (ipaddress.IPv6Address, "ipv6"),
        (Email, "email"),
        (Url, "url"),
        (Raw, "raw"),
        (UploadFile, "raw"),
    ],
)
def test_scalar_kinds(hint: Any, kind: str) -> None:
    assert SpecBuilder().type_spec(hint, "t") == {"kind": kind}


def test_any_none_and_unknown() -> None:
    b = SpecBuilder()
    assert b.type_spec(Any, "t") == {"kind": "any"}
    assert b.type_spec(object, "t") == {"kind": "any"}
    assert b.type_spec(None, "t") == {"kind": "none"}
    assert b.type_spec(type(None), "t") == {"kind": "none"}
    tvar = typing.TypeVar("T")
    assert b.type_spec(tvar, "t") == {"kind": "typevar", "name": "T"}


def test_unsupported_base_and_classvar() -> None:
    with pytest.raises(AnnotationError, match="unsupported"):
        SpecBuilder().type_spec(complex, "a body")
    with pytest.raises(AnnotationError, match="ClassVar"):
        SpecBuilder().type_spec(typing.ClassVar[int], "t")


def test_request_is_injected_not_a_schema() -> None:
    with pytest.raises(AnnotationError, match="injected"):
        SpecBuilder().type_spec(Request, "t")


def test_newtype() -> None:
    UserId = NewType("UserId", int)
    spec = SpecBuilder().type_spec(UserId, "t")
    assert spec["kind"] == "newtype"
    assert spec["name"] == "UserId"
    assert spec["inner"] == {"kind": "int"}


# ---- container/alias kinds -----------------------------------------------------


def test_containers() -> None:
    b = SpecBuilder()
    assert b.type_spec(list[int], "t") == {"kind": "list", "item": {"kind": "int"}}
    assert b.type_spec(set[str], "t")["item"] == {"kind": "str"}
    assert b.type_spec(frozenset[int], "t")["kind"] == "frozenset"
    assert b.type_spec(list, "t")["item"] == {"kind": "any"}
    assert b.type_spec(dict[str, int], "t")["value"] == {"kind": "int"}
    assert b.type_spec(dict, "t")["key"] == {"kind": "str"}
    assert b.type_spec(dict[Any, int], "t")["key"] == {"kind": "str"}
    variadic = b.type_spec(tuple[int, ...], "t")
    assert variadic == {"kind": "tuple", "item": {"kind": "int"}}
    fixed = b.type_spec(tuple[int, str], "t")
    assert fixed["kind"] == "tuple_fixed"
    assert [i["kind"] for i in fixed["items"]] == ["int", "str"]


def test_union_and_optional() -> None:
    b = SpecBuilder()
    opt = b.type_spec(Optional[int], "t")  # noqa: UP045 - typing.Union origin is the subject
    assert opt == {"kind": "optional", "inner": {"kind": "int"}}
    none_only = b.type_spec(Union[None], "t")  # noqa: UP007 - typing.Union origin is the subject
    assert none_only == {"kind": "none"}
    multi = b.type_spec(int | str | None, "t")
    assert multi["kind"] == "optional"
    assert multi["inner"]["kind"] == "union"
    assert [v["kind"] for v in multi["inner"]["variants"]] == ["int", "str"]
    two = b.type_spec(int | str, "t")
    assert two["kind"] == "union"


def test_literal() -> None:
    spec = SpecBuilder().type_spec(Literal["a", 1, True], "t")
    assert spec == {"kind": "literal", "members": ["a", 1, True]}
    with pytest.raises(AnnotationError, match="literal member"):
        SpecBuilder().type_spec(Literal[object()], "t")


def test_annotated_mid_tree_constraints() -> None:
    spec = SpecBuilder().type_spec(Annotated[int, Range(ge=1, lt=10)], "t")
    assert spec["range"] == {"ge": 1, "lt": 10}
    s = SpecBuilder().type_spec(Annotated[str, Len(min=2, max=4), Pattern(r"^a+$")], "t")
    assert s["len"] == {"min": 2, "max": 4}
    assert s["pattern"] == "^a+$"


def test_enum_spec() -> None:
    import enum

    class Shade(enum.Enum):
        A = "a"

    class Num(enum.IntEnum):
        B = 2

    class Text(enum.StrEnum):
        C = "c"

    plain = SpecBuilder().type_spec(Shade, "t")
    assert plain["kind"] == "enum"
    assert plain["flavor"] == "plain"
    assert plain["members"] == [{"name": "A", "value": "a"}]
    assert SpecBuilder().type_spec(Num, "t")["flavor"] == "int"
    assert SpecBuilder().type_spec(Text, "t")["flavor"] == "str"


# ---- record flavors ------------------------------------------------------------


class Point(Model):
    x: int
    y: int = 0


def test_model_record_spec() -> None:
    spec = SpecBuilder().type_spec(Point, "t")
    assert spec["kind"] == "record"
    assert spec["flavor"] == "model"
    assert spec["class"]["name"] == "Point"
    assert [f["name"] for f in spec["fields"]] == ["x", "y"]
    assert spec["fields"][0]["missing"] == "required"
    assert spec["fields"][1]["missing"] == {"literal": 0}


def test_generic_model_record() -> None:
    spec = SpecBuilder().type_spec(Point, "t")
    assert spec["type_params"] == []


def test_model_field_markers() -> None:
    class Fancy(Model):
        a: Annotated[int, Field(alias="A", description="the a", example=3, deprecated=True)]
        b: Annotated[str, Alias("B")]
        c: Annotated[int, Default(5)]
        d: Annotated[str, AfterValidator(lambda s: s.strip()), Description("d")]
        e: Annotated[str, Example("hi")]

    spec = SpecBuilder().record_spec(Fancy)
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["alias"] == "A"
    assert fields["a"]["meta"]["description"] == "the a"
    assert fields["a"]["meta"]["example"] == 3
    assert fields["a"]["meta"]["deprecated"] is True
    assert fields["b"]["alias"] == "B"
    assert fields["c"]["missing"] == {"literal": 5}
    assert fields["d"]["meta"]["description"] == "d"
    assert fields["e"]["meta"]["example"] == "hi"

    with pytest.raises(AnnotationError, match="Discriminator"):

        class WithDisc(Model):
            f: Annotated[int, Discriminator("kind")] = 0


def test_model_field_rejects_param_and_unknown_markers() -> None:
    with pytest.raises(AnnotationError, match="parameter marker"):

        class Bad(Model):
            a: Annotated[int, Query()]

    with pytest.raises(AnnotationError, match="unsupported Annotated marker"):

        class Bad2(Model):
            a: Annotated[int, object()]


def test_model_declared_class_field() -> None:
    class WithField(Model):
        a: int = Field(default=9, ge=0)
        b: str = "plain-default"

    spec = SpecBuilder().record_spec(WithField)
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["missing"] == {"literal": 9}
    assert fields["a"]["type"]["range"] == {"ge": 0}
    assert fields["b"]["missing"] == {"literal": "plain-default"}


def test_model_mutable_default_gets_factory() -> None:
    class WithList(Model):
        items: list[int] = Field(default_factory=list)

    spec = SpecBuilder().record_spec(WithList)
    field = spec["fields"][0]
    assert set(field["missing"]) == {"factory"}

    # A non-scalar literal default also becomes a factory (deepcopy).
    class WithList2(Model):
        items: list[int] = [1, 2]  # noqa: RUF012 - literal default becomes a per-instance factory

    spec2 = SpecBuilder().record_spec(WithList2)
    assert set(spec2["fields"][0]["missing"]) == {"factory"}


def test_dataclass_spec() -> None:
    @dataclasses.dataclass
    class DC:
        a: int
        b: Annotated[str, Description("bee")] = "x"
        c: list[int] = dataclasses.field(default_factory=list)
        d: int = dataclasses.field(init=False, default=4)

    spec = SpecBuilder().type_spec(DC, "t")
    assert spec["flavor"] == "dataclass"
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["missing"] == "required"
    assert fields["b"]["meta"]["description"] == "bee"
    assert fields["c"]["missing"] == "omit"  # cls(**kw) calls the factory
    assert fields["d"]["missing"] == "omit"  # not an init field


@dataclasses.dataclass
class _DCLoop:
    child: _DCLoop


def test_dataclass_recursion_rejected() -> None:
    with pytest.raises(AnnotationError, match="recursive dataclass"):
        SpecBuilder().type_spec(_DCLoop, "t")


class _TD(TypedDict):
    req: int
    opt: typing.NotRequired[str]


class _TDNotTotal(TypedDict, total=False):
    a: int
    b: list[str]


def test_typed_dict_spec() -> None:
    # `__required_keys__` misreports under PEP 563; the spec reads qualifiers
    # off the resolved hints instead.
    spec = SpecBuilder().type_spec(_TD, "t")
    assert spec["flavor"] == "typed_dict"
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["req"]["missing"] == "required"
    assert fields["opt"]["missing"] == "omit"
    not_total = SpecBuilder().type_spec(_TDNotTotal, "t")
    assert all(f["missing"] == "omit" for f in not_total["fields"])


class _TDLoop(TypedDict):
    child: _TDLoop


def test_typed_dict_recursion_rejected() -> None:
    with pytest.raises(AnnotationError, match="recursive TypedDict"):
        SpecBuilder().type_spec(_TDLoop, "t")


_T = typing.TypeVar("_T")


@dataclasses.dataclass
class _Box(typing.Generic[_T]):  # noqa: UP046 - classic generic spelling is the subject
    item: _T


def test_generic_dataclass() -> None:
    spec = SpecBuilder().type_spec(_Box[int], "t")
    assert spec["kind"] == "generic"
    assert spec["args"] == [{"kind": "int"}]
    assert spec["origin"]["flavor"] == "dataclass"


# ---- constraint application ----------------------------------------------------


def test_constraint_kind_errors() -> None:
    with pytest.raises(AnnotationError, match="Range/MultipleOf"):
        SpecBuilder().type_spec(Annotated[str, Range(ge=1)], "t")
    with pytest.raises(AnnotationError, match="Len applies"):
        SpecBuilder().type_spec(Annotated[int, Len(min=1)], "t")
    with pytest.raises(AnnotationError, match="Pattern applies"):
        SpecBuilder().type_spec(Annotated[int, Pattern("x")], "t")
    with pytest.raises(AnnotationError, match="Unique applies"):
        SpecBuilder().type_spec(Annotated[int, Unique()], "t")


def test_bound_spec_errors() -> None:
    with pytest.raises(AnnotationError, match="bound must be a number"):
        SpecBuilder().type_spec(Annotated[int, Range(ge=True)], "t")
    with pytest.raises(AnnotationError, match="bound must be"):
        SpecBuilder().type_spec(Annotated[int, Range(ge="x")], "t")


def test_decimal_bound_and_transforms() -> None:
    spec = SpecBuilder().type_spec(Annotated[Decimal, Range(ge=Decimal("1.5"))], "t")
    assert spec["range"]["ge"] == {"decimal": "1.5"}
    spec2 = SpecBuilder().type_spec(Annotated[str, Strip()], "t")
    assert spec2["meta"]["transforms"]["strip"] is True


def test_multiple_of_and_unique_on_list() -> None:
    spec = SpecBuilder().type_spec(Annotated[list[int], Unique(), Len(max=3)], "t")
    assert spec["unique"] is True
    assert spec["len"]["max"] == 3
    spec2 = SpecBuilder().type_spec(Annotated[int, MultipleOf(4)], "t")
    assert spec2["multiple_of"] == 4


def test_meta_markers_mid_tree() -> None:
    spec = SpecBuilder().type_spec(Annotated[int, Description("d"), Example(2), Deprecated()], "t")
    meta = spec["meta"]
    assert meta["description"] == "d"
    assert meta["example"] == 2
    assert meta["deprecated"] is True


def test_discriminator_on_union() -> None:
    spec = SpecBuilder().type_spec(Annotated[Point | int, Discriminator("kind")], "t")
    assert spec["kind"] == "union"
    assert spec["tag"] == "kind"
    with pytest.raises(AnnotationError, match="union"):
        SpecBuilder().type_spec(Annotated[int, Discriminator("kind")], "t")


def test_after_validator_registers_callback() -> None:
    def scrub(v: Any) -> Any:
        return v

    b = SpecBuilder()
    spec = b.type_spec(Annotated[str, AfterValidator(scrub)], "t")
    assert spec["meta"]["after"] == [0]
    assert b.callbacks == [scrub]


# ---- param_spec -----------------------------------------------------------------


def test_param_source_inference() -> None:
    assert p("page", int).source == "query"
    assert p("q", Optional[str]).source == "query"  # noqa: UP045
    assert p("limit", Union[int, str]).source == "query"  # noqa: UP007
    assert p("body", Point).source == "body"
    assert p("items", list[int]).source == "body"


def test_param_markers_and_aliases() -> None:
    info = p("x", Annotated[str, Header(alias="X-Thing")])
    assert info.source == "header"
    assert info.wire_name == "X-Thing"
    # Header names default to the kebab-case parameter name.
    assert p("x_thing", Annotated[str, Header()]).wire_name == "x-thing"
    assert p("page", Annotated[int, Query()]).wire_name == "page"
    assert p("uid", Annotated[int, Cookie()]).source == "cookie"
    assert p("data", Annotated[Point, Body()]).source == "body"


def test_param_marker_as_signature_default() -> None:
    info = p("x", int, default=Query(default=3))
    assert info.source == "query"
    assert not info.required
    assert info.default == 3


def test_param_fieldspec_as_signature_default() -> None:
    info = p("items", list[int], default=Field(default_factory=list))
    assert info.factory is not None
    assert not info.required
    assert info.factory() == []


def test_param_marker_error_cases() -> None:
    with pytest.raises(AnnotationError, match="two source markers"):
        p("x", Annotated[int, Query(), Header()])
    with pytest.raises(AnnotationError, match="unsupported Annotated marker"):
        p("x", Annotated[int, object()])


def test_param_default_markers() -> None:
    info = p("x", Annotated[int, Alias("ex"), Default(5), Description("d"), Example(1)])
    assert info.wire_name == "ex"
    assert info.default == 5
    meta = info.spec["meta"]
    assert meta["description"] == "d"
    assert meta["example"] == 1


def test_param_deprecated_and_after() -> None:
    info = p("x", Annotated[int, Deprecated(), AfterValidator(lambda v: v)])
    assert info.spec["meta"]["deprecated"] is True
    assert info.spec["meta"]["after"] == [0]


def test_param_request_and_file_sources() -> None:
    assert p("request", Request).source == "request"
    info = p("f", UploadFile)
    assert info.source == "file"
    assert info.spec["kind"] == "raw"


def test_param_path_rules() -> None:
    names = frozenset({"id"})
    info = p("id", int, path=names)
    assert info.source == "path"
    assert info.required
    # An explicit Path marker on a matching name is also fine.
    info2 = p("id", Annotated[int, Path()], path=names)
    assert info2.source == "path"

    with pytest.raises(AnnotationError, match="does not appear in the route"):
        p("nope", Annotated[int, Path()], path=EMPTY_PATH)
    with pytest.raises(AnnotationError, match="int` or `str"):
        p("id", Annotated[list[int], Path()], path=names)
    with pytest.raises(AnnotationError, match="cannot have a default"):
        p("id", Annotated[int, Path()], path=names, default=1)


def test_param_non_scalar_default_becomes_factory() -> None:
    info = p("tags", list[int], default=[1])
    assert info.factory is not None
    assert info.factory() == [1]


def test_param_kinds() -> None:
    pos = p("a", int, pkind=inspect.Parameter.POSITIONAL_ONLY)
    kw = p("b", int, pkind=inspect.Parameter.KEYWORD_ONLY)
    assert (pos.kind, kw.kind) == ("pos_only", "kw_only")
    assert p("c", int).kind == "normal"


def test_param_query_constraints() -> None:
    info = p("x", Annotated[int, Query(ge=0, le=10, multiple_of=2)])
    assert info.spec["range"] == {"ge": 0, "le": 10}
    assert info.spec["multiple_of"] == 2


# ---- response_spec --------------------------------------------------------------


def test_response_spec_variants() -> None:
    b = SpecBuilder()
    assert response_spec(b, inspect.Signature.empty) == []
    assert response_spec(b, None) == [ResponseVariant(204, None)]
    assert response_spec(b, int) == [ResponseVariant(200, {"kind": "int"})]
    union = response_spec(b, Point | None)
    assert [v.status for v in union] == [200, 204]
    resp = response_spec(b, Response)
    assert resp == [ResponseVariant(None, None)]
    typed = response_spec(b, JSONResponse[Point])
    assert typed[0].status == 200
    assert typed[0].body is not None
    err = response_spec(b, NotFound[dict[str, int]])
    assert err[0].status == 404
    assert err[0].body is not None
    bare_err = response_spec(b, NotFound)
    assert bare_err[0].status == 404


# ---- compile_model / model_descriptor --------------------------------------------


def test_model_descriptor_lazy() -> None:
    d1 = _typing.model_descriptor(Point)
    d2 = _typing.model_descriptor(Point)
    assert d1 is d2


# ---- errors.py edges ---------------------------------------------------------------


def test_http_error_problem_and_reason() -> None:
    err = NotFound(detail="gone", headers={"x-a": "b"}, errors=[{"loc": ["q"]}])
    assert err.status == 404
    body = err.problem()
    assert body["status"] == 404
    assert body["detail"] == "gone"
    assert body["errors"] == [{"loc": ["q"]}]
    assert err.headers == {"x-a": "b"}
    assert HTTPError(status=599).reason == "Error"
    inst = NotFound(instance="/here")
    assert inst.problem()["instance"] == "/here"
    assert inst.problem("/there")["instance"] == "/here"
    assert NotFound().problem("/fallback")["instance"] == "/fallback"


def test_problem_for_all_shapes() -> None:
    status, body, headers = problem_for(NotFound(detail="x", headers={"h": "v"}))
    assert status == 404
    assert body["detail"] == "x"
    assert headers == {"h": "v"}

    ve = ValidationError(entries=[{"loc": ["q", "p"], "msg": "required"}], truncated=False)
    status, body, _ = problem_for(ve)
    assert status == 422
    assert body["errors"][0]["msg"] == "required"

    status, body, _ = problem_for(RuntimeError("secret internals"))
    assert status == 500
    assert "secret" not in str(body)


# ---- params.py edges ----------------------------------------------------------------


def test_marker_class_getitem_and_repr() -> None:
    ann = Header[str, "X-Key"]
    args = typing.get_args(ann)
    marker = args[1]
    assert isinstance(marker, Header)
    assert marker.alias == "X-Key"
    ann2 = Query[int]
    assert isinstance(typing.get_args(ann2)[1], Query)
    with pytest.raises(TypeError, match="wire name"):
        Header[str, 3]  # type: ignore[misc]
    assert repr(Query()) == "Query()"
    assert repr(Query(default=5)) == "Query(5)"


def test_upload_file_repr() -> None:
    f = UploadFile(b"1234", filename="a.bin")
    assert repr(f) == "UploadFile(filename='a.bin', 4 bytes)"


# ---- responses.py edges -------------------------------------------------------------


def test_response_classes_direct() -> None:
    import mimetypes

    from fulmen.responses import (
        Accepted,
        Created,
        FileResponse,
        HTMLResponse,
        NoContent,
        Redirect,
        Stream,
        TextResponse,
    )

    assert JSONResponse({"a": 1}).content_type == "application/json"
    assert TextResponse("hi").content_type.startswith("text/plain")
    assert HTMLResponse("<b>x</b>").content_type.startswith("text/html")
    assert Created({"id": 1}).status == 201
    assert Accepted({"job": 2}).status == 202
    assert NoContent().status == 204
    r = Redirect("/new", status=308)
    assert r.status == 308
    assert r.headers["location"] == "/new"
    assert Redirect("/x").status == 307

    import pathlib
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        fp = pathlib.Path(td) / "f.txt"
        fp.write_bytes(b"data")
        fr = FileResponse(fp)
        assert fr.status == 200
        assert "f.txt" in fr.headers["content-disposition"]
        assert fr.content_type == mimetypes.guess_type("f.txt")[0]
        fr2 = FileResponse(fp, filename="named.bin", content_type="application/x-b")
        assert fr2.content_type == "application/x-b"
        assert 'filename="named.bin"' in fr2.headers["content-disposition"]

    s = Stream(iter([b"a", "b"]))
    assert s.status == 200
    with pytest.raises(TypeError, match="sync iterable"):
        Stream(42)  # type: ignore[arg-type]

    async def agen():
        yield b"x"

    with pytest.raises(TypeError, match="M5"):
        Stream(agen())  # type: ignore[arg-type]


# ---- model.py edges ------------------------------------------------------------------


def test_model_config_errors_and_frozen() -> None:
    with pytest.raises(TypeError, match="extra must be"):

        class BadExtra(Model, extra="nope"):  # type: ignore[call-arg]
            a: int = 0

    with pytest.raises(TypeError, match="rename must be"):

        class BadRename(Model, rename="nope"):  # type: ignore[call-arg]
            a: int = 0

    class Frozen(Model, frozen=True):
        a: int = 0

    f = Frozen(a=1)
    with pytest.raises(TypeError, match="frozen"):
        f.a = 2
    assert hash(f) == hash(Frozen(a=1))


def test_model_forward_ref_lazy_descriptor() -> None:
    class Lazy(Model):
        ref: DoesNotExist  # noqa: F821

    assert Lazy.__descriptor__ is None


def test_model_setattr_eq_repr() -> None:
    m = Point(x=1, y=2)
    assert m == Point(x=1, y=2)
    assert m != Point(x=9, y=2)
    assert m != "not-a-model"
    assert repr(m) == "Point(x=1, y=2)"
    m.x = 7  # validated assignment through the descriptor
    assert m.x == 7
    m._private = "ok"  # private names bypass validation
    m.extra_thing = 1  # undeclared names pass through (extra='ignore')
    with pytest.raises(ValidationError):
        m.x = "not-an-int-at-all"


# ---- app.py edges --------------------------------------------------------------------


def test_on_error_rejects_non_exceptions() -> None:
    from fulmen import Fulmen

    app = Fulmen()
    with pytest.raises(TypeError, match="exception class"):
        app.on_error(42)
    with pytest.raises(TypeError, match="exception class"):
        app.on_error(str)


# -- additional spec-tree edges ----------------------------------------------------


def test_bare_container_hints() -> None:
    """Unparameterized `set`/`frozenset`/`tuple` produce `any`-item nodes."""
    b = SpecBuilder()
    assert b.type_spec(set, "t") == {"kind": "set", "item": {"kind": "any"}}
    assert b.type_spec(frozenset, "t") == {"kind": "frozenset", "item": {"kind": "any"}}
    assert b.type_spec(tuple, "t") == {"kind": "tuple", "item": {"kind": "any"}}


def test_annotated_reaches_alias_spec_directly() -> None:
    """`type_spec` on a raw `Annotated` peels markers through `_alias_spec`."""
    b = SpecBuilder()
    node = b.type_spec(Annotated[str, Len(min=1)], "t")
    assert node["kind"] == "str"
    assert node["len"] == {"min": 1}


def test_non_builtin_generic_is_unsupported() -> None:
    """`G[int]` where `G` is neither dataclass, model nor pydantic model."""
    T = typing.TypeVar("T")

    class G(typing.Generic[T]):
        pass

    with pytest.raises(AnnotationError, match="unsupported"):
        SpecBuilder().type_spec(G[int], "t")


def test_recursive_model_is_an_annotation_error() -> None:
    """`child: "Self"` — the record IR has no reference type yet."""

    with pytest.raises(AnnotationError, match="recursive model"):
        _LazyRecursive(x=None)


class _LazyRecursive(Model):
    x: "Optional[_LazyRecursive]"  # noqa: UP037,UP045 -- deliberate string forward reference


def test_fieldspec_markers_inside_annotated() -> None:
    """`Annotated[int, Field(...)]` merges default/alias/factory like `=`."""

    class M(Model):
        a: Annotated[int, Field(5, alias="wireA")]
        b: Annotated[list[int], Field(default_factory=list)]

    assert M().a == 5
    assert M().b == []
    spec = SpecBuilder().record_spec(M)
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["alias"] == "wireA"
    assert set(fields["b"]["missing"]) == {"factory"}


def test_declared_fieldspec_alias_and_default() -> None:
    """`x = Field(alias=..)` at the declared position wires the alias;
    `Field(default)` without factory resolves the literal default."""

    class M(Model):
        a: int = Field(alias="wireA")
        b: int = Field(9)

    assert M(wireA=3).a == 3
    assert M(wireA=3).b == 9
    spec = SpecBuilder().record_spec(M)
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["alias"] == "wireA"
    assert fields["b"]["missing"] == {"literal": 9}


def test_annotated_default_marker_without_declared_default() -> None:
    """`x: Annotated[int, Default(5)]` — the node-level default fills `missing`."""

    class M(Model):
        x: Annotated[int, Default(5)]

    assert M().x == 5


def test_midtree_annotated_markers() -> None:
    """Markers nested inside a container reach `apply_markers`."""

    class M(Model):
        xs: list[Annotated[int, Alias("item"), Default(1), Description("d")]]

    spec = SpecBuilder().record_spec(M)
    item = spec["fields"][0]["type"]["item"]
    assert item["meta"]["alias"] == "item"
    assert item["meta"]["default"] == 1
    assert item["meta"]["description"] == "d"


def test_midtree_unknown_marker_is_an_error() -> None:
    with pytest.raises(AnnotationError, match="unsupported Annotated marker"):

        class M(Model):
            xs: list[Annotated[int, "junk"]]


def _shout(v: str) -> str:
    return v.upper()


def test_dataclass_field_markers() -> None:
    """Dataclass fields accept `Annotated` metadata — description, example,
    deprecated, validators, `Default`, and constraints. Dataclasses enter
    through `type_spec`, not `record_spec`."""

    @dataclasses.dataclass
    class D:
        a: Annotated[int, Description("the a"), Example(1), Deprecated(), Range(ge=0)]
        b: Annotated[str, AfterValidator(_shout), Default("x")]

    spec = SpecBuilder().type_spec(D)
    assert spec["flavor"] == "dataclass"
    fields = {f["name"]: f for f in spec["fields"]}
    assert fields["a"]["meta"]["description"] == "the a"
    assert fields["a"]["meta"]["deprecated"] is True
    assert fields["a"]["type"]["range"] == {"ge": 0}
    # The `Default` marker lands on the node — a dataclass default still
    # requires a real `field(default=...)` to change `missing`.
    assert fields["b"]["missing"] == "required"
    assert fields["b"]["type"]["meta"]["default"] == "x"
    assert fields["b"]["type"]["meta"]["after"]


def test_dataclass_field_unknown_marker_is_an_error() -> None:
    @dataclasses.dataclass
    class D:
        x: Annotated[int, 42]

    with pytest.raises(AnnotationError, match="unsupported Annotated marker"):
        SpecBuilder().type_spec(D)


def test_dataclass_missing_policies() -> None:
    """`init=False` and `default_factory` fields are `omit`; a plain
    default runs through `_missing`; no default is `required`."""

    @dataclasses.dataclass
    class D:
        req: int
        lazy: list[int] = dataclasses.field(default_factory=list)
        defaulted: int = 7
        derived: str = dataclasses.field(init=False, default="d")

    spec = SpecBuilder().type_spec(D)
    fields = {f["name"]: f["missing"] for f in spec["fields"]}
    assert fields == {
        "req": "required",
        "lazy": "omit",
        "defaulted": {"literal": 7},
        "derived": "omit",
    }


@dataclasses.dataclass
class _LinkedNode:
    next: "_LinkedNode | None"  # noqa: UP037 -- deliberate string forward reference


def test_recursive_dataclass_is_rejected() -> None:
    builder = SpecBuilder()
    with pytest.raises(AnnotationError, match="recursive dataclass"):
        builder.type_spec(_LinkedNode)


def test_fieldspec_unique_constraint() -> None:
    """`Field(unique=True)` lands in the field's constraint dict."""

    class M(Model):
        tags: list[str] = Field(unique=True)

    spec = SpecBuilder().record_spec(M)
    assert spec["fields"][0]["type"]["unique"] is True


def test_param_fieldspec_in_annotated() -> None:
    """`Field()` inside a param `Annotated` merges alias/default/factory."""
    info = p("x", Annotated[int, Query(), Field(alias="wire", default=3)])
    assert info.wire_name == "wire"
    assert info.default == 3
    info2 = p("y", Annotated[list[int], Query(), Field(default_factory=list)])
    assert info2.factory is list


def test_param_marker_unique() -> None:
    info = p("x", Annotated[list[int], Query(unique=True)])
    assert info.spec["unique"] is True


def test_compile_model_rename_overrides_spec() -> None:
    """`compile_model(cls, rename=…)` writes the wire-rename rule."""

    class M(Model):
        field_one: int

    _typing.compile_model(M, rename="kebab")
    assert M.__spec__["rename"] == "kebab"


def test_lazy_model_descriptor_compiles_on_first_use() -> None:
    """A model declared before its field type compiles lazily."""
    instance = _LazyOuter(inner=_LazyInner(v=7))
    assert instance.inner.v == 7


class _LazyOuter(Model):
    inner: "_LazyInner"  # noqa: UP037 -- deliberate string forward reference


class _LazyInner(Model):
    v: int


def test_unsupported_generic_alias_is_an_error() -> None:
    """`Callable[[int], int]` and friends hit `_alias_spec`'s fallthrough."""
    import collections.abc

    with pytest.raises(AnnotationError, match="unsupported annotation Callable"):
        SpecBuilder().type_spec(collections.abc.Callable[[int], int])
