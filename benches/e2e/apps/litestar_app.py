"""Litestar comparison app for the M1.25 end-to-end benchmarks.

Run: uvicorn benches.e2e.apps.litestar_app:app --workers N --no-access-log --port 8101
"""

from litestar import Litestar, get
from litestar.enums import MediaType
from litestar.response import Response


@get("/")
def index() -> Response:
    return Response(content="hello world", media_type=MediaType.TEXT)


@get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@get("/items/{item_id:int}")
def get_item(item_id: int) -> dict[str, int]:
    return {"item_id": item_id}


app = Litestar(route_handlers=[index, json_route, get_item])
