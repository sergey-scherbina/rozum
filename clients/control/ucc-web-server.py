#!/usr/bin/env python3
# UCC web server: serves the control-center SPA (static) AND, same-origin, the data
# the SPA needs so the whole app (dashboard + chat) lives inside one PWA origin:
#   GET /control/status        -> proxy the rust control-API (:8411)
#   GET /chat/messages?room=X   -> the room's messages as JSON (read the daemon's jsonl)
#   POST /chat/post             -> post a message (proxy the meeting web's single-writer /p)
# One origin => clicking a meeting opens the native chat view IN the standalone PWA.
import json, os, glob, urllib.request, urllib.parse, urllib.error
from datetime import datetime
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

SITE = os.path.expanduser("~/.rozum/ucc/site")
CONTROL = "http://127.0.0.1:8411/control/status"
MEETING = "http://127.0.0.1:8405"          # meeting web (single-writer post endpoint)
PORT = 8410

def _state_dir():
    base = os.environ.get("XDG_STATE_HOME") or os.path.join(os.path.expanduser("~"), ".local", "state")
    return os.path.join(base, "rozum")

def _room_root(name):
    try:
        rooms = json.load(open(os.path.join(_state_dir(), "rooms.json")))
    except Exception:
        return None
    rooms = rooms if isinstance(rooms, list) else rooms.get("rooms", [])
    for r in rooms:
        if r.get("name") == name:
            return r.get("root")
    return None

def _messages(room, limit=80):
    root = _room_root(room)
    if not root or not os.path.isdir(root):
        return []
    out = []
    for fp in sorted(glob.glob(os.path.join(root, "*.jsonl"))):
        try:
            for line in open(fp, encoding="utf-8"):
                line = line.strip()
                if not line:
                    continue
                try:
                    m = json.loads(line)
                except Exception:
                    continue
                if "content" not in m:
                    continue
                ts = m.get("ts", 0)
                try:
                    hm = datetime.fromtimestamp(int(ts)).strftime("%H:%M")
                except Exception:
                    hm = ""
                out.append({"time": hm, "author": m.get("display_name", ""), "content": m.get("content", ""), "ts": ts})
        except Exception:
            continue
    return out[-limit:]

def _threads(room):
    # The room's incidents (threads.json), flattened for a declarative table — convergence Phase 3:
    # the one UCC SPA shows meetings' incidents next to chat, not just the separate meeting PWA.
    root = _room_root(room)
    if not root:
        return []
    try:
        m = json.load(open(os.path.join(root, "threads.json")))
    except Exception:
        return []
    out = []
    for tid, t in (m.items() if isinstance(m, dict) else []):
        if not isinstance(t, dict):
            continue
        out.append({"id": t.get("id", tid), "title": t.get("title", ""), "state": t.get("state", "open"),
                    "severity": t.get("severity", ""), "owner": t.get("owner", "")})
    return out

class H(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map,
                      ".js": "application/javascript", ".mjs": "application/javascript",
                      ".webmanifest": "application/manifest+json"}

    def __init__(self, *a, **k):
        super().__init__(*a, directory=SITE, **k)

    def _send_json(self, obj, code=200):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/control/status":
            try:
                body = urllib.request.urlopen(CONTROL, timeout=4).read()
                return self._send_raw(body, 200)
            except Exception as e:
                return self._send_json({"error": str(e)}, 502)
        if path == "/chat/messages":
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            room = (q.get("room") or ["demo"])[0]
            return self._send_json(_messages(room))
        if path == "/chat/incidents":
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            room = (q.get("room") or ["demo"])[0]
            return self._send_json(_threads(room))
        # Other control-API reads (e.g. /control/coder/log?id=&tail=) → proxy to :8411, query and all.
        if path.startswith("/control/"):
            try:
                hdrs = {}
                ck = self.headers.get("Cookie")
                if ck:
                    hdrs["Cookie"] = ck
                req = urllib.request.Request("http://127.0.0.1:8411" + self.path, headers=hdrs)
                resp = urllib.request.urlopen(req, timeout=8)
                return self._send_raw(resp.read(), resp.status, resp.headers.get_all("Set-Cookie"))
            except urllib.error.HTTPError as e:
                return self._send_raw(e.read(), e.code)
            except Exception as e:
                return self._send_json({"error": str(e)}, 502)
        return super().do_GET()

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "POST, GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "X-Room, Content-Type")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        if self.path.split("?")[0] == "/chat/post":
            room = self.headers.get("X-Room", "demo")
            n = int(self.headers.get("Content-Length", 0) or 0)
            content = self.rfile.read(n).decode("utf-8", "replace") if n else ""
            if not content.strip():
                return self._send_json({"ok": True})   # ignore empty
            body = ("room=" + room + "\ncontent=" + content).encode("utf-8")
            try:
                req = urllib.request.Request(MEETING + "/p", data=body,
                                             headers={"Content-Type": "text/plain"}, method="POST")
                urllib.request.urlopen(req, timeout=6).read()
                return self._send_json({"ok": True})
            except Exception as e:
                return self._send_json({"error": str(e)}, 502)
        # Control-API write actions (agent launch/stop, gateway load, task) — proxy to control-serve
        # (:8411), forwarding the JSON body AND the status code (so a 409 admission refusal + its verdict
        # reach the SPA). Same-origin so the standalone PWA can drive agents.
        if self.path.split("?")[0].startswith("/control/"):
            n = int(self.headers.get("Content-Length", 0) or 0)
            body = self.rfile.read(n) if n else b""
            url = "http://127.0.0.1:8411" + self.path
            try:
                # forward the caller's Cookie so control-serve can authenticate (session/SSO)
                hdrs = {"Content-Type": "application/json"}
                ck = self.headers.get("Cookie")
                if ck:
                    hdrs["Cookie"] = ck
                req = urllib.request.Request(url, data=body, headers=hdrs, method="POST")
                resp = urllib.request.urlopen(req, timeout=45)
                return self._send_raw(resp.read(), resp.status, resp.headers.get_all("Set-Cookie"))
            except urllib.error.HTTPError as e:
                return self._send_raw(e.read(), e.code)   # forward 4xx/5xx (admission verdict / 401) verbatim
            except Exception as e:
                return self._send_json({"error": str(e)}, 502)
        self.send_response(404); self.end_headers()

    def _send_raw(self, body, code, set_cookies=None):
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Access-Control-Allow-Origin", "*")
        for c in (set_cookies or []):
            self.send_header("Set-Cookie", c)   # forward the login session cookie from control-serve
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
