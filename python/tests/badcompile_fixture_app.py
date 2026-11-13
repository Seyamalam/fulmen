"""Fixture app that fails to compile — a plugin whose `setup` raises."""

from __future__ import annotations

import fulmen


class _Broken:
    name = "broken"
    version = "1.0"

    def setup(self, ctx: fulmen.PluginContext) -> None:
        raise RuntimeError("setup exploded")


app = fulmen.Fulmen(openapi_url=None)
app.use(_Broken())


@app.get("/x")
def x() -> str:
    return "x"
