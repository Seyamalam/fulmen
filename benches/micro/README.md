# Micro-benchmarks

Criterion micro-benchmarks for the Rust core (`docs/DESIGN.md` §25.3,
`docs/ROADMAP.md` M1.24). They measure the parts of the request path
that are pure Rust, before any interpreter work is counted:

- how fast route lookup and route-table compilation are in
  `fulmen-core`, and
- the pure-Rust dispatcher ceiling in `fulmen-server` — the number
  M1.27 compares Python dispatch against.

The crate is `fulmen-bench`, a workspace member that is never
published (`publish = false`).

## Running

```sh
cargo bench -p fulmen-bench                       # all groups
cargo bench -p fulmen-bench -- --quick            # short sanity pass
cargo bench -p fulmen-bench -- router_match       # one group
cargo bench -p fulmen-bench -- --save-baseline main
cargo bench -p fulmen-bench -- --baseline main
```

Results land in `target/criterion/` (git-ignored).

## Groups

| Group | What it measures |
| --- | --- |
| `router_match/<table>/<case>` | `CompiledApp::route` on a 20- and a 200-route table: static hit, typed `{id}` (`int`) hit, `{*catch-all}` hit, deepest match, 404 miss, percent-encoded segment, 405 (`Allow`) lookup |
| `router_compile/routes/<n>` | `CompiledApp::compile` for 50, 200 and 1000 routes — the cold-start cost behind the 300 ms startup target for 200 routes (`docs/DESIGN.md` §25.1) |
| `dispatch/block_on/<route>` | `TestDispatcher` dispatch + response awaited on a tokio runtime: `/plaintext`, `/json`, a 404 and a 1 KiB body echo |
| `dispatch/spawn/plaintext` | the same request spawned as its own task, matching the server's real per-request shape (`docs/DESIGN.md` §14.6) |
| `dispatch/response/<kind>` | `Response::text` / `json` / `bytes` / `status` construction |

Not measured here, on purpose:

- the cached `Date` header (`crates/fulmen-server/src/date.rs`) and the
  wire `Response` → `hyper::Response` conversion
  (`crates/fulmen-server/src/service.rs`) are crate-private, so they are
  only covered indirectly by end-to-end runs;
- everything past the dispatcher boundary — sockets, parsing, Python —
  belongs to the e2e suite, not to a micro benchmark.

`just bench` orchestration, `benches/baseline.json` and the `oha`
end-to-end scenarios in `benches/e2e/` are separate roadmap tasks
(M1.25, M1.26); this directory is only the criterion layer.
