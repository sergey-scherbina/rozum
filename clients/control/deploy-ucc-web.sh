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
#   center.ssc  ->  site/index.html   (browser SPA, reactive)
#   login.ssc                ->  site/login.html   (Face ID / WebAuthn page)
#   terminal.ssc             ->  site/terminal.html (xterm.js WebSocket terminal)
#   session.ssc              ->  site/session.html (raw-pane chat over an existing tmux session)
#   chat.ssc                 ->  site/chat.html    (phone chat app: starts + drives an agent on the model)
set -euo pipefail

SSC="${SSC:-/tmp/ssc-tk/bin/ssc}"
HERE="$(cd "$(dirname "$0")" && pwd)"
SITE="$HOME/.rozum/ucc/site"
BINDIR="$HOME/.rozum/bin"
BIN="$BINDIR/rozum-ctrl"
REPO="$(cd "$HERE/../.." && pwd)"

mkdir -p "$SITE" "$BINDIR"

# Bootstrap the ssc-tk launcher. When the operator's canonical launcher exists
# (~/work/my/scalascript/bin/ssc — kept fresh together with bin/lib by the
# scalascript repo itself), the /tmp launcher DELEGATES to it, so we can never
# again emit with a compiler that skews from the live std/plugins tree: the
# 2026-07-07 nav regression came exactly from a hand-rolled /tmp launcher pinning
# a Jun-29 ssc.jar against a Jul-7 std — the emitted SPA lost the
# `hashchange → _syncBridgeSignals()` hook and menu clicks stopped re-rendering.
# The heredoc below remains only as a fallback for hosts without the canonical
# launcher. Never rewrite a caller-provided $SSC (env override).
_SSC_ROOT_DEFAULT="$HOME/work/my/scalascript"
_SSC_JAR_DIR_DEFAULT="$HOME/work/my/scalascript/bin/lib"
_SSC_STD_DEFAULT="$HOME/work/my/scalascript/v1/runtime"
_SSC_CANONICAL="$_SSC_ROOT_DEFAULT/bin/ssc"
if [ "$SSC" = "/tmp/ssc-tk/bin/ssc" ] && [ -x "$_SSC_CANONICAL" ]; then
  mkdir -p "$(dirname "$SSC")"
  printf '#!/usr/bin/env bash\nexec "%s" "$@"\n' "$_SSC_CANONICAL" > "$SSC"
  chmod +x "$SSC"
elif [ ! -f "$SSC" ] && [ -d "$_SSC_JAR_DIR_DEFAULT" ]; then
  # Fallback (no canonical launcher): ssc.lib.path must point at the scalascript
  # root (not v1/) so the CLI auto-loads plugin .sscpkg files from
  # bin/lib/compiler/plugins/; ssc.std.path overrides std/ resolution independently.
  echo ">> bootstrapping $SSC (fallback jar launcher) ..."
  mkdir -p "$(dirname "$SSC")"
  cat > "$SSC" <<LAUNCHER
#!/usr/bin/env bash
exec java \\
  -Dssc.lib.path="$_SSC_ROOT_DEFAULT" \\
  -Dssc.std.path="$_SSC_STD_DEFAULT" \\
  -cp "$_SSC_JAR_DIR_DEFAULT/jars/*:$_SSC_JAR_DIR_DEFAULT/ssc.jar" \\
  scalascript.cli.ssc "\$@"
LAUNCHER
  chmod +x "$SSC"
fi

# SPA-only fast path: re-emit index.html (+ inject) WITHOUT rebuilding the Rust binaries or bouncing
# the model gateway. For UI-only changes (center.ssc / the injected CSS+i18n) this keeps the
# operator's resident model on :8089 untouched — index.html is served statically from disk, so a
# file swap takes effect with no restart. UCC_SPA_ONLY=1 skips the build + restart + gateway stages.
if [ "${UCC_SPA_ONLY:-0}" = "1" ]; then
  echo ">> UCC_SPA_ONLY=1 — skipping Rust binary builds (index/SPA-only redeploy)"
else
# 1) Build the thin dispatcher used by launchd. It execs rozum-gateway for control-serve.
# Install a binary where something may be executing the old one.
#
# Writing a Mach-O in place poisons the NEXT exec of that inode: macOS kills it with SIGKILL, so
# `launchctl list` shows -9, every start dies with rc=137, and the log is empty because nothing
# gets far enough to write to it. Caused live on 2026-08-05 by one hand-typed `cp`, and measured
# afterwards rather than assumed — the first three explanations were wrong:
#
#   cp over the file, then exec it   -> rc=137
#   cp to a sibling, mv -f, exec it  -> rc=0
#
# The RUNNING process survives either way (also measured), which is why this looks harmless right
# up until the service is restarted. `rm -f` before `cp` — what this script already did, for the
# same cache, 2026-07-07 — gets a fresh inode and is enough; rename is that plus atomicity, so a
# deploy has no window where the path is missing or half-written.
install_bin() {
  local src="$1" dst="$2"
  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst.new.$$"
  chmod +x "$dst.new.$$"
  mv -f "$dst.new.$$" "$dst"
}

echo ">> building rozum dispatcher ..."
( cd "$REPO" && cargo build -p rozum-cli --bin rozum )
install_bin "$REPO/target/debug/rozum" "$BIN"

# 1b) Build the ACTUAL engine binary the dispatcher execs for every subcommand incl. control-serve
# (`rozum-cli` is a pure-std shim with no dependency on `rozum-gateway` — rebuilding just the
# dispatcher above does NOT rebuild this, so a control.rs change would silently keep serving the
# OLD binary otherwise). `resolve()` in rozum-cli looks next to the dispatcher first, so dropping
# it in the same bin dir takes effect without touching a global `cargo install`ed copy elsewhere
# on PATH (which other rozum services may reference directly).
echo ">> building rozum-gateway (the engine binary control-serve actually runs) ..."
( cd "$REPO" && cargo build -p rozum --bin rozum-gateway --release )
# rm BEFORE cp: overwriting a running Mach-O in place poisons the kernel's per-inode
# code-signature cache and every subsequent exec dies with SIGKILL (rc=137, no output) —
# hit live 2026-07-07. A fresh inode sidesteps it.
install_bin "$REPO/target/release/rozum-gateway" "$BINDIR/rozum-gateway"

# 1c) nadia — an agent chip in the UCC is a promise the machine can run it, and the coder/session
# routes now REFUSE a launch whose agent is not on PATH rather than let it die as a 127 inside a
# log file. claude/codex/opencode are third-party CLIs the operator installs; nadia is ours, built
# from this tree, so the deploy owes it the same freshness as the gateway. It goes to ~/.cargo/bin —
# where `cargo install --path crates/nadia` puts it and where every launchd service's PATH already
# points — deliberately NOT to ~/.rozum/bin, which would create a second copy that drifts.
# It links no model engine (rozum-agent/core/gateway only), so this costs seconds.
echo ">> building + installing nadia (the agent chip in Coders/Sessions/chat) ..."
( cd "$REPO" && cargo build -p nadia --release )
NADIA_BIN="${NADIA_BIN:-$HOME/.cargo/bin/nadia}"
install_bin "$REPO/target/release/nadia" "$NADIA_BIN"
echo ">> nadia -> $NADIA_BIN"
fi

# Helper: compile a server-side .ssc file, serve briefly, capture HTML, shut down.
# Falls back to the pre-built site/<basename> if SSC compilation fails (e.g. when the
# SSC binary's interpreter doesn't register std/http.ssc extern defs like `route`).
emit_html() {
  # $2 (port) is legacy/ignored — the static login+terminal pages now `println(html)` to stdout,
  # which we capture directly. The old `ssc run` + serve-on-a-port + curl path silently fell back to
  # a stale pre-built copy whenever http `serve` broke (an ssc regression once shipped a MONTHS-old
  # terminal.html this way); stdout capture has no server, port, or timing dependency.
  local ssc_file="$1" out="$3"
  local base tmp
  base="$(basename "$out")"; tmp="$out.emit"
  echo ">> emitting $out (from $(basename "$ssc_file")) ..."
  if "$SSC" run "$ssc_file" > "$tmp" 2>/dev/null && grep -qi '<!doctype html' "$tmp"; then
    mv "$tmp" "$out"
  else
    rm -f "$tmp"
    if [ -f "$HERE/site/$base" ]; then
      cp "$HERE/site/$base" "$out"
      echo "  ✗ SSC emit failed (no <!doctype> in output) — used pre-built site/$base" >&2
    else
      echo "  ✗ SSC emit failed and no site/$base fallback" >&2
    fi
  fi
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

# 2) Compile the browser SPA (center.ssc → index.html).
# Emit to a temp file and mv into place only on success — a direct `> "$SITE/index.html"`
# truncates the LIVE page before ssc even starts, so an ssc failure (set -e) leaves
# production serving a 0-byte blank dashboard (this happened 2026-07-07).
#
# `emit-spa` was moved to the OPTIONAL tools/compatibility tier in the latest ScalaScript
# v2 — the standard `bin/ssc` now refuses it ("requires the optional ScalaScript
# tools/compatibility tier; run ssc-tools explicitly"), which silently broke this deploy
# (2026-07-13). Emit through `ssc-tools` when the canonical launcher exists; fall back to
# `$SSC` for an older toolchain that still carried emit-spa in the standard tier.
EMIT_SSC="$SSC"
if [ -x "$_SSC_ROOT_DEFAULT/bin/ssc-tools" ]; then EMIT_SSC="$_SSC_ROOT_DEFAULT/bin/ssc-tools"; fi
echo ">> emitting index.html (browser SPA, via $(basename "$EMIT_SSC")) ..."
"$EMIT_SSC" emit-spa --frontend react "$HERE/center.ssc" > "$SITE/index.html.new"
[ -s "$SITE/index.html.new" ] || { echo "✗ emit-spa produced an empty index.html — aborting (live page untouched)" >&2; exit 1; }
mv "$SITE/index.html.new" "$SITE/index.html"
echo ">> index.html: $(wc -c < "$SITE/index.html") bytes"
# Page canvas background: now set at the language level via `serve(view, port, extraCss)`
# (center.ssc passes `body{background:#111827}`), so the old post-emit sed of body{#fff} is gone.
# Models panel action buttons.  The `lcol` link text = field value, so CSS hides the raw text
# (font-size:0) and shows the real label via ::before.  Empty-value links are hidden with display:none.
# Note: </head> (line 7) is in the actual HTML head — NOT inside JS strings (those are </style> at ~4k).
# All post-emit HTML patches via Python — avoids sed & escaping pitfalls.
# The compiled SPA JS contains </head> and </body> in a template literal (line ~4134),
# so plain sed hits the wrong occurrences.  Python targets the FIRST </head> (real
# HTML head at line 7) and the LAST </body> (real HTML body close), skipping the ones
# inside the JS template literal.
python3 - "$SITE/index.html" <<'PYEOF'
import sys
p = sys.argv[1]
h = open(p).read()

css = (
    'a[href^="#/detail/"]{color:#60a5fa;text-decoration:none}'
    'a[href^="#/chat/"]{color:#60a5fa!important;text-decoration:none}'
    'a[href^="#/chat/"]:visited{color:#60a5fa!important}'
    '[data-ssc-datatable] thead{background:#1f2937!important}'
    '[data-ssc-datatable] th{color:#6b7280!important;border-bottom-color:#374151!important}'
    '[data-ssc-datatable]{max-width:100%;overflow-x:auto}'
    '[data-ssc-datatable] table{table-layout:auto;width:100%}'
    '[data-ssc-datatable] td,[data-ssc-datatable] th{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:4px 6px}'
    # First column (Model/name): header stays HORIZONTAL (short), body wraps at word/hyphen
    # boundaries. min-width stops the column collapsing so narrow that word-break splits the model
    # spec CHARACTER-BY-CHARACTER (the vertical-text bug on the phone model picker once a Driver
    # column was added). overflow-wrap:break-word wraps only when a token would overflow — not per
    # char — and the container scrolls horizontally if the row still exceeds the card width.
    '[data-ssc-datatable] th:nth-child(1){white-space:nowrap}'
    '[data-ssc-datatable] td:nth-child(1){white-space:normal;overflow-wrap:break-word;word-break:normal;min-width:110px;max-width:150px}'
    # Dashboard Models panel (targeted by its rows-path, the only stable handle now that load/unload
    # are rowPost BUTTONS, not gateway anchor links): Model + GiB + one button = 361px overflowed the
    # 316px card. Cap the model column (name still wraps whole) and shrink the action button — its
    # inline runtime style needs !important — so the row fits with the button fully on-screen.
    '[data-ssc-datatable][data-ssc-datatable-rows-path="models"] td:nth-child(1){max-width:120px}'
    '[data-ssc-datatable][data-ssc-datatable-rows-path="models"] tbody button{font-size:12px!important;padding:5px 8px!important}'
    # Single-column tables (chat messages, meetings): the one cell spans the full card and wraps
    # — the picker-oriented nth-child(1) max-width must NOT cap a chat message to 150px, so a
    # long message shows all at once instead of scrolling horizontally in one line.
    '[data-ssc-datatable] td:only-child{max-width:none;min-width:0;white-space:normal;overflow-wrap:break-word}'
    '[role="dialog"]{position:relative;max-height:82vh;overflow-y:auto}'
    '[role="dialog"] a[href="#/"]{position:absolute;top:8px;right:10px;display:inline-flex;align-items:center;justify-content:center;width:32px;height:32px;font-size:22px;font-weight:300;color:#e5e7eb;text-decoration:none;line-height:1;z-index:1;border-radius:50%;background:rgba(255,255,255,0.08);box-shadow:0 2px 8px rgba(0,0,0,0.55),0 1px 2px rgba(0,0,0,0.35)}'
    '#rozum-lang{position:fixed;bottom:16px;right:16px;display:flex;gap:1px;z-index:999;box-shadow:0 2px 8px rgba(0,0,0,.5)}'
    '.rl-btn{background:#21262d;color:#6e7681;border:1px solid #30363d;padding:4px 8px;cursor:pointer;font-size:11px;font-weight:600;letter-spacing:.03em}'
    '.rl-btn:first-child{border-radius:5px 0 0 5px}.rl-btn:last-child{border-radius:0 5px 5px 0}'
    '.rl-btn.active{background:#1f3a28;border-color:#2ea043;color:#56d364}'
    # CSS-driven translations for the load/unload button labels via lang attribute on <html>
)
# Global 401 interceptor: any API returning 401 → redirect to login.html.
# Skips auth endpoints and public routes to avoid redirect loops.
fetch_interceptor = (
    '<script>'
    '(function(){'
    'var _f=window.fetch;'
    'window.fetch=function(){'
    'var args=Array.from(arguments);'
    'return _f.apply(this,args).then(function(r){'
    'if(r.status===401){'
    'var url=String(args[0]||"");'
    'if(url.indexOf("/control/auth/")<0&&url.indexOf("/control/public/")<0)'
    '{location.href="/login.html";}'
    '}'
    'return r;'
    '});};'
    '})();'
    '</script>'
)
# FIRST </head> = real HTML head close (line 7); JS template literal </head> is further down
h = h.replace('</head>', '<style>' + css + '</style>' + fetch_interceptor + '</head>', 1)

# Translation maps: EN (native) → RU and UK
TR_RU = {
    # nav
    '🤖 Agents':'🤖 Агенты','💻 Coders':'💻 Кодеры','🖥 Sessions':'🖥 Сессии',
    '📊 Matrix':'📊 Матрица','🔐 Login':'🔐 Вход',
    # chat front door
    '💬 Chat':'💬 Чат','Open chat →':'Открыть чат →',
    'Talk to your local model, or drive a coding agent.':'Поговори с локальной моделью или запусти кодинг-агента.',
    # dashboard cards
    'Memory':'Память',
    'free:':'свободно:','limit:':'лимит:','used:':'занято:',
    '↻ refresh':'↻ обновить','Models':'Модели','Model':'Модель','Driver':'Драйвер','load':'загрузить','unload':'выгрузить','downloading…':'скачивание…','loading…':'загрузка…',
    # chat
    '← back':'← назад','send':'отправить','message…':'сообщение…',
    'Room incidents':'Инциденты комнаты','incident':'инцидент',
    # agents
    'Agents':'Агенты','Running agents':'Запущенные агенты','select':'выбрать',
    'Launch agent (model + room)':'Запустить агента','free RAM:':'свободно RAM:',
    'model:':'модель:','room…':'комната…',
    'reply policy: mention | always | manual':'когда отвечать: mention | always | manual',
    'persona / context (optional)…':'персона / контекст (необязательно)…',
    '🤖 launch (RAM check)':'🤖 запустить (проверка памяти)',
    'Stop agent':'Остановить агента','agent id (from list)…':'id агента (из списка)…','stop':'стоп',
    # coders
    'Coders':'Кодеры','Running coders':'Запущенные кодеры',
    'Launch coder (real work in repo)':'Запустить кодера (реальная работа)',
    'agent:':'агент:','folder:':'папка:','project':'проект',
    'project name…':'имя проекта…','task…':'задание…','＋ create':'＋ создать',
    '💻 launch (RAM check)':'💻 запустить (проверка памяти)',
    'Coder log':'Лог кодера','coder id (from list)…':'id кодера (из списка)…',
    '↻ log':'↻ лог','Stop coder':'Остановить кодера','coder id…':'id кодера…',
    # sessions
    'Sessions':'Сессии','Live sessions':'Живые сессии','Live sessions (terminal)':'Живые сессии (терминал)',
    '💬 chat':'💬 чат','🖥 term':'🖥 терм','🖥 terminal':'🖥 терминал','New interactive session':'Новая интерактивная сессия',
    'first task (optional)…':'первое задание (необязательно)…',
    '💬 launch session':'💬 запустить сессию','🖥 launch session':'🖥 запустить сессию','✕ close':'✕ закрыть','status':'статус',
    # model detail modal
    'Model details':'Характеристики модели',
    'Architecture:':'Архитектура:','Quantization:':'Квантизация:','Context:':'Контекст:',
    'Layers:':'Слои:','Size:':'Размер:','Resident:':'В памяти:','Notes:':'Примечание:',
    'no':'нет',
}
TR_UK = {
    '🤖 Agents':'🤖 Агенти','💻 Coders':'💻 Кодери','🖥 Sessions':'🖥 Сесії',
    '📊 Matrix':'📊 Матриця','🔐 Login':'🔐 Вхід',
    '💬 Chat':'💬 Чат','Open chat →':'Відкрити чат →',
    'Talk to your local model, or drive a coding agent.':'Поговори з локальною моделлю або запусти кодинг-агента.',
    'Memory':"Пам'ять",
    'free:':'вільно:','limit:':'ліміт:','used:':'зайнято:',
    '↻ refresh':'↻ оновити','Models':'Моделі','Model':'Модель','Driver':'Драйвер','load':'завантажити','unload':'вивантажити','downloading…':'завантаження…','loading…':'завантаження…',
    '← back':'← назад','send':'надіслати','message…':'повідомлення…',
    'Room incidents':'Інциденти кімнати','incident':'інцидент',
    'Agents':'Агенти','Running agents':'Запущені агенти','select':'вибрати',
    'Launch agent (model + room)':'Запустити агента','free RAM:':'вільно RAM:',
    'model:':'модель:','room…':'кімната…',
    'reply policy: mention | always | manual':'коли відповідати: mention | always | manual',
    "persona / context (optional)…":"персона / контекст (необов'язково)…",
    '🤖 launch (RAM check)':'🤖 запустити (перевірка RAM)',
    'Stop agent':'Зупинити агента','agent id (from list)…':'id агента (зі списку)…','stop':'стоп',
    'Coders':'Кодери','Running coders':'Запущені кодери',
    'Launch coder (real work in repo)':'Запустити кодера (реальна робота)',
    'agent:':'агент:','folder:':'папка:','project':'проект',
    'project name…':'назва проекту…','task…':'завдання…','＋ create':'＋ створити',
    '💻 launch (RAM check)':'💻 запустити (перевірка RAM)',
    'Coder log':'Лог кодера','coder id (from list)…':'id кодера (зі списку)…',
    '↻ log':'↻ лог','Stop coder':'Зупинити кодера','coder id…':'id кодера…',
    'Sessions':'Сесії','Live sessions':'Живі сесії','Live sessions (terminal)':'Живі сесії (термінал)',
    '💬 chat':'💬 чат','🖥 term':'🖥 терм','🖥 terminal':'🖥 термінал','New interactive session':'Нова інтерактивна сесія',
    "first task (optional)…":"перше завдання (необов'язково)…",
    '💬 launch session':'💬 запустити сесію','🖥 launch session':'🖥 запустити сесію','✕ close':'✕ закрити','status':'статус',
    'Model details':'Характеристики моделі',
    'Architecture:':'Архітектура:','Quantization:':'Квантизація:','Context:':'Контекст:',
    'Layers:':'Шари:','Size:':'Розмір:','Resident:':'У пам\'яті:','Notes:':'Примітка:',
    'no':'ні',
}

import json
tr_ru_js = json.dumps(TR_RU, ensure_ascii=False)
tr_uk_js = json.dumps(TR_UK, ensure_ascii=False)

# LAST </body> = real HTML body close
script = ('<script>'
    # i18n overlay: text-node replacement + placeholder translation
    'var _ROZUM_TR={'
    '"ru":' + tr_ru_js + ','
    '"uk":' + tr_uk_js +
    '};'
    'var _rlang=(localStorage.getItem("rozum_lang")||navigator.language.split("-")[0]||"en");'
    'if(!_ROZUM_TR[_rlang])_rlang="en";'
    'document.documentElement.lang=_rlang;'
    'function _rlApply(){'
    '  var map=_ROZUM_TR[_rlang];'
    '  if(!map)return;'
    '  var w=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,null);'
    '  var n;'
    '  while((n=w.nextNode())){'
    '    var tx=n.textContent.trim();'
    '    if(map[tx])n.textContent=map[tx];'
    '  }'
    '  document.querySelectorAll("[placeholder]").forEach(function(el){'
    '    if(map[el.placeholder])el.placeholder=map[el.placeholder];'
    '  });'
    '  document.querySelectorAll(".rl-btn").forEach(function(b){'
    '    b.classList.toggle("active",b.dataset.lang===_rlang);'
    '  });'
    '}'
    'function _rlSet(lang){'
    '  localStorage.setItem("rozum_lang",lang);'
    '  _rlang=_ROZUM_TR[lang]?lang:"en";'
    '  document.documentElement.lang=_rlang;'
    '  _rlApply();'
    '}'
    # lang toggle button (fixed bottom-right)
    '(function(){'
    '  var d=document.createElement("div");'
    '  d.id="rozum-lang";'
    '  ["en","ru","uk"].forEach(function(l){'
    '    var b=document.createElement("button");'
    '    b.className="rl-btn";b.textContent=l.toUpperCase();'
    '    b.dataset.lang=l;b.onclick=function(){_rlSet(l);};'
    '    if(l===_rlang)b.classList.add("active");'
    '    d.appendChild(b);'
    '  });'
    '  document.body.appendChild(d);'
    '})();'
    # Apply on load + on hash navigation
    'setTimeout(_rlApply,300);'
    'window.addEventListener("hashchange",function(){setTimeout(_rlApply,150);});'
    # MutationObserver for data tables (rows added dynamically)
    '(function(){'
    '  var pend=false,obs=new MutationObserver(function(ms){'
    '    if(pend)return;'
    '    var ok=ms.some(function(m){'
    '      return Array.from(m.addedNodes).some(function(n){'
    '        return n.nodeType===1&&(n.tagName==="TR"||n.tagName==="TBODY"||n.tagName==="TABLE");'
    '      });'
    '    });'
    '    if(!ok)return;'
    '    pend=true;requestAnimationFrame(function(){_rlApply();pend=false;});'
    '  });'
    '  document.addEventListener("click",function(e){'
    # Close-on-click-outside must fire only when the dialog is actually VISIBLE.
    # The modal sits in an always-present data-ssc-cond branch (display:none when
    # closed), and querySelector finds hidden nodes too — the unguarded version
    # sent EVERY page click to #/ (BUG-009: agent/model pickers "did nothing").
    # getClientRects().length is 0 inside display:none subtrees regardless of
    # position:fixed (unlike offsetParent).
    '    var _dlg=document.querySelector("[role=dialog]");'
    '    if(_dlg&&_dlg.getClientRects().length&&!e.target.closest("[role=dialog]"))'
    '    {window.location.hash="/"}'
    '  });'
    '  function init(){'
    '    _rlApply();'
    '    document.querySelectorAll("[data-ssc-datatable]").forEach(function(el){'
    '      obs.observe(el,{childList:true,subtree:true});'
    '    });'
    '  }'
    '  if(document.readyState==="loading")document.addEventListener("DOMContentLoaded",init);'
    '  else init();'
    '})();'
    '<' + '/script>')
i = h.rfind('</body>')
h = h[:i] + script + h[i:]

open(p, 'w').write(h)
PYEOF
check_js_syntax "$SITE/index.html"
check_js_runtime "$SITE/index.html"

# 2b) Compile the reactive chat SPA (chat.ssc → chat.html). It's a pure emit-spa page like
# index.html, so it rebuilds in an SPA-only redeploy too. Its fetchStreamSignal/intervalTick/forJson
# toolkit primitives are now in the canonical toolchain (scalascript main + staged bin/lib), so
# emit-spa reproduces the full reactive chat from source. FAIL-SAFE retained: if a toolchain
# regression makes the emit produce nothing, keep the live chat.html rather than aborting the deploy.
echo ">> emitting chat.html (reactive SPA, via $(basename "$EMIT_SSC")) ..."
"$EMIT_SSC" emit-spa --frontend react "$HERE/chat.ssc" > "$SITE/chat.html.new" 2>/dev/null || true
if [ -s "$SITE/chat.html.new" ] && grep -qi '<!doctype html' "$SITE/chat.html.new"; then
  mv "$SITE/chat.html.new" "$SITE/chat.html"
  echo ">> chat.html: $(wc -c < "$SITE/chat.html") bytes (reactive)"
  check_js_syntax "$SITE/chat.html"
else
  rm -f "$SITE/chat.html.new"
  echo "  ✗ chat.ssc emit-spa produced nothing — canonical toolchain missing fetchStreamSignal/forJson?  Kept the existing chat.html (live chat unchanged)." >&2
fi

if [ "${UCC_SPA_ONLY:-0}" = "1" ]; then
  echo ">> UCC_SPA_ONLY=1 — skipping other-page emits, service restart, and gateway bounce (resident model untouched)"
else
# 3) Compile login.ssc → login.html, terminal.ssc → terminal.html, session.ssc → session.html.
emit_html "$HERE/login.ssc"    8421 "$SITE/login.html"
emit_html "$HERE/terminal.ssc" 8422 "$SITE/terminal.html"
emit_html "$HERE/session.ssc"  8423 "$SITE/session.html"
check_js_syntax "$SITE/login.html"
check_js_syntax "$SITE/terminal.html"
check_js_syntax "$SITE/session.html"

# 4) Copy PWA assets.
for f in manifest.webmanifest icon.svg icon-180.png sw.js; do
  [ -f "$HERE/pwa/$f" ] && cp "$HERE/pwa/$f" "$SITE/$f" && echo ">> copied $f"
done

# 4b) Copy hand-authored site pages (admin, invite, matrix, view, coder-log).
#     These are NOT compiled from .ssc — they live in clients/control/site/ in the repo.
for f in admin.html invite.html coder-log.html matrix.html view.html; do
  [ -f "$HERE/site/$f" ] && cp "$HERE/site/$f" "$SITE/$f" && echo ">> copied $f"
done

# 5) (Re)load only com.rozum.ucc-control (no more ucc-web Python service).
UID_=$(id -u)

# bootout is ASYNC: launchctl returns before the job's slot is actually free, and bootstrapping
# into a slot that is still draining fails with "Bootstrap failed: 5: Input/output error". A fixed
# `sleep 1` is not enough when the job holds a multi-GB model — hit live 2026-07-27 deploying this
# very change: the gateway was booted out, both bootstrap attempts lost the race, and the service
# ended up GONE (nothing on :8089) until it was bootstrapped by hand, which then worked first try.
# Retry with a real budget instead of guessing a sleep.
rebootstrap() {
  local label="$1" plist="$2" i
  launchctl bootout "gui/$UID_/$label" 2>/dev/null || true
  for i in $(seq 1 10); do
    if launchctl bootstrap "gui/$UID_" "$plist" 2>/dev/null; then
      [ "$i" -gt 1 ] && echo ">> $label: bootstrap succeeded on attempt $i (slot was still draining)"
      return 0
    fi
    sleep 2
  done
  echo ">> ERROR: $label could not be bootstrapped after 20s" >&2
  launchctl bootstrap "gui/$UID_" "$plist" >&2   # once more, unsilenced, so the reason is visible
  return 1
}

plist="$HOME/Library/LaunchAgents/com.rozum.ucc-control.plist"
rebootstrap com.rozum.ucc-control "$plist" || exit 1
sleep 2
curl -sf --max-time 4 http://127.0.0.1:8411/ -o /dev/null -w "spa+api        :8411 -> %{http_code}\n"
# /control/status now requires auth (401 without a session is CORRECT, not a failure — see
# ucc-auth-status-leak) — /control/auth/status is the genuinely-public route to smoke-check instead.
curl -sf --max-time 4 http://127.0.0.1:8411/control/auth/status -o /dev/null -w "auth/status    :8411 -> %{http_code}\n"
curl -s  --max-time 4 http://127.0.0.1:8411/control/status -o /dev/null -w "status (expect 401 unauthed) :8411 -> %{http_code}\n"

# 5b) The always-available chat model — install/refresh com.rozum.gateway: a durable shared gateway
#     holding Qwen3.5-4B resident on :8089 so the phone chat (/chat.html -> coder/launch -> `rozum
#     launch`) reuses it instead of cold-loading per message. Auto-sleeps after 20 min idle (unloads
#     the model, keeps the daemon) — the operator's RAM policy. Template + rationale:
#     clients/control/launchd/com.rozum.gateway.plist. Reboot-safe: startup admission WAITS for RAM
#     headroom before loading (never overcommits a jetsam-pressure host, BUG-003).
gw_plist="$HOME/Library/LaunchAgents/com.rozum.gateway.plist"
cp "$HERE/launchd/com.rozum.gateway.plist" "$gw_plist"
# This is the one that actually loses the race — it holds a 4B model, so its slot drains slowly.
rebootstrap com.rozum.gateway "$gw_plist" || exit 1
# The model load is async (cold-loads Qwen ~30s); the daemon answers `gateway status` once resident.
echo ">> com.rozum.gateway installed (Qwen3.5-4B on :8089, idle-unload 20m) — warming in background"

# 5c) VERIFY the gateway actually came up. Without this, a job that can never exec is
#     indistinguishable from one that is still warming: launchd's KeepAlive respawns it forever,
#     the failure never reaches the job's own log (it dies before writing a line), and the ONLY
#     symptom is that everything pointing at :8089 (the phone chat, the messenger assistant
#     participants) silently stops answering. Hit live 2026-07-23 -> 2026-07-27: 36k respawns at
#     `last exit code = 78 (EX_CONFIG)` over 4 days while the same command by hand served fine —
#     a stale job registration against a replaced binary; bootout+bootstrap cleared it instantly.
#     See BUGS.md BUG-013.
gw_up=0
for _ in $(seq 1 45); do
  if lsof -nP -iTCP:8089 -sTCP:LISTEN >/dev/null 2>&1; then gw_up=1; break; fi
  sleep 2
done
if [ "$gw_up" = "1" ]; then
  echo "gateway        :8089 -> LISTEN"
else
  gw_state=$(launchctl print "gui/$UID_/com.rozum.gateway" 2>/dev/null | awk -F'= ' '/^\tstate = /{print $2; exit}')
  gw_exit=$(launchctl print "gui/$UID_/com.rozum.gateway" 2>/dev/null | awk -F'= ' '/last exit code/{print $2; exit}')
  if [ "$gw_state" = "running" ]; then
    echo ">> WARNING: com.rozum.gateway is running but :8089 is not listening after 90s (slow cold load?)" >&2
  else
    echo ">> ERROR: com.rozum.gateway is NOT running (state=$gw_state, last exit code=$gw_exit)." >&2
    echo "   It is respawning under KeepAlive and will never serve :8089. Check ~/.rozum-gateway.log;" >&2
    echo "   if it is empty, the job cannot exec its binary at all — re-run:" >&2
    echo "     launchctl bootout gui/$UID_/com.rozum.gateway && launchctl bootstrap gui/$UID_ $gw_plist" >&2
    exit 1
  fi
fi
fi

# Browser smoke gate — the 2026-07-07 bug classes (blank init, dead navigation, click-eater,
# empty formBody, missing status column) checked against the LIVE page in headless Chrome.
# Soft-skips when node/Chrome/token/puppeteer-core are absent; a real CHECK failure fails
# the deploy. UCC_SMOKE=0 to skip explicitly.
if [ "${UCC_SMOKE:-1}" = "1" ] && command -v node >/dev/null 2>&1; then
  echo ">> browser smoke (ucc-smoke.js) ..."
  ( cd "$HERE" && node ucc-smoke.js ) || { echo "✗ ucc-smoke FAILED — deployed page is broken" >&2; exit 1; }
fi
echo ">> done. open https://busi.tail1174e2.ts.net:8448/"
echo ">> (Tailscale: tailscale serve --bg --https=8448 http://127.0.0.1:8411)"
