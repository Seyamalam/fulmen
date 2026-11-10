# Fulmen task runner. All CI/CD runs locally through these recipes.
# `just` lists them. `just ci` is the merge gate; the pre-push hook runs it.

set shell := ["bash", "-euo", "pipefail", "-c"]

venv := ".venv"
py := venv / "bin" / "python"
bin := venv / "bin"

default:
    @just --list

# One-time setup: venv, dev tools, cargo tools, git hooks, first build.
setup:
    uv venv -p 3.12 {{venv}}
    uv pip install -p {{venv}} maturin pytest pytest-cov ruff pyright
    @command -v cargo-deny >/dev/null || { echo "installing cargo-deny"; cargo install --locked cargo-deny; }
    @command -v typos >/dev/null || { echo "installing typos"; cargo install --locked typos-cli; }
    @command -v cargo-llvm-cov >/dev/null || { echo "installing cargo-llvm-cov"; cargo install --locked cargo-llvm-cov; }
    just hooks
    just develop

# Install git hooks by symlinking into .git/hooks (does not touch git config).
hooks:
    @for h in pre-commit commit-msg pre-push; do \
        chmod +x scripts/hooks/$h; \
        ln -sf ../../scripts/hooks/$h .git/hooks/$h; \
        echo "installed .git/hooks/$h"; \
    done

# Build the extension into the venv (debug). Use `just develop -r` for release.
develop *args:
    {{bin}}/maturin develop -q {{args}}

# Format Rust and Python in place.
fmt:
    cargo fmt --all
    {{bin}}/ruff format python scripts
    {{bin}}/ruff check --fix python scripts

# ---- checks (all read-only) ---------------------------------------------

fmt-check:
    cargo fmt --all --check
    {{bin}}/ruff format --check python scripts

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings
    {{bin}}/ruff check python scripts
    {{bin}}/pyright

typos:
    typos

guardrails:
    {{py}} scripts/guardrails.py

deny:
    cargo deny --locked check

test-rust:
    cargo test --workspace --exclude fulmen-py --locked

test-py: develop
    {{bin}}/pytest -q --cov --cov-report=term

# Run the Python tests against every supported interpreter, incl. free-threaded.
# `+gil` is explicit because uv may otherwise pick a free-threaded build for "3.14".
# Each leg verifies it got the interpreter it asked for, and that importing
# fulmen does not re-enable the GIL on free-threaded builds.
test-matrix:
    @for v in 3.12+gil 3.13+gil 3.14+gil 3.14t; do \
        want="${v%+gil}"; want="${want%t}"; \
        case "$v" in *t) want_gil=False ;; *) want_gil=True ;; esac; \
        echo "== Python $v"; \
        d=$(mktemp -d); \
        uv venv -q -p "$v" "$d/venv"; \
        uv pip install -q -p "$d/venv" maturin pytest; \
        VIRTUAL_ENV="$d/venv" "$d/venv/bin/maturin" develop -q >/dev/null; \
        "$d/venv/bin/python" -c "import sys, fulmen; v = '%d.%d' % sys.version_info[:2]; gil = sys._is_gil_enabled() if hasattr(sys, '_is_gil_enabled') else True; assert v == '$want', f'wanted $want, got {v}'; assert str(gil) == '$want_gil', f'GIL enabled={gil}, wanted $want_gil'; print(f'interpreter ok: {v}, GIL enabled={gil}')"; \
        "$d/venv/bin/pytest" -q -p no:cacheprovider; \
        rm -rf "$d"; \
    done

# Unpushed commits follow Conventional Commits and are signed off.
commits:
    @if git rev-parse -q --verify origin/main >/dev/null; then \
        {{py}} scripts/check_commits.py --range origin/main..HEAD; \
    fi

# Rust line coverage (HTML in target/llvm-cov/html).
cov-rust:
    cargo llvm-cov --workspace --exclude fulmen-py --html

# ---- gates -------------------------------------------------------------

# Fast gate for every commit.
pre-commit: fmt-check typos guardrails

# Full gate: run before every push (the pre-push hook does this).
ci: fmt-check typos guardrails commits lint deny test-rust test-py test-matrix
    @echo "local CI passed"

# ---- release (CD) ------------------------------------------------------

# Build and smoke-test release artifacts for the current version. No git changes.
release-check:
    {{py}} scripts/release.py

# Cut a release: bump, CI, build, smoke-test, commit, tag, confirm, publish.
release version:
    {{py}} scripts/release.py {{version}}

clean:
    cargo clean
    rm -rf dist .pytest_cache .ruff_cache .coverage python/fulmen/*.so
