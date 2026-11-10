# Contributing to Fulmen

Thanks for your interest. Fulmen is maintained by
[@Seyamalam](https://github.com/Seyamalam). Before writing a large change,
open an issue or discussion so we can agree on the approach. Pull requests
that arrive without prior discussion and change public API or architecture
will likely be closed.

Read [docs/DESIGN.md](docs/DESIGN.md) first. It explains why the code is split
the way it is.

## Development setup

Requirements: [rustup](https://rustup.rs) (the toolchain is pinned in
`rust-toolchain.toml` and installed automatically), [uv](https://docs.astral.sh/uv/)
and [just](https://just.systems). Docker is optional (Linux wheels, later
integration tests).

```sh
just setup      # venv, dev tools, cargo-deny, typos, git hooks, first build
just develop    # rebuild the extension after changing Rust code
just test-py    # Python tests
```

Use `just develop -r` for benchmarks; debug builds are not representative.

## CI is local

There is no hosted CI. The git hooks installed by `just setup` are the CI:

- **pre-commit:** format check, spell check, repository guardrails.
- **commit-msg:** Conventional Commits and DCO sign-off.
- **pre-push:** `just ci`, the full suite, including all four Python
  interpreters (3.12, 3.13, 3.14, 3.14t).

Run `just ci` yourself before opening a PR and paste the last line of its
output in the PR. Every rule and why it exists is in
[docs/GUARDRAILS.md](docs/GUARDRAILS.md).

## Ground rules for code

- **Only `crates/fulmen-py` may depend on `pyo3`.** Every other crate is plain
  Rust so it can be tested and benchmarked without an interpreter.
- **Hot path changes need numbers.** A PR that touches routing, parsing,
  validation, serialization or the server must include before/after results
  from the benchmark suite (`just bench`, from roadmap task M1.26).
- **Never hold the GIL while doing I/O or waiting.** Release it with
  `Python::detach` around anything that can block.
- **No panics or prints on the request path**, and **no `unsafe`** outside
  crates approved in `scripts/guardrails.py`. Lints enforce both.
- **Work on the current milestone** in [docs/ROADMAP.md](docs/ROADMAP.md) and
  reference the task ID (`Refs: M1.3`) in the commit body.
- **Public Python API must be fully typed** and pass `pyright --strict`.
- Secure defaults are not optional. A change must not weaken a default
  (limits, headers, CORS policy) without a documented reason.

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org/):
  `feat(core): ...`, `fix(schema): ...`, `perf(server): ...`, `docs: ...`.
  Allowed types and scopes are listed in docs/GUARDRAILS.md; the commit-msg
  hook enforces them.
- Keep a PR to one logical change. Add tests for behaviour changes.
- Fill in the pull request template.

## Developer Certificate of Origin (DCO)

Every commit must be signed off to certify that you wrote it or have the right
to submit it under the project's license, as described in the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).

```sh
git commit -s -m "feat(core): add catch-all route segments"
```

This appends `Signed-off-by: Your Name <you@example.com>` to the commit. PRs
with unsigned commits cannot be merged. Your contribution is licensed under
Apache-2.0, and you keep the copyright to your contribution.

## Plugins and adapters

You do not need to contribute to this repository to extend Fulmen. Publish a
plugin or adapter as your own package (see [TRADEMARKS.md](TRADEMARKS.md) for
naming). Built-in plugins use the same public API as third-party ones.

## Security issues

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

## Conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md).
