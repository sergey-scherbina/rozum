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
(`REPOS.md` → `../scalascript`). **Filed 2026-08-04** via their own `scripts/inbox-add` as
`tui-fetch-post` and `tui-fetch-url-signal` (branch `feature/inbox-rozum-tui-gaps`, their
`inbox-gate.sh` green; their triager routes lane/area — we do not).

### A third limit, found while scoping Stage A: rich rows cannot be fetch-bound on the TUI target

`collectFetches` registers a managed GET **only** when the fetch signal is reached through
`View.SignalText`, `View.DataTable(Remote(…))` or `View.ModelView`. Notably `View.For(items, render)`
recurses into the rendered children but **does not `record(items)`** — and `forJsonView` (the
primitive behind per-row custom rendering) is what a transcript with date dividers, a coloured
severity badge and a bold author would need.

Consequence: **on the terminal target, fetched data can be rendered as a table or as text, and
nothing else.** The date dividers, per-row badge colours and bold-author styling in the parity list
are reachable on the React target only. This is not a reason to branch on target — the whole point is
one source — so **Stage A lives in the intersection of the two targets**, and the rich transcript
becomes a *third* upstream ask if the product decides the terminal must have it. It is deliberately
NOT filed yet: two blocking reports are already open, and this one is cosmetic next to a composer
that cannot submit.

### A fourth limit, and it blocks earliest of all: the TUI target sends no headers

`fetchUrlSignal` takes a `headers: Signal[String]` (a JSON object, documented with an
`Authorization` example) and the React target honours it. The terminal target does not — the emitted
helper is `fn fetch_text(url) { ureq::get(url).call() … }`, with no header plumbing at all, and
`FetchInfo` never carried the header signal in the first place.

Combined with the next section — every daemon route requires HTTP Basic — **the generated terminal
client cannot read the live room at all.** Not the composer, not the switcher: the plain read.
Filed upstream as **`tui-fetch-headers`** (2026-08-04, same branch), and it is ahead of the other
two in the queue.

Why nobody hit this before: `specs/frontend-tui-fetch-refresh.md` and rozum's own PoC smoke both
run against **fixtures with no auth**. The dual-target proof was real, but it had never been pointed
at a production endpoint. Worth remembering as a shape — *a capability proven against a fixture is
proven only for fixtures.*

### And a fifth thing, harmless once known: `env()` resolves at different times per target

On the terminal target `ssc run --v1` executes the program at emit time, so `env(...)` is read in
the emitting process and the resolved URL is baked into the Rust as a string literal. The React
emission keeps the program — the artifact still contains `_arith('+', "rozum · ", room)` — so the
same `env(...)` is a *runtime* read in the browser, where those variables do not exist.

Consequences, both already handled in Stage A: the web client must take its base/room from the page
(origin, route) rather than from the environment; and a smoke can only assert *presence* of the
shared view/binding in the web artifact, because the room name never appears literally in it. The
behavioural assertion belongs to the terminal half.

### And the data plane is authenticated — which settles the identity question

`GET http://127.0.0.1:8401/rooms` answers **`401 Unauthorized`**. `rest_read.rs` puts an
`auth_layer` in front of every route and injects a `ConsoleUser`; tokens are per-operator with an
RBAC role (`rozum meetings token issue <handle> --role observer|responder|admin`). So a token is
needed to **read**, not just to post — Stage A needs one too.

That reframes the Stage C identity choice, and improves it. The daemon already models exactly what
was feared missing: a token *carries a handle*. So option (b) — the generated client holds an
operator token — is no longer a compromise that fakes the human's identity; it preserves
who-said-what **provided the token is issued to the human's own handle** (the same handle
`meeting::local_identity` uses), and `--role responder` is the least privilege that can post.
**Decision (revisit only with evidence): (b), with the handle-matching requirement as a hard
condition.** Option (a) — teaching REST to accept a local-identity token — stays the "right" answer
if the handles ever diverge.

## Plan — three stages, because stage B is in another repo

### Stage A — everything the emitter can already do (rozum, unblocked)

**Rescoped after the two limits above.** The original wording here promised date dividers and
coloured badges; the terminal target cannot fetch-bind that rendering, so promising it would have
meant either a target branch (against the whole point) or a stage that could not close.

What Stage A is: `clients/control/meetings.ssc` (new; the PoC `meeting-message-list.ssc` stays until
A lands), pointed at the **real** meeting daemon instead of the PoC's `:8410/chat/messages`:

- `GET :8401/rooms/{room}/messages/{date}` behind a `remoteTable` — the one fetch binding the
  terminal target registers — with the transcript's real columns: time, author, severity/kind badge
  as its own column, content.
- An operator token, per the decision above; the source takes it from the environment the same way it
  takes `ROZUM_UCC_BASE`, so it is configuration, not a target branch.
- Live refresh on the existing tick binding (the PoC's proven mechanism).
- Both targets from the one source: `emit-spa --frontend react`, and `emit(view(), "tui-out")`.

**Done when:** both artifacts build from the one source; the headless `SSC_TUI_SNAPSHOT=1` run shows
real transcript rows *including the badge column* from an isolated fixture; the React artifact shows
the same columns; no target check anywhere in the source; the existing PoC smoke
(`clients/control/test/ucc-msglist-dual-target.sh`) still passes, and the new one does not touch
`:8089`, `:8401`, `:8411` or any operator service.

**DONE 2026-08-04 (`3e62b99`).** `clients/control/meetings.ssc` +
`clients/control/test/ucc-meetings-dual-target.sh` + `ucc-meetings-fixture.py`; the daemon side
gained `StoredTurn::time_hm()` and `rest_read::message_json` (additive `badge`/`time`), covered by
`messages_carry_derived_badge_and_time`, which asserts against `StoredTurn::badge()` rather than a
literal so the two cannot drift while the test stays green. rozum-meeting 193/193; the new smoke
green on both targets; the PoC smoke still green.

**One open caveat, deliberately not resolved by me:** every emission above was measured with
`bin/ssc-tools` built from `ec70eb062` while scalascript is at `bb22c9d4b` — the CLI prints
`STALE BUILD` on each run. The result needs re-confirming after `install.sh --dev`. I did not
rebuild a staged toolchain that other agents are using mid-flight; that is the operator's call or a
quiet moment's.

**What Stage A does NOT prove, and cannot until `tui-fetch-headers` lands:** that the terminal
client reads the *live* daemon. It reads an unauthenticated fixture. The fixture already accepts
`--require-auth`, so the day headers work, flipping that flag turns this same script into the
authenticated proof — no new fixture needed.

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

**STAGE C SPLIT IN TWO, 2026-08-04 — and the second half is blocked on a FIFTH capability.**

*Part one — DONE (`fb09bf4`).* The composer works: a text field bound to a draft signal, a send
button whose `fetchActionClear` POSTs it, clears the field on success and bumps the transcript's own
fetch tick, so the list re-reads with no extra wiring. Reads are authenticated end to end — the
smoke's fixture now runs with `--require-auth` and refuses a header-less GET, so a regression in the
headers support turns the gate red instead of quietly emptying the transcript. The daemon also
accepts the message text on its own (`submit_payload`), because a terminal target cannot compose
strings and so the composer physically cannot wrap what was typed in JSON.

*Part two — BLOCKED on `tui-table-selection` (filed upstream 2026-08-04).* `View.DataTable` carries
an `actions` list that the terminal emitter discards, and a table has no selection — so a room list
renders and nothing can be picked from it. Everything else for a switcher now exists: the list is a
remote table, `fetchUrlSignalTo` retargets the transcript, and the daemon can ship a ready-made URL
per row so the client never composes a string (the same trick as `badge`/`time`). Only the last hop
is missing. Rejected alternatives, so nobody re-derives them: a `TextInput` bound to the URL signal
(the user types a URL — works, is not a product); one `SetSignalLiteral` button per room (needs the
rooms known at emit time; they are not); composing from a room-name signal (`computedSignal` has no
static-model equivalent at all).

**Therefore `attach.rs` CANNOT be deleted yet, and this task is not done.** Its room picker is the
one remaining gap, which is exactly the difference between "a second client we maintain" and
"delete the hand-written one".

**STAGE C PART TWO — the picker is built (2026-08-04).** `GET /rooms` gained an additive
`entries` array: per room a `name`, a `last` date, a `mentions` count, and a **ready-made absolute
transcript url** derived from the request's own origin. The url is the load-bearing field — the
client cannot compose an address, and a url hard-coded to `127.0.0.1` would be useless to anything
reaching the daemon through a proxy. The client binds that list as a selectable table
(`rowLinkAction`) whose chosen row writes the url into the signal the transcript follows
(`fetchUrlSignalTo`).

**On "unread".** The parity list asked for it; the daemon cannot honestly produce it. A raw unread
count needs a per-viewer read marker for every message and none exists. What does exist is a
per-handle cursor over messages that ADDRESS you, which is what `inbox` already uses — so the column
is `mentions`, labelled `@you`, and it means "addressed to you, unseen". In a busy room that is the
more useful number anyway, and it is one the server can actually stand behind. Recorded rather than
faked.

**A sixth upstream defect fell out of building it**, fixed by us as `tui-remote-table-height`: a
vertical column gave a remote table `Constraint::Length(1)`, so a fetched list rendered its header
and not one row — while the fetch worked and the rows sat in the store. The lesson generalises past
this task: **an unknowable quantity must not be answered with a fixed number.** A remote table's row
count is genuinely unknown at emit time, and `1` was a guess dressed as a measurement.

### What is proven, and what is not

| Claim | Evidence |
|---|---|
| badge/time are computed once, server-side | `messages_carry_derived_badge_and_time` — compares against `StoredTurn::badge()`, not a literal |
| Bearer works and equals Basic | `bearer_is_accepted_alongside_basic` — compares the two schemes against each other |
| the message text may be posted on its own | `submit_payload_takes_text_or_the_structured_form` — including the quoted-string case |
| the terminal client reads an AUTHENTICATED endpoint | dual-target smoke, fixture `--require-auth`, refuses header-less |
| the emitted client can actually POST | scalascript `TuiFetchCargoTest` — compiles the crate, posts to a real local server, asserts the SERVER saw the body |
| a signal URL retargets the GET | same file — retarget with the tick untouched |
| **generated client ↔ rozum's own REST, one process** | **NOT RUN.** The daemon lives in the heavy `rozum` binary (MLX); standing one up for this is out of proportion, and the operator's live daemon runs the deployed build without Bearer. Each piece above is proven separately; the seam between them is not. Worth doing when that binary is being built anyway — do NOT disturb the operator's services for it. |

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
