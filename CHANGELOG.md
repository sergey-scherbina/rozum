# Changelog

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
