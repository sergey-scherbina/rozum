#!/usr/bin/env bash
# Build + (re)deploy the UCC web control-center as a durable launchd service.
#
# Architecture (all ScalaScript sources, no Python proxy):
#   com.rozum.ucc-control  ->  ~/.rozum/bin/rozum-ctrl gateway control-serve --port 8411
#                              serves SPA + API + chat — single port, same origin
# Tailscale expose (run once manually if not already set):
#   tailscale serve --bg --https=8448 http://127.0.0.1:8411
# Operator opens  https://busi.tail1174e2.ts.net:8448/
#
# ScalaScript sources:
#   control-center-live.ssc  ->  site/index.html   (browser SPA, reactive)
#   login.ssc                ->  site/login.html   (Face ID / WebAuthn page)
#   terminal.ssc             ->  site/terminal.html (xterm.js WebSocket terminal)
set -euo pipefail

SSC="${SSC:-/tmp/ssc-tk/bin/ssc}"
HERE="$(cd "$(dirname "$0")" && pwd)"
SITE="$HOME/.rozum/ucc/site"
BIN="$HOME/.rozum/bin/rozum-ctrl"
REPO="$(cd "$HERE/../.." && pwd)"

mkdir -p "$SITE" "$(dirname "$BIN")"

# 1) Build the control-serve binary (now also serves static files + chat).
echo ">> building rozum (control-serve) ..."
( cd "$REPO" && cargo build --bin rozum )
cp "$REPO/target/debug/rozum" "$BIN"

# Helper: compile a server-side .ssc file, serve briefly, capture HTML, shut down.
emit_html() {
  local ssc_file="$1" port="$2" out="$3"
  echo ">> emitting $out (from $(basename "$ssc_file")) ..."
  "$SSC" run "$ssc_file" &
  local ssc_pid=$!
  sleep 4
  curl -sf "http://127.0.0.1:${port}/" -o "$out" || echo "  (warn: curl failed for $out)"
  kill "$ssc_pid" 2>/dev/null || true
}

# 2) Compile the browser SPA (control-center-live.ssc → index.html).
echo ">> emitting index.html (browser SPA) ..."
"$SSC" run --frontend react --mode client "$HERE/control-center-live.ssc" &
SSC_PID=$!
sleep 6
DEV=$(lsof -nP -iTCP -sTCP:LISTEN -a -p "$SSC_PID" 2>/dev/null | grep -oE ':[0-9]+' | tr -d ':' | head -1 || true)
curl -sf "http://127.0.0.1:${DEV:-0}/" -o "$SITE/index.html" || true
kill "$SSC_PID" 2>/dev/null || true
echo ">> index.html: $(wc -c < "$SITE/index.html") bytes"

# 3) Compile login.ssc → login.html and terminal.ssc → terminal.html.
emit_html "$HERE/login.ssc"    8421 "$SITE/login.html"
emit_html "$HERE/terminal.ssc" 8422 "$SITE/terminal.html"

# 4) Copy PWA assets.
for f in manifest.webmanifest icon.svg icon-180.png; do
  [ -f "$HERE/pwa/$f" ] && cp "$HERE/pwa/$f" "$SITE/$f" && echo ">> copied $f"
done

# 5) (Re)load only com.rozum.ucc-control (no more ucc-web Python service).
UID_=$(id -u)
plist="$HOME/Library/LaunchAgents/com.rozum.ucc-control.plist"
launchctl bootout "gui/$UID_/com.rozum.ucc-control" 2>/dev/null || true
launchctl bootstrap "gui/$UID_" "$plist"
sleep 2
curl -sf --max-time 4 http://127.0.0.1:8411/ -o /dev/null -w "spa+api :8411 -> %{http_code}\n"
curl -sf --max-time 4 http://127.0.0.1:8411/control/status -o /dev/null -w "status  :8411 -> %{http_code}\n"
echo ">> done. open https://busi.tail1174e2.ts.net:8448/"
echo ">> (Tailscale: tailscale serve --bg --https=8448 http://127.0.0.1:8411)"
