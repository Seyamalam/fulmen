"""Litestar comparison app for the M1.25 end-to-end benchmarks.

Run: uvicorn benches.e2e.apps.litestar_app:app --workers N --no-access-log --port 8101
"""

from typing import Annotated

from litestar import Litestar, get, post
from litestar.enums import MediaType
from litestar.response import Response
from pydantic import BaseModel, Field


@get("/")
def index() -> Response:
    return Response(content="hello world", media_type=MediaType.TEXT)


@get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@get("/items/{item_id:int}")
def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}


class Geo(BaseModel):
    """Nested model — mirrors fulmen_app.Geo (M2.17)."""

    lat: float = Field(ge=-90, le=90)
    lon: float = Field(ge=-180, le=180)


class Order(BaseModel):
    """The ~10-field body record shared with the fulmen app (M2.17)."""

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


@post("/validate")
def validate(data: Order) -> dict[str, int]:
    return {"order_id": data.order_id}


app = Litestar(route_handlers=[index, json_route, get_item, validate])
