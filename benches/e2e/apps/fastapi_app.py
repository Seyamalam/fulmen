"""FastAPI comparison app for the M1.25 end-to-end benchmarks.

Run: uvicorn benches.e2e.apps.fastapi_app:app --workers N --no-access-log --port 8100
"""

import multiprocessing.sharedctypes
from typing import Annotated

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse
from pydantic import BaseModel, Field

app = FastAPI()

# Mirrors fulmen_app.CALLS; uvicorn's `--workers` uses spawn, so each
# worker has its own counter — `reject`'s delta still reads 0 (the
# endpoint never runs on a validation error), `validated`'s reads >0.
# `RawValue` keeps the increment cheap; racy updates are fine here.
CALLS = multiprocessing.sharedctypes.RawValue("i", 0)


@app.get("/")
def index() -> PlainTextResponse:
    return PlainTextResponse("hello world")


@app.get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}


class Geo(BaseModel):
    """Nested model — lat/lon bounds make `reject`'s lon=999 invalid."""

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


@app.post("/validate")
def validate(order: Order) -> dict[str, int]:
    CALLS.value += 1
    return {"order_id": order.order_id}


@app.get("/__counter")
def counter() -> dict[str, int]:
    return {"calls": CALLS.value}
