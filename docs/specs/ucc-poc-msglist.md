# UCC read-only meeting message list — one source, web + TUI

Status: complete (2026-07-20)
Owner: `ucc-poc-msglist`
Parent: [`unified-control-center.md`](unified-control-center.md)

## Goal

Prove the first Unified Control Center slice with one framework-neutral Tk
source: a read-only list of messages from the `rozum` meeting room that builds
and renders as both a React web app and a native ratatui terminal app, including
a user-triggered live refresh.

The deliverable is one `.ssc` application under `clients/control/`. It contains
no target checks and no duplicated target-specific view. Both outputs consume
the same `View`, `FetchUrlSignal`, table columns, and refresh event.

## Data contract

By default the app reads the existing local UCC endpoint:

```text
GET http://127.0.0.1:8410/chat/messages?room=rozum
```

`ROZUM_UCC_BASE` may replace the origin at build/emission time. This is a data
configuration seam, not a target branch: both frontends receive the same
resolved URL. The deterministic smoke uses it to point both artifacts at an
isolated fixture; an empty value produces the same-origin browser path.

The response is a root JSON array. Each row has:

- `time`: display timestamp;
- `author`: meeting identity;
- `content`: message text;
- `ts`: stable integral timestamp used as `DataTable.rowKeyPath`.

The source uses `FetchUrlSignal` and `DataTable.Remote`/the portable
`remoteTable` builder. A `ReactiveSignal[Int]` is the binding's refresh tick; a
visible refresh button increments it. Fetching once on mount and again after
that event is part of the feature, not test-only behavior.

## View contract

The shared view contains:

1. a title identifying the `rozum` meeting room;
2. a compact read-only table with Time, Author, and Message columns;
3. a refresh control bound to the fetch tick;
4. no composer, room mutation, model action, or platform-specific branch.

The default frontend in source is `tui`, allowing the ordinary v1 interpreter
entrypoint to emit a native crate through `emit(view(), "tui-out")`. The web
artifact is selected externally with `emit-spa --frontend react`; changing the
target must not require editing the source.

## Build and smoke contract

A repository smoke script shall exercise both artifacts from the same source:

- web: emit React SPA output and assert the artifact is non-empty and contains
  the meeting title/table contract;
- TUI: emit the ratatui crate, run `cargo build`, and execute the headless
  `SSC_TUI_SNAPSHOT=1` path against an isolated local fixture;
- fail loudly when the ScalaScript toolchain, Cargo, endpoint, generated files,
  or expected rendered content are missing.

The script must use temporary output, bind its fixture to an OS-assigned port,
and clean up only processes it started. It must not stop, restart, or reuse the
operator's gateway on `127.0.0.1:8089`, UCC on `8410`, or any other already-owned
service.

Cross-repo conformance for refresh itself lives in ScalaScript's
`specs/frontend-tui-fetch-refresh.md`: a deterministic local HTTP test proves
that incrementing the shared tick replaces an initial table payload with a
second payload in generated Rust. Rozum's smoke proves consumption of that
backend contract from the real `.ssc` application.

## Done when

- exactly one `.ssc` source defines the message list;
- React emission succeeds and includes the shared view/data binding;
- native TUI emission produces a Cargo crate that builds;
- a headless TUI snapshot contains rows returned by the meeting endpoint;
- changing the refresh tick causes a second GET in the ScalaScript integration
  gate and both targets use that same tick binding;
- existing rozum tests and repository checks remain green;
- the sprint and changelog identify the exact commands and results.

## What this proof did NOT cover (added 2026-08-04, by its successor)

The dual-target mechanism is real and still green. But everything it proved, it proved **against an
isolated fixture with no authentication** — as did ScalaScript's own
`specs/frontend-tui-fetch-refresh.md`. The real data plane requires HTTP Basic on every route, and
the terminal target turns out to send **no headers at all** (`ureq::get(url).call()`, and the
`headers` signal never reaches the emitter's fetch record). So a generated terminal client can read
a fixture and cannot read the live daemon.

That is not a defect of this PoC — it was a stated non-goal to touch production networking. It is
recorded here because the *shape* is worth carrying: **a capability proven against a fixture is
proven only for fixtures.** Filed upstream as `tui-fetch-headers`; the successor spec is
`ucc-meetings-in-tk.md`.

## Non-goals

- message composition or posting;
- room selection, unread state, or full meeting-TUI parity;
- replacing the deployed UCC or hand-written meeting TUI in this slice;
- background polling without an explicit refresh event;
- production networking/auth redesign.

## Result

Implemented as `clients/control/meeting-message-list.ssc`, with an isolated
fixture and `clients/control/test/ucc-msglist-dual-target.sh`. The exact staged
ScalaScript toolchain emits a non-empty React artifact, emits a ratatui Cargo
crate whose source retains `meetingMessageRefresh`, builds it, and renders the
fixture's `smoke-agent` / `smoke-message` row headlessly. ScalaScript's generated
Cargo regression separately proves successful tick refresh, unchanged-tick
no-op behavior, and last-good retention after HTTP 500 (`frontendTui/test`
36/36). The smoke binds only an OS-assigned fixture port and leaves `:8089`,
`:8410`, and all operator services untouched.
