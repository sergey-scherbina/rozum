#!/usr/bin/env python3
"""Deterministic HTTP fixture for the dual-target meeting-transcript smoke.

Serves the SHAPE the meeting daemon's read API returns — the `{room,date,…,messages:[…]}`
envelope, with the two derived display fields (`badge`, `time`) the daemon now computes in
`rest_read::message_json`. The rows are what the smoke asserts against, so this file and
that function are a contract pair: change one and the smoke tells you about the other.

`--require-auth` makes the fixture answer 401 without an `Authorization` header. That is off by
default because the TUI target cannot send one yet (reported upstream as `tui-fetch-headers`);
it exists so the day the emitter honours the header signal, the smoke can turn it on and prove
the authenticated path end to end without inventing a new fixture.
"""

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse


ROOM = "smoke-room"
DATE = "2026-08-04"

# One plain note and one flagged message: the badge column has to be exercised BOTH ways, because
# "" and a real badge are the two cases a table column binds to.
#
# Every value is kept SHORT on purpose. The terminal target renders a real table and truncates each
# cell to its column width, so a long fixture string turns the smoke into an assertion about column
# arithmetic ("smoke-agent" arrived as "smoke-agen") instead of about the binding under test. Badge
# COMPOSITION is not this gate's job either — that is unit-tested in Rust against
# `StoredTurn::badge()`, which is the only implementation of it.
MESSAGES = [
    {
        "date": DATE,
        "n": 0,
        "participant_id": "p",
        "display_name": "agent",
        "content": "hello",
        "ts": 1718000000,
        "badge": "",
        "time": "12:34",
    },
    {
        "date": DATE,
        "n": 1,
        "participant_id": "p",
        "display_name": "agent",
        "content": "db down",
        "ts": 1718000060,
        "kind": "alert",
        "meta": {"severity": "critical", "tags": ["db"]},
        "badge": "[ALERT]",
        "time": "12:35",
    },
]

ENVELOPE = {
    "room": ROOM,
    "date": DATE,
    "from": 0,
    "count": len(MESSAGES),
    "next_from": len(MESSAGES),
    "has_more": False,
    "messages": MESSAGES,
}


# The room list the picker reads. Each entry carries a READY-MADE url, because the generated
# client cannot compose one — that is the whole reason the daemon ships it (see rest_read::rooms).
def rooms_envelope(origin):
    return {
        "rooms": [ROOM, "other-room"],
        "entries": [
            {"name": ROOM, "url": f"{origin}/rooms/{ROOM}/messages/{DATE}", "last": DATE, "mentions": 2},
            {"name": "other-room", "url": f"{origin}/rooms/other-room/messages/{DATE}", "last": DATE, "mentions": 0},
        ],
    }


class Handler(BaseHTTPRequestHandler):
    require_auth = False

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/rooms":
            if self.require_auth and not self.headers.get("Authorization"):
                self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers()
                return
            origin = "http://" + self.headers.get("Host", "127.0.0.1")
            body = json.dumps(rooms_envelope(origin)).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if parsed.path != f"/rooms/{ROOM}/messages/{DATE}":
            self.send_error(404)
            return
        if self.require_auth and not self.headers.get("Authorization"):
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="rozum meeting"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        body = json.dumps(ENVELOPE).encode("utf-8")
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
    parser.add_argument("--require-auth", action="store_true")
    args = parser.parse_args()
    Handler.require_auth = args.require_auth
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"PORT={server.server_port}", flush=True)
    print(f"ROOM={ROOM}", flush=True)
    print(f"DATE={DATE}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
