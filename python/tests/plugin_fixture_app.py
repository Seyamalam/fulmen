"""Fixture app for `fulmen <cmd>` plugin-dispatch tests (M3.8)."""

from __future__ import annotations

from collections.abc import Sequence

import fulmen


class _KeyTools:
    """Toy plugin contributing a CLI command."""

    name = "key-tools"
    version = "1.0"

    def setup(self, ctx: fulmen.PluginContext) -> None:
        @ctx.cli_command("rotate-keys")
        def rotate_keys(app: fulmen.Fulmen, argv: Sequence[str]) -> int:
            print(f"rotated:{','.join(argv)}")  # noqa: T201 -- the test asserts on the command's stdout
            return 0


app = fulmen.Fulmen(openapi_url=None)
app.use(_KeyTools())


@app.get("/x")
def x() -> str:
    return "x"
