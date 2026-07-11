#!/usr/bin/env python3
"""Acceptance e2e smoke test for the rozum UCC (Unified Control Center) PWA.

Headless + MOCKED — no browser, no Face ID hardware, no external deps (Python stdlib only):

  * Spins up an ISOLATED `control-serve` (temp XDG_STATE_HOME, rp_id=localhost) so the test
    never touches the operator's real credentials / sessions / passkeys.
  * Tier 1 smoke-tests the UNAUTHENTICATED surface and the WebAuthn (Face ID) *wiring*: the PWA
    is served, every protected route returns 401, register/begin is invite/bootstrap-gated, and
    it issues a well-formed passkey creation challenge (rp.id, ES256, challenge).
  * Tier 2 MOCKS the Face ID login — the ONE thing a headless test can't do is the Secure-Enclave
    signature, so we inject a session straight into the isolated store (exactly the record a real
    login would mint) and verify the AUTHENTICATED surface opens, that expiry is enforced, and
    that a bogus token is rejected. Everything downstream of the signature is exercised for real.

Exit 0 iff every check passes.  Usage:  python3 scripts/test/ucc_e2e_smoke.py
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import shutil
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))


def find_bin():
    for c in (
        os.path.join(REPO, "target/release/rozum-gateway"),
        os.path.expanduser("~/.rozum/bin/rozum-gateway"),
    ):
        if os.path.exists(c):
            return c
    sys.exit("rozum-gateway not built (cargo build -p rozum --bin rozum-gateway --release)")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def http(method, base, path, cookie=None, body=None, timeout=8):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if cookie:
        req.add_header("Cookie", cookie)
    try:
        r = urllib.request.urlopen(req, timeout=timeout)
        raw, code = r.read().decode("utf-8", "replace"), r.status
    except urllib.error.HTTPError as e:
        raw, code = e.read().decode("utf-8", "replace"), e.code
    except Exception as e:  # noqa: BLE001
        return 0, {"_err": str(e)}
    try:
        return code, json.loads(raw)
    except Exception:  # noqa: BLE001
        return code, raw


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append(bool(ok))
    mark = "✅" if ok else "❌"
    line = f"  {mark} {name}"
    if detail and not ok:
        line += f"  — {detail}"
    print(line)


def wait_health(base, secs=30):
    end = time.time() + secs
    while time.time() < end:
        code, _ = http("GET", base, "/", timeout=3)
        if code == 200:
            return True
        time.sleep(1)
    return False


def main():
    binp = find_bin()
    state = tempfile.mkdtemp(prefix="ucc-e2e-")
    ucc = os.path.join(state, "rozum")  # state_dir() == $XDG_STATE_HOME/rozum
    port = free_port()
    base = f"http://127.0.0.1:{port}"

    env = dict(os.environ)
    env["XDG_STATE_HOME"] = state
    env["ROZUM_UCC_RP_ID"] = "localhost"          # so a localhost origin verifies
    env["ROZUM_UCC_ORIGIN"] = f"http://localhost:{port}"

    proc = subprocess.Popen(
        [binp, "gateway", "control-serve", "--port", str(port)],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_health(base):
            print("control-serve did not come up")
            return 1
        print(f"\n== UCC acceptance e2e  (isolated :{port})  ==")

        # ---------- Tier 1: unauthenticated surface + WebAuthn (Face ID) wiring ----------
        print("\n[Tier 1] unauthenticated surface + Face ID wiring")
        for pg in ("/", "/login.html", "/terminal.html", "/admin.html", "/matrix.html"):
            code, _ = http("GET", base, pg)
            check(f"PWA serves {pg}", code == 200, f"HTTP {code}")

        code, _ = http("GET", base, "/control/status")
        check("gated /control/status → 401 unauth", code == 401, f"HTTP {code}")
        code, _ = http("GET", base, "/control/session/attach/x")
        check("gated terminal attach → 401 unauth", code == 401, f"HTTP {code}")

        code, j = http("GET", base, "/control/auth/status")
        check("auth/status reports webauthn_ok=true",
              code == 200 and isinstance(j, dict) and j.get("webauthn_ok") is True, str(j)[:140])

        code, _ = http("POST", base, "/control/auth/register/begin", body={})
        check("register/begin without token → 403", code == 403, f"HTTP {code}")

        boot = ""
        bpath = os.path.join(ucc, "ucc-bootstrap-token.txt")
        if os.path.exists(bpath):
            boot = open(bpath).read().strip()
        check("bootstrap token issued for first registration", bool(boot))

        code, j = http("POST", base, "/control/auth/register/begin",
                       body={"invite": boot, "display_name": "e2e"})
        pk = j.get("publicKey", {}) if isinstance(j, dict) else {}
        params = pk.get("pubKeyCredParams", []) or []
        good_reg = (
            code == 200
            and bool(pk.get("challenge"))
            and pk.get("rp", {}).get("id") == "localhost"
            and any(p.get("alg") == -7 for p in params)  # ES256, the Face ID algorithm
        )
        check("register/begin issues a valid passkey challenge (rp.id, ES256)", good_reg, f"HTTP {code} {str(pk)[:100]}")

        code, j = http("POST", base, "/control/auth/login/begin")
        check("login/begin with no enrolled passkey → 400", code == 400, f"HTTP {code}")

        # ---------- Tier 2: MOCK Face ID by injecting the session a real login would mint ----------
        print("\n[Tier 2] mocked Face ID auth (injected session)")
        os.makedirs(ucc, exist_ok=True)
        now = int(time.time())
        # RBAC roles (mirrors default_roles(); `admin` grants every permission via the wildcard).
        json.dump(
            [{"id": "readonly", "name": "Read only", "permissions": ["read"]},
             {"id": "admin", "name": "Administrator", "permissions": ["admin"]}],
            open(os.path.join(ucc, "ucc-roles.json"), "w"),
        )
        json.dump(
            [{"id": "e2e-mock", "display_name": "E2E", "role_ids": ["admin"],
              "passkey_ids": [], "created_at": now, "created_by": None}],
            open(os.path.join(ucc, "ucc-users.json"), "w"),
        )
        tok = "e2e-mock-session-000000000000000000000000"
        json.dump(
            [{"token": tok, "user_id": "e2e-mock", "expires_at": now + 3600}],
            open(os.path.join(ucc, "ucc-auth-sessions.json"), "w"),
        )
        ck = f"rozum_sess={tok}"

        code, _ = http("GET", base, "/control/status", cookie=ck)
        check("valid session → /control/status 200 (gate opens)", code == 200, f"HTTP {code}")
        code, j = http("GET", base, "/control/auth/status", cookie=ck)
        check("valid session → auth/status authed=true",
              code == 200 and isinstance(j, dict) and j.get("authed") is True, str(j)[:140])

        # expiry enforced
        json.dump(
            [{"token": tok, "user_id": "e2e-mock", "expires_at": now - 10}],
            open(os.path.join(ucc, "ucc-auth-sessions.json"), "w"),
        )
        code, _ = http("GET", base, "/control/status", cookie=ck)
        check("EXPIRED session → 401 (expiry enforced)", code == 401, f"HTTP {code}")

        # bogus token rejected
        code, _ = http("GET", base, "/control/status", cookie="rozum_sess=not-a-real-token")
        check("bogus session token → 401", code == 401, f"HTTP {code}")

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:  # noqa: BLE001
            proc.kill()
        shutil.rmtree(state, ignore_errors=True)

    passed = sum(RESULTS)
    total = len(RESULTS)
    print(f"\n== {passed}/{total} checks passed ==")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
