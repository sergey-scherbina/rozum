# Web UI improvements — claude-code#2's draft

## Overview

The current Rozum room clients (web at `src/web/index.html` and TUI at
`src/tui/mod.rs`) have three concrete usability gaps observed live in the
`clear-sage` meeting on 2026-06-06:

1. **Presence invisibility.** The `responding-indicator` spec is implemented
   on the server side and even the TUI uses it, but the web client renders
   nothing. The operator (sergiy) saw two agents go silent for ~30 s while we
   composed long replies and assumed we had disconnected.
2. **Single-line input that scrolls horizontally.** Long messages disappear off
   the left edge of the box; there is no way to see/edit what was already
   typed. The TUI has a fixed 3-line input area that does not grow either.
3. **Scrollback works mechanically but is unusable.** New messages auto-snap
   the operator to the bottom mid-read, long agent replies push earlier
   context off-screen, and there is no way to recover history after reload.

This spec covers the web client end-to-end and the TUI input only. Server-
side protocol additions are limited to one optional helper
(`meeting.mark_responding` auto-emission inside `mcp-proxy`) so existing
agents work without code changes.

## Interface

### Web (`src/web/index.html` + `src/web/mod.rs`)

#### Presence row

Render below the transcript, above the input:

```
sergiy ● claude-code ✏️ typing… claude-code#2 ⏳ waiting
```

Icons map from existing `wait_my_turn` / `meeting.status` data:

| State              | Source                                             | Glyph      |
|--------------------|----------------------------------------------------|------------|
| typing             | `responding[]` entry, `age_ms < 30000`             | `✏️`        |
| waiting            | `polling[]` entry, no responding entry             | `⏳`        |
| idle / connected   | in participant list, no polling, no responding     | `●`        |
| disconnected       | not in polling, last poll age > 60 s               | `○` (grey) |

The web client must consume `responding` / `polling` payloads. The bridge in
`src/web/mod.rs` currently only forwards `transcript_delta`; it must also
forward the `polling` and `responding` arrays from every `wait_my_turn`
response as a separate WebSocket message type:

```json
{ "type": "presence",
  "responding": [{ "participant_id": "...", "display_name": "...", "age_ms": 1234 }],
  "polling":    [{ "participant_id": "...", "display_name": "...", "age_ms": 1234 }] }
```

`transcript_delta` payloads gain a `type: "message"` discriminator for clarity.

#### Autosize input

Replace `<input id="msg">` with a `<textarea id="msg">` that:

- starts at 1 visible row;
- grows upward up to `min(8, viewport_height * 0.4)` rows as text wraps;
- shows a scrollbar once content exceeds that cap;
- never scrolls horizontally (`word-wrap: anywhere`, no `overflow-x`);
- `Enter` sends, `Shift+Enter` inserts a newline, `Esc` clears.

Sizing is recalculated on every `input` event by setting
`textarea.style.height = 'auto'` then `textarea.style.height = textarea.scrollHeight + 'px'`,
clamped to the cap. The transcript container keeps `flex: 1` so it shrinks
as input grows.

#### Scrollback with sticky-bottom

The `#log` container keeps `overflow-y: auto`, plus:

- `data-stick="true|false"` attribute updated on `scroll` event: `true` when
  `scrollTop + clientHeight >= scrollHeight - 4`;
- new message handler appends, then auto-scrolls **only if `stick="true"`**;
- when `stick="false"` and a new message arrives, a small floating chip
  appears: `↓ 3 new`. Clicking it scrolls to bottom and re-enables stick.

#### Collapsing long messages

If a single message has more than 8 lines after wrapping, it renders with the
first 4 lines visible and a `[expand ▾]` footer. The expanded state is
remembered per-message in the DOM (no server change).

#### Transcript hydration on reconnect

The bridge gains a hydration message sent immediately after WebSocket
connect: it long-polls `meeting.wait_my_turn` with `since_seq: 0` and pushes
the full transcript history before subscribing to new entries. The client
clears `#log` and replays. No new MCP tool needed.

#### Persistence on disk (optional)

The bridge appends every transcript entry to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line per
turn). This persists across reloads even if the room process exits. Disabled
when `--no-persist` is passed to `rozum web`.

### TUI (`src/tui/mod.rs`)

#### Autosize input area

`draw_ui` currently uses `Constraint::Length(3)` for the input row. Replace
with `Constraint::Length(clamp(textarea.lines().len() as u16, 1, max_input_h))`
where `max_input_h = max(3, area.height / 3)`. The transcript area shrinks
proportionally.

Keybindings stay as-is: `Enter` sends, `Alt+Enter` (or `Ctrl+J`) inserts a
newline (already supported by `tui_textarea`), `Esc` cancels.

No horizontal scrolling: `tui_textarea` already wraps; verify and add a wrap
flag if disabled.

### Server / proxy

#### Auto `mark_responding` in `mcp-proxy`

`src/meeting/proxy.rs` currently forwards `meeting.mark_responding` as a
manual call. Add: whenever the proxy returns `your_turn: true` from
`wait_my_turn` to the agent, also call `meeting.mark_responding` on the
agent's behalf, then schedule a refresh every 15 s until the next
`meeting.submit` / `meeting.leave` / process exit.

This is the only protocol-side change. It is backwards compatible: a manual
`mark_responding` from the agent still works and refreshes the timer
identically.

No new tools, no new fields in existing tool payloads, no new events.

## Behavior

- [ ] Web presence row updates within 1 s of a `RespondingChanged` /
      `PollingChanged` event on the server.
- [ ] When a participant's last poll is older than 60 s, the web shows them
      as disconnected (grey `○`).
- [ ] The web `#msg` textarea grows up to the cap as the user types; the
      transcript shrinks to fit.
- [ ] `Enter` sends; `Shift+Enter` adds a newline; `Esc` clears the textarea.
- [ ] Reading-mid-scroll: when the user is scrolled away from the bottom,
      new incoming messages do not move the viewport; a "↓ N new" chip
      appears and the count increments.
- [ ] Messages longer than 8 wrapped lines render collapsed with `[expand ▾]`.
- [ ] After page reload the transcript is rehydrated to the full history.
- [ ] With persistence on, transcript.jsonl contains every turn in order;
      relaunching the room and reloading the page replays the same content.
- [ ] TUI input area grows from 1 line up to `max(3, area.height/3)` lines
      as the user types multi-line content; transcript shrinks to fit.
- [ ] `mcp-proxy` agents that never call `mark_responding` still appear as
      typing in both web and TUI for the entire duration of their reply.
- [ ] All existing TUI behavior covered in `turn-control-liveness.md` and
      `responding-indicator.md` continues to work.

## Out of scope

- Streaming partial submit (token-by-token transcript). Tracked separately;
  this spec keeps request/response semantics.
- New presence states beyond typing/waiting/idle/disconnected. No "away" or
  "do not disturb."
- `@mention` addressing or per-message reply-to threading.
- Visual redesign / color scheme refresh.
- Mobile-specific gestures beyond what `viewport-fit=cover` already covers.
- Authentication / multi-room session management.
- Replacing the `web` participant id with per-browser session ids.

## Design

### Why a flat `presence` message type instead of diffing transcript events

The server already emits `RespondingChanged` / `PollingChanged` events on the
internal broadcast channel, but the web bridge (`room_loop` in
`src/web/mod.rs`) only forwards `transcript_delta`. The cheapest fix is to
also forward `responding` and `polling` arrays from every `wait_my_turn`
response. They are already present in the payload; we are throwing them away
today. A WebSocket presence message is rendered idempotently — last write
wins per `participant_id` — so missed events do not desync the UI.

### Why textarea instead of a contenteditable div

Plain `<textarea>` gives us correct mobile keyboard behavior, IME composition,
and selection semantics for free. A `contenteditable` div would require
re-implementing all three. Autosize via `scrollHeight` is a well-trodden
pattern with no edge cases on monospace text.

### Why client-side stickiness instead of server-driven cursor

Stickiness is purely viewport state. Servers do not need to know whether the
operator is reading history; the bridge keeps streaming and the client
chooses whether to follow. This matches the way every modern chat UI works.

### Why hydration via `wait_my_turn(since_seq: 0)` instead of a new endpoint

`wait_my_turn` already returns `transcript_delta` for any `since_seq` lower
than the server's. We don't need a new MCP tool, we just need the bridge to
ask for the full history on connect. The room already keeps the full
transcript in memory.

### Why mcp-proxy heartbeat instead of agent-side discipline

Agents forget to call `mark_responding`. The proxy lives in our codebase and
runs whether the agent is Claude Code, Codex, or anything else MCP-shaped. A
single fix there covers all current and future agents. The proxy already
owns the `wait_my_turn` ↔ agent round trip, so adding a fire-and-forget
mark + refresh timer is small and contained.

## Decisions

- **Web client consumes existing `responding` / `polling` data** — chosen
  because the data is already produced and surfaced in `meeting.status` and
  `wait_my_turn` payloads. Rejected: a new `presence` MCP tool (redundant).
- **Bridge forwards arrays per-poll, not deltas** — chosen for idempotence
  under reconnect and packet loss. Rejected: forwarding individual
  `RespondingChanged` events (need to replay state on reconnect anyway).
- **mcp-proxy is the auto-mark location, not agents** — chosen because the
  problem is "agents forget"; fixing it at the agent is per-agent work, the
  proxy is one place. Rejected: server-side heuristic on polling age
  (false positives when an agent really is idle).
- **Disk persistence behind opt-out flag, not opt-in** — chosen because the
  operator's expected experience is "my chat history sticks around." Privacy
  escape hatch via `--no-persist`. Rejected: opt-in (wrong default).
- **Web autosize cap at min(8 rows, 40% of viewport)** — chosen so the
  transcript stays usable on small mobile viewports while allowing long
  composition on desktop. Rejected: uncapped (transcript disappears).
- **TUI autosize cap at `max(3, area.height/3)`** — chosen for symmetry with
  the web cap proportion. Rejected: fixed cap (different terminal sizes).
- **`Enter` sends, `Shift+Enter` newline in web; `Enter` sends, `Alt+Enter`/
  `Ctrl+J` newline in TUI** — chosen to match Claude Code conventions per
  the operator's explicit request. Rejected: `Ctrl+Enter` to send (slower).

## Sprint plan

Tasks to add to `SPRINT.md` (in priority order; each is a single claim):

1. **`web-presence-row`** — bridge forwards `responding`/`polling`; web
   renders typing/waiting/idle/disconnected glyphs. Touches:
   `src/web/mod.rs`, `src/web/index.html`. No schema changes.
2. **`web-autosize-input`** — textarea growing upward, Enter/Shift-Enter
   keymap, no horizontal overflow. Touches: `src/web/index.html` only.
3. **`web-scrollback-sticky`** — sticky-bottom heuristic + "↓ N new" chip.
   Touches: `src/web/index.html` only.
4. **`web-transcript-hydrate`** — bridge calls `wait_my_turn(since_seq: 0)`
   on connect, web replays history. Touches: `src/web/mod.rs`,
   `src/web/index.html`.
5. **`tui-autosize-input`** — replace `Constraint::Length(3)` with dynamic
   line-count constraint. Touches: `src/tui/mod.rs` only.
6. **`mcp-proxy-auto-mark`** — proxy emits `mark_responding` with 15s
   refresh while agent holds an active turn. Touches: `src/meeting/proxy.rs`.
7. **`web-collapse-long`** — collapse messages >8 lines with expand toggle.
   Touches: `src/web/index.html` only.
8. **`web-transcript-persist`** — append-only `transcript.jsonl` with
   `--no-persist` flag. Touches: `src/web/mod.rs`, CLI flag plumbing.

Tasks 1–5 are the must-haves; 6 is the second-order fix that prevents the
"silent agent" UX; 7–8 are quality-of-life.

Each task gets its own claim/branch/worktree per `multi-agent` protocol. No
two tasks above conflict on the same file beyond `index.html`, where the
sequence 2 → 3 → 7 is the natural order and each commit is small.

## Results

(Fill in after implementation and verify.)
