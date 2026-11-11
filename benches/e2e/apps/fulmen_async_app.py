"""Fulmen `async def` comparison app for the M1.25 end-to-end benchmarks.

The same three endpoints as `fulmen_app.py`, declared `async def` so the
asyncio loop-thread path is measured against the sync pool (M1.27, M1.28 Q1).

Run: fulmen run benches.e2e.apps.fulmen_async_app:app --workers N --port 8105
"""

from fulmen import Fulmen

app = Fulmen()


@app.get("/")
async def index() -> str:
    return "hello world"


@app.get("/json")
async def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
async def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}
