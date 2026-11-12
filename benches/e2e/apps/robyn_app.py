"""Robyn comparison app for the M1.25 end-to-end benchmarks.

Run: python benches/e2e/apps/robyn_app.py --processes N
(robyn reads `--processes`/`--workers` from argv; the bind address comes
from the ROBYN_HOST/ROBYN_PORT environment variables — there is no
`--port` CLI flag in robyn 0.88).
"""

import json

from robyn import Response, Robyn

app = Robyn(__file__)


@app.get("/")
def index() -> Response:
    return Response(
        status_code=200,
        headers={"Content-Type": "text/plain"},
        description="hello world",
    )


@app.get("/json")
def json_route() -> dict[str, str]:
    return {"message": "hello world"}


@app.get("/items/:item_id")
def get_item(item_id: str) -> dict[str, int]:
    return {"item_id": int(item_id)}


@app.post("/validate")
def validate(request) -> dict[str, int]:
    # Robyn has no schema validation — parse + read (the `reject` column
    # honestly reports 200s: nothing rejects the invalid body).
    body = json.loads(request.body)
    return {"order_id": body["order_id"]}


if __name__ == "__main__":
    app.start()
