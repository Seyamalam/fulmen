"""Static type tests for Fulmen's public API (ROADMAP M2.18).

Checked by `pyright` in strict mode under `just lint` (and therefore
`just ci`) — this file is not collected by pytest and never runs.

Mechanism: `typing.assert_type` fails the check when the inferred type
differs from the expected one, and `# pyright: ignore[...]` asserts a
usage is *rejected* — strict mode flags unused ignores, so a call the
checker suddenly accepts makes the ignore itself an error.

Note on source markers: `Query[T]`/`Path[T]`/… subscription sugar is a
*runtime* convenience — `Marker.__class_getitem__` returns
`Annotated[T, Marker()]` at runtime, but type checkers do not evaluate
`__class_getitem__` and read `q: Query[int]` as `q: Query`. For
statically-typed signatures use the `Annotated[T, Marker()]` form
checked below; both forms are accepted at runtime.
"""

from __future__ import annotations

from collections.abc import Callable
from decimal import Decimal
from typing import Annotated, Any, assert_type

from fulmen import (
    Accepted,
    Alias,
    Body,
    Cookie,
    Created,
    Description,
    Discriminator,
    Email,
    Field,
    File,
    FileResponse,
    Form,
    Fulmen,
    Header,
    HTMLResponse,
    HTTPError,
    JSONResponse,
    Len,
    Model,
    MultipleOf,
    NoContent,
    NotFound,
    Path,
    Pattern,
    Query,
    Range,
    Raw,
    Redirect,
    Request,
    Response,
    Stream,
    TextResponse,
    UploadFile,
    Url,
    ValidationError,
)
from fulmen._fulmen import (  # pyright: ignore[reportPrivateImportUsage] -- testing
    # the native surface directly; `Descriptor` is exported by `_fulmen`
    # but intentionally not re-exported at the package root.
    Descriptor,
    compile_descriptor,
)
from fulmen.errors import ErrorDetail

# ---- the app object and its decorators ------------------------------------

app = Fulmen()
assert_type(app, Fulmen)

# Every route decorator is `_Handler -> _Handler` preserving.
_Decorator = Callable[[Callable[..., object]], Callable[..., object]]
assert_type(app.get("/users/{user_id}"), _Decorator)
assert_type(app.post("/users"), _Decorator)
assert_type(app.put("/users/{user_id}"), _Decorator)
assert_type(app.patch("/users/{user_id}"), _Decorator)
assert_type(app.delete("/users/{user_id}"), _Decorator)
assert_type(app.head("/users"), _Decorator)
assert_type(app.options("/users"), _Decorator)
assert_type(app.route("/multi", methods=("GET", "POST")), _Decorator)
assert_type(app.on_error(NotFound), _Decorator)

assert_type(app.run(host="127.0.0.1", port=8000), None)


# ---- models: fields, __init__ inference, generics -------------------------


class User(Model):
    name: str
    age: int = 0
    email: Email | None = None


assert_type(User(name="ada"), User)
assert_type(User(name="ada", age=3, email=None), User)

_user = User(name="ada")
assert_type(_user.name, str)
assert_type(_user.age, int)
assert_type(_user.email, Email | None)
assert_type(_user.model_dump(), dict[str, Any])
assert_type(User.model_validate({"name": "ada"}), User)

# `dataclass_transform(kw_only_default=True)`: positional construction and
# undeclared fields are rejected (the ignores are exercised, not dead).
User("ada")  # pyright: ignore[reportCallIssue]
User(name="ada", nope=1)  # pyright: ignore[reportCallIssue]


class Product(Model, extra="forbid", rename="camel"):
    sku: str = Field(min_length=3, max_length=8)
    price: float = Field(ge=0.0)
    tags: list[str] = Field(default_factory=list)
    wire_name: str = Field(default="sku", alias="sku_wire")


assert_type(Product(sku="abc", price=1.5), Product)
assert_type(Product(sku="abc", price=1.5, tags=["a"], wire_name="w"), Product)
assert_type(Product(price=9.99), Product)  # every Field() carries a default
_product = Product(sku="abc", price=1.5)
assert_type(_product.tags, list[str])


class Page[T](Model, extra="forbid"):
    items: list[T]
    total: int


_page = Page[int](items=[1, 2], total=2)
assert_type(_page, Page[int])
assert_type(_page.items, list[int])
assert_type(_page.total, int)
assert_type(Page[str].model_validate({"items": [], "total": 0}), Page[str])


# ---- source markers: `Annotated[T, Marker()]` reads as `T` -----------------


@app.get("/users/{user_id}")
def read_user(
    user_id: Annotated[int, Path()],
    q: Annotated[list[str], Query()],
    page: Annotated[int, Query()] = 1,
    verbose: Annotated[bool, Query()] = False,
    agent: Annotated[str | None, Header()] = None,
) -> JSONResponse[User]:
    assert_type(user_id, int)
    assert_type(q, list[str])
    assert_type(page, int)
    assert_type(verbose, bool)
    assert_type(agent, str | None)
    return JSONResponse[User]()


@app.post("/users")
def create_user(
    body: Annotated[User, Body()],
    token: Annotated[str, Header(alias="X-Api-Key")],
    session: Annotated[str, Cookie()] = "anon",
    form: Annotated[User, Form()] | None = None,
    upload: Annotated[UploadFile, File()] | None = None,
    raw: Raw | None = None,
    request: Request | None = None,
) -> Created[User]:
    assert_type(body, User)
    assert_type(token, str)
    assert_type(session, str)
    assert_type(form, User | None)
    assert_type(upload, UploadFile | None)
    assert_type(raw, Raw | None)
    assert_type(request, Request | None)
    return Created[User](body)


# A decorated handler is still a callable `_Handler`.
assert_type(read_user, Callable[..., object])

# Async handlers work identically.


@app.get("/ping")
async def ping() -> TextResponse:
    return TextResponse("pong")


@app.on_error(NotFound)
def on_not_found(err: NotFound[Any]) -> Response:
    assert_type(err, NotFound[Any])
    return Response(status=404)


# The marker classes themselves are typed — construction, defaults and
# the `source` class variable.
assert_type(Query, type[Query])
assert_type(Query(default=5), Query)
assert_type(Query(default=5).source, str)
assert_type(Header(alias="X-Api-Key"), Header)
assert_type(Header(alias="X-Api-Key").alias, str | None)
assert_type(Path(), Path)
assert_type(Cookie(), Cookie)
assert_type(Body(), Body)
assert_type(Form(), Form)
assert_type(File(), File)


# ---- constraint markers inside `Annotated` --------------------------------


class Constrained(Model):
    name: Annotated[str, Len(1, 8), Pattern("^[a-z]+$")]
    n: Annotated[int, Range(ge=0, lt=10), MultipleOf(2)]
    label: Annotated[str, Alias("lbl"), Description("the label")]
    kind: Annotated[str, Discriminator("kind")]


assert_type(Constrained(name="a", n=2, label="x", kind="k"), Constrained)
assert_type(Len(1, 8).min, int | None)
assert_type(Range(ge=0).ge, int | float | Decimal | None)
assert_type(Pattern("^[a-z]+$").pattern, str)
assert_type(MultipleOf(2).value, int | float | Decimal)
assert_type(Alias("lbl").name, str)
assert_type(Discriminator("kind").field, str)


# ---- response classes ------------------------------------------------------

assert_type(JSONResponse[User](_user), JSONResponse[User])
assert_type(TextResponse("hi"), TextResponse)
assert_type(HTMLResponse("<p>x</p>"), HTMLResponse)
assert_type(Created[User](_user), Created[User])
assert_type(Accepted[User](_user), Accepted[User])
assert_type(NoContent(), NoContent)
assert_type(Redirect("/new", status=308), Redirect)
assert_type(Response(body={"ok": True}, status=201), Response)
assert_type(Response(), Response)

# Subclasses are assignable to the base `Response` — handlers may return
# any of them interchangeably.
_response: Response = Created[User](_user)
_responses: list[Response] = [
    JSONResponse[User](_user),
    TextResponse("x"),
    NoContent(),
    Redirect("/new"),
]

# Covariance in the payload parameter: `User <: Model` implies
# `JSONResponse[User] <: JSONResponse[Model]`.
_json_model: JSONResponse[Model] = JSONResponse[User](_user)
_created_model: Created[Model] = Created[User](_user)


# ---- errors ----------------------------------------------------------------

_err = NotFound[Any](detail="no user 7")
assert_type(_err.status, int)
assert_type(_err.detail, str | None)
assert_type(_err.reason, str)
assert_type(_err.problem(), dict[str, Any])
assert_type(_err.problem("/req/1"), dict[str, Any])
assert_type(_err.headers, dict[str, str])
assert_type(HTTPError[Any](418, "short and stout").status, int)
assert_type(NotFound[User](detail="x"), NotFound[User])


def raise_http() -> None:
    raise NotFound[Any](detail="gone")


# `ValidationError.entries` is the typed problem list the native side
# fills in.
try:
    _user.model_dump()
except ValidationError as _ve:
    assert_type(_ve.entries, list[ErrorDetail])
    assert_type(_ve.truncated, bool)


# ---- marker/value types ----------------------------------------------------

assert_type(Email("a@b.co"), Email)
assert_type(Url("https://x.io"), Url)
assert_type(UploadFile(b"x", filename="f.bin"), UploadFile)
assert_type(UploadFile(b"x").content, bytes)
_email = Email("a@b.co")
assert_type(_email + "x", str)  # `Email`/`Url` behave as `str`
_raw: Raw | None = None


# ---- the native `Descriptor` (exported by `fulmen._fulmen`) ----------------

_desc: Descriptor = compile_descriptor({"kind": "int"})
assert_type(_desc.spec, dict[str, Any])
assert_type(_desc.field_names, list[str])
assert_type(_desc.record_class, type[Any] | None)
assert_type(_desc.describe(), dict[str, Any])
assert_type(_desc.validate({"a": 1}), Any)
assert_type(_desc.validate_field("a", 1, lax=True), Any)
assert_type(_desc.validate({"a": 1}, source="query", lax=True), Any)


# ---- Stream / FileResponse bodies ------------------------------------------

assert_type(Stream(iter([b"a", b"b"])), Stream)
assert_type(Stream([b"a", "b"]), Stream)
assert_type(FileResponse("report.pdf"), FileResponse)
