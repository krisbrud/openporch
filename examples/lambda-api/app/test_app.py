"""Run with `python3 -m unittest discover examples/lambda-api/app`."""

import json
import os
import unittest
from unittest import mock

import app


def get(path, method="GET"):
    return app.handler({"rawPath": path, "requestContext": {"http": {"method": method}}}, None)


class ApiTest(unittest.TestCase):
    def test_the_root_greets_with_the_configured_greeting(self):
        with mock.patch.dict(os.environ, {"GREETING": "Hei"}):
            response = get("/")
        self.assertEqual(response["statusCode"], 200)
        self.assertEqual(json.loads(response["body"]), {"message": "Hei"})

    def test_health_and_unknown_paths(self):
        self.assertEqual(json.loads(get("/health")["body"]), {"status": "ok"})
        self.assertEqual(get("/nope")["statusCode"], 404)
        self.assertEqual(get("/", "POST")["statusCode"], 405)


if __name__ == "__main__":
    unittest.main()
