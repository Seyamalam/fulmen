"""Local release pipeline (CD). There is no hosted CI; this is the only way to ship.

    just release-check    build + smoke-test artifacts for the current version, no git changes
    just release 0.2.0    bump, run CI, build, smoke-test, commit, tag, then ask before
                          pushing to GitHub and publishing to PyPI

Artifacts land in dist/. macOS wheels are built natively for every supported
interpreter. Linux (manylinux) wheels are built in the official maturin Docker
image. Windows wheels are not built locally yet; Windows users install from
the sdist (needs a Rust toolchain). See docs/ROADMAP.md.

Publishing reads the PyPI token from UV_PUBLISH_TOKEN or the system keyring,
never from a file in the repo.
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import NoReturn

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
# `+gil` is explicit: uv may otherwise resolve "3.14" to a free-threaded build.
PYTHONS = ["3.12+gil", "3.13+gil", "3.14+gil", "3.14t"]
MATURIN_IMAGE = "ghcr.io/pyo3/maturin:latest"
LINUX_PYTHONS = ["python3.12", "python3.13", "python3.14", "python3.14t"]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:alpha|beta|rc)\.\d+)?$")


def run(*cmd: str, capture: bool = False) -> str:
    print(f"$ {' '.join(cmd)}", flush=True)
    result = subprocess.run(cmd, cwd=ROOT, check=True, text=True, capture_output=capture)
    return result.stdout.strip() if capture else ""


def fail(message: str) -> NoReturn:
    print(f"release: {message}", file=sys.stderr)
    sys.exit(1)


def current_version() -> str:
    return tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


STAGES = {"alpha": 0, "beta": 1, "rc": 2}


def version_key(v: str) -> tuple[int, int, int, int, int]:
    """Sort key: 1.0.0-alpha.1 < 1.0.0-beta.1 < 1.0.0-rc.2 < 1.0.0."""
    core, _, pre = v.partition("-")
    major, minor, patch = (int(x) for x in core.split("."))
    if not pre:
        return (major, minor, patch, len(STAGES), 0)
    stage, _, number = pre.partition(".")
    return (major, minor, patch, STAGES[stage], int(number))


def require_clean_main() -> None:
    if run("git", "branch", "--show-current", capture=True) != "main":
        fail("releases are cut from `main` only")
    if run("git", "status", "--porcelain", capture=True):
        fail("working tree is not clean; commit or stash first")
    run("git", "fetch", "origin", "main")
    if run("git", "rev-parse", "HEAD", capture=True) != run(
        "git", "rev-parse", "origin/main", capture=True
    ):
        fail("local main differs from origin/main; pull or push first")


def require_changelog_entry(version: str) -> str:
    text = (ROOT / "CHANGELOG.md").read_text()
    match = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)", text, re.S | re.M)
    if not match or not match.group(1).strip():
        fail(f"CHANGELOG.md has no non-empty `## [{version}]` section")
    return match.group(1).strip()


def bump(version: str) -> None:
    manifest = ROOT / "Cargo.toml"
    old = current_version()
    text = manifest.read_text()
    text = text.replace(f'version = "{old}"\nedition', f'version = "{version}"\nedition', 1)
    text = re.sub(
        rf'(fulmen-\w+ = \{{ path = "crates/fulmen-\w+", version = )"{re.escape(old)}"',
        rf'\1"{version}"',
        text,
    )
    manifest.write_text(text)
    if current_version() != version:
        fail("failed to update the workspace version in Cargo.toml")
    run("cargo", "update", "--workspace")


def docker_available() -> bool:
    if not shutil.which("docker"):
        return False
    return subprocess.run(["docker", "info"], capture_output=True, check=False).returncode == 0


def build() -> list[Path]:
    shutil.rmtree(DIST, ignore_errors=True)
    run("maturin", "sdist", "--out", str(DIST))
    for py in PYTHONS:
        interpreter = run("uv", "python", "find", py, capture=True)
        run("maturin", "build", "--release", "--out", str(DIST), "--interpreter", interpreter)
    if docker_available():
        # Native-arch Linux wheels only (aarch64 on Apple Silicon). x86_64 and
        # musllinux are roadmap task M6.14. A separate target dir keeps the
        # Linux build cache from clobbering the macOS one.
        run(
            "docker",
            "run",
            "--rm",
            "-v",
            f"{ROOT}:/io",
            "-e",
            "CARGO_TARGET_DIR=/io/target/docker-linux",
            MATURIN_IMAGE,
            "build",
            "--release",
            "--out",
            "dist",
            "--interpreter",
            *LINUX_PYTHONS,
        )
    else:
        print("release: docker is not running, skipping Linux wheels", file=sys.stderr)
    return sorted(DIST.iterdir())


def smoke_test(version: str) -> None:
    """Install each native wheel into a fresh venv and import it."""
    for py in PYTHONS:
        with tempfile.TemporaryDirectory() as tmp:
            venv = Path(tmp) / "venv"
            run("uv", "venv", "-q", "-p", py, str(venv))
            run(
                "uv",
                "pip",
                "install",
                "-q",
                "-p",
                str(venv),
                "--no-index",
                "--find-links",
                str(DIST),
                f"fulmen=={version}",
            )
            out = run(str(venv / "bin" / "fulmen"), "info", capture=True)
            if f"fulmen {version}" not in out:
                fail(f"smoke test failed on Python {py}:\n{out}")
            if py.endswith("t") and "GIL disabled" not in out:
                fail(f"importing fulmen re-enabled the GIL on Python {py}")
            print(f"smoke test ok: Python {py}")


def confirm(version: str) -> bool:
    answer = input(
        f"\nPush v{version} to GitHub and publish to PyPI? Type the version to confirm: "
    )
    return answer.strip() == version


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawTextHelpFormatter
    )
    parser.add_argument("version", nargs="?", help="new version; omit to only build and smoke-test")
    args = parser.parse_args()

    if args.version is None:
        artifacts = build()
        smoke_test(current_version())
        print("\n".join(f"  {p.name}" for p in artifacts))
        return 0

    version: str = args.version
    if not SEMVER.match(version):
        fail(f"{version!r} is not a valid version (X.Y.Z or X.Y.Z-alpha.N/-beta.N/-rc.N)")
    if version_key(version) <= version_key(current_version()):
        fail(f"{version} is not newer than {current_version()}")

    require_clean_main()
    notes = require_changelog_entry(version)
    bump(version)
    run("just", "ci")
    artifacts = build()
    smoke_test(version)

    run("git", "add", "Cargo.toml", "Cargo.lock")
    run("git", "commit", "-s", "-m", f"chore(release): v{version}")
    run("git", "tag", "-a", f"v{version}", "-m", f"Fulmen v{version}\n\n{notes}")
    print(f"\nCommitted and tagged v{version} locally. Artifacts:")
    print("\n".join(f"  {p.name}" for p in artifacts))

    if not confirm(version):
        print(
            f"Stopped before publishing. To undo: git tag -d v{version} && git reset --soft HEAD~1"
        )
        return 0
    run("git", "push", "origin", "main", f"v{version}")
    run("uv", "publish", *(str(p) for p in artifacts))
    run(
        "gh",
        "release",
        "create",
        f"v{version}",
        "--title",
        f"v{version}",
        "--notes",
        notes,
        *(str(p) for p in artifacts),
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
