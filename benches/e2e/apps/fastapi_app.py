"""FastAPI comparison app for the M1.25 end-to-end benchmarks.

Run: uvicorn benches.e2e.apps.fastapi_app:app --workers N --no-access-log --port 8100
"""

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse

app = FastAPI()


@app.get("/")
def index() -> PlainTextResponse:
    return PlainTextResponse("hello world")


@app.get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/{item_id}")
def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}
