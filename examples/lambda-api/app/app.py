"""A function URL handler: `GET /` greets, `GET /health` answers `ok`, and
anything else is 404."""

import json
import os


def handler(event, context):
    http = event.get("requestContext", {}).get("http", {})
    method, path = http.get("method", "GET"), event.get("rawPath", "/")
    if method != "GET":
        return _json(405, {"error": "only GET"})
    if path == "/":
        return _json(200, {"message": os.environ.get("GREETING", "Hello")})
    if path == "/health":
        return _json(200, {"status": "ok"})
    return _json(404, {"error": f"no route for {path}"})


def _json(status, body):
    return {
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "body": json.dumps(body),
    }
