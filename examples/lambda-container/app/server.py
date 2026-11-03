"""An ordinary HTTP server that knows nothing about Lambda: `GET /` greets,
`GET /health` answers `ok`, and anything else is 404.

    python3 server.py --port 8080 --greeting "Hello"

The port and the greeting come from the command line only, so a server that
answers with them was started with the arguments it was given.
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--greeting", required=True)
    return parser.parse_args(argv)


def handler_for(greeting, port):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path == "/":
                self._json(200, {"message": greeting, "port": port})
            elif path == "/health":
                self._json(200, {"status": "ok"})
            else:
                self._json(404, {"error": f"no route for {path}"})

        def do_POST(self):
            self._json(405, {"error": "only GET"})

        do_PUT = do_DELETE = do_PATCH = do_POST

        def _json(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, format, *args):
            # One line per request, without headers or bodies.
            print(f"{self.command} {self.path.split('?', 1)[0]} {args[1] if len(args) > 1 else ''}", flush=True)

    return Handler


def serve(args, host="0.0.0.0"):
    return ThreadingHTTPServer((host, args.port), handler_for(args.greeting, args.port))


if __name__ == "__main__":
    args = parse_args()
    server = serve(args)
    print(f"listening on port {args.port}", flush=True)
    server.serve_forever()
