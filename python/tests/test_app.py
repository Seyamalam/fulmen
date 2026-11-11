"""M1.13-M1.22 integration tests: real server, stdlib urllib clients."""

from __future__ import annotations

import logging
from collections.abc import Callable

import pytest
from conftest import fetch
from fulmen import Fulmen, Response, StartupError
from fulmen._fulmen import serve_for_test


def test_text_response(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/hello")
    def hello() -> str:
        return "hi"

    res = fetch(f"{serve(app)}/hello")
    assert res.status == 200
    assert res.body == b"hi"
    assert res.header("Content-Type") == "text/plain; charset=utf-8"


def test_json_response(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/data")
    def data() -> dict[str, object]:
        return {"a": [1, 2.5], "b": {"c": None, "d": True}, "e": "x\ny"}

    res = fetch(f"{serve(app)}/data")
    assert res.status == 200
    assert res.header("Content-Type") == "application/json"
    assert res.body == b'{"a":[1,2.5],"b":{"c":null,"d":true},"e":"x\\ny"}'


def test_bytes_response(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/blob")
    def blob() -> bytes:
        return b"\x00\x01\xfe"

    res = fetch(f"{serve(app)}/blob")
    assert res.status == 200
    assert res.body == b"\x00\x01\xfe"
    assert res.header("Content-Type") == "application/octet-stream"


def test_explicit_response(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/created")
    def created() -> Response:
        return Response({"id": 7}, status=201, headers={"X-Kind": "thing"})

    res = fetch(f"{serve(app)}/created")
    assert res.status == 201
    assert res.body == b'{"id":7}'
    assert res.header("Content-Type") == "application/json"
    assert res.header("X-Kind") == "thing"


def test_int_path_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/users/{user_id}")
    def user(user_id: int) -> str:
        assert isinstance(user_id, int)
        return f"user:{user_id}"

    res = fetch(f"{serve(app)}/users/41")
    assert res.status == 200
    assert res.body == b"user:41"


def test_int_path_param_mismatch_is_404(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/users/{user_id}")
    def user(user_id: int) -> str:
        return f"user:{user_id}"

    res = fetch(f"{serve(app)}/users/abc")
    assert res.status == 404


def test_str_path_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/items/{name}")
    def item(name: str) -> str:
        return f"item:{name}"

    res = fetch(f"{serve(app)}/items/widget")
    assert res.status == 200
    assert res.body == b"item:widget"


def test_route_decorator_multiple_methods(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.route("/echo", methods=["GET", "POST"])
    def echo() -> str:
        return "echo"

    base = serve(app)
    assert fetch(f"{base}/echo").status == 200
    assert fetch(f"{base}/echo", method="POST").status == 200


def test_405_with_allow(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/only-get")
    def only_get() -> str:
        return "ok"

    res = fetch(f"{serve(app)}/only-get", method="POST", data=b"x")
    assert res.status == 405
    allow = {m.strip() for m in res.header("Allow").split(",")}
    assert {"GET", "HEAD", "OPTIONS"} <= allow


def test_404(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/exists")
    def exists() -> str:
        return "ok"

    res = fetch(f"{serve(app)}/missing")
    assert res.status == 404


def test_implicit_head(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/doc")
    def doc() -> str:
        return "document body"

    base = serve(app)
    get = fetch(f"{base}/doc")
    head = fetch(f"{base}/doc", method="HEAD")
    assert head.status == get.status == 200
    assert head.body == b""
    for header in ("Content-Type", "Content-Length"):
        assert head.header(header) == get.header(header)


def test_handler_exception_is_500_without_traceback(
    serve: Callable[[Fulmen], str], caplog: pytest.LogCaptureFixture
) -> None:
    app = Fulmen()

    @app.get("/boom")
    def boom() -> str:
        raise RuntimeError("secret internals xyzzy")

    with caplog.at_level(logging.ERROR, logger="fulmen"):
        res = fetch(f"{serve(app)}/boom")
    assert res.status == 500
    assert b"secret internals xyzzy" not in res.body
    assert b"Traceback" not in res.body
    assert "boom" in caplog.text
    assert "RuntimeError" in caplog.text


def test_duplicate_route_is_startup_error() -> None:
    app = Fulmen()

    @app.get("/dup")
    def first() -> str:
        return "one"

    @app.get("/dup")
    def second() -> str:
        return "two"

    # Duplicate detection lives in the core compiler, which runs inside
    # `serve`/`serve_for_test`.
    with pytest.raises(StartupError, match=r"[Dd]uplicate"):
        serve_for_test(app._compile_native())


def test_bad_param_declaration_is_startup_error() -> None:
    app = Fulmen()

    @app.get("/users/{user_id}")
    def user(user_id: float) -> str:
        return str(user_id)

    with pytest.raises(StartupError, match=r"int.*or.*str|path parameter"):
        app._compile_native()


def test_unknown_handler_param_is_startup_error() -> None:
    app = Fulmen()

    @app.get("/users/{user_id}")
    def user(user_id: int, limit: int) -> str:
        return f"{user_id}:{limit}"

    with pytest.raises(StartupError, match=r"limit.*not a path parameter"):
        app._compile_native()


def test_keyword_only_path_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/kw/{item_id}")
    def kw(*, item_id: int) -> str:
        return f"kw:{item_id}"

    res = fetch(f"{serve(app)}/kw/3")
    assert res.status == 200
    assert res.body == b"kw:3"


def test_catch_all_str_param(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/files/{*path}")
    def files(path: str) -> str:
        return f"file:{path}"

    res = fetch(f"{serve(app)}/files/a/b/c.txt")
    assert res.status == 200
    assert res.body == b"file:a/b/c.txt"


def test_unsupported_return_type_is_500(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.get("/weird")
    def weird() -> object:
        return object()

    res = fetch(f"{serve(app)}/weird")
    assert res.status == 500


def test_compile_native_is_idempotent(serve: Callable[[Fulmen], str]) -> None:
    # `_compile_native` builds a fresh NativeApp per call: a compile followed
    # by another compile still leaves a servable app.
    app = Fulmen()

    @app.get("/again")
    def again() -> str:
        return "again"

    app._compile_native()
    assert fetch(f"{serve(app)}/again").status == 200


def test_all_method_decorators(serve: Callable[[Fulmen], str]) -> None:
    app = Fulmen()

    @app.post("/m")
    def post() -> str:
        return "post"

    @app.put("/m")
    def put() -> str:
        return "put"

    @app.patch("/m")
    def patch() -> str:
        return "patch"

    @app.delete("/m")
    def delete() -> str:
        return "delete"

    @app.head("/m")
    def head() -> str:
        return "head"

    @app.options("/m")
    def options() -> str:
        return "options"

    base = serve(app)
    assert fetch(f"{base}/m", method="POST").body == b"post"
    assert fetch(f"{base}/m", method="PUT").body == b"put"
    assert fetch(f"{base}/m", method="PATCH").body == b"patch"
    assert fetch(f"{base}/m", method="DELETE").body == b"delete"
    assert fetch(f"{base}/m", method="HEAD").status == 200
    assert fetch(f"{base}/m", method="OPTIONS").body == b"options"


def test_variadic_param_is_startup_error() -> None:
    app = Fulmen()

    @app.get("/v")
    def v(*args: str) -> str:
        return ",".join(args)

    with pytest.raises(StartupError, match=r"variadic"):
        app._compile_native()


def test_unbound_path_param_is_startup_error() -> None:
    app = Fulmen()

    @app.get("/u/{x}")
    def u() -> str:
        return "u"

    with pytest.raises(StartupError, match=r"not bound"):
        app._compile_native()


def test_non_callable_handler_is_startup_error() -> None:
    app = Fulmen()
    app.get("/nc")(42)  # type: ignore[arg-type]

    with pytest.raises(StartupError, match=r"cannot inspect"):
        app._compile_native()


def test_unresolvable_hints_are_startup_error() -> None:
    app = Fulmen()

    @app.get("/h/{x}")
    def h(x: does_not_exist) -> str:  # noqa: F821 -- deliberately unresolved forward ref
        return str(x)

    with pytest.raises(StartupError, match=r"cannot resolve type hints"):
        app._compile_native()


def test_response_repr() -> None:
    assert "status=418" in repr(Response("x", status=418))


def test_run_calls_native_serve(monkeypatch: pytest.MonkeyPatch) -> None:
    app = Fulmen()

    @app.get("/r")
    def r() -> str:
        return "r"

    called: dict[str, object] = {}
    monkeypatch.setattr(
        "fulmen.app._serve",
        lambda native, host, port, **kwargs: called.update(host=host, port=port, **kwargs),
    )
    app.run(host="127.0.0.1", port=9000, workers=1, debug=True, uvloop=False)
    assert called == {
        "host": "127.0.0.1",
        "port": 9000,
        "debug": True,
        "uvloop": False,
        "gil_batch_size": 32,
    }


def test_run_workers_dispatches_to_supervisor(monkeypatch: pytest.MonkeyPatch) -> None:
    app = Fulmen()
    called: dict[str, object] = {}
    monkeypatch.setattr(
        "fulmen.workers.run_workers",
        lambda app, **kwargs: called.update(kwargs),
    )
    app.run(host="127.0.0.1", port=9000, workers=4, uvloop=True, gil_batch_size=8)
    assert called == {
        "host": "127.0.0.1",
        "port": 9000,
        "workers": 4,
        "debug": False,
        "uvloop": True,
        "gil_batch_size": 8,
    }


def test_run_workers_rejects_zero() -> None:
    app = Fulmen()
    with pytest.raises(ValueError, match="workers must be >= 1"):
        app.run(workers=0)
