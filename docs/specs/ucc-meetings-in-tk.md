# ucc-meetings-in-tk — one `.ssc` meeting client, web + terminal, and the hand-written shell retired

Status: draft (2026-08-04)
Owner: `ucc-meetings-in-tk`
Parent: [`ucc-poc-msglist.md`](ucc-poc-msglist.md) (the proof this is possible at all)
Related: [`agent-meetings-daemon.md`](agent-meetings-daemon.md) (the data plane),
[`meetings-rest-read.md`](meetings-rest-read.md) (the HTTP surface),
[`agent-meetings-tui.md`](agent-meetings-tui.md) (**mostly superseded** — do not read it as the parity list)

## Goal

One `.ssc` source that emits **both** the meeting client in the UCC web app **and** the native
terminal client, so `rozum meetings attach` stops being hand-written Rust and the two clients cannot
drift apart. Reaching that means adding the three things the read-only proof-of-concept explicitly
left out — **composer, room switcher, unread** — and then deleting the hand-written shell.

## Why this is worth doing

`ucc-poc-msglist` already proved the hard part: one framework-neutral `Tk` source emits a React SPA
*and* a ratatui crate that builds and renders real rows headlessly. What it proved was a read-only
table. Everything a human actually does in a meeting room — type a message, switch rooms, see where
there is something new — was a stated non-goal. So today we still maintain a hand-written terminal
client whose behaviour is defined only by its own source.

The payoff is not lines deleted. It is that **the terminal and the web client stop being two
implementations of one product**: a date divider, an incident badge, a new-message rule gets written
once.

## What actually retires (the board figure was wrong)

`SPRINT.md` said "parity with the 1389-line hand-written Rust TUI". That number conflates two
different programs, and getting this wrong would make the task look ~4× bigger than it is:

| File | Lines | What it is | Fate |
|---|---:|---|---|
| `crates/rozum-meeting/src/tui/attach.rs` | 312 | **The current daemon TUI** — `rozum` / `rozum meetings attach`. The ratatui shell over `MeetingClient`. | **This is what retires.** |
| `crates/rozum-meeting/src/meeting/tui_client.rs` | 1010 | The daemon *client model* — connect, enter, poll, day-scoped reads, plus `post_once`/`call_once`. | **Stays.** `rozum meetings post`, the coordination hooks and the messenger bridges all go through it. It is not TUI code. |
| `crates/rozum-meeting/src/tui/mod.rs` + `app.rs` | 766 + 325 | The **legacy in-process room** (`--legacy-room`, or implicitly with `--web-port`; `src/main.rs:1226`). Pre-daemon: moderator, budget, participant panels. | **Out of scope.** A separate question — audit and retire it on its own merits, not under this task. |

So the deliverable is: a generated terminal client good enough that **`attach.rs` can be deleted**,
and `rozum` / `rozum meetings attach` dispatches to the emitted binary instead.

## Parity list — measured from `attach.rs`, not from the old spec

`agent-meetings-tui.md` describes moderator modes, turn timeouts, interject and a budget panel. All
of that was **removed** when the TUI became a daemon client. Parity means what `attach.rs` does
today:

1. **Header** — `rozum · <room name>`.
2. **Transcript**, newest at the bottom, day-scoped:
   - a dim `── <date> ──` divider whenever the date changes;
   - an incident badge coloured by severity — red for critical/high, yellow for medium and `Alert`,
     green for `Resolution`, cyan otherwise;
   - `<display_name>: ` in bold, then the content;
   - the viewport keeps the newest content visible.
3. **Older history** — `PgUp` splices in the previous day (`prev_day_turns`).
4. **Live arrival** — new messages appear without a keypress, over a *dedicated* poll connection so
   typing never cancels an in-flight long-poll.
5. **Composer** — a one-line input; `Enter` submits; `Backspace` edits.
6. **Slash commands in the composer** — `/quit` `/q`, `/rooms`, `/new [topic]`.
7. **Room switcher** — `Ctrl-O` or `/rooms` opens a picker listing name · topic-or-project ·
   participant count · last date, `↑↓` to move, `Enter` to enter, a final `[ + new room ]` row that
   creates an ad-hoc room, `Esc` to cancel. Entering a room reseeds the transcript and restarts the
   poll.
8. **Quit** — `Esc` or `Ctrl-C`.

**Unread** is *not* in `attach.rs` — it is in the web PWA (`clients/meeting/meeting.ssc`, the `/u`
endpoint plus a `localStorage` seen-map). The sprint item names it, so it is in scope, but as an
**addition to both targets**, not as parity.

## Data plane — no new daemon endpoints are needed for reading

The meeting daemon already serves HTTP on `127.0.0.1:8401` (`crates/rozum-meeting/src/meeting/rest_read.rs`):

| Need | Endpoint |
|---|---|
| room list for the switcher | `GET /rooms` |
| transcript for a day | `GET /rooms/{name}/messages/{date}` |
| older days (`PgUp`) | `GET /rooms/{name}/days` |
| live arrival | `GET /rooms/{name}/events` (SSE) |
| submit | `POST /rooms/{name}/messages` |
| identity | `GET /whoami`, `GET /roster` |
| mentions | `GET /rooms/{name}/inbox/{handle}` |

## The blocker — two capabilities the ScalaScript TUI frontend does not have

**This task cannot be finished inside rozum.** `frontend/tui` in ScalaScript
(`frontend/tui/src/main/scala/scalascript/frontend/tui/TuiEmitter.scala`, 828 lines) emits
`Button` / `TextInput` / `Toggle` with a focus ring, Tab/arrow traversal, `Enter` activation and
typed-character editing — so the *widgets* for a composer and a picker exist. What is missing is how
they reach the network:

1. **No POST.** `grep -rE 'fetchAction|"POST"|Method::Post' frontend/tui/src/main/` returns nothing.
   The emitter's own header calls its binding "**Managed GET** metadata". → **the composer cannot
   submit.**
2. **No signal-driven URL.** `collectFetches` records `FetchInfo(f.fetchUrl, f.tickId)` — the URL is
   a literal captured at emit time; only the *tick* is dynamic. → **the switcher cannot re-target
   the transcript fetch, and `PgUp` cannot fetch another day.**

Both are ScalaScript work, in a repo this project already treats as part of its virtual monorepo
(`REPOS.md` → `../scalascript`). They must be filed and done there.

## Plan — three stages, because stage B is in another repo

### Stage A — everything the emitter can already do (rozum, unblocked)

Extend the PoC from a 3-column table to the real transcript, from one source:
`clients/control/meetings.ssc` (new; `meeting-message-list.ssc` is the PoC and stays until A lands).

- Header, date dividers, severity-coloured badges, bold author, content.
- Live refresh on the existing tick binding (the PoC's proven mechanism).
- Emits both targets: `emit-spa --frontend react` for UCC, `emit(view(), "tui-out")` for ratatui.

**Done when:** both artifacts build from the one source; the headless
`SSC_TUI_SNAPSHOT=1` run shows a fixture transcript with a date divider and a badge; the web artifact
shows the same; no target-specific branch in the source.

### Stage B — the two capabilities (scalascript, blocking C)

Filed on scalascript's board, not this one:

- **`tui-fetch-post`** — a POST/action binding for the TUI frontend, the emitter-side counterpart of
  `fetchAction`. Acceptance: a generated crate posts a body and re-reads on success, proven by a
  deterministic local-HTTP test in scalascript's own gate (the same shape as
  `specs/frontend-tui-fetch-refresh.md` did for GET).
- **`tui-fetch-url-signal`** — let the fetch URL come from a signal (`fetchUrlSignalTo`), so changing
  a signal re-targets the GET. Acceptance: a generated crate switches endpoints when the signal
  changes, same gate style.

### Stage C — parity and retirement (rozum, after B)

- Room switcher on the signal-driven URL; composer + slash commands on the POST binding; unread.
- **Identity decision, must be settled before C:** `POST /rooms/{name}/messages` authenticates as a
  `ConsoleUser` (support-console token, RBAC), while `attach.rs` posts under the human's *local*
  identity (`meeting::local_identity::load_or_create`) over the unix socket. A generated client that
  posts over REST would appear as the console operator, not as the human — **that is a regression in
  who-said-what and is not acceptable silently.** Pick one, write it down here: (a) teach the REST
  submit path to accept a local-identity token; (b) give the generated TUI a console token bound to
  the local identity; (c) keep submit on the socket via a small Rust shim the generated client calls.
  (a) is the honest one; (c) is the cheapest and keeps the socket's identity guarantees.
- Delete `attach.rs`; point `rozum` / `rozum meetings attach` at the emitted binary; drop the
  now-unused ratatui/crossterm deps from `rozum-meeting` if nothing else uses them.

**Done when:** every numbered item in the parity list is demonstrated in the *generated* terminal
client; `attach.rs` is deleted and the tree builds; `rozum meetings attach` still opens a room,
switches rooms, and posts as the human's own handle.

## Non-goals

- The legacy in-process room (`tui/mod.rs`, `app.rs`, `--legacy-room`) — separate task.
- Retiring `meeting.ssc` (the standalone PWA on `:8405`) or the incident/thread console.
- Moderator modes, turn timeouts, interject, budget panels — removed from the product; do not
  resurrect them from `agent-meetings-tui.md`.
- New daemon endpoints for reading (they exist).

## Risks

- **Stage B is the whole schedule.** If ScalaScript's TUI frontend does not gain POST + dynamic URL,
  C cannot start, and this task stalls at a read-only client that does not let `attach.rs` go. Treat
  B as the critical path from day one, not as a follow-up.
- **Emitter coverage is per-slice.** `TuiEmitter`'s header describes capability arriving in numbered
  slices; assume nothing is supported until grepped for. That check is cheap and has already been
  wrong once in this task's own history (the board's line-count).
