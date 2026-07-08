# UCC test plan

The UCC is a browser SPA (emit-spa React) over a control API. **curl smokes pass while the UI is
broken** (learned repeatedly) — so the automated tests drive a real headless Chrome
(`puppeteer-core` + system Chrome, 390px phone viewport, `busi_device` SSO cookie) and assert on the
rendered DOM + the network traffic the taps actually produce. A thin API layer (`fetch`) checks the
control endpoints directly.

Run: `node clients/control/test/ucc-e2e.mjs` (control-serve must be live on :8411). It prints a
PASS/FAIL line per case and writes `DEFECTS.md` (machine-readable defect list). Exit non-zero if any
case fails.

## Surfaces & cases

**NAV** — SPA routing (the class of bug that "did nothing" / warped to `#/`)
- nav-home: `#/` renders h1 "control center" + the 5 nav links
- nav-routes: each of #/agents #/coders #/sessions routes to its heading (no warp to #/)
- nav-chat: a room link opens the chat view (composer present)
- nav-back: "← back" returns home

**MEMORY** — the residency panel
- mem-render: card titled Память/Memory with свободно/лимит/занято as GiB numbers
- mem-refresh: tapping ↻ issues a fresh GET /control/status  ← the reported defect
- mem-no-source: no leftover "источник/always-up" line

**MODELS** — one conditional action per row
- models-fit: table fits the 390px card, no horizontal overflow, every action button on-screen
- models-one-btn: each row shows exactly ONE action (load XOR unload), never zero/both
- models-name: full model spec visible (wraps), not clipped to a common prefix
- models-load-post: tapping load POSTs /control/gateway/load (not a GET navigation)
- models-feedback: on tap the button disables + shows … immediately

**CHAT** — meeting-room messages
- chat-wrap: a long message wraps (cell taller than one line), no horizontal scroll
- chat-send: composer posts to /chat/post

**PICKERS** — session/agent/coder launch forms
- picker-select: tapping a model row marks it selected (✓) and fills the bound field
- picker-close: the live-rows ✕ close action is a POST (async), present per row

**API** — control endpoints (fetch, no browser)
- api-status: /control/status returns residency{available,host_budget,committed} + models[]
- api-stop-guard: /control/gateway/stop with a live client → 409 with an error message
- api-stop-deadlease: a stale lease from a DEAD pid must NOT block stop (regression for the
  live_lease_count fix)

## Known defects this suite was seeded from (fix + keep as regressions)
- D1 mem-refresh: `incSignal` → `_IncSignal` event handler is never wired in the emit-spa mount →
  every `↻` (Память/Агенты/Кодеры/чат) is dead. Toolkit fix in scalascript.
- D2 api-stop-deadlease: `live_lease_count` counted a dead PID's still-fresh lease → 409. Fixed in
  `rozum-core/share.rs` (kill(pid,0) check).
- D3 (UX) rowPostAction failures (e.g. stop 409) are swallowed silently — no error reaches the phone.
