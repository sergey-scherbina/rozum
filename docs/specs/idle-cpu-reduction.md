# Idle CPU Reduction

## Goal
Reduce CPU use in idle states so the `rozum` process sits near 0% when no keys are pressed and no active long-poll work is happening.

## Scope
- `src/tui/mod.rs` — TUI rendering/input loop.
- `src/main.rs` — web-bridge launch wait loop before handoff.
- `src/web/mod.rs` — room bridge loop audit.
- `src/discord/mod.rs` — room bridge loop audit.
- `src/telegram/mod.rs` — room bridge loop audit.
- `src/meeting/proxy.rs` — reconnect/heartbeat/backoff audit.

## Behavioral Checks

- [x] `src/tui/mod.rs`: replace `event::poll(50ms)` + immediate key read with async, event-driven UI loop.
  - Uses `tokio::select!` on:
    - `events_rx.recv()`
    - `crossterm::EventStream` key events
    - 100ms ticker for presence-timeout refresh only
- [x] `src/tui/mod.rs`: remove per-iteration synchronous event draining/repolling pattern and redraw only when there are state/event changes.
- [x] `src/tui/mod.rs`: update meeting-derived UI state in one helper path and avoid busy synchronous polling on every frame.
- [x] `src/main.rs`: replace startup web-bridge socket wait loop sleep from 50ms to 100ms.
- [x] `src/web/mod.rs`: `room_loop` remains long-poll driven via `meeting.wait_my_turn` with no tight polling loop.
- [x] `src/discord/mod.rs`: `room_loop` remains long-poll driven via `meeting.wait_my_turn` with no tight polling loop.
- [x] `src/telegram/mod.rs`: `room_loop` remains long-poll driven via `meeting.wait_my_turn` with no tight polling loop.
- [x] `src/meeting/proxy.rs`: reconnect backoff and heartbeat paths use bounded delays, not spin loops.
- [x] Idle CPU behavior no longer depends on tight polling; loops are event-driven and should settle near 0% when no input or wait events are pending.

## Results
- Implemented event-driven TUI loop with `crossterm` event stream.
- Kept room bridge loops on long-poll boundaries.
- Reduced short-delay spin potential in web-bridge startup by using a 100ms cadence.
- No new busy-wait loops introduced.
- Manual runtime CPU measurement (`top`/`Activity Monitor`) was not executed in this change set; recommended as the final validation step.
