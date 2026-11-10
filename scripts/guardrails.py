"""Repository guardrails that linters cannot express.

Run with `just guardrails` (part of `just ci` and the pre-commit hook).
Each check prints the offending file and the rule it breaks. Exit code 1 on
any violation.

To change a rule, edit it here and explain why in the commit message. Do not
add per-file exceptions without a comment that says why.
"""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any, cast

ROOT = Path(__file__).resolve().parent.parent

# The only crate allowed to depend on pyo3. Everything else is plain Rust.
PYO3_CRATE = "fulmen-py"

# Crates allowed to opt into `unsafe` with `#![allow(unsafe_code)]` /
# `#![expect(unsafe_code)]`. Adding a crate here needs a design-doc entry.
UNSAFE_ALLOWED: set[str] = set()

MAX_FILE_BYTES = 512 * 1024

SECRET_PATTERNS = {
    "private key": re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----"),
    "AWS access key": re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    "GitHub token": re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    "PyPI token": re.compile(r"\bpypi-[A-Za-z0-9_-]{50,}\b"),
    "Slack token": re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
    "generic secret assignment": re.compile(
        r"""(?i)\b(?:secret|password|passwd|api_key|token)\s*=\s*["'][^"'\s]{12,}["']"""
    ),
}

# Files that are allowed to contain secret-looking strings (tests of the scanner itself).
SECRET_SCAN_SKIP = {"scripts/guardrails.py"}

REQUIRED_FILES = [
    "LICENSE",
    "NOTICE",
    "TRADEMARKS.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "CHANGELOG.md",
    "docs/DESIGN.md",
    "docs/ROADMAP.md",
    "docs/GUARDRAILS.md",
]

Violation = tuple[str, str]


def tracked_files() -> list[Path]:
    """Files tracked by git plus new files not ignored by .gitignore."""
    out = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=ROOT,
        check=True,
        capture_output=True,
    ).stdout
    return [ROOT / p for p in out.decode().split("\0") if p and (ROOT / p).is_file()]


def rel(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


TomlTable = dict[str, Any]


def table(data: TomlTable, key: str) -> TomlTable:
    """`data[key]` if it is a TOML table, else an empty table."""
    value = data.get(key)
    return cast("TomlTable", value) if isinstance(value, dict) else {}


def inherits_workspace(value: object) -> bool:
    return isinstance(value, dict) and cast("TomlTable", value).get("workspace") is True


def crate_manifests() -> Iterator[tuple[str, Path, TomlTable]]:
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        data = tomllib.loads(manifest.read_text())
        name = table(data, "package").get("name", manifest.parent.name)
        yield str(name), manifest, data


def check_crate_manifests() -> Iterator[Violation]:
    for name, manifest, data in crate_manifests():
        where = rel(manifest)
        package = table(data, "package")
        for key in ("version", "edition", "license", "rust-version"):
            if not inherits_workspace(package.get(key)):
                yield where, f"`package.{key}` must be `{{ workspace = true }}`"
        if not inherits_workspace(data.get("lints")):
            yield where, "`[lints] workspace = true` is required"
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            for dep, spec in table(data, section).items():
                if not inherits_workspace(spec):
                    yield (
                        where,
                        (
                            f"dependency `{dep}` must be declared in the root "
                            "[workspace.dependencies] and used with `workspace = true`"
                        ),
                    )
                if dep == "pyo3" and name != PYO3_CRATE:
                    yield where, f"only `{PYO3_CRATE}` may depend on pyo3 (see docs/DESIGN.md)"


def check_unsafe_opt_in(files: list[Path]) -> Iterator[Violation]:
    pattern = re.compile(r"#!\[(?:allow|expect)\([^)]*\bunsafe_code\b")
    for path in files:
        if path.suffix != ".rs" or not rel(path).startswith("crates/"):
            continue
        crate = rel(path).split("/")[1]
        if crate not in UNSAFE_ALLOWED and pattern.search(path.read_text()):
            yield rel(path), f"crate `{crate}` is not in UNSAFE_ALLOWED in scripts/guardrails.py"


def check_stubs_match_bindings() -> Iterator[Violation]:
    """Every #[pyfunction] / #[pyclass] in fulmen-py must appear in the .pyi stub."""
    stub_path = ROOT / "python/fulmen/_fulmen.pyi"
    stub = stub_path.read_text()
    exported: set[str] = set()
    for src in (ROOT / "crates" / PYO3_CRATE / "src").rglob("*.rs"):
        text = src.read_text()
        exported |= set(re.findall(r"#\[pyfunction[^\]]*\]\s*(?:pub\s+)?fn\s+(\w+)", text))
        exported |= set(re.findall(r"#\[pyclass[^\]]*\]\s*(?:pub\s+)?struct\s+(\w+)", text))
    for name in sorted(exported):
        if not re.search(rf"^(?:def|class)\s+{name}\b", stub, re.MULTILINE):
            yield rel(stub_path), f"`{name}` is exported by {PYO3_CRATE} but missing from the stub"


def check_no_ci_workflows() -> Iterator[Violation]:
    workflows = ROOT / ".github" / "workflows"
    if workflows.exists() and any(workflows.iterdir()):
        yield (
            ".github/workflows",
            "CI/CD runs locally (`just ci`, `just release`); no hosted workflows",
        )


def check_required_files() -> Iterator[Violation]:
    for name in REQUIRED_FILES:
        if not (ROOT / name).is_file():
            yield name, "required file is missing"


def check_file_sizes(files: list[Path]) -> Iterator[Violation]:
    for path in files:
        size = path.stat().st_size
        if size > MAX_FILE_BYTES and path.name != "Cargo.lock":
            yield rel(path), f"file is {size // 1024} KiB, limit is {MAX_FILE_BYTES // 1024} KiB"


def check_secrets(files: list[Path]) -> Iterator[Violation]:
    for path in files:
        if rel(path) in SECRET_SCAN_SKIP:
            continue
        try:
            text = path.read_text()
        except UnicodeDecodeError:
            continue
        for label, pattern in SECRET_PATTERNS.items():
            for match in pattern.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                yield f"{rel(path)}:{line}", f"looks like a {label}; use env vars or a secret store"


def check_python_version_floor() -> Iterator[Violation]:
    project = tomllib.loads((ROOT / "pyproject.toml").read_text())["project"]
    if project.get("requires-python") != ">=3.12":
        yield "pyproject.toml", "requires-python must stay `>=3.12` (see docs/DESIGN.md)"


def main() -> int:
    files = tracked_files()
    checks: list[tuple[str, Callable[[], Iterator[Violation]]]] = [
        ("crate manifests", check_crate_manifests),
        ("unsafe opt-in", lambda: check_unsafe_opt_in(files)),
        ("stubs match bindings", check_stubs_match_bindings),
        ("no hosted CI", check_no_ci_workflows),
        ("required files", check_required_files),
        ("file sizes", lambda: check_file_sizes(files)),
        ("secrets", lambda: check_secrets(files)),
        ("python version floor", check_python_version_floor),
    ]
    failed = 0
    for label, check in checks:
        violations = list(check())
        status = "ok" if not violations else f"{len(violations)} violation(s)"
        print(f"guardrail: {label:<22} {status}")
        for where, why in violations:
            print(f"  {where}: {why}")
        failed += len(violations)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
