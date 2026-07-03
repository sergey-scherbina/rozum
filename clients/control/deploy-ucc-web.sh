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
BINDIR="$HOME/.rozum/bin"
BIN="$BINDIR/rozum-ctrl"
REPO="$(cd "$HERE/../.." && pwd)"

mkdir -p "$SITE" "$BINDIR"

# 1) Build the thin dispatcher used by launchd. It execs rozum-gateway for control-serve.
echo ">> building rozum dispatcher ..."
( cd "$REPO" && cargo build -p rozum-cli --bin rozum )
cp "$REPO/target/debug/rozum" "$BIN"

# 1b) Build the ACTUAL engine binary the dispatcher execs for every subcommand incl. control-serve
# (`rozum-cli` is a pure-std shim with no dependency on `rozum-gateway` — rebuilding just the
# dispatcher above does NOT rebuild this, so a control.rs change would silently keep serving the
# OLD binary otherwise). `resolve()` in rozum-cli looks next to the dispatcher first, so dropping
# it in the same bin dir takes effect without touching a global `cargo install`ed copy elsewhere
# on PATH (which other rozum services may reference directly).
echo ">> building rozum-gateway (the engine binary control-serve actually runs) ..."
( cd "$REPO" && cargo build -p rozum --bin rozum-gateway --release )
cp "$REPO/target/release/rozum-gateway" "$BINDIR/rozum-gateway"

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

# Guard: parse every inline <script> block in an emitted HTML file with Node (syntax-check only, no
# DOM/fetch needed) and abort the deploy if any fails. Catches codegen bugs like the duplicate
# top-level `const agentModelList` (ucc-duplicate-const-fix) BEFORE they ship as a silent blank page
# — the browser gave no server-side signal at all when that shipped.
check_js_syntax() {
  local html_file="$1"
  command -v node >/dev/null 2>&1 || { echo "  (node not found — skipping JS syntax check for $html_file)" >&2; return 0; }
  node -e '
    const fs = require("fs");
    const vm = require("vm");
    const html = fs.readFileSync(process.argv[1], "utf8");
    const re = /<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g;
    let m, count = 0, failed = false;
    while ((m = re.exec(html))) {
      count++;
      try { new vm.Script(m[1], { filename: `${process.argv[1]}#inline-${count}` }); }
      catch (e) { failed = true; console.error(`  ✗ ${process.argv[1]} inline <script> #${count}: ${e.message}`); }
    }
    if (html.trim().length === 0 || count === 0) { failed = true; console.error(`  ✗ ${process.argv[1]}: empty or no inline <script> blocks found`); }
    if (failed) process.exit(1);
    console.error(`  ✓ ${process.argv[1]}: ${count} inline <script> block(s) parse OK`);
  ' "$html_file"
}

# Runtime init check for index.html only: runs the compiled JS in a Node sandbox with browser
# stubs to catch ReferenceErrors (undefined variables) that syntax-check misses. Catches cases
# like a refactor removing a val that other code still references — this bit us twice (modelSelectCols,
# catCard): syntax was valid but the page was blank on load due to a JS ReferenceError.
check_js_runtime() {
  local html_file="$1"
  command -v node >/dev/null 2>&1 || return 0
  node -e '
    const fs=require("fs"),vm=require("vm");
    const html=fs.readFileSync(process.argv[1],"utf8");
    const m=html.match(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/);
    if (!m) { console.error("  ✗ no inline script"); process.exit(1); }
    function el() {
      const e={style:{},classList:{add:()=>{},remove:()=>{},toggle:()=>{}}};
      e.setAttribute=()=>{}; e.getAttribute=()=>null; e.addEventListener=()=>{}; e.removeEventListener=()=>{};
      e.appendChild=(c)=>c; e.removeChild=()=>{}; e.insertBefore=(c)=>c; e.replaceChild=()=>{};
      e.querySelector=()=>el(); e.querySelectorAll=()=>[];
      e.remove=()=>{}; e.closest=()=>null;
      e.getBoundingClientRect=()=>({top:0,left:0,bottom:0,right:0,width:0,height:0});
      Object.defineProperty(e,"innerHTML",{set:()=>{},get:()=>""});
      Object.defineProperty(e,"textContent",{set:()=>{},get:()=>""});
      Object.defineProperty(e,"innerText",{set:()=>{},get:()=>""});
      return e;
    }
    const ctx={
      window:{addEventListener:()=>{},removeEventListener:()=>{},location:{hash:"",href:""},history:{pushState:()=>{},replaceState:()=>{}}},
      document:{getElementById:()=>el(),querySelector:()=>el(),querySelectorAll:()=>[],createElement:()=>el(),createTextNode:()=>el(),body:el(),head:el(),documentElement:el(),addEventListener:()=>{}},
      fetch:()=>Promise.resolve({ok:true,json:()=>Promise.resolve({}),text:()=>Promise.resolve("")}),
      console,setTimeout:()=>0,clearTimeout:()=>{},setInterval:()=>0,clearInterval:()=>{},
      navigator:{serviceWorker:undefined,userAgent:"node"},performance:{now:()=>0},location:{hash:""},
      MutationObserver:class{constructor(cb){}observe(){}disconnect(){}},
      ResizeObserver:class{constructor(cb){}observe(){}disconnect(){}},
      CustomEvent:class{constructor(t,d){this.type=t;this.detail=d?.detail}},
      Event:class{constructor(t){this.type=t}},
    };
    ctx.self=ctx; ctx.globalThis=ctx;
    try {
      new vm.Script(m[1]).runInContext(vm.createContext(ctx));
      console.error("  ✓ "+process.argv[1]+": runtime init OK");
    } catch(e) {
      console.error("  ✗ "+process.argv[1]+": RUNTIME ERROR: "+e.message);
      process.exit(1);
    }
  ' "$html_file"
}

# 2) Compile the browser SPA (control-center-live.ssc → index.html).
echo ">> emitting index.html (browser SPA) ..."
"$SSC" emit-spa --frontend react "$HERE/control-center-live.ssc" > "$SITE/index.html"
echo ">> index.html: $(wc -c < "$SITE/index.html") bytes"
# Framework gap (ucc-theme-bg): `serve(view, port)`'s extern signature has no extraCss param yet,
# so an .ssc app has no way to override the emitted base template's hardcoded
# `body{background:#fff}` — every card/text DOES pick up darkTheme correctly (theme.colors.surface/
# onSurface flow through `lower`), only the page canvas stays white behind/around them. Patch it
# post-emit to match this app's `darkTheme.colors.background` (#111827) until scalascript exposes
# extraCss (or derives body background from the theme) at the language level.
sed -i '' 's/body{margin:0;padding:0;background:#fff;/body{margin:0;padding:0;background:#111827;/' "$SITE/index.html"
# Models panel action buttons. The link text IS the field value (spec / "STOP"), but CSS
# overrides it with the correct label via ::before and hides the raw text with font-size:0.
# Empty hrefs (?model= or ?k=) are produced for the inapplicable action and hidden.
sed -i '' 's|</style>|a[href^="/control/gateway/load"]{display:inline-block;padding:2px 10px;border:1px solid #374151;border-radius:4px;background:#1f2937;text-decoration:none;font-size:0;cursor:pointer}a[href^="/control/gateway/load"]:hover{background:#374151;border-color:#6b7280}a[href^="/control/gateway/load"]:not([href$="model="])::before{content:"загрузить";font-size:13px;color:#d1d5db}a[href$="?model="]{display:none}a[href^="/control/gateway/stop"]{display:inline-block;padding:2px 10px;border:1px solid #374151;border-radius:4px;background:#1f2937;text-decoration:none;font-size:0;cursor:pointer}a[href^="/control/gateway/stop"]:hover{background:#374151;border-color:#6b7280}a[href="/control/gateway/stop?k=STOP"]::before{content:"выгрузить";font-size:13px;color:#ef4444}a[href$="?k="]{display:none}</style>|' "$SITE/index.html"
check_js_syntax "$SITE/index.html"
check_js_runtime "$SITE/index.html"

# 3) Compile login.ssc → login.html and terminal.ssc → terminal.html.
emit_html "$HERE/login.ssc"    8421 "$SITE/login.html"
emit_html "$HERE/terminal.ssc" 8422 "$SITE/terminal.html"
check_js_syntax "$SITE/login.html"
check_js_syntax "$SITE/terminal.html"

# 4) Copy PWA assets.
for f in manifest.webmanifest icon.svg icon-180.png sw.js; do
  [ -f "$HERE/pwa/$f" ] && cp "$HERE/pwa/$f" "$SITE/$f" && echo ">> copied $f"
done

# 5) (Re)load only com.rozum.ucc-control (no more ucc-web Python service).
UID_=$(id -u)
plist="$HOME/Library/LaunchAgents/com.rozum.ucc-control.plist"
launchctl bootout "gui/$UID_/com.rozum.ucc-control" 2>/dev/null || true
sleep 1  # bootout is async; bootstrapping immediately after can race and fail with "Input/output error"
if ! launchctl bootstrap "gui/$UID_" "$plist"; then
  echo ">> bootstrap failed, retrying once ..." >&2
  sleep 1
  launchctl bootstrap "gui/$UID_" "$plist"
fi
sleep 2
curl -sf --max-time 4 http://127.0.0.1:8411/ -o /dev/null -w "spa+api        :8411 -> %{http_code}\n"
# /control/status now requires auth (401 without a session is CORRECT, not a failure — see
# ucc-auth-status-leak) — /control/auth/status is the genuinely-public route to smoke-check instead.
curl -sf --max-time 4 http://127.0.0.1:8411/control/auth/status -o /dev/null -w "auth/status    :8411 -> %{http_code}\n"
curl -s  --max-time 4 http://127.0.0.1:8411/control/status -o /dev/null -w "status (expect 401 unauthed) :8411 -> %{http_code}\n"
echo ">> done. open https://busi.tail1174e2.ts.net:8448/"
echo ">> (Tailscale: tailscale serve --bg --https=8448 http://127.0.0.1:8411)"
