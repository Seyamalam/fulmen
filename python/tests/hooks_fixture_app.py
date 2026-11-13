"""Fixture app for `fulmen routes --hooks` (M3.4): one hooked route,
one bare route, one `on_error` handler."""

from __future__ import annotations

import fulmen

app = fulmen.Fulmen(openapi_url=None)


@app.guard
def allow_all(request: fulmen.Request) -> None:
    return None


@app.on_error(ValueError)
def handle_value_error(request: fulmen.Request, exc: ValueError) -> fulmen.Response:
    return fulmen.Response("handled", status=400)


@app.get("/hooked")
def hooked() -> str:
    return "x"


@app.get("/bare")
def bare() -> str:
    return "y"
