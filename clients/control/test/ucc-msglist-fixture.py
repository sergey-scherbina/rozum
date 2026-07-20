#!/usr/bin/env python3
"""Deterministic HTTP fixture for the dual-target UCC message-list smoke."""

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


PAYLOAD = [
    {
        "time": "12:34",
        "author": "smoke-agent",
        "content": "smoke-message",
        "ts": 1,
    }
]


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        room = (parse_qs(parsed.query).get("room") or [""])[0]
        if parsed.path != "/chat/messages" or room != "rozum":
            self.send_error(404)
            return

        body = json.dumps(PAYLOAD).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"PORT={server.server_port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
