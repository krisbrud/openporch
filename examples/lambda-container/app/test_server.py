"""Run with `python3 -m unittest discover examples/lambda-container/app`."""

import contextlib
import json
import threading
import unittest
import urllib.error
import urllib.request

import server


@contextlib.contextmanager
def running(argv):
    # Port 0 picks a free port; the server reports the one it was given.
    httpd = server.serve(server.parse_args(argv), host="127.0.0.1")
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{httpd.server_address[1]}"
    finally:
        httpd.shutdown()
        httpd.server_close()


def call(url, method="GET"):
    try:
        with urllib.request.urlopen(urllib.request.Request(url, method=method)) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


class ServerTest(unittest.TestCase):
    def test_the_port_and_greeting_are_required_arguments(self):
        with self.assertRaises(SystemExit):
            server.parse_args([])
        with self.assertRaises(SystemExit):
            server.parse_args(["--port", "8080"])
        args = server.parse_args(["--port", "8080", "--greeting", "Hei"])
        self.assertEqual((args.port, args.greeting), (8080, "Hei"))

    def test_the_root_greets_with_the_greeting_it_was_started_with(self):
        with running(["--port", "0", "--greeting", "Hei fra en container"]) as url:
            self.assertEqual(call(url + "/"), (200, {"message": "Hei fra en container", "port": 0}))

    def test_health_and_unknown_paths(self):
        with running(["--port", "0", "--greeting", "Hei"]) as url:
            self.assertEqual(call(url + "/health"), (200, {"status": "ok"}))
            self.assertEqual(call(url + "/health?x=1"), (200, {"status": "ok"}))
            self.assertEqual(call(url + "/nope")[0], 404)
            self.assertEqual(call(url + "/", "POST")[0], 405)


if __name__ == "__main__":
    unittest.main()
