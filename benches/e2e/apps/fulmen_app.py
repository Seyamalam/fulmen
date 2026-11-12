"""Fulmen comparison app for the M1.25 end-to-end benchmarks.

Run: fulmen run benches.e2e.apps.fulmen_app:app --workers N --port 8104
"""

import multiprocessing.sharedctypes
from typing import Annotated

from fulmen import Field, Fulmen, Len, Model, Pattern, Range

app = Fulmen()

# Handler invocations across forked workers (module level, so the shared
# memory survives `fork`). `RawValue` keeps the increment cheap on the
# measured path — a few lost updates under contention are fine; `reject`
# still reads exactly 0 and `validated` stays in the hundreds of thousands.
CALLS = multiprocessing.sharedctypes.RawValue("i", 0)


@app.get("/")
def index() -> str:
    return "hello world"


@app.get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
def get_item(item_id: int) -> dict[str, int]:
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
def validate(order: Order) -> dict[str, int]:
    CALLS.value += 1
    return {"order_id": order.order_id}


@app.get("/__counter")
def counter() -> dict[str, int]:
    return {"calls": CALLS.value}
