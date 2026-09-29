# okay-workspace-ui — one remote workspace over rozum + nadia, drawn by any host

Status: draft (2026-09-28). Cross-repo (rozum + `../nadia` + `../okay`).
Owner: `okay-workspace-ui`
Supersedes, for the UI layer only: [`unified-control-center.md`](unified-control-center.md) and
[`ucc-meetings-in-tk.md`](ucc-meetings-in-tk.md) (the ScalaScript Tk track). Their data layer —
`rozum gateway control-serve`, the daemon REST/SSE — stays and is consumed here.
Related: [`agent-meetings-daemon.md`](agent-meetings-daemon.md) (rooms, the single writer),
[`meetings-rest-read.md`](meetings-rest-read.md) (the HTTP surface; now read+write),
[`agent-meeting-coordination.md`](agent-meeting-coordination.md) (the client contract, §5–6),
[`messenger-bridges-daemon.md`](messenger-bridges-daemon.md) + [`messenger-access-control.md`](messenger-access-control.md)
(the Rust Telegram bridge, stage 1's transport), `nadia:SPEC.md` §6–7 (subagents, Telegram),
`okay:specs/frontend.md`, `okay:specs/ui.md`, `okay:specs/ui-telegram.md` (the "how" layer).

## Direction (operator, 2026-09-28, in-session — binding)

> Не хватает полноценного удалённого интерфейса пользователя для одновременной работы с несколькими
> проектами, репозиториями, агентами, чатами и возможности между ними переключаться, сохраняя фокус и
> контекст. […] Этот интерфейс должен работать в любой технологии — через телеграм, через веб, нативно,
> в консоли и т.д. Абстрактно интерфейс один и тот же — меняться должны только низкоуровневые детали —
> но эти низкоуровневые детали должны иногда определять всё необходимое самым правильным образом для
> каждого конкретного случая. Абстрактный интерфейс пользователя логически в одном экземпляре отвечает
> на вопрос «что», а платформенный слой из библиотеки okay отвечает на вопрос «как».

> Okay тоже есть практически всё, что есть в scalascript.

> Телеграм-бота делаем нового на okay — там для этого будет всё необходимое. Но на первом этапе может
> быть проще сделать так, как ты предлагаешь [Rust-мост как транспорт] — я не возражаю.

Three decisions follow, and the rest of this document is their consequences:

1. **The "how" layer is okay-ui, not ScalaScript Tk.** The UCC track is not continued; its surfaces
   (the meeting PWA on `:8405`, the UCC SPA on `:8410`, the generated `rozum-meeting-tui`) are retired
   one by one as this reaches parity, the same rule `ucc-meetings-in-tk` applied to `attach.rs`.
2. **The "what" layer exists once**, as one pure `State` / `view` / `update` over `okay.ui.Ui`, in one
   long-running process, and every client — terminal, browser, Telegram, a native app — is a host of
   that one program. There is no per-platform application code; there are per-platform hosts and the
   vocabulary each host claims.
3. **The Telegram bot is eventually a new bot on okay.** Stage 1 keeps the existing Rust bridge as a
   transport (it already carries the ACL and the launchd plist); the bot moves onto okay when okay has
   a Bot API transport, which the operator owns.

## Goal

A human at any device opens ONE workspace: every project rozum knows, its rooms, the agents live in
them (Claude Code, codex, nadia, model participants), and the chats — a room transcript, or a
conversation with one agent. Switching between any two of these keeps each one's context (the draft
being typed, where the reader was in the transcript, who the input is addressed to, what is unread),
and the context is the SAME on the phone and in the terminal, because it lives in the workspace
session, not in the client.

## What exists (recon 2026-09-28, three repos read)

**okay already IS the "how" layer.** Its own spec states the operator's requirement in its words
(`okay:specs/frontend.md`: "A frontend's logic is defined by the structure, logic and design of the
system it shows, not by the technology that draws it"). In code:

| Piece | Where (in `../okay`) | What it gives us |
|---|---|---|
| `enum Ui`, `Event`, `Patch`, `Ui.diff`/`patch` | `okay-ui/src/main/scala/okay/ui/Ui.scala` | the view as a value with string keys, cross-built JVM + JS + Native |
| two vocabulary levels + `Ui.lower(ui, vocab)` | same, `specs/frontend.md` | a closed layout level every host draws; an open semantic level (`Form`, `Table`, `Tabs`, `Modal`, `Items`) defined by its lowering. **This is the mechanism for "the low-level details sometimes decide everything":** a host claims what it draws natively in `Hello{vocab}` and the server lowers the rest |
| `Host` / `Backend` seam; `Ui.run`, `Ui.runCmd` | `Ui.scala` | the Elm loop, effects as the row, subscriptions as `merge` |
| `Wire.serve` / `Wire.client` / `Protocol` | `okay-ui/src/main/scala-form/okay/ui/{Wire,Protocol}.scala`, contract `docs/protocol/frontend.md` + `conformance.jsonl` | server-driven UI: full tree once, patches after; events accepted only for keys on the shown tree; JSON lines or CBOR |
| `Sessions` (event-sourced) and `Live.durable` | `okay-ui/.../Sessions.scala`, `okay-script/.../api/Live.scala` | a UI session journaled on okay-persist, recovered by refold; the browser page over WebSocket with reconnect, installable (`api.installable`), offline shell |
| hosts | `Frame`+`Terminal` (JVM/Native), `ReactJs`, `Dom`, `Swing`, `okay-ui-gtk`, `Telegram`, thin clients `okay-swift/` (SwiftUI, iOS) and `okay-compose/` (Kotlin, desktop + Android) | terminal, web, native |
| `Telegram.host` | `okay-ui/.../Telegram.scala`, `specs/ui-telegram.md` (all boxes ticked) | a pure host: `Act.Send/Edit/Answer/Ask` out, `Update.Pressed/Said` in, one message edited in place, callback data bound to the frame. **No Bot API transport, by decision** — the consumer performs the acts |
| `Nav`/`Screen`, `Dialog`, `Scope`, `Form` from `Schema`, MCP elicitation → `Form` | `Screen.scala`, `Dialog.scala`, `Toolkit.scala`, `okay-demo` `TestElicitForm` | a screen stack, scenarios as programs, the approval circle |

What okay does NOT have, and this spec adds on top of it: a pane/focus manager (focus today is a
linear tab order held by the host), any model of several agents or sessions, a Telegram transport,
and a real product screen on the native hosts (they have drawn the conformance script and a counter).

**rozum is the data plane, and it is enough for stage 1–4.**

| Source | Surface | Used for |
|---|---|---|
| meeting daemon | REST+SSE `127.0.0.1:8401` (`crates/rozum-meeting/src/meeting/rest_read.rs`): `GET /rooms` (with `last`, `mentions`), `/rooms/{n}/days`, `/rooms/{n}/messages/{date}?from&count`, `POST /rooms/{n}/messages`, `/rooms/{n}/events` (SSE "changed"), `/roster`, `/whoami`, threads/react/redact | projects (a room is a project), transcripts, posting, unread, who is live |
| meeting daemon | MCP over `meeting.sock` (`meeting.status`, `wait_my_turn`, `mark_responding`) | **presence ("responding") — MCP-only today**, not on REST; stage 4 adds it to REST |
| `rozum gateway control-serve :8411` | `GET /control/status` (agents/coders/sessions it launched, residency, catalog), `/control/session/*` (tmux attach over WebSocket), `/control/messenger/*`, `/control/project/add` | processes launched through the UCC, models, the messenger admin |
| `nadia serve :8790` (Rust nadia) | `GET/POST /agents`, `/agents/{id}`, `/agents/{id}/{tell,pause,resume,stop}`, `x-nadia-token` | nadia subagents; **no event stream, no approvals, transcript not persisted** |
| `rozum meetings hello` principals | `<state>/principals/agents/<session>.json`, REST `/roster` | every agent that identified itself, across projects, with cwd/worktree; TTL 15 min |
| Rust Telegram bridge | `crates/rozum-meeting/src/telegram/` + `nadia.rs` | one chat ↔ one room, per-room ACL (`/grant`, `/revoke`), nadia verbs (`/spawn /agents /tell /project …`); state in `~/.local/state/rozum/nadia-telegram.json` |

Three separate web surfaces exist today (PWA `:8405`, UCC `:8410`/`:8411`, REST console `:8401/`),
each with its own room switching and no shared state; only the PWA remembers anything per room (a
`localStorage` seen-map). No client shows presence. No client keeps a draft across a switch. This is
the gap the operator named.

**nadia has no UI and no session model.** The Scala 3 implementation (`nadia/scala`, scala-cli,
Scala 3.6.3, one dependency, deliberately "no SDK underneath") holds one conversation per process
and does not persist it; the Rust one has a supervisor and `serve`, polled, with spawned agents on
auto-approve. Projects and chats exist only in the Telegram bridge's `ChatState`.

## Architecture

```
                 the "what" — ONE program, ONE process (JVM), pure State/view/update
   ┌──────────────────────────────────────────────────────────────────────────────┐
   │  Workspace(projects, focus, contexts)  ──view──▶  Ui   ◀──update──  Event    │
   │      ▲ merged sources: daemon SSE · roster · control/status · nadia /agents  │
   │      │ Sessions / Live.durable: ONE journal per human principal              │
   └──────┼───────────────────────────────────────────────────────────────────────┘
          │  Wire (JSON lines / CBOR) — full tree, then patches; Hello{vocab} first
   ┌──────┴────────┬──────────────────┬──────────────────────┬─────────────────────┐
   │ terminal      │ browser          │ Telegram             │ native thin client   │
   │ Terminal.host │ Live page (WS,   │ Telegram.host;       │ okay-swift /         │
   │ Scala Native  │ reconnect, PWA)  │ stage 1: Rust bridge │ okay-compose /       │
   │ binary        │                  │ performs the Acts;   │ GTK — claim Table,   │
   │ claims: Table │ claims: all S    │ stage 2: bot on okay │ Tabs; draw natively  │
   └───────────────┴──────────────────┴──────────────────────┴─────────────────────┘
          │ data, unchanged                 ▲
   meeting daemon :8401 REST/SSE · control-serve :8411 · nadia serve :8790 · meeting.sock (MCP)
```

**One process, one program.** The workspace is a JVM service (okay is JVM/JS/Native; the terminal
host is also a Native binary, but the server that holds sessions and talks to the daemons is one JVM
under launchd, next to `rozum-meet`). It runs the SAME `Ui.run(init)(view)(update)` in-process for the
terminal when the terminal is on the same box, and behind `Wire.serve` for everything else — okay's
"deployment is a flag" (`specs/frontend.md`), not a second code path.

**The model of "what".** Plain data, `Schema`-derived so it journals and crosses the wire:

- `Project(name, path, rooms, agents)` — a project is what `rooms.json` + `~/.rozum/ucc/projects.json`
  say it is; its canonical room is its name.
- `Item` — the things one can be "in": `Room(project, name)`, `Agent(principal)` (a Claude Code /
  codex / nadia / model participant, from `/roster` ∪ `/control/status` ∪ nadia `/agents`), `Chat`
  (a room transcript, or an agent's own conversation where one exists).
- `Context(item)` — what switching preserves: `draft: String`, `anchor: (date, n)` (the message the
  reader was at — the daemon's own address, not a pixel), `addressee: Option[Agent]`, `seen: (date, n)`
  high-water for unread. One per item, in the session, never in the client.
- `Focus(item, pane)` — the current item and, where the host draws several panes, which one has the
  keyboard. Focus is workspace state so it survives a device switch; the host's own tab order stays
  the host's.
- `Layout` is NOT state: it is derived from the host's claimed vocabulary at `view` time. A chat host
  (Telegram, vocab `link`) sees one item at a time behind a `Nav` stack; a terminal (claims `Table`)
  sees a sidebar and one item; a browser (claims all of level S) sees the sidebar, the item, and the
  agents panel. Same keys, same events, same `update` — the law `keys(s) == keys(lower(s))` is what
  makes "one instance of the what" literally true.

**Keys are addresses.** Every node's key is the dotted path of the thing it shows
(`project.rozum.room.rozum.compose`, `agent.<principal>.tell`), so an `Event.Pressed(key)` names the
item without a lookup table, and the same key is the journal's replay identity.

**"Sometimes the low-level details decide everything."** Two mechanisms, both okay's, no third:
(1) vocabulary claims + lowering, above; (2) the hybrid rule in the tree — a `Form`'s edits fold on
the client and cross once as `Submitted`, a `live` input speaks per change. The composer is a `Form`
with a `live` multiline input, so the draft reaches the session as it is typed (that is what makes
it survive a device switch) while the room only sees a message on submit. A host that cannot do
`live` (Telegram) asks with `ForceReply` — also already in the host.

**Sessions.** One durable session per human principal (the `Principal` of
`agent-meeting-coordination.md`; today: the REST token's identity, or the Telegram user id the bridge
maps). Every device attaches to that one session; the wire's reconnect shape (full tree, then patches)
is exactly "switch device, continue". `Sessions.recover` proves it: a live run and a recovery reach
the same state, which is the test this spec inherits from `ui-durable`.

**Telegram, two stages.** Stage 1: the Rust bridge gains a mode in which it is a TRANSPORT — it
forwards each `Update` (a press, a message) to the workspace over a local socket and performs each
`Act` (send, edit, answer, ask) it gets back, keeping its ACL as the gate in front. Nothing UI-shaped
stays in Rust; `nadia.rs`'s commands become workspace items. Stage 2: a Bot API transport in okay
(operator-owned, `okay`), the workspace talks to Telegram itself, the ACL roster moves with it, the
Rust bridge is retired for this bot. The application does not change between the stages — that is
the point of the host being pure.

## Where the code lands

- **The workspace program + service: `../nadia/ui/`**, its own sbt build (Scala 3.9.0, okay from
  `sbt publishLocal`, `dev.okay` `0.2.0-SNAPSHOT` — `okay:docs/building-a-chat-app.md` §1 is the
  recipe). NOT inside `nadia/scala/`: that build is a deliberate one-dependency, scala-cli statement
  of "how much of this is the framework", and this module is the opposite statement. nadia is the app
  leaf of `integration.md`; the human's workspace over agents is an app.
- **rozum**: this spec; the REST additions (presence on `/rooms/{n}/events`, an `id`-addressed
  `/agents` view over the roster + control status); the Telegram bridge transport mode (stage 1);
  launchd plists + Tailscale for the new service, under `clients/control/launchd/`.
- **okay**: the Bot API transport (operator); fixes found in the hosts land upstream, never as forks.

## Stages (each a claim on its own; a stage lands only with its test)

- [x] **S0 — skeleton (`workspace-ui-skeleton`)**: `nadia/ui` builds against okay; `Workspace` data
      with `Schema`; a `view` that renders the project list from a fixture; `TestPortable`-style test:
      the same program on the scripted host and on `Frame` yields the same frames. No network.
      **Landed 2026-09-28** (nadia `ui/`, 5 tests; the `Schema` derivation is deferred to S2's durable
      half). The prototype also carries S2's in-session half and the seam test of S3 with `Telegram.host`
      behind `Wire.serve` — see nadia `ui/README.md` and rozum SPRINT `okay-workspace-ui`.
- [ ] **S1 — read-only workspace, terminal + web from one program**: projects from `/rooms`, a
      transcript from `/messages/{date}`, live refresh from `/rooms/{n}/events`; `Wire.serve` behind
      a Live page; the terminal binary via `Wire.client(Terminal.host)`. Gate: one headless browser
      and one headless terminal show the same rooms and the same last message from the live daemon.
- [x] **S2 — switching with context**: the composer, `Context` per item, `Focus` in the session,
      `Sessions`-durable. Gates: (a) type in room A, switch to B, switch back — the draft and the
      anchor are there; (b) the same, but "back" happens on a second attached client; (c) live run ==
      recovery after a restart of the service. **Landed 2026-09-29** (nadia `ui/Shared.scala`): (a)
      in `WorkspaceTest`, (b) and (c) in `SharedTest`. The journal is a JSONL file of the human's
      events, not okay's `Sessions` (Maps in the state, no file store) — same doctrine, refold
      through the same `update`, and through a feed whose posts already happened. Focus is shared
      across devices; per-device focus is an open decision.
- [ ] **S3 — Telegram over the Rust bridge as transport**: the same program, `Telegram.host`, the
      bridge performs `Act`s. Gate: the `ui-telegram` seam test, plus a scripted `Update` walk that
      switches rooms and posts, asserted equal to the terminal's final state.
- [x] **S4 — agents**: the agents panel from roster ∪ control/status ∪ nadia `/agents`; presence
      added to the daemon's SSE; a chat with a nadia agent via `/tell`; `rozum launch` from the panel.
      Gate: an agent that ran `hello` appears within its TTL; `responding` flips while it types.
      **Landed 2026-09-29**, offline-tested: presence is `GET /rooms/{n}/presence` (the SSE only says
      "changed"; the client re-reads on it); agents carry their commands as caps; tell for inbox
      agents, @-mention for room agents; a nadia spawn form. Coder launch from the panel is open.
- [ ] **S5 — approvals**: a nadia tool approval shown as a `Form` (the elicitation circle), answered
      from any host. **Blocked on nadia**: `serve` needs an event stream and an approval hook
      (`nadia:BACKLOG.md` NAD-14).
- [ ] **S6 — native**: the first product screen on a thin client (SwiftUI or Compose — which first is
      open, below), claiming `Table`/`Tabs`; the conformance script extended with a workspace walk.
- [ ] **retire**: the meeting PWA, the UCC SPA, `rozum-meeting-tui` — each when this covers what it
      did, with the `ucc-meetings-in-tk` rule: delete, do not keep two.

## Open questions (operator)

1. **nadia's session model.** S5 (and a two-way agent chat in S4) need nadia to persist a transcript,
   stream events instead of being polled, and expose an approval hook remotely. Is that in scope of
   this work, or does the workspace compose only what nadia has today? Filed as NAD-14 either way.
2. **Which native target first** for S6 — a macOS SwiftUI app (needs an app target okay lacks; iOS
   package exists), Compose desktop/Android (APK builds, never run), or GTK?
3. **The Bot API transport in okay** — operator-owned; S3's stage-2 flip waits on it, nothing else does.

## Non-goals

Model inference in the UI process (the gateway is unchanged); a second Telegram bot; turn-taking or
moderation of any kind (rooms have none); a per-client state store — anything worth keeping is in
the session; styling beyond okay's tokens (a native client looks native, a terminal stays a terminal).
