"""Fixture app with two plugins registering the same CLI command —
`_plugin_cli_commands` reports the collision as problems."""

from __future__ import annotations

import fulmen


class _First:
    name = "first"
    version = "1.0"

    def setup(self, ctx: fulmen.PluginContext) -> None:
        ctx.cli_command("shared-cmd", lambda app, argv: 0)


class _Second:
    name = "second"
    version = "1.0"

    def setup(self, ctx: fulmen.PluginContext) -> None:
        ctx.cli_command("shared-cmd", lambda app, argv: 0)


app = fulmen.Fulmen(openapi_url=None)
app.use(_First())
app.use(_Second())


@app.get("/x")
def x() -> str:
    return "x"
