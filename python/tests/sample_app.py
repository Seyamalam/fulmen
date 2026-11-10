"""A minimal importable app for `fulmen run` tests (not a test file itself)."""

from fulmen import Fulmen

app = Fulmen()


@app.get("/ping")
def ping() -> str:
    return "pong"
