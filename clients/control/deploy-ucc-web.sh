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
# Falls back to the pre-built site/<basename> if SSC compilation fails (e.g. when the
# SSC binary's interpreter doesn't register std/http.ssc extern defs like `route`).
emit_html() {
  local ssc_file="$1" port="$2" out="$3"
  local base
  base="$(basename "$out")"
  echo ">> emitting $out (from $(basename "$ssc_file")) ..."
  "$SSC" run "$ssc_file" &
  local ssc_pid=$!
  sleep 4
  if ! curl -sf "http://127.0.0.1:${port}/" -o "$out"; then
    if [ -f "$HERE/site/$base" ]; then
      cp "$HERE/site/$base" "$out"
      echo "  (SSC emit failed — used pre-built site/$base)"
    else
      echo "  (warn: SSC emit failed and no site/$base fallback)"
    fi
  fi
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
# Emit to a temp file and mv into place only on success — a direct `> "$SITE/index.html"`
# truncates the LIVE page before ssc even starts, so an ssc failure (set -e) leaves
# production serving a 0-byte blank dashboard (this happened 2026-07-07).
echo ">> emitting index.html (browser SPA) ..."
"$SSC" emit-spa --frontend react "$HERE/control-center-live.ssc" > "$SITE/index.html.new"
[ -s "$SITE/index.html.new" ] || { echo "✗ emit-spa produced an empty index.html — aborting (live page untouched)" >&2; exit 1; }
mv "$SITE/index.html.new" "$SITE/index.html"
echo ">> index.html: $(wc -c < "$SITE/index.html") bytes"
# Framework gap (ucc-theme-bg): `serve(view, port)`'s extern signature has no extraCss param yet,
# so an .ssc app has no way to override the emitted base template's hardcoded
# `body{background:#fff}` — every card/text DOES pick up darkTheme correctly (theme.colors.surface/
# onSurface flow through `lower`), only the page canvas stays white behind/around them. Patch it
# post-emit to match this app's `darkTheme.colors.background` (#111827) until scalascript exposes
# extraCss (or derives body background from the theme) at the language level.
sed -i '' 's/body{margin:0;padding:0;background:#fff;/body{margin:0;padding:0;background:#111827;/' "$SITE/index.html"
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
    'a[href^="/control/gateway/load?model="]:not([href$="model="]){display:inline-block;padding:3px 6px;border-radius:4px;background:#3b82f6;color:#fff;text-decoration:none;cursor:pointer;white-space:nowrap;font-size:0}'
    'a[href^="/control/gateway/load?model="]:not([href$="model="])::before{content:"load";font-size:11px;color:#fff}'
    'a[href$="?model="]{display:none}'
    'a[href^="/control/gateway/stop?k="]:not([href$="?k="]){display:inline-block;padding:3px 6px;border-radius:4px;background:#374151;color:#fff;text-decoration:none;cursor:pointer;white-space:nowrap;font-size:0}'
    'a[href^="/control/gateway/stop?k="]:not([href$="?k="])::before{content:"unload";font-size:11px;color:#fff}'
    'a[href$="?k="]{display:none}'
    'a[href^="#/chat/"]{color:#60a5fa!important;text-decoration:none}'
    'a[href^="#/chat/"]:visited{color:#60a5fa!important}'
    '[data-ssc-datatable] thead{background:#1f2937!important}'
    '[data-ssc-datatable] th{color:#6b7280!important;border-bottom-color:#374151!important}'
    '[data-ssc-datatable]{max-width:100%;overflow:hidden}'
    '[data-ssc-datatable] table{table-layout:auto;width:100%}'
    '[data-ssc-datatable] td,[data-ssc-datatable] th{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:4px 6px}'
    '[data-ssc-datatable] td:nth-child(1),[data-ssc-datatable] th:nth-child(1){white-space:normal;word-break:break-word}'
    '[role="dialog"]{position:relative;max-height:82vh;overflow-y:auto}'
    '[role="dialog"] a[href="#/"]{position:absolute;top:8px;right:10px;display:inline-flex;align-items:center;justify-content:center;width:32px;height:32px;font-size:22px;font-weight:300;color:#e5e7eb;text-decoration:none;line-height:1;z-index:1;border-radius:50%;background:rgba(255,255,255,0.08);box-shadow:0 2px 8px rgba(0,0,0,0.55),0 1px 2px rgba(0,0,0,0.35)}'
    '#rozum-lang{position:fixed;bottom:16px;right:16px;display:flex;gap:1px;z-index:999;box-shadow:0 2px 8px rgba(0,0,0,.5)}'
    '.rl-btn{background:#21262d;color:#6e7681;border:1px solid #30363d;padding:4px 8px;cursor:pointer;font-size:11px;font-weight:600;letter-spacing:.03em}'
    '.rl-btn:first-child{border-radius:5px 0 0 5px}.rl-btn:last-child{border-radius:0 5px 5px 0}'
    '.rl-btn.active{background:#1f3a28;border-color:#2ea043;color:#56d364}'
    # CSS-driven translations for the load/unload button labels via lang attribute on <html>
    'html[lang=ru] a[href^="/control/gateway/load?model="]:not([href$="model="])::before{content:"загрузить"}'
    'html[lang=ru] a[href^="/control/gateway/stop?k="]:not([href$="?k="])::before{content:"выгрузить"}'
    'html[lang=uk] a[href^="/control/gateway/load?model="]:not([href$="model="])::before{content:"завантажити"}'
    'html[lang=uk] a[href^="/control/gateway/stop?k="]:not([href$="?k="])::before{content:"вивантажити"}'
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
    # dashboard cards
    'Gateway / residency':'Шлюз / резидентность',
    'source:':'источник:','available:':'доступно:','host budget:':'бюджет:','committed:':'используется:',
    '↻ refresh':'↻ обновить','Models':'Модели','Model':'Модель',
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
    'Sessions':'Сессии','Live sessions (terminal)':'Живые сессии (терминал)',
    '🖥 terminal':'🖥 терминал','New interactive session':'Новая интерактивная сессия',
    'first task (optional)…':'первое задание (необязательно)…',
    '🖥 launch session':'🖥 запустить сессию','Stop session':'Остановить сессию',
    'session id…':'id сессии…',
    # model detail modal
    'Model details':'Характеристики модели',
    'Architecture:':'Архитектура:','Quantization:':'Квантизация:','Context:':'Контекст:',
    'Layers:':'Слои:','Size:':'Размер:','Resident:':'В памяти:','Notes:':'Примечание:',
    'no':'нет',
}
TR_UK = {
    '🤖 Agents':'🤖 Агенти','💻 Coders':'💻 Кодери','🖥 Sessions':'🖥 Сесії',
    '📊 Matrix':'📊 Матриця','🔐 Login':'🔐 Вхід',
    'Gateway / residency':'Шлюз / резидентність',
    'source:':'джерело:','available:':'доступно:','host budget:':"бюджет хоста:",'committed:':'зайнято:',
    '↻ refresh':'↻ оновити','Models':'Моделі','Model':'Модель',
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
    'Sessions':'Сесії','Live sessions (terminal)':'Живі сесії (термінал)',
    '🖥 terminal':'🖥 термінал','New interactive session':'Нова інтерактивна сесія',
    "first task (optional)…":"перше завдання (необов'язково)…",
    '🖥 launch session':'🖥 запустити сесію','Stop session':'Зупинити сесію',
    'session id…':'id сесії…',
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
    # Load/stop button column merge
    '  var SV="[href^=\'/control/gateway/stop?k=\']:not([href$=\'?k=\'])";'
    '  function fix(){'
    '    document.querySelectorAll("[data-ssc-datatable]").forEach(function(w){'
    '      if(!w.querySelector("[href^=\'/control/gateway/\']"))return;'
    '      var th4=w.querySelector("thead th:nth-child(4)");'
    '      if(th4)th4.style.display="none";'
    '      w.querySelectorAll("tbody tr").forEach(function(tr){'
    '        var tds=tr.querySelectorAll(":scope>td");'
    '        if(tds.length<4)return;'
    '        var t3=tds[2],t4=tds[3];'
    '        if(t3.dataset.bf)return;'
    '        var stop=t4.querySelector(SV);'
    '        if(stop)t3.appendChild(stop);'
    '        t4.style.display="none";'
    '        t3.dataset.bf="1";'
    '      });'
    '    });'
    '    _rlApply();'
    '  }'
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
    '    fix();'
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

# 3) Compile login.ssc → login.html and terminal.ssc → terminal.html.
emit_html "$HERE/login.ssc"    8421 "$SITE/login.html"
emit_html "$HERE/terminal.ssc" 8422 "$SITE/terminal.html"
check_js_syntax "$SITE/login.html"
check_js_syntax "$SITE/terminal.html"

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
