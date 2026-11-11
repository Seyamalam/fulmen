# End-to-end benchmark apps

One tiny app per framework, all serving the same three endpoints so `oha`
runs are comparable (DESIGN.md §25.3, ROADMAP.md M1.25).

Run commands below assume the working directory is `benches/e2e` and use
module path `apps.<name>_app`. `N` is the worker (process) count; use the
same `N` for every app. Ports follow the convention 8100 + app index.

| App | File | Run command |
| --- | --- | --- |
| FastAPI | `apps/fastapi_app.py` | `uvicorn apps.fastapi_app:app --workers N --no-access-log --port 8100` |
| Litestar | `apps/litestar_app.py` | `uvicorn apps.litestar_app:app --workers N --no-access-log --port 8101` |
| Robyn | `apps/robyn_app.py` | `ROBYN_PORT=8102 python apps/robyn_app.py --processes N` |
| Flask | `apps/flask_app.py` | `gunicorn -w N -b 127.0.0.1:8103 apps.flask_app:app` |
| Fulmen (sync) | `apps/fulmen_app.py` | `fulmen run apps.fulmen_app:app --workers N --port 8104` |
| Fulmen (async) | `apps/fulmen_async_app.py` | `fulmen run apps.fulmen_async_app:app --workers N --port 8105` |
| Ceiling (Rust) | `benches/micro/src/ceiling_server.rs` | `target/release/ceiling-server --port 8106` |

Endpoints, identical across apps:

- `GET /` -> `200`, `hello world`, `text/plain`
- `GET /json` -> `200`, `{"message": "hello world"}`, `application/json`
- `GET /items/{item_id}` -> `200`, `{"item_id": <int>}`, `application/json`

Robyn notes: `--processes N` is its analogue of uvicorn/gunicorn workers
(socket shared across N processes; `--workers` is the per-process runtime
thread count and stays 1). There is no `--port` flag in robyn 0.88 — the
app reads `ROBYN_HOST`/`ROBYN_PORT` inside `app.start()`.

The ceiling server is `TestDispatcher` over real sockets: the pure-Rust
bound before any Python work is counted (M1.27). Build it with
`cargo build -r -p fulmen-bench --bin ceiling-server`.

## Runner

`run.py` (stdlib only) starts each app, waits for `GET /` -> 200, warms up
with `oha`, measures with `oha --output-format json`, and writes
`results/latest.json`:

```sh
python benches/e2e/run.py                      # all apps x all scenarios
python benches/e2e/run.py --apps fulmen,ceiling --scenarios json
python benches/e2e/run.py --sweep-gil-batch 1,8,32,64   # Q2 sweep
python benches/e2e/run.py --soak 600 --apps fulmen --scenarios json
just bench       # develop -r + venv + matrix + benches/baseline.json
just bench-check # latest.json vs baseline, >5% req/s regression fails
```

Install dependencies with
`uv pip install -p benches/e2e/.venv -r benches/e2e/requirements.txt`
(`just bench` creates the venv when missing).
