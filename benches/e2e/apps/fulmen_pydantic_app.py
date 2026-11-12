"""Fulmen + pydantic adapter app for the M2.17 end-to-end benchmarks.

Identical routes to `fulmen_app.py` but the body model is a pydantic
`BaseModel` — the adapter translates it to a native record spec, so the
`validated`/`reject` scenarios measure pydantic-class ergonomics at
native-descriptor speed (no pydantic-core on the request path).

Run: fulmen run benches.e2e.apps.fulmen_pydantic_app:app --workers N --port 8107
"""

import multiprocessing.sharedctypes
from typing import Annotated

from fulmen import Fulmen
from pydantic import BaseModel, Field

app = Fulmen()

# See fulmen_app.CALLS — shared across forked workers; racy increments
# keep the measured path cheap (`reject` still reads exactly 0).
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


class Geo(BaseModel):
    """Nested pydantic model — mirrors fulmen_app.Geo (M2.17)."""

    lat: float = Field(ge=-90, le=90)
    lon: float = Field(ge=-180, le=180)


class Order(BaseModel):
    """The ~10-field body record shared with the fastapi app (M2.17)."""

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


@app.post("/validate")
def validate(order: Order) -> dict[str, int]:
    CALLS.value += 1
    return {"order_id": order.order_id}


@app.get("/__counter")
def counter() -> dict[str, int]:
    return {"calls": CALLS.value}
