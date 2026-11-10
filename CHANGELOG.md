# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, minor versions may
break the API; every break is listed under **Breaking**.

`just release X.Y.Z` refuses to run unless a non-empty `## [X.Y.Z]` section
exists. Move the `Unreleased` entries under the new version heading first.

## [Unreleased]

### Added

- Project skeleton: Cargo workspace (`fulmen-core`, `-server`, `-schema`,
  `-security`, `-py`), Python package with type stubs, `fulmen info` CLI.
- Local CI/CD through `just`, git hooks, and repository guardrails.
- Design document and v1 roadmap.
