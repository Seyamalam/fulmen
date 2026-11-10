# Guardrails

Fulmen has two kinds of guardrails:

- **Project guardrails** keep the codebase fast, safe and consistent. They run
  locally through `just` and git hooks. There is no hosted CI.
- **Runtime guardrails** protect people who build apps with Fulmen. They are
  framework features, specified in [DESIGN.md §14](DESIGN.md#14-runtime-guardrails).

This document covers project guardrails.

## Setup

```sh
just setup     # venv, dev tools, cargo-deny, typos, cargo-llvm-cov, git hooks, first build
```

`just hooks` installs the hooks as symlinks in `.git/hooks`. It does not change
git config.

## Gates

| When                 | Command           | What runs                                                                 |
| -------------------- | ----------------- | ------------------------------------------------------------------------- |
| Every commit         | `just pre-commit` | rustfmt and ruff format check, typos, `scripts/guardrails.py`             |
| Every commit message | `commit-msg` hook | Conventional Commits header, DCO `Signed-off-by`                          |
| Every push           | `just ci`         | everything above, plus clippy, ruff lint, pyright strict, cargo-deny, Rust tests, Python tests with coverage, Python 3.12/3.13/3.14/3.14t matrix |
| Every release        | `just release`    | clean `main` in sync with origin, changelog entry, `just ci`, build, wheel smoke tests, confirmation prompt |

Skipping a hook with `--no-verify` is allowed for work-in-progress on a local
branch. Nothing may be pushed to `main` without `just ci` passing.

## Rust rules

Configured in the root `Cargo.toml` (`[workspace.lints]`), `clippy.toml`,
`rust-toolchain.toml` and `deny.toml`.

- **Toolchain is pinned** (`rust-toolchain.toml`). MSRV is 1.85 (edition 2024).
- **No panics on the request path.** `unwrap`, `panic!`, `todo!`,
  `unimplemented!` are errors outside tests. `expect` is a warning, and CI
  treats warnings as errors, so use it only with a message that states the
  invariant, inside `#[expect(clippy::expect_used, reason = "...")]`.
- **No printing.** `print!`, `eprint!` and `dbg!` are errors. Use `tracing`.
- **No `unsafe` by default.** `unsafe_code` is denied for every crate. A crate
  that needs it must be added to `UNSAFE_ALLOWED` in `scripts/guardrails.py`
  and documented in DESIGN.md. Every `unsafe` block needs a `// SAFETY:`
  comment, one unsafe operation per block.
- **Every `allow` needs a reason.** Bare `#[allow(...)]` is rejected; write
  `#[expect(lint, reason = "...")]`.
- **No lock held across `.await`.**
- **Pedantic clippy** is on, minus two noisy lints.
- **Release profile:** fat LTO, one codegen unit, `panic = "unwind"` so a
  panic becomes a 500 instead of killing the worker (DESIGN.md §14.6).

## Dependency rules (`deny.toml`, `scripts/guardrails.py`)

- Every dependency is declared once in the root `[workspace.dependencies]` and
  used with `workspace = true`, so versions cannot drift between crates.
- Only crates.io. No git dependencies, no other registries.
- Licenses: Apache-2.0, MIT, BSD, ISC, Zlib, Unicode-3.0, CC0. Anything
  copyleft is rejected so Fulmen stays Apache-2.0 compatible.
- Known vulnerabilities and yanked crates fail the build.
- OpenSSL and native-tls are banned; TLS is rustls.
- Wildcard versions are banned.
- A new dependency in a hot-path crate needs a sentence in the PR on why it
  is worth its compile time and binary size.

## Architecture rules (`scripts/guardrails.py`)

- Only `fulmen-py` depends on `pyo3`.
- Every crate inherits version, edition, license, MSRV and lints from the workspace.
- Every `#[pyfunction]` and `#[pyclass]` must appear in `python/fulmen/_fulmen.pyi`.
- `requires-python` stays `>=3.12`.
- No `.github/workflows`: CI/CD is local.

## Python rules (`pyproject.toml`)

- `pyright --strict` on `python/fulmen` and `scripts`.
- Ruff with security (`S`), no `print` outside the CLI (`T20`), no
  `breakpoint()` (`T10`), no blind `except Exception` (`BLE`), no blocking
  calls in async code (`ASYNC`), plus pyflakes, bugbear, pyupgrade, naming,
  performance and pytest style.
- Coverage must stay at or above `fail_under` in `pyproject.toml`. The number
  only goes up.

## Repository hygiene (`scripts/guardrails.py`, `typos`)

- No file over 512 KiB (except `Cargo.lock`).
- Secret scanner for private keys, AWS/GitHub/PyPI/Slack tokens and hard-coded
  `password = "..."` style assignments.
- Required files: LICENSE, NOTICE, TRADEMARKS.md, CONTRIBUTING.md,
  SECURITY.md, CHANGELOG.md, docs/DESIGN.md, docs/ROADMAP.md, this file.
- Spell check on code and docs.

## Commit rules (`scripts/check_commits.py`)

- Header: `type(scope): summary`, lowercase start, no trailing period, ≤ 72 chars.
- Types: build, chore, ci, docs, feat, fix, perf, refactor, revert, security, style, test.
- Scopes: core, server, schema, security, py, cli, plugins, adapters, docs, deps, release, repo.
- Every commit has a DCO `Signed-off-by:` line (`git commit -s`).

## Performance rules

Added in M1, once the benchmark harness exists (see ROADMAP.md):

- `just bench` runs criterion micro-benchmarks and end-to-end `oha` scenarios.
- Results are stored in `benches/baseline.json`. A change that makes any
  tracked scenario more than 5% slower fails `just bench-check` unless the
  baseline is updated in the same commit with a reason.

## Changing a guardrail

Guardrails are code. Change them in their own commit with type `ci` or
`chore(repo)`, and say in the message why the rule was wrong or too strict.
Loosening a security-related rule also needs an entry in CHANGELOG.md.
