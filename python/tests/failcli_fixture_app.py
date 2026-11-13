"""Fixture app whose plugin CLI command raises — dispatch reports exit 1."""

from __future__ import annotations

import fulmen


class _Boom:
    name = "boom"
    version = "1.0"

    def setup(self, ctx: fulmen.PluginContext) -> None:
        @ctx.cli_command("explode")
        def explode(app: fulmen.Fulmen, argv: object) -> int:
            raise RuntimeError("command blew up")


app = fulmen.Fulmen(openapi_url=None)
app.use(_Boom())


@app.get("/x")
def x() -> str:
    return "x"
