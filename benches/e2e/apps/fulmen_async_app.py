"""Fulmen `async def` comparison app for the M1.25 end-to-end benchmarks.

The same endpoints as `fulmen_app.py`, declared `async def` so the
asyncio loop-thread path is measured against the sync pool (M1.27, M1.28
Q1, M2.17's validated/reject scenarios).

Run: fulmen run benches.e2e.apps.fulmen_async_app:app --workers N --port 8105
"""

import multiprocessing.sharedctypes
from typing import Annotated

from fulmen import Field, Fulmen, Len, Model, Pattern, Range

app = Fulmen()

# See fulmen_app.CALLS — shared across forked workers; racy increments
# keep the measured path cheap (`reject` still reads exactly 0).
CALLS = multiprocessing.sharedctypes.RawValue("i", 0)


@app.get("/")
async def index() -> str:
    return "hello world"


@app.get("/json")
async def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
async def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}


class Geo(Model):
    """Nested record — lat/lon bounds make `reject`'s lon=999 invalid."""

    lat: Annotated[float, Range(ge=-90, le=90)]
    lon: Annotated[float, Range(ge=-180, le=180)]


class Order(Model):
    """The ~10-field body record shared with the fastapi app (M2.17)."""

    order_id: Annotated[int, Range(ge=1)]
    customer: Annotated[str, Len(min=1, max=50)]
    email: str
    age: Annotated[int, Range(ge=0, le=150)] = 18
    price: float = 0.0
    quantity: Annotated[int, Range(gt=0)] = 1
    sku: Annotated[str, Pattern("^[A-Z]+")] = "SKU"
    tags: list[str] = Field(default_factory=list)
    geo: Geo | None = None
    express: bool = False


@app.post("/validate")
async def validate(order: Order) -> dict[str, int]:
    CALLS.value += 1
    return {"order_id": order.order_id}


@app.get("/__counter")
async def counter() -> dict[str, int]:
    return {"calls": CALLS.value}
