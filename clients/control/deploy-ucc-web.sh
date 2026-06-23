#!/usr/bin/env bash
# Build + (re)deploy the UCC web control-center as a durable launchd service.
#
# Durable stack (macOS user LaunchAgents, RunAtLoad + KeepAlive):
#   com.rozum.ucc-control  ->  ~/.rozum/bin/rozum-ctrl gateway control-serve --port 8411
#   com.rozum.ucc-web      ->  python3 -m http.server 8410 --directory ~/.rozum/ucc/site
# Tailscale serve (persists via --bg) exposes them to the tailnet:
#   :8447 -> 127.0.0.1:8410 (SPA)    :8448 -> 127.0.0.1:8411 (control-API, CORS *)
# Operator opens  https://<machine>.tailnet.ts.net:8447/
#
# The SPA fetches /control/status from the injected backend base URL (the :8448
# Tailscale URL), parses it client-side (jsonValue) and renders structured fields.
set -euo pipefail

SSC="${SSC:-/tmp/ssc-tk/bin/ssc}"          # path to the scalascript CLI
APP="$(cd "$(dirname "$0")" && pwd)/control-center-live.ssc"
TS_BASE="${TS_BASE:-https://busi.tail1174e2.ts.net:8448}"   # control-API behind Tailscale
SITE="$HOME/.rozum/ucc/site"
BIN="$HOME/.rozum/bin/rozum-ctrl"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"

mkdir -p "$SITE" "$(dirname "$BIN")"

# 1) Install the control-API binary (needs `gateway control-serve`, on master).
echo ">> building rozum (control-serve) ..."
( cd "$REPO" && cargo build --bin rozum )
cp "$REPO/target/debug/rozum" "$BIN"

# 2) Emit the SPA with the Tailscale control-API as the injected backend base,
#    then snapshot its self-contained index.html into the served dir.
echo ">> emitting SPA (base=$TS_BASE) ..."
PORT=$("$SSC" run --frontend react --mode client --server-url "$TS_BASE" "$APP" 2>&1 \
        | grep -oE 'http://127.0.0.1:[0-9]+' | head -1 | grep -oE '[0-9]+$') &
SSC_PID=$!
sleep 6
DEV=$(lsof -nP -iTCP -sTCP:LISTEN -a -p "$SSC_PID" 2>/dev/null | grep -oE ':[0-9]+' | tr -d ':' | head -1 || true)
curl -s "http://127.0.0.1:${DEV:-0}/" -o "$SITE/index.html"
kill "$SSC_PID" 2>/dev/null || true
echo ">> SPA saved: $(wc -c < "$SITE/index.html") bytes"

# 3) (Re)load the launchd services.
UID_=$(id -u)
for svc in ucc-control ucc-web; do
  plist="$HOME/Library/LaunchAgents/com.rozum.$svc.plist"
  launchctl bootout "gui/$UID_/com.rozum.$svc" 2>/dev/null || true
  launchctl bootstrap "gui/$UID_" "$plist"
done
sleep 3
curl -s --max-time 4 http://127.0.0.1:8411/control/status -o /dev/null -w "control :8411 -> %{http_code}\n"
curl -s --max-time 4 http://127.0.0.1:8410/ -o /dev/null -w "spa     :8410 -> %{http_code}\n"
echo ">> done. open https://busi.tail1174e2.ts.net:8447/"
