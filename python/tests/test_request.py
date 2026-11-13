"""Unit tests for `Headers` and `Request` (DESIGN.md §10.1, §8).

The dispatch path builds these on executor threads, which coverage
cannot trace — exercising them directly pins the wire-snapshot
semantics: case-insensitive merged headers, `json()` decoding and the
`state` context dict hooks write into.
"""

from __future__ import annotations

import pytest
from fulmen.request import Headers, Request


def _request(**overrides: object) -> Request:
    kwargs: dict[str, object] = {
        "method": "POST",
        "path": "/users",
        "query_string": "a=1&a=2",
        "headers": Headers([("Content-Type", "application/json")]),
        "body": b'{"name": "fulmen"}',
        "remote_addr": "127.0.0.1:4321",
    }
    kwargs.update(overrides)
    return Request(**kwargs)  # type: ignore[arg-type]


# -- Headers ---------------------------------------------------------------


def test_headers_lookup_is_case_insensitive() -> None:
    headers = Headers([("Content-Type", "application/json"), ("X-TOKEN", "abc")])
    assert headers["content-type"] == "application/json"
    assert headers["Content-Type"] == "application/json"
    assert headers["x-token"] == "abc"
    assert "missing" not in headers
    with pytest.raises(KeyError):
        headers["missing"]


def test_headers_duplicate_names_join_with_comma() -> None:
    """RFC 9110 list semantics: repeated header names merge with `, `."""
    headers = Headers([("X-A", "1"), ("x-a", "2"), ("B", "3")])
    assert headers["x-a"] == "1, 2"
    assert len(headers) == 2
    assert set(headers) == {"x-a", "b"}
    assert dict(headers.items()) == {"x-a": "1, 2", "b": "3"}


def test_headers_repr_shows_the_merged_map() -> None:
    headers = Headers([("A", "1")])
    assert repr(headers) == "Headers({'a': '1'})"


# -- Request ---------------------------------------------------------------


def test_request_from_wire_builds_the_snapshot() -> None:
    """`_from_wire` is the Rust bridge's constructor — same inputs."""
    request = Request._from_wire(
        "GET",
        "/items",
        "page=2",
        [("Accept", "text/html"), ("accept", "application/json")],
        b"",
        "10.0.0.1:80",
    )
    assert request.method == "GET"
    assert request.path == "/items"
    assert request.query_string == "page=2"
    assert request.remote_addr == "10.0.0.1:80"
    assert request.body == b""
    assert request.headers["accept"] == "text/html, application/json"
    assert request.state == {}


def test_request_json_decodes_the_body() -> None:
    assert _request().json() == {"name": "fulmen"}
    assert _request(body=b"[1, 2]").json() == [1, 2]
    assert _request(body=b'"s"').json() == "s"


def test_request_json_raises_on_bad_input() -> None:
    import json

    with pytest.raises(json.JSONDecodeError):
        _request(body=b"not json").json()


def test_request_repr() -> None:
    assert repr(_request(method="GET", path="/p")) == "Request(GET /p)"


def test_request_state_is_a_shared_mutable_dict() -> None:
    """Hooks communicate through `state` — writes are visible to later phases."""
    request = _request()
    request.state["user"] = "anne"
    assert request.state == {"user": "anne"}
