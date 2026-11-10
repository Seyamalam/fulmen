# End-to-end benchmark apps

One tiny app per framework, all serving the same three endpoints so `oha`
runs are comparable (DESIGN.md §25.3, ROADMAP.md M1.25).

Run commands below assume the working directory is `benches/e2e` and use
module path `apps.<name>_app`. `N` is the worker count; use the same `N`
for every app. Ports follow the convention 8100 + app index (8100–8104).

| App | File | Run command |
| --- | --- | --- |
| FastAPI | `apps/fastapi_app.py` | `uvicorn apps.fastapi_app:app -w N --no-access-log -p 8100` |
| Litestar | `apps/litestar_app.py` | `uvicorn apps.litestar_app:app -w N --no-access-log -p 8101` |
| Robyn | `apps/robyn_app.py` | `python -m robyn apps/robyn_app.py --port 8102` |
| Flask | `apps/flask_app.py` | `gunicorn -w N -b 127.0.0.1:8103 apps.flask_app:app` |
| Fulmen | `apps/fulmen_app.py` | `fulmen run apps.fulmen_app:app --workers N --port 8104` |

Endpoints, identical across apps:

- `GET /` -> `200`, `hello world`, `text/plain`
- `GET /json` -> `200`, `{"message": "hello world"}`, `application/json`
- `GET /items/{item_id}` -> `200`, `{"item_id": <int>}`, `application/json`

Install dependencies with `pip install -r benches/e2e/requirements.txt`.

The `oha` scenario definitions and `just bench` wiring are a separate task
(M1.26); this directory only contains the apps under test.
