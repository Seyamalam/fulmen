"""Fulmen comparison app for the M1.25 end-to-end benchmarks.

Run: fulmen run benches.e2e.apps.fulmen_app:app --workers N --port 8104
"""

from fulmen import Fulmen

app = Fulmen()


@app.get("/")
def index() -> str:
    return "hello world"


@app.get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}
