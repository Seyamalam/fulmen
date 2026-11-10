"""Flask comparison app for the M1.25 end-to-end benchmarks.

Run: gunicorn --workers N -b 127.0.0.1:8103 benches.e2e.apps.flask_app:app
"""

from flask import Flask, Response, jsonify

app = Flask(__name__)


@app.get("/")
def index() -> Response:
    return Response("hello world", mimetype="text/plain")


@app.get("/json")
def json_route():
    return jsonify(message="hello world")


@app.get("/items/<int:item_id>")
def get_item(item_id: int):
    return jsonify(item_id=item_id)
