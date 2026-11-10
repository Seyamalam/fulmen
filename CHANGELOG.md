# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, minor versions may
break the API; every break is listed under **Breaking**.

`just release X.Y.Z` refuses to run unless a non-empty `## [X.Y.Z]` section
exists. Move the `Unreleased` entries under the new version heading first.

## [Unreleased]

### Added

- `fulmen-server`: pure-Rust HTTP/1.1 transport on tokio + hyper with a
  `Dispatcher` trait boundary (`serve`/`Server`, `ServerConfig`,
  `Request`/`Response` wire types), keep-alive and pipelining, request
  limits (431/414/413), read and keep-alive-idle timeouts, SIGINT/SIGTERM
  and programmatic graceful shutdown with a 30 s drain deadline, cached
  `Date` header, and a pure-Rust `TestDispatcher` for tests and
  benchmarks. Refs: M1.1–M1.7.
- Project skeleton: Cargo workspace (`fulmen-core`, `-server`, `-schema`,
  `-security`, `-py`), Python package with type stubs, `fulmen info` CLI.
- Local CI/CD through `just`, git hooks, and repository guardrails.
- Design document and v1 roadmap.
- Router in `fulmen-core` (M1.8–M1.12): route declarations compiled into an
  immutable `CompiledApp` (one `matchit` tree per HTTP method), typed `int`/
  `str` path parameters returned in declaration order, automatic `HEAD`,
  404/405 results with `Allow` (incl. `OPTIONS` metadata), route groups and
  `mount` with exact prefix joining, strict/redirect/ignore trailing-slash
  policy, and startup errors that aggregate duplicates, pattern conflicts,
  and parameter-type mismatches.
