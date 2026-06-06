# Web UI improvements (merged)

> Merged from `web-bridge-ux-claude-code.md` (claude-code) and
> `web-ui-improvements-claude-code-2.md` (claude-code#2) after live
> agreement in room `clear-sage` on 2026-06-06.

## Overview

The Rozum room clients — web (`src/web/`) and TUI (`src/tui/mod.rs`) —
have three concrete usability gaps observed live in the `clear-sage`
session:

1. **Presence invisibility in web.** The room already produces
   `responding` / `polling` state (see
   `docs/specs/responding-indicator.md`) and the TUI renders it. The
   web bridge throws it away and the page has no UI for it. The
   operator (sergiy) saw two agents go silent for ~30 s while we
   composed long replies and assumed we had disconnected.
2. **Single-line input that scrolls horizontally.** Long messages
   disappear off the left edge with no way to see what was typed. The
   TUI has a fixed 3-line input area that does not grow either. The
   operator asked for "input like in Claude — expand upward, not
   scroll".
3. **Unusable scrollback in web.** New messages auto-snap the operator
   to the bottom mid-read; long agent replies push earlier context
   off-screen; nothing replays after page reload.

This spec covers the web client end-to-end, the TUI input area, and a
single proxy-level fix that makes every MCP-side agent appear as
"typing" automatically.

## Interface

### Web bridge (`src/web/mod.rs`)

The bridge currently broadcasts raw transcript entries as `{speaker,
content, injected}` JSON. Replace with tagged envelopes so the client
can dispatch on `kind`:

```
{ "kind": "msg",     "speaker", "content", "injected", "seq", "ts" }
{ "kind": "presence","responding": [ { participant_id, display_name, age_ms } ],
                    "polling":    [ { participant_id, display_name, age_ms } ] }
{ "kind": "history", "messages": [ ...msg, ...msg ] }
{ "kind": "joined",  "participant_id", "display_name" }
{ "kind": "left",    "participant_id", "display_name" }
```

`room_loop` is extended to:

- Forward every `turn.transcript_delta` entry as one `msg` event,
  including `seq` and `ts`.
- After each `wait_my_turn` reply (both `still_waiting` and turn case)
  diff `polling` / `responding` against the last snapshot and emit
  `presence` only when something changed. Presence is forwarded even
  on `still_waiting:true` — that is exactly the case where the human
  needs to see "X is typing" while no new message has arrived.
- Diff participant list against the last snapshot and emit `joined`
  / `left` envelopes.

New HTTP endpoint (preferred over reusing `wait_my_turn(since_seq:0)`
because the broadcast channel has fixed capacity and `wait_my_turn`
is the long-poll path, not a paging path):

```
GET /transcript?from_seq=<n>&limit=<n>  →  { messages: [ ...msg ] }
```

On WebSocket open the bridge sends a `history` envelope with the last
200 transcript entries (single `GET /transcript` under the hood). Used
again by the client when it scrolls to the top to lazy-load older
chunks.

The bridge calls `meeting.mark_responding` opportunistically when the
page sends `{kind:"typing"}` (server-side debounced to one call per
10 s per connection). This brings the human web user into the same
presence model as the MCP-side agents.

Incoming wire format from the page:

```
{ "kind": "msg",    "name", "content" }
{ "kind": "typing", "name" }
```

`meeting.submit` is called with `content` verbatim. The bridge no
longer prepends `[name]:` into the content — speaker identity lives in
the room (the bridge already joined with a display name).

### Web page (`src/web/index.html`)

Layout:

```
┌───────────────────────────────┐
│ topic · participant chips     │  ← header (small)
├───────────────────────────────┤
│                               │
│   #log — message list         │  ← grows, flex:1
│                               │
├───────────────────────────────┤
│ presence line                 │  ← single line, hidden when empty
├───────────────────────────────┤
│ name | <textarea> | ⇧ Send    │  ← input row
└───────────────────────────────┘
```

#### Presence row

Renders below the transcript, above the input. Glyph map:

| State            | Source                                          | Glyph      | ASCII fallback |
|------------------|-------------------------------------------------|------------|----------------|
| typing           | `responding[]`, `age_ms < 30000`                | `✏️`        | `*`            |
| waiting          | `polling[]`, no responding entry                | `⏳`        | `…`            |
| idle / connected | in participant list, no polling, no responding  | `●`        | `o`            |
| disconnected     | not in polling, last poll age > 60 s            | `○` (grey) | `·` (grey)     |

Each glyph is rendered inside a `<span class="presence-glyph">` whose
`font-family` cascades from `"Apple Color Emoji", "Segoe UI Emoji",
"Noto Color Emoji"` to monospace. Browsers without emoji support fall
through to the monospace ASCII glyph above (no JS detect).

Rendered as `"<name> <glyph> <verb…>"`, joined with " · " for multiple,
omits the current user. Hidden (`display:none`) when both arrays are
empty. Idempotent: last write per `participant_id` wins, so missed
deltas do not desync the UI.

#### Scrollback with sticky-bottom

`#log` keeps `overflow-y: auto`, plus:

- A `data-stick` attribute toggled on the `scroll` event:
  `"true"` when `scrollTop + clientHeight >= scrollHeight - 40` (px).
- On new `msg`: append at end, then auto-scroll **only if
  `stick="true"`**.
- When `stick="false"` and a `msg` arrives, a sticky "↓ N new" pill is
  shown. Clicking it scrolls to bottom and re-enables stick.
- When the user scrolls within 60 px of the top, fetch
  `GET /transcript?from_seq=<oldest_seen-1>&limit=200` and prepend,
  preserving scroll position.

#### Collapsing long messages

If a single message body exceeds 6 lines or 600 characters after
wrapping, render the first 4 lines and a `[expand ▾]` toggle. State
remembered per message in the DOM.

#### Join/leave system lines

`joined` / `left` envelopes render one dim, single-line system entry
in `#log` (`--- claude-code joined ---`). Participant chips in the
header update.

#### Autosize input

Replace `<input id="msg">` with `<textarea id="msg" rows="1">`:

- Grows on `input` event:
  `el.style.height='auto'; el.style.height = el.scrollHeight+'px'`.
- `max-height: 30vh` desktop, `20vh` mobile (`@media (max-width:480px)`).
- `word-wrap: anywhere`; never scrolls horizontally.
- `Enter` sends; `Shift+Enter` inserts a newline; `Esc` clears.
- After send: textarea collapses back to 1 row.
- Typing more than 1 character triggers one debounced
  `{kind:"typing"}` WebSocket frame (max once per 5 s, cleared on
  send).

### TUI (`src/tui/mod.rs`)

Replace the fixed input row constraint with a dynamic one. In
`draw_ui`:

```rust
let input_lines = textarea.lines().len() as u16;
let max_input_h = (area.height / 3).max(3);
let input_h    = input_lines.clamp(1, max_input_h);
let constraints = [Constraint::Min(1), Constraint::Length(input_h)];
```

Keybindings: `Enter` sends, `Alt+Enter` inserts a newline (provided by
`tui_textarea`), `Esc` cancels. `Ctrl+J` is intentionally not bound — it
is ASCII LF and would collide with `Enter` in raw mode. Long input
lines wrap inside the input area and never scroll horizontally.

The TUI already renders presence per
`docs/specs/responding-indicator.md` — no change in this spec.

### MCP proxy (`src/meeting/proxy.rs`)

The proxy currently forwards `meeting.mark_responding` only on
explicit caller request. Add: whenever the proxy returns a turn with
`your_turn: true` from `wait_my_turn` to the agent, also call
`meeting.mark_responding` on the agent's behalf, then refresh every
15 s until the agent's next `meeting.submit` / `meeting.leave` /
process exit.

This is the only protocol-side change in this spec. It is backwards
compatible: a manual `mark_responding` from the agent still works and
refreshes the timer identically. No new tools, no new fields in
existing tool payloads, no new events.

### Disk persistence (web bridge)

The bridge appends every transcript entry to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line
per turn). The `GET /transcript` endpoint reads from this file first
when the in-memory window is exhausted. Disabled when `--no-persist`
is passed to `rozum web` (default: on).

## Behavior

- [x] On WebSocket connect, the bridge sends a `history` envelope with
      the last 200 transcript entries before any live events.
- [ ] `room_loop` forwards `presence` envelopes on both
      `still_waiting:true` and turn cases when polling/responding diff
      from the last snapshot.
- [ ] Outgoing `msg` from the page is passed verbatim to
      `meeting.submit`; the bridge does not prepend a `[name]:` prefix.
- [ ] When the page sends `kind:"typing"`, the bridge calls
      `meeting.mark_responding`, debounced server-side to at most one
      call per 10 s per connection.
- [ ] Web presence row updates within 1 s of a `RespondingChanged` /
      `PollingChanged` event, using the four glyphs `✏️ ⏳ ● ○`.
- [ ] A participant whose last poll is older than 60 s shows as
      disconnected (grey `○`) until the next poll arrives.
- [x] When the user is scrolled to the bottom, a new `msg` event
      auto-scrolls. When scrolled away, the viewport stays put and a
      "↓ N new" pill appears, counting unread `msg` events.
- [x] Scrolling within 60 px of `#log` top triggers
      `GET /transcript?from_seq=<oldest_seen-1>&limit=200`; results
      are prepended without moving the viewport.
- [x] Messages whose body exceeds 6 lines OR 600 chars render with the
      first 4 lines visible and a `[expand ▾]` toggle.
- [ ] On `joined`/`left` envelopes the page renders one dim system
      line in `#log` and updates the header participant chips.
- [x] The web `#msg` textarea grows from 1 line up to `30vh`
      (`20vh` on `max-width:480px`) as the user types; the transcript
      shrinks to fit.
- [x] `Enter` sends; `Shift+Enter` inserts a newline; `Esc` clears.
- [x] The TUI input area grows from 1 line up to `max(3, area.height/3)`
      lines; transcript shrinks to fit. `Alt+Enter` inserts a newline;
      `Enter` sends; `Esc` cancels.
- [ ] Long input lines wrap inside the input area and never scroll
      horizontally. *(open — tui-textarea 0.7 lacks soft-wrap; pending
      decision on `tui-soft-wrap` slug.)*
- [ ] `mcp-proxy` agents that never call `mark_responding` themselves
      still appear as typing in both web and TUI for the entire
      duration of their reply.
- [x] With persistence on, `transcript.jsonl` contains every turn in
      order; relaunching the room and reloading the page replays the
      same content. `--no-persist` disables both the write and the
      read fallback.
- [ ] All existing TUI behavior covered in `turn-control-liveness.md`
      and `responding-indicator.md` continues to work.

## Out of scope

- Replacing the vanilla-JS shell with a framework (React, Svelte).
- Streaming partial token output into the transcript (separate spec;
  request/response semantics stay).
- New presence states beyond typing / waiting / idle / disconnected.
- `@mention` addressing or per-message reply-to threading.
- Visual redesign / colour scheme refresh.
- Authentication / multi-room session management.
- Replacing the `web` participant id with per-browser session ids.
- Mobile-specific gestures beyond what `viewport-fit=cover` already
  covers.

## Design

### Tagged envelopes over a single WebSocket

The bridge already holds a `broadcast::Sender<String>` of capacity 64.
Keep that channel, change the payload to tagged JSON. The page-side
dispatcher is a small `switch (env.kind)` — no framework needed; the
current vanilla JS shell is enough.

### `GET /transcript` REST endpoint, not `wait_my_turn(since_seq:0)`

A dedicated read endpoint is preferred because:

- The broadcast channel has fixed capacity (64) and lossy semantics —
  not a source of truth.
- `wait_my_turn` is the long-poll path; reusing it for one-shot
  history reads complicates both the bridge state machine and the
  room loop.
- Lazy top-scroll paging is natural with REST (`from_seq`, `limit`).
- Disk persistence (`transcript.jsonl`) can be read by the same
  endpoint as a fallback when the in-memory window is exhausted.

### Stickiness is client-side viewport state

The server should not need to know whether the operator is reading
history. The bridge keeps streaming; the page chooses whether to
follow the tail. This matches every modern chat UI.

### Forward presence as snapshot arrays, not as deltas

Idempotent under reconnect and lossy WebSocket: last snapshot per
`participant_id` wins. A `RespondingChanged` event stream would
require replay-from-state on reconnect; arrays do not.

### mcp-proxy as the auto-mark location

Agents forget to call `mark_responding`. The proxy is one place and
covers every current and future MCP-shaped agent (Claude Code, Codex,
…). A server-side heuristic on polling age would false-positive when
an agent is genuinely idle.

### Persistence on by default

The operator's expected experience is "my chat history sticks around".
Privacy escape hatch via `--no-persist`. Opt-in would be the wrong
default.

### Textarea over contenteditable

`<textarea>` gives correct mobile keyboard, IME composition, and
selection semantics for free. Autosize via `scrollHeight` is a
well-trodden pattern with no edge cases on monospace text.

## Decisions

- **`GET /transcript` REST endpoint** — chosen for clean separation
  from the long-poll path and natural pagination. Rejected:
  `wait_my_turn(since_seq:0)` on connect (overloads the long-poll
  path; no lazy load).
- **Tagged envelopes (`kind:`), single WebSocket** — chosen for
  simplicity; one dispatcher. Rejected: SSE for events + WS for chat
  (two channels to keep in sync); separate `type:` discriminator (cosmetic).
- **Bridge forwards arrays per-poll, not deltas** — chosen for
  idempotence under reconnect / packet loss. Rejected: forwarding
  individual `RespondingChanged` events.
- **Web autosize cap at `30vh` (`20vh` mobile)** — chosen because CSS
  expression is concrete and tracks viewport directly. Rejected:
  `min(8 rows, 40% viewport)` (same intent, more JS).
- **Collapse threshold 6 lines OR 600 chars** — chosen as a
  conservative trigger that catches long agent monologues without
  collapsing normal human messages. Rejected: 8 lines only (misses
  long single-line bodies).
- **`mark_responding` debounce 10 s on bridge, 15 s refresh in
  mcp-proxy** — chosen to sit safely under the 30 s server-side stale
  cleanup (`responding-indicator.md`) with margin for jitter. Invariant:
  every refresh path keeps `max_gap < 30 s`. Client-side WS typing
  debounce (5 s) + bridge-side `mark_responding` debounce (10 s) ≤ 15 s
  < 30 s; mcp-proxy 15 s refresh < 30 s. Any future tuning must preserve
  this margin.
- **TUI cap at `max(3, area.height/3)`** — chosen for symmetry with
  web's 30vh / 33% proportion. Rejected: fixed cap (different terminal
  sizes).
- **Disk persistence opt-out, not opt-in** — chosen to match operator
  expectations; `--no-persist` escape hatch. Rejected: opt-in.
- **`Enter` sends, `Shift+Enter` newline (web); `Alt+Enter` newline
  (TUI)** — matches Claude Code conventions per operator's explicit
  request. Rejected: `Ctrl+Enter` to send (slower). Rejected:
  `Ctrl+J` as TUI newline — `Ctrl+J` is ASCII LF and would collide
  with `Enter` in most terminal raw-mode configurations.
- **mcp-proxy as the auto-mark location, not the room** — chosen
  because the problem is "agents forget"; fixing at the proxy is one
  place. Rejected: server heuristic on polling age (false positives).
- **"working: ..." status convention for long offline work** — when an
  agent leaves the room to do file edits, spec writes, or local builds
  that take more than ~30 s, it MUST first `meeting.submit` a short
  line `working: <what>` and, on return, `meeting.submit` a `done:
  <result>` line. This is a convention, not a protocol change: the
  human in the room sees a transcript entry rather than 60 s of silence
  while `mark_responding` decays. Rejected: a new `meeting.set_status`
  tool (premature; not needed if the convention is honored). The
  convention is referenced from `AGENTS.md` so every future agent
  picks it up.

## Sprint plan

Seven sprint slugs, in priority order. Each is a single claim under
`vendor/agent-plugins/multi-agent/commands/multi-agent.md` and lands
in its own worktree branch.

1. **`web-presence-row`** — bridge forwards `presence` envelopes
   (and `joined` / `left`); page renders the presence line and
   participant chips with the four glyphs. Fixes complaint 1.
   Touches: `src/web/mod.rs`, `src/web/index.html`. No schema change
   to MCP. Must-have.

2. **`web-autosize-input`** — `<textarea>`, autosize with `30vh` /
   `20vh` cap, `Enter` / `Shift+Enter` / `Esc` keymap, no horizontal
   overflow. Fixes complaint 3 (web half). Touches:
   `src/web/index.html` only.

3. **`web-scrollback-sticky`** — `data-stick` heuristic + "↓ N new"
   pill + long-message collapse (6 lines / 600 chars / `[expand ▾]`).
   Fixes complaint 2 (upper half: don't yank). Touches:
   `src/web/index.html` only.

4. **`web-transcript-history`** — `GET /transcript?from_seq=&limit=`
   endpoint, `history` envelope on connect, top-scroll pagination.
   Fixes complaint 2 (lower half: see past viewport, survive reload).
   Touches: `src/web/mod.rs`, `src/web/index.html`.

5. **`tui-autosize-input`** — replace `Constraint::Length(3)` with
   dynamic `max(3, area.height/3)` clamp; `Alt+Enter` newline.
   Fixes complaint 3 (TUI half). Touches: `src/tui/mod.rs` only.

6. **`mcp-proxy-auto-mark`** — proxy calls `meeting.mark_responding`
   on agent's behalf when `wait_my_turn` returns `your_turn:true`,
   refreshes every 15 s until `submit` / `leave` / exit. Touches:
   `src/meeting/proxy.rs`. Backwards compatible. Second-order fix
   that prevents the "silent agent" UX repeating with any future
   MCP-shaped agent.

7. **`web-transcript-persist`** — append-only `transcript.jsonl`
   under `$XDG_STATE_HOME/rozum/rooms/<room>/`; `--no-persist` CLI
   flag; `GET /transcript` reads from file as fallback. Touches:
   `src/web/mod.rs`, CLI flag plumbing. Quality-of-life: persist
   across room restart.

Items 1–5 are the must-haves; 6 is the second-order fix; 7 is the
quality-of-life. Items 1–5 are independent and can be claimed in
parallel. Item 7 depends on 4 (REST endpoint already exists). All
remaining work uses one shared file (`index.html`) only in items 2,
3 (and a small touch in 1 and 4) — the order 1 → 4 → 2 → 3 minimises
rebases on `index.html`.

## Results

(Fill in after implementation per `/spec-dev verify` flow.)
