# Web Bridge UX (claude-code variant)

## Overview

The `rozum` web bridge (`src/web/`) is currently a thin transcript pipe:
incoming `meeting.wait_my_turn` deltas are rebroadcast to all WebSocket
clients, outgoing client messages are wrapped into `meeting.submit`. It
ignores the presence data the room already exposes (`polling`,
`responding`), it does not replay history to late joiners, and the
HTML/JS shell has no UI for scrollback, long-message collapse, or a
multi-line input.

The human operator's three concrete complaints from session
`clear-sage` (2026-06-06):

1. "I didn't see your statuses that you were writing to me — I thought
   you'd dropped off."
2. "There's no scrollback — when I miss a message it scrolls off and
   I cannot read it anymore."
3. "I need the input row to work like in Claude — not scroll left, but
   grow upward."

This spec covers all three plus the smaller protocol-level mismatches
that surfaced while diagnosing the above.

## Interface

### Bridge (`src/web/mod.rs`)

New broadcast envelope: instead of broadcasting a transcript entry as
JSON, broadcast a tagged event so the client can dispatch:

```
{ "kind": "msg",     "speaker", "content", "injected", "seq", "ts" }
{ "kind": "presence","responding": [ { participant_id, display_name, age_ms } ],
                    "polling":    [ { participant_id, display_name, age_ms } ] }
{ "kind": "history", "messages": [ ...msg, ...msg ] }
{ "kind": "joined",  "participant_id", "display_name" }
{ "kind": "left",    "participant_id", "display_name" }
```

`room_loop` is extended to:

- Forward `turn.transcript_delta` as one `msg` event per entry, including
  `seq` and `ts`.
- After each `wait_my_turn` reply (both `still_waiting` and turn case),
  diff `polling` and `responding` against the last snapshot and emit
  `presence` only when something changed.
- Forward presence events even on `still_waiting:true` — that's exactly
  the case where the human needs to see "X is typing" while no new
  message has arrived.

New endpoint:

```
GET /transcript?from_seq=<n>  →  { messages: [ ...msg ] }
```

Server-side this calls `meeting.status` (or a new
`meeting.transcript(from_seq)` if needed) to pull the persisted
transcript. Used by the client when it opens or when the user scrolls
to the top.

On WebSocket open the bridge sends a `history` event with the last N
(default 200) transcript entries, so a freshly reloaded page is not
blank.

The bridge also calls `meeting.mark_responding` opportunistically: when
forwarding an outgoing client message that has non-empty content **and**
the client previously emitted a `typing` ping (see below), the bridge
calls `mark_responding` immediately before `meeting.submit`. This way
the operator sees other web clients as "typing" without the client
having to know the MCP tool.

### HTML/JS (`src/web/index.html`)

Layout (mobile-friendly, preserves current safe-area handling):

```
┌───────────────────────────────┐
│ topic / participants chips    │  ← header
├───────────────────────────────┤
│                               │
│   message list (#log)         │  ← grows
│                               │
├───────────────────────────────┤
│ presence line: "X is typing…" │  ← single line, hidden when empty
├───────────────────────────────┤
│ name | autosize textarea | ⇧  │  ← input row
└───────────────────────────────┘
```

Changes:

- `#log` becomes a virtualized-ish list (vanilla DOM is fine; no
  framework) with:
  - `overflow-y: auto`, fills available height.
  - On `msg` event: append at end, then **only** auto-scroll to bottom
    if the user was within 40 px of the bottom before the append.
    Otherwise show a sticky "↓ N new" pill that snaps to bottom on
    click.
  - Each message rendered as a block with speaker, timestamp (`HH:MM`),
    and content. Long bodies (> 6 lines / > 600 chars) get a "show
    more" toggle.
- Presence line subscribes to `presence` events. Format: `"claude-code
  is typing…"`, joined with ", " for multiple, omits the current user.
  Hidden (`display:none`) when both arrays are empty.
- Input row:
  - `<input id="msg">` is replaced by a `<textarea id="msg" rows="1">`
    that autosizes on `input` event (set `height` to `scrollHeight`,
    capped at e.g. 30vh). No horizontal scroll.
  - Enter sends; Shift+Enter inserts a newline.
  - Typing > 1 character sends a single `{kind:"typing"}` WebSocket
    frame; debounced to once per 5 s. Cleared by send.
- New endpoint `GET /transcript?from_seq=` is called when the user
  scrolls within 60 px of the top, to lazy-load older messages.

### Wire format (between page and bridge)

Incoming (client → bridge):

```
{ "kind": "msg",    "name", "content" }
{ "kind": "typing", "name" }
```

Outgoing (bridge → client): the four envelopes listed above.

## Behavior

- [ ] On WebSocket connect, the bridge sends a `history` event with the
  last 200 transcript entries before any live events.
- [ ] `room_loop` forwards `presence` events on both `still_waiting:true`
  and turn cases when polling/responding diff from the last snapshot.
- [ ] Outgoing `msg` from the page omits the speaker prefix in
  `meeting.submit.content`; the bridge passes content verbatim. The
  speaker is already carried by the room via `display_name` on the
  bridge's MCP join.
- [ ] When the page sends `kind:"typing"`, the bridge calls
  `meeting.mark_responding` on its room connection. Debounced server-side
  to at most one call per 10 s per connection.
- [ ] When the user is scrolled to the bottom of `#log`, a new `msg`
  event auto-scrolls. When the user is scrolled up, the page shows a
  "↓ N new" pill and does not move the viewport.
- [ ] When the user scrolls within 60 px of the top, the page fetches
  `GET /transcript?from_seq=<oldest_seen-1>` and prepends, preserving
  scroll position.
- [ ] Messages whose body exceeds 6 lines OR 600 chars render with a
  truncated body and a "show more" button.
- [ ] The input is a `<textarea>` that grows in height up to ~30vh,
  never scrolls horizontally; Enter sends, Shift+Enter inserts newline.
- [ ] Presence line shows `"<names> is/are typing…"` for `responding`,
  `"<names> waiting"` (smaller, dimmer) for `polling`-only entries,
  omitting the current user. Hidden when both empty.
- [ ] On `joined`/`left` events the page renders a single dim system
  line in `#log` (`--- claude-code joined ---`) and updates the
  header chips.
- [ ] Mobile: same behavior, presence line truncates with ellipsis,
  textarea capped at ~20vh.

## Out of scope

- Replacing the vanilla-JS shell with a framework (React, Svelte).
- Streaming partial token output into the message list (requires
  protocol-level change to `transcript_delta`).
- Multi-room navigation in the web UI; the bridge is per-room.
- Authentication / per-user identity beyond the editable `name` field.
- TUI changes — the TUI already renders presence via the in-process
  broadcast (`docs/specs/responding-indicator.md`); this spec is
  web-only.

## Design

The bridge already holds a `broadcast::Sender<String>` of 64 slots; we
keep that but change the payload to tagged JSON envelopes. The
client-side dispatcher is a small switch on `kind` — no framework
needed; current vanilla JS suffices.

History endpoint is preferred over "replay everything through the
broadcast channel" because:

- The broadcast channel has fixed capacity (64) and lossy semantics —
  not a source of truth.
- The room already persists the transcript (`responding-indicator.md`
  results note `state.rs` tests inspecting transcript); a thin read
  endpoint matches existing storage.
- Late joiners (page reload) hit `GET /transcript` once instead of
  spamming the broadcast.

Scroll-pinning is implemented with a single `isAtBottom()` check before
DOM insert, plus a sticky pill that listens on scroll to re-check. No
ResizeObserver needed.

Autosize textarea uses the
`textarea.style.height = 'auto'; textarea.style.height = textarea.scrollHeight + 'px'`
idiom, capped via CSS `max-height: 30vh`. This is the same pattern
Claude.ai uses for its composer.

## Decisions

- **Server-side debounce of `mark_responding`** — chosen because the
  page should not have to know about MCP timings (10 s window matches
  the 30 s stale cleanup in `responding-indicator.md` with margin).
  Rejected: client-side debounce only — different tabs would all hammer
  the room.
- **Tagged JSON envelopes over multiple WS endpoints** — chosen for
  simplicity; one socket, one dispatcher. Rejected: SSE for events +
  WS for chat (two channels to keep in sync).
- **Lazy `GET /transcript`** — chosen so opening a long-running room
  is fast; only paginate when the user actually scrolls up. Rejected:
  preload full history on connect.
- **No framework** — chosen because the surface area is small (one
  page, ~300 lines after this spec lands). Rejected: React/Svelte.

## Sprint sequencing

Suggested split into sprint slugs (smallest viable units, ordered by
user-visible impact):

1. `web-bridge-presence` — surface `polling`/`responding` in WebSocket
   stream, add presence line to `index.html`. Fixes complaint 1.
2. `web-bridge-history` — `GET /transcript` endpoint, replay last 200
   on connect, top-scroll pagination. Fixes complaint 2 lower half
   (history past viewport).
3. `web-bridge-scrollback-pin` — scroll-position preservation and
   "↓ N new" pill, plus long-message collapse. Fixes complaint 2 upper
   half (don't get yanked away).
4. `web-bridge-autosize-input` — textarea + Shift+Enter, autosize cap.
   Fixes complaint 3.
5. `web-bridge-typing-ping` — page-side `typing` event, bridge-side
   `mark_responding` call. Quality-of-life — bridges sergiy's manual
   keystroke through to other agents' presence view.
6. `web-bridge-join-leave` — render `joined`/`left` system lines,
   participant chips header.

Items 1–4 are independent and can be picked in parallel by separate
agents; 5 depends on 1; 6 depends on 1.

## Results

(filled in after implementation)
