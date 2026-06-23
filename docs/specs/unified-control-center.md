# Unified control center — one `.ssc` UI for TUI + web/PWA (and beyond)

Status: design (sunny-civet, 2026-06-23). Spec-dev; no code yet. Cross-repo (rozum + `scalascript`).

## Vision (operator)

The rozum control surface — TUI, web, `.ssc`, PWA — should be **one app, one `.ssc` codebase**,
compiled twice (once to TUI, once to web/React/PWA), covering **everything important in rozum**: not
just meetings, but models, gateway/residency, and whatever else matters. The ssc toolkit on Rust must
be able to compile to TUI.

## Key finding (recon, 2026-06-23) — this is mostly a SOLVED problem in ssc

ssc already ships **Tk**, a mature, framework-agnostic, reactive declarative UI toolkit, with **11
render backends and tests**. So the unification layer is *not* greenfield — most of it exists.

- **Neutral UI vocabulary** — `scalascript/runtime/std/ui/` (`layout`/`typography`/`containers`/
  `input`/`data`/`display`/`reactive`/`routing`/`theme`): `vstack`/`hstack`/`spacer`/`divider`,
  `heading`/`text`, `card*`, `textField`/`signalButton`/`signalActionButton`, `table`/`signalDataTable`,
  `badge`/`link`, `showWhen`/`signalText`. AST = `TkNode`/`View` (`nodes.ssc`). Reactivity =
  `ReactiveSignal[T]` + `EventHandler`.
- **The backend SPI** — `scalascript.frontend.FrontendFrameworkSpi`: `def emit(module: FrontendModule):
  EmittedSpa`. A backend pattern-matches the framework-agnostic `View` AST and emits target code.
- **Existing backends** (`scalascript/frontend/`): `react`, `solid`, `vue` (web), `swing`, `javafx`
  (desktop JVM), `swiftui` (Apple), `electron`, `custom`, + `core`/`toolkit`. Each ~1.1–1.2 k lines.
  - **`react` (1142 L)** lowers `ReactiveSignal→useState`, `View.Element→React.createElement`,
    `View.SignalText→useState interpolation`, `Fragment→React.Fragment`, with an HTML shell loading
    React+ReactDOM — i.e. **the operator's "web/React/PWA" target already exists.**
  - **SSR is built in** — `frontend/toolkit/.../Ssr.scala::renderToHtml(view)` — i.e. **the chosen
    hybrid (SSR shell + client islands) is already expressible** (SSR via `Ssr`, islands via `react`/
    `solid`).

**So the real gap is exactly what the operator named: a TUI render backend.** Everything else (neutral
UI, reactivity, web/React, SSR, even desktop/Apple) is present and tested.

## What's actually missing

1. **`frontend/tui` — a ratatui Tk backend** implementing `FrontendFrameworkSpi.emit`, lowering the
   same `View` AST onto **ratatui + crossterm** idioms (the new compiler-side work; ~swing-sized).
2. **rozum control-center written in Tk** (`std/ui`), replacing the meeting client's current low-level
   `std/http` (`route`/`serve`/`element`) approach — so it can target *both* web and TUI.
3. **A rozum control data-API** wired to Tk signals: rooms/messages/post, models/gateway/residency
   status + actions — consumed by both targets (TUI in-process, web over HTTP).

## Architecture

```
rozum control-center app (.ssc, ONE source, std/ui Tk):
    Model (signals)  +  view(): View   +  handlers (EventHandler)
        │  framework-agnostic View AST + ReactiveSignal + Style/Theme
        ▼   FrontendFrameworkSpi.emit(FrontendModule): EmittedSpa
   ┌───────────────────────────────┬────────────────────────────────┐
   │ WEB  (EXISTS)                 │ TUI  (NEW: frontend/tui)         │
   │  Ssr.renderToHtml → shell     │  emit → Rust ratatui app +       │
   │  + react/solid island app.js  │  crossterm event loop            │
   │  → PWA on the meeting server  │  → `rozum` terminal binary       │
   └───────────────────────────────┴────────────────────────────────┘
        │ data via Tk signals + std/ui/fetch-json
        ▼
   rozum control-API:  TUI → in-process / meeting-daemon socket / gateway calls
                       web → HTTP /api/* (rooms, messages, models, gateway, residency)
```

"Compiled twice" = the same Tk app run through two `FrontendFrameworkSpi` backends. Bonus: `swing`/
`javafx`/`swiftui`/`electron` come essentially for free later (native desktop / Apple / app-shell).

### The TUI backend (the one real lift)

Mirror `react`/`swing` (~1.1 k lines each). Lower the framework-agnostic `View` AST onto ratatui:

| Tk primitive | ratatui / crossterm lowering |
|---|---|
| `VStackNode`/`HStackNode`/`SpacerNode` | `Layout` + `Direction` + `Constraint` (grow → `Min(0)`/ratio) |
| `TextNode`/`HeadingNode`/`SignalTextNode` | `Paragraph`/`Span`, re-rendered from signal each frame |
| `CardNode`/`DividerNode` | `Block` with `Borders`, horizontal rule |
| `TableNode`/`DataTableNode` | ratatui `Table` |
| `TextFieldNode` | focusable input widget (track cursor + buffer in state) |
| `ActionButtonNode`/`SignalButtonNode` | focusable item; `Enter`/click-key → `EventHandler` |
| `ReactiveSignal[T]` | app state cell; mutation → request redraw |
| `EventHandler` | crossterm `KeyEvent` dispatch on the focused node |
| `RouterNode`/`LinkNode` | screen stack / tab state |
| `BadgeNode`/`SpinnerNode`/`Style`/`Theme` | styled `Span` (fg/bg/bold/dim); spinner = tick frame |

New concept the terminal forces that the web gets for free: **focus/keyboard navigation**. The Tk
model must express focusability (web maps it to tab/click; TUI needs an explicit focus ring + Tab/arrow
traversal). Decide whether this lives in the Tk core (so all backends agree) or only in the TUI backend.

## Chosen path (operator decisions, 2026-06-23)

- **Web model = hybrid SSR + islands** → `Ssr.renderToHtml` shell + `react`/`solid` island app for the
  interactive parts; reuses today's PWA serving on :8405.
- **First slice = meetings** → it already exists as both a web client and a 1389-line hand-written Rust
  ratatui TUI (`crates/rozum-meeting/src/tui`), giving a direct "hand-written vs Tk-generated" A/B.

## Proof-of-concept sequence

1. **Read-only message list** as a Tk component (`vstack(messages.map(msgRow))` bound to a signal fed
   by a tick/poll). Build it to **both** targets: the new `frontend/tui` Rust binary and the web island.
   Success = identical render + live update from one `.ssc` source. Proves the whole stack on the
   smallest surface.
2. **+ composer** (`textField` + submit `EventHandler`) — proves input + events + focus on both.
3. **+ room switcher / unread badges** — folds in the unread feature just shipped.
4. **Full meeting view in Tk** → reach parity with the hand-written Rust TUI, then retire it.
5. **Models/gateway/residency panel** — proves "beyond meetings" + forces the clean control-API.

## rozum side (mine)

- Rewrite the control-center in `std/ui` Tk (start: meetings).
- A `rozum` control data-API consumed by both targets: rooms/messages/post (have `/m`,`/u`,`/p` +
  the daemon socket), models/gateway/residency status + start/stop/swap (have `gateway status`,
  `meetings status`, the residency ledger). Normalize into `/api/*` JSON + in-process equivalents and
  bind to Tk signals via `std/ui/fetch-json`.
- The new `rozum` TUI binary target (built from the Tk app via the `tui` backend), coexisting with the
  hand-written TUI until parity, then replacing it.

## scalascript side (co-own with plucky-fox)

- New `frontend/tui` backend implementing `FrontendFrameworkSpi` (ratatui+crossterm lowering), modeled
  on `frontend/react` + `frontend/swing`, with the same per-node emit tests.
- Whatever focus/keyboard-navigation support belongs in the Tk core vs the TUI backend.
- Confirm how `ssc build` selects a frontend backend (the build-flag/config path — not yet located).

## Open questions / risks

- **Build-target selection — PARTLY ANSWERED (recon 2026-06-23):** it is NOT a `ssc build` CLI flag.
  The backend is `FrontendFrameworks.current()` (a registered framework; default `frontend-custom`,
  with `react`/`vue`/`solid`/`swing`/… registered impls). The app PATTERN: build a `View` with the
  `std/ui` builders (`vstack`/`hstack`/`text`/`card`/`signal`/…) and call the entrypoint intrinsic
  **`serve(view, port)`** (or `emit(view, outDir)` → `index.html` + `app.js`). So a `.ssc` Tk app is
  written against `std/ui` + `serve`; the `frontend-plugin` lowers it via `FrontendFrameworks.current()`.
  STILL FUZZY (needs plucky-fox / a toolchain run): the exact *user-facing build/run command* for a Tk
  `.ssc` web app (does `serve` run via interpret, or `ssc build-rust`/`build-js`?), how to *select* a
  non-default framework (react), and whether the new `frontend/tui` registers the same way. Live react
  E2E tests (`frontend/react/src/test/.../ReactCounterE2ETest`) are the working template for the build.
- **Signals → terminal redraw loop:** Tk reactivity is push (`ReactiveSignal`); ratatui is a pull
  redraw loop. Map signal-dirty → schedule redraw; confirm no per-frame rebuild blow-up.
- **Focus model:** terminal needs an explicit focus ring + key traversal with no web analog — core vs
  backend (above).
- **Fidelity / escape hatches:** terminal can't do images / arbitrary CSS. Default to the common
  subset; allow rare `when(platform)` branches (operator approved sparing use).
- **Web/PWA integration:** the existing meeting server is SSR-Rust (`std/http`); the Tk web target is
  `Ssr` + react/solid (JS). Reconcile: does the Tk web app serve from the same launchd :8405 process,
  and how does it coexist with / replace the current `meeting.ssc`?
- **Two output languages:** web target emits JS (react), TUI target emits Rust (ratatui). Fine for
  "compiled twice" (two backends), but the rozum control-API must be reachable from both (HTTP for JS,
  in-process for Rust).
- **Maturity of Tk:** backends are tested, but how production-ready is `std/ui` for a real multi-screen
  app? De-risk on the PoC before committing the full rewrite.

## Live binding + dual-target — RESOLVED / DELEGATED (2026-06-23, sunny-civet)

**Web target works (proven in headless Chrome against live `/control/status`):**

- Build/run a Tk `.ssc` web app: `ssc run --frontend react --mode client --server-url <BASE> app.ssc`
  → emits a self-contained `index.html` (runtime + fetch + `useState` inline; `app.js` empty) and runs a
  dev server that injects `globalThis.__sscBackendBaseUrl=<BASE>`. Durable static deploy: `curl` that
  `index.html` → `ssc serve <port> <dir>`. NOT `bin/ssc app.ssc` — the interpreter cannot run the
  reactive codegen externs (`signalText`/`fetchUrlSignal`) → `InterpretError: signalText(signal)`.
- Frontend is selected by front-matter `frontend: react` (NOT a `ssc build` flag).
- A signal gets a React `useState` ONLY if it is **mutated**. A fetch-only `refreshTick` must be anchored
  by a mutator (`signalActionButton(incSignal(refresh), "↻ refresh")`) or the page throws
  `ReferenceError: refresh is not defined` → blank page. (This was the "Пусто" bug.)
- Live scalar fields: `fetchJsonValue(name,url,tick,headers)` (`std/ui/fetch-json`) → navigable
  `JsonValue`; read via `computedSignal(() => st().get("residency").get("available_bytes").asString)` +
  `signalText_`. Stays inside the working `std/ui` + `serve(lower(view, theme), port)` entrypoint.
- jsdom is unreliable here (chokes on the runtime's Worker-based fetch); verify with real headless Chrome
  (`--headless=new --virtual-time-budget=7000 --dump-dom`).

**Dual-compile mechanism:** `react` (browser SPA) and `tui` (ratatui) are both `FrontendFrameworkSpi`
backends consuming ONE lowered `FrontendModule` (View IR). The view lowers once; each backend emits its
own form. tui's `emit()` throws — only `emitNative(Platform.Terminal)` → a ratatui+crossterm crate → `cargo run`.

**Portable subset (web ∩ tui) today:** `Column`/`Row`/`Text`/`Divider` + signals + `DataTable` +
`fetchUrlSignal` (tui slice 5 → ureq). NOT portable: model-DSL (`fetchJsonSignal`/`ModelView`/`ForModel`)
= react-only (renders empty in react client-mode; `def view()` not auto-discovered there).

**Two gaps blocking "one `.ssc` → web + tui" — DELEGATED to scalascript (room, 2026-06-23):**

- **[A] Portable dynamic table from a NESTED JSON field.** `dataTable(sig, cols)` needs a `FetchUrlSignal`
  whose value is a *list of row-maps* — works only when the array is at the response ROOT (local-first
  `dataTable(fetchUrlSignal("notes","/api/notes",tick), cols)`). Rendering a table from a nested field
  (`status.installed`) of a single fetch fails: `dataTable(computedSignal(()=>jsonStringify(st().get("installed"))), cols)`
  yields one EMPTY row (value is a JSON *string*, not row-maps). Need a nested-field source working in BOTH
  backends (e.g. `jsonTable(sig, path, cols)` or a normalizing computedSignal) + a tui DataTable-from-fetched-JSON
  test. (= scalascript's flagged "typed-model dynamic tables from fetched JSON, needs serde_json".)
- **[B] Common entrypoint.** Web = `serve(lower(view,theme),port)` (web-specific); tui = `emitNative`
  (no port); `--frontend tui` is NOT in `validFrontendNames` and there is NO native-emit CLI command
  (emit-* = `build-rust`/`emit-js`/`emit-rust`/`emit-spa`/`emit-scala`/`emit-spark` — none emit a
  terminal crate). Need one render-trigger (proposed `def view()` + runner dispatches serve-vs-emitNative
  by `frontend:`) + wire `--frontend tui`. Empirically confirmed on toolchain `8eea211f8` (tui slices 0-5).

## Registry

scalascript is registered in `REPOS.md` (`../scalascript`). The `frontend/tui` backend lands there;
the control-center app + control-API land in rozum. Coordinate the compiler-side work with plucky-fox.

## Durable deployment (2026-06-23, sunny-civet)

The web control-center runs as two macOS user LaunchAgents (RunAtLoad + KeepAlive — verified
respawn on kill), fronted by Tailscale serve (`--bg`, persists across reboots):

| Service | LaunchAgent | Listens | Command |
| --- | --- | --- | --- |
| control-API | `com.rozum.ucc-control` | `127.0.0.1:8411` | `~/.rozum/bin/rozum-ctrl gateway control-serve --port 8411` |
| SPA static  | `com.rozum.ucc-web`     | `127.0.0.1:8410` | `python3 -m http.server 8410 --directory ~/.rozum/ucc/site` |

Tailscale: `:8447 → 8410` (SPA), `:8448 → 8411` (control-API, `Access-Control-Allow-Origin: *`).
Operator opens `https://busi.tail1174e2.ts.net:8447/`. The SPA is a self-contained `index.html`
snapshot (built with the `:8448` Tailscale URL as the injected backend base) under
`~/.rozum/ucc/site/`; the control-API binary is a stable copy at `~/.rozum/bin/rozum-ctrl`
(rebuild-proof — `~/.cargo/bin/rozum` predates `control-serve`).

Plist templates: `clients/control/launchd/`. Rebuild + redeploy: `clients/control/deploy-ucc-web.sh`
(`SSC=<ssc cli> TS_BASE=<control-api url> ./deploy-ucc-web.sh`). NOTE: the control binary must be
built from `master` (has `gateway control-serve`); the warm workspace target makes it a ~15s incremental.
