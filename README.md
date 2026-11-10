<img src="assets/icon.svg" width="64" height="64" alt="">

# Fulmen

A hyper-fast, type-safe Python backend framework with a Rust core.

You write typed Python handlers. Fulmen runs the server, routing, validation,
serialization and security checks in Rust, so most of the request never
touches the interpreter.

> **Status: pre-alpha.** The project skeleton exists; the framework does not
> yet. See the [roadmap](docs/ROADMAP.md).

## Goals

- Own Rust HTTP server (tokio + hyper), no ASGI layer
- `def` and `async def` handlers, Python 3.12+ including free-threaded builds
- Type hints as the contract: validation, OpenAPI and typed clients from one signature
- Secure by default: limits, timeouts, security headers, deny-by-default CORS
- Small core with plugins (features) and adapters (swappable infrastructure)
- CLI that checks, diagnoses and fixes projects

Read the [design](docs/DESIGN.md) for how it works.

## Development

Needs rustup, [uv](https://docs.astral.sh/uv/) and [just](https://just.systems).

```sh
just setup     # venv, tools, git hooks, first build
just ci        # the full local CI suite (also runs on every push)
```

CI/CD runs locally; there are no hosted workflows. See
[CONTRIBUTING.md](CONTRIBUTING.md) and [docs/GUARDRAILS.md](docs/GUARDRAILS.md).

## License

[Apache-2.0](LICENSE). The Fulmen name and logo are covered by the
[trademark policy](TRADEMARKS.md).
