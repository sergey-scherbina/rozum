# Changelog

## concurrency-budget — load-time budgeted engine max_num_seqs (Phase A)
Completed: 2026-06-11
First phase of `mistralrs-concurrency-scheduling`. Replaces the total-`hw.memsize`
1/2 ladder with a footprint budget: `budgeted_max_num_seqs(ConcurrencyBudget)`
(pure, in the lib) returns `clamp((0.8·available − weights − kv_pool) /
per_seq_peak, 1, ceiling)`, where `per_seq_peak = prefill_chunk × ~465 KB/token`
(constant under chunked prefill) and `ceiling` defaults to 8 (Metal is one GPU —
past a handful of concurrent prefills you gain tail latency, not throughput).
`resolve_max_num_seqs` in `main.rs` gathers the footprint from the existing
preflight helpers and applies env overrides (`ROZUM_MISTRALRS_MAX_SEQS` forces,
`ROZUM_MISTRALRS_SEQS_CEILING` caps, `MISTRALRS_PREFILL_CHUNK` sizes the per-slot
cost), logging a `concurrency_budget` obs event. `MistralrsOptions::default()`
now carries a plain serialised floor of 1. 6 lib unit tests (no Xcode), feature
build clean. Phases B+C (admission scheduler + fast lane) and D (backpressure +
circuit breaker) remain in SPRINT.md.

## mistralrs-adaptive-concurrency — memory-adaptive default for max_num_seqs
Completed: 2026-06-11
The mistralrs backend's concurrent-prefill cap (`max_num_seqs`) default is no
longer a fixed `1`. A new pure `default_max_num_seqs(total_ram)` policy keeps
the serialised `1` floor on the 24–36 GB Apple Silicon target band (where two
concurrent large-prompt prefills can OOM the Metal command buffer) and lifts it
to `2` on machines with ≥ 48 GB total unified memory, where PagedAttention +
chunked prefill + the disconnected-seq reaping fix make real concurrency safe.
The gate is on total `hw.memsize` rather than instantaneous free memory (which
over-predicts runtime headroom at load time). `ROZUM_MISTRALRS_MAX_SEQS`
overrides. Rationale + trade-offs documented in
`docs/specs/mistralrs-backend.md`.

## web-basic-auth — HTTP Basic Auth on the web bridge
Completed: 2026-06-06
The web bridge now requires HTTP Basic Auth for `/`, `/ws`, and `/transcript`.
The password must equal the room name; the username is unconstrained and is
used as the participant's alias in the chat. The server stamps every outgoing
`meeting.submit` with the authenticated alias regardless of any client-supplied
`name` field, so a tampered client cannot post under a different name. The
auth username is sent to the client via a new `{kind:"hello",name:...}` WS
envelope right after connect; the page-side name input is removed.

## tui-soft-wrap — soft-wrap long input lines in the TUI
Completed: 2026-06-06
Custom render of the input area: `tui-textarea 0.7` still holds the data and
processes input events, but its renderer is bypassed. `draw_input` builds
visual rows by wrapping each logical line at `inner_width` and places the
cursor manually via `f.set_cursor_position`. Autosize now counts wrapped
visual rows, so a single long line grows the input chunk upward instead of
scrolling horizontally.

## mcp-proxy-auto-mark — auto-emit mark_responding from mcp-proxy
Completed: 2026-06-06
`ProxyState` gained a `heartbeat_task` handle. When `meeting.wait_my_turn`
returns `your_turn:true`, the proxy fires an immediate `meeting.mark_responding`
and spawns a background task that refreshes it every 15 s. The task is aborted
on the agent's next `submit`/`leave` and on a fresh `your_turn:true` (which
restarts the heartbeat). Manual `meeting.mark_responding` calls from the agent
still work and refresh the timer identically.

## mcp-proxy-reconnect — transparent reconnect of mcp-proxy after rozum restart
Completed: 2026-06-06
`ProxyState` remembers the joined room name; `call_room_tool` now
catches transport failures and calls a new `try_reconnect_current_room`
that sleeps a capped backoff (`200ms…5s`, ~18 s total) waiting for the
Unix socket to reappear, reconnects, re-issues `_join_internal` with
the same display name, and retries the original tool call. The agent's
MCP session no longer sees `Transport closed` during a `rozum --room R`
restart.

## room-transcript-persist — room transcript persisted across rozum restarts
Completed: 2026-06-06
`Meeting` gained `persist_path: Option<PathBuf>` and an
`enable_persistence` method that loads
`$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl` on
construction and re-numbers seq. `post_submission` appends every Turn
as one JSON line. A new top-level `--no-persist` flag disables both
(independent of the existing `rozum web --no-persist`). Web bridges
pick up the loaded history through their normal
`wait_my_turn(since_seq:0)` path. With `rozum --room R` the same room
name reopened after a restart resumes with full transcript intact.

## web-transcript-persist — bridge transcript persisted to disk
Completed: 2026-06-06
The web bridge now appends every `msg` envelope to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line per
turn). On startup the bridge loads the last `TRANSCRIPT_CAP=2000` lines back
into the in-memory ring so a page reload after a rozum restart still shows
recent history. A new `--no-persist` flag on `rozum web` disables both the
write and the load. Client-side deduplication now keys on `(seq, ts)` so
persisted entries from earlier sessions — where seq numbering restarts — do
not collide with current-session entries.

## web-transcript-history — transcript replay on connect + lazy older-history paging
Completed: 2026-06-06
The web bridge keeps a bounded in-memory transcript ring (cap 2000). A new
`GET /transcript?from_seq=&limit=` REST endpoint returns slices for paging.
On WebSocket connect the bridge sends a `kind:"history"` envelope with the
last 200 entries; the client replays them through the normal append path with
seq-based deduplication. Scrolling within 60 px of the log top triggers a
fetch of the next older 200 entries and prepends them while preserving the
viewport. `web-transcript-persist` (separate slug) will lift the in-memory
2000 cap by reading from `transcript.jsonl`.

## tui-arrow-scroll — Arrow Up/Down always scrolls the transcript
Completed: 2026-06-06
Dropped the `textarea.lines().len() <= 1` guard so the Up/Down arrows scroll
transcript history even when the input area is multi-line. Textarea cursor
navigation moves to `Ctrl+Arrow` / `Home` / `End`. Per operator request.

## tui-autosize-input — TUI input area grows with multi-line composition
Completed: 2026-06-06
Replaced fixed `Constraint::Length(3)` with a dynamic
`(textarea.lines().len() + 2).clamp(3, max(3, area.height/3))` so the input
area grows upward when the user enters multi-line content via `Alt+Enter`.
Up/Down arrows now scroll the transcript history (in addition to PgUp/PgDn).
Soft-wrap of a single overflowing line is **not** in this slug — split into
`tui-soft-wrap` because `tui-textarea 0.7` has no native wrap.

## web-scrollback-sticky — sticky-bottom scroll, "↓ N new" pill, long-message collapse
Completed: 2026-06-06
`#log` now tracks `data-stick` on scroll; new messages auto-scroll only when
the user is within 40 px of the bottom, otherwise a sticky `↓ N new` pill
appears and clicking it snaps to bottom. Messages whose body exceeds 6 wrapped
lines or 600 characters render collapsed with an `[expand ▾]` / `[collapse ▴]`
toggle. Pure client-side change in `src/web/index.html`.

## web-presence-row — presence row, joined/left, tagged envelopes for the web bridge
Completed: 2026-06-06
`src/web/mod.rs` `room_loop` now emits tagged JSON envelopes
(`kind:"msg"|"presence"|"joined"|"left"`) instead of raw transcript JSON.
`src/web/index.html` dispatches on `env.kind`: presence line above the input
with `✏️` / `⏳` glyphs, header chips for participants, dim system lines for
join/leave. Display names are rendered with `textContent` (no innerHTML) so
they cannot inject HTML.

## web-autosize-input — Claude-style autosizing textarea in the web client
Completed: 2026-06-06
Replaced the single-line `<input id="msg">` with a `<textarea rows="1">` that
grows upward on input up to `30vh` (`20vh` on mobile). `Enter` sends,
`Shift+Enter` inserts a newline, `Esc` clears, no horizontal scroll, collapses
back to one row after send. Verified live by the operator.
