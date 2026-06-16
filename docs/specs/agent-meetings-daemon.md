# Agent Meetings — Daemon-Hosted, Disk-Backed Rooms

## Overview

Move meeting rooms into a **dedicated meeting daemon** (`rozum meetings`),
separate from the model gateway — **the gateway and model serving are untouched**.
One long-lived meeting daemon hosts an unbounded number of rooms, and transcripts
live on disk rather than in RAM. A room stops being a process and becomes a
logical, disk-backed object inside the meeting daemon; the TUI becomes a client
that attaches to a room over the daemon's socket. The end state is one
collaborative dev environment of two cooperating local services — agents, the
meeting daemon, and the gateway — where an agent (or a room) that needs a model
calls the gateway over its local HTTP, exactly like any other client.

For scale and resilience, **message bytes never fan out through the daemon**: a
write is one small `submit` RPC to the daemon (the single writer), but reads
never touch it — local clients (the TUI and each `mcp-proxy`) read the transcript
straight from disk. The daemon ships only small coordination signals (a
high-water `(date, n)`, roster, phase, responding events); read fan-out is
handled by the OS page cache over the shared day files.

This supersedes the one-process-one-room topology in
`agent-meetings-process.md` and the "no persistence / no multi-room registry"
non-goals in `agent-meetings.md`. All existing meeting behavior (free-form submit
— anyone, any time — responding/polling indicators, budget, bridges, channels)
is retained. There is **no turn-taking and no moderator** (free submit only);
only the topology and storage change.

## Interface

### Process / discovery
- A **dedicated meeting daemon** (`rozum meetings`) hosts the meeting subsystem;
  the model gateway (`rozum gateway`) is a separate process and is **unchanged**.
  There is no per-room process. The meeting daemon auto-spawns on first use (TUI
  or `mcp-proxy`) and can be installed as a user service like the gateway.
- A single MCP endpoint per daemon at `$XDG_RUNTIME_DIR/rozum/meeting.sock`
  (fallback `~/.run/rozum/meeting.sock`). Rooms are selected **within** an MCP
  session via `rooms.join(name)`, not by a per-room socket.
- `rozum mcp-proxy` (unchanged shim shape) dials this one socket, auto-spawning
  the daemon if absent, then forwards `rooms.*` / `meeting.*`.
- `rooms.list` is answered by the daemon (a live RPC over its `rooms.json`
  registry of room locations), not by scanning a sockets directory. Rooms are
  grouped/annotated by project.
- The proxy passes its **project** (cwd / git root); an agent's default room is
  that project's canonical room. `rooms.list` / `rooms.join` still let it choose
  another room.

### Daemon control (`rozum meetings`)
Modeled on `rozum gateway`:
- `rozum meetings start [--port N] [--foreground]` — start the meeting daemon.
  Idempotent: a daemon already bound to `meeting.sock` is reused (single owner via
  the socket + an advisory lock), not duplicated. Detached by default;
  `--foreground` runs it in the terminal. Auto-spawn from the TUI / `mcp-proxy`
  takes this same path.
- `rozum meetings stop [--force]` — graceful shutdown: pending `wait_my_turn`
  long-polls get `{ended:"server-shutdown"}`, in-flight writes flush, the socket
  is removed; rooms remain on disk and reopen on next start. `--force` skips the
  drain.
- `rozum meetings status` — daemon liveness (pings `meeting.sock`), socket path,
  and a summary of open rooms (name, project, participant count). Exits non-zero
  when no daemon is running.

**User service (auto-start at login)**, mirroring `rozum service` for the gateway
(spec: `shared-gateway-service.md`):
- `rozum meetings install [--port N]` — register the meeting daemon as a user
  service: launchd `~/Library/LaunchAgents/com.rozum.meetings.plist` (macOS) or
  systemd `~/.config/systemd/user/rozum-meetings.service` (Linux), running
  `rozum meetings start --foreground` with `RunAtLoad` + `KeepAlive` /
  `Restart=on-failure`. Logs to `$XDG_STATE_HOME/rozum/meetings/service.log`.
  Idempotent (unload first), then starts it. Runs as a **user** agent — no root.
- `rozum meetings uninstall` — stop the service and remove the unit/plist.
- With a KeepAlive service installed the service manager owns lifecycle, so a
  plain `rozum meetings stop` is auto-respawned — use `uninstall` to keep it down.
- The plist/unit **generation** lives in the library (`meetings_plist(program,
  args, env)` / `meetings_unit(...)` + path fns, pure + unit-tested), exactly as
  `src/service.rs` does for the gateway; the `launchctl` / `systemctl --user`
  invocation is validated by the operator (it touches the real service manager).

### Meeting tools (retained, unchanged call shape)
- `rooms.list`, `rooms.join(name)`
- `meeting.wait_my_turn` (25 s long-poll wakeup), `meeting.submit(content)`,
  `meeting.mark_responding`, `meeting.status`, `meeting.leave`
- `_join_internal` — extended params (see Identity).
- `wait_my_turn` is a **historical name** for the long-poll wakeup — there is no
  turn-taking; it simply returns when something new is posted (or the timeout
  elapses). Renaming it would break installed MCP configs, so the name stays.
- Messages are addressed by `(date, n)` (not a global `seq`) in return payloads;
  the tool **names and call shape are unchanged** — the proxy preserves the JSON
  the agent sees.

### Human attach (TUI as client)
- Bare `rozum` launched **inside a project** enters that project's room directly
  (shown if it exists; opened fresh otherwise — the room is not materialized on
  disk until its first message, see Lifecycle). Launched with no project context
  — or to change rooms — it opens a **room picker** listing the daemon's rooms
  (grouped by project, with topic / participant count / activity). `rozum --room
  NAME` jumps straight to a named room. The daemon is auto-spawned if needed.
- After entering, the view is exactly today's room TUI. A statusline shortcut
  (`[o]rooms`, shown in the TUI footer) opens the picker to **switch** to another
  room at runtime without leaving the TUI — the TUI re-subscribes to the selected
  room's feed.
- The TUI is a pure client: it does **not** own room lifetime — closing it (or
  switching away) leaves rooms running. It renders one current room at a time
  from a streamed coordination feed (`MeetingEvent`) plus direct disk reads; no
  in-process `Arc<Mutex<Meeting>>` sharing.
- Multiple TUIs may run at once, each on its own selected room, watching rooms
  independently. They share nothing but the daemon and the on-disk logs.
- Existing in-room controls remain: type to submit, `/pause` / `/resume`,
  `/name`, `/kick`, `/stop` (ends the room), `[o]rooms` (picker), `[q]` detach TUI
  (room survives). No turn-taking, interject, or moderator controls.

### On-disk layout (canonical store)
A project's room lives **in the project**, under `.rozum/room/`, with the
transcript split into **daily files**:
```
<project>/.rozum/
  .gitignore          auto-written `*` so nothing under .rozum/ is committed
  room/               the canonical project room
    meta.json         name, topic, project, phase, created_at, budget_chars
    roster.json       participants: {id, handle, kind, project, session_token}
    index.json        date → {count, bytes} — rebuildable accelerator (days list, totals)
    2026-06-16.jsonl  one local-calendar day of turns, one Turn per line
    2026-06-17.jsonl  a new file opens on the first message of each new day
```
- A message's canonical address is `(date, n)`, where `n` is its **0-based index
  within that day's file**, reset to 0 each new day — line `n` of the file *is*
  message `n`. No global counter spans days; `index.json` (date→count) supplies
  the days list and totals. Within-day `from=N` is resolved by reading the
  (bounded) day file — there is no per-message offset index.
- Ad-hoc / non-project rooms (a picker "new room" with no project context) fall
  back to `$XDG_STATE_HOME/rozum/rooms/<name>/` with the same internal layout.
- The daemon keeps a small global registry of room locations at
  `$XDG_STATE_HOME/rozum/rooms.json` (project paths + ad-hoc rooms) so
  `rooms.list` and startup reopen work without scanning the filesystem.
- An extra named room in a project (`--room NAME`) lives at
  `.rozum/rooms/<name>/`; the canonical project room is the single `.rozum/room/`.
- Local clients open the day files read-only and tail them; only the daemon
  writes.

## Behavior

### Multi-room daemon
- [ ] The daemon holds a `RoomRegistry` mapping room id → open room; it serves
      many rooms concurrently with no fixed cap.
- [ ] Each room runs as a supervised async task, not an OS thread; an idle but
      open room consumes ~no CPU (notify-driven) and bounded RAM (high-water +
      counters only). Long-idle rooms are evicted entirely and reopened on demand
      (see Lifecycle).
- [ ] A panic inside one room's handling is caught and isolated — it must not
      abort other rooms or the meeting daemon.
- [ ] The meeting daemon has **no model code**. A room that needs a model calls
      the gateway's local HTTP API like any other client — model serving and room
      coordination never share a process, so neither can stall the other.

### Single-writer daemon, direct-read clients
- [ ] The daily files under `.rozum/room/` are the source of truth. The
      **daemon is the only writer**; it owns the per-day index `n`, ordering,
      atomic append into the current day's file, and budget accounting.
      `meeting.submit` is therefore still an RPC to the daemon.
- [ ] A turn's canonical address is `(date, n)`: `n` starts at 0 in every new day
      file and increments per append. It is never renumbered on load
      (random-access by `(date, n)` or `(date, byte-offset)`). There is no global
      counter across days.
- [ ] The active day file is `YYYY-MM-DD.jsonl` by the append's **local calendar
      date**; the first message of a new day opens a new file (no message ever
      splits across files).
- [ ] Write-before-notify: the daemon flushes the appended line durably to disk,
      **then** publishes the new high-water `(date, n, end_offset)`. A client
      that reads up to a notified `end_offset` always sees complete, durable lines
      — never a torn final line.
- [ ] Clients (TUI, `mcp-proxy`) read turns **directly from disk**, not via the
      daemon: they tail the `date` day file `[last_seen_offset, end_offset)`,
      parse whole lines, and advance; when the notified `date` rolls forward they
      open the new day file from offset 0. Content bytes never pass through the
      daemon socket.
- [ ] The daemon holds **no transcript turns in RAM** for serving reads — only a
      per-room high-water `(date, n, end_offset)` and budget counters. An idle open room is
      roster + counters + a notify; this is what makes rooms effectively
      unbounded.
- [ ] Wakeups are daemon-driven: `wait_my_turn` long-polls and returns the
      coordination delta (new high-water `(date, n)`, phase, roster, responding) —
      a wakeup, not a turn grant (submit is free-form, anyone any time). The
      client then does the disk read. (Filesystem watch — kqueue/FSEvents — is an
      optional resilience add-on, not required.)
- [ ] The `mcp-proxy` implements the agent-facing tools by reading disk + the
      daemon's coordination signal; the **agent-visible tool contract
      (`meeting.*`) is unchanged**.
- [ ] Resilience: while the daemon is down, clients still read full history from
      disk (read-only); on daemon restart they resync the high-water `(date, n)`
      and continue. Submits pause only while the single writer is absent.
- [ ] Opening a room reads only the **newest day file** to recover the
      high-water `(date, n, end_offset)`; the budget total comes from `meta.json`
      and per-day counts from `index.json` (both rebuilt by a one-time scan if
      absent). It does not materialize every turn or read older day files.

### Stable identity (session-lifetime)
- [ ] `ParticipantId` is an opaque stable id, decoupled from display name. The
      positional `#N` suffix is removed.
- [ ] Each participant gets a friendly, memorable handle minted once (e.g. an
      adjective-animal), namespaced/grouped by project; display reads like
      `claude · eager-otter`.
- [ ] `_join_internal { client_info_name, project, session_token }`: a proxy
      generates a random `session_token` at startup and holds it in memory for
      its lifetime. First join mints `{participant_id, handle}` bound to the
      token and returns them; any reconnect within the session re-presents the
      token and rebinds the **same** id and handle — no reshuffle.
- [ ] The token→participant binding is persisted in `roster.json` (which exists
      once the room has its first message), so a daemon restart rebinds a live
      proxy's token to its prior handle. An unspoken room has no `roster.json`
      yet, so a restart before the first message re-mints handles — acceptable.
- [ ] A full agent restart (new proxy process ⇒ new token) mints a new handle —
      session-lifetime stability, by design.
- [ ] Liveness staleness (responding/polling 30 s windows) governs only
      online/typing indicators, never identity.

### Project-based room naming
- [ ] A project maps to one canonical room whose name is the project (git repo
      name / cwd basename), e.g. project `rozum` → room `rozum`. All agents from
      that project resolve to the **same** room (idempotent — no `-2` suffix for
      the second agent of the same project).
- [ ] An explicitly-named extra room (`--room NAME`) is distinct; the
      adjective-noun generator (`rapid-finch`) is the fallback when no project
      context is available, and remains selectable explicitly.

### TUI room selection
- [ ] Launching the TUI inside a project enters that project's room directly,
      skipping the picker (shown if it exists, opened fresh otherwise).
- [ ] Launching with no project context (or an explicit switch) presents a
      picker listing all daemon rooms (grouped by project); selecting one enters
      that room.
- [ ] The picker can be reopened in-room to switch to a different room without
      restarting the TUI; the feed re-subscribes to the newly selected room.
- [ ] Several TUIs run concurrently, each viewing its own selected room, with
      independent event feeds and no cross-interference.
- [ ] With no rooms present, the picker offers creating a new room.
- [ ] The in-room TUI footer/statusline shows a room-picker shortcut
      (`[o]rooms`); pressing it opens the picker to select/switch without
      restarting or leaving the TUI.

### TUI day-scoped rendering
- [ ] On entering a room the TUI loads and live-tails the **current day** file;
      it does not load the whole history.
- [ ] Scrolling above the earliest loaded message lazily loads the previous day
      file (one day per step, via `index.json`); already-loaded days are not
      re-read.
- [ ] Day boundaries render as separators (`── YYYY-MM-DD ──`).
- [ ] When a new local day starts, new turns appear under a fresh day separator
      (the TUI follows the writer's day rollover via the `date` in the feed).

### Lifecycle
- [ ] A room's on-disk directory + transcript log are created lazily on the
      **first submitted message** — launching a TUI, connecting, or joining does
      not by itself create an empty room. Once created it persists, and
      `rooms.list` shows it whether or not anyone is currently joined.
- [ ] A room persists independently of connections: it survives all participants
      leaving and survives TUI detach.
- [ ] A room **ends** only on explicit operator `/stop` (phase → `Ended`,
      pending long-polls get `{ended}`). Idle does **not** end a project room.
- [ ] A long-idle open room is **evicted** from the daemon's open set (its task
      stops, RAM freed) but its files remain; it reopens lazily on next reference.
      Deleting a room's files is always explicit — never automatic for a project
      room.
- [ ] On daemon startup, rooms are reopened from the global registry
      (`rooms.json`): each room's `meta.json` / `roster.json` and the high-water
      from its newest day file are restored, without reading older day files.
- [ ] On graceful daemon shutdown, pending `wait_my_turn` calls return
      `{ended:"server-shutdown"}`; rooms remain on disk and reopen next start.

## Out of scope
- Authentication / multi-user / multi-tenant isolation (still single-user local).
- Remote / cross-machine meetings and any network MCP transport — but see
  **Future: remote stateless read** for the planned read path.
- Model-as-participant as a default — a model joining a room stays an explicit
  opt-in capability, off by default (consistent with `SPEC.md`).
- Full-text / cross-room search and SQL-style queries over transcripts (the
  append-only log is canonical; a query engine is a later, separate item).
- Changing the model-serving HTTP contract (`/v1/*` is untouched).

## Future — remote stateless read, by day (not now)

The direct-disk read path requires clients to share the daemon's filesystem, so
it only serves local clients. For remote readers, the **meeting daemon** exposes
a **stateless, day-scoped REST read API** on its own HTTP listener (the model
gateway stays untouched; this is the meeting daemon's namespace, not `/v1/*`). It
mirrors the on-disk daily layout one-to-one:

```
GET /rooms/{name}/days
    → [{date, count, bytes}, …]   newest-first (straight from index.json)

GET /rooms/{name}/messages/YYYY-MM-DD[?from=N][&count=M]
    → that day's turns, each carrying {date, n, participant_id, ts, content}
```

- The **date is a path segment** — the day file is the resource. `from` is the
  0-based `n` within that day and `count` bounds the slice (default: the whole
  day). A day file is bounded, so "the last N of today" is the tail of one GET.
- "Newest day" is the first entry of `GET …/days`; the client resolves it, then
  GETs `…/messages/<that-date>`. No reverse-offset query — `/days` is newest-first.
- Read-only and stateless: the request carries everything (room + date + range);
  the meeting daemon reads that **one** day file and answers — no server-side
  cursor, no cross-file walk. It is the only sanctioned content path for a
  non-local reader; local clients keep tailing the day files directly.

Submits from remote participants (a write path over HTTP) are a separate, later
question — this entry covers reads only.

### Topology
A dedicated meeting daemon (`rozum meetings`) owns
`RoomRegistry { rooms: HashMap<RoomId, Arc<RoomHandle>> }`, backed by the on-disk
`rooms.json` registry of room locations. It has **no `Switchboard` and no model
code** — the gateway is a separate, unchanged process. A `RoomHandle` owns the
room's notify, event broadcast, budget counters, high-water `(date, n,
end_offset)`, and the path to its on-disk dir (`<project>/.rozum/room/`). The
meeting MCP is one rmcp server bound to `meeting.sock`; each MCP session carries
`current_room: Option<RoomId>` set by `rooms.join`. This mirrors the existing
`mcp-proxy` join-then-forward shape, so the proxy change is "dial one socket and
pass the room name" rather than "scan a directory of sockets."

### Transcript as a daily log, split writer/reader
Split the old `Meeting.transcript: Vec<Turn>` into a daemon-side **writer** and a
client-side **reader** over the same daily files.

Daemon (`TranscriptWriter`): owns the append fd for the current day file, the
active `date`, the per-day index `n` (reset to 0 on day rollover), `end_offset`,
and budget totals. `append(turn)` rolls to a new `YYYY-MM-DD.jsonl` if the local
date changed, writes one JSONL line, fsync/flushes, advances the high-water,
updates `index.json`, then notifies — and publishes `(date, n, end_offset)` on
the event stream. It keeps **no turns** in memory. On the first `append` it creates
`<project>/.rozum/` (with a `.gitignore` of `*`) and its `room/` dir, and
registers the room in `rooms.json`: a referenced-but-unspoken room stays purely in-registry (name +
roster, no disk footprint) until its first message.

Client (`TranscriptReader`, used by TUI and `mcp-proxy`): opens the current day
file read-only, tracks `(date, last_offset)`, and on each coordination wakeup
reads `[last_offset, end_offset)` (rolling to the next day file when the notified
`date` advances), parses complete lines into `Turn`s, and renders / returns them.
The client owns whatever tail/scrollback it wants in its own RAM, independent of
the daemon and of other clients.

`MeetingEvent` shrinks: turn events carry `{date, n, participant_id, ts,
end_offset}` but **not** `content`. `enable_persistence` stops being optional —
every daemon-hosted room is disk-backed; the in-memory `Vec` and the
daemon-serves-`turns_since` path are dropped.

### Identity
`Participant { id: ParticipantId(opaque), handle: String, kind, project,
session_token }`. Join resolves a participant by `session_token` first; absent →
mint id + handle and persist to `roster.json`. Reclaim is keyed on the token, so
the old name+staleness reclaim heuristic in `join_non_human` is replaced.

### TUI client, day-scoped
The TUI subscribes to the room's `MeetingEvent` stream over the socket and
renders incrementally; control actions become RPCs. The in-process
`Arc<Mutex<Meeting>>` sharing between TUI and server (today in `run_room`) is
removed for the daemon path.

The TUI reads **by day**, matching the storage: on entry it loads the current
day file and live-tails it; scrolling above the earliest loaded message lazily
loads the previous day file (one day at a time, via `index.json`), so memory
stays bounded on a long-lived room. Day boundaries render as separators
(`── 2026-06-16 ──`), and the writer's day rollover (the `date` in the feed)
opens a fresh day section for new turns.

## Decisions
- **Dedicated meeting daemon; the model gateway is untouched** — reverses the
  initial one-daemon idea (user's call). The gateway is stateless, horizontally
  scalable, and idle-exiting by design (`distributed-readiness.md`); rooms are
  stateful, long-lived, single-writer. Co-hosting them fights both: you cannot run
  gateway replicas if one process owns room write-state, and the gateway's
  idle-exit / rolling-deploy lifecycle conflicts with rooms that must persist. A
  separate meeting daemon lets each keep its natural lifecycle, drops blast radius
  to zero, and leaves `gateway.rs` unchanged. Cost: two daemons instead of one,
  and model-as-participant becomes a localhost HTTP call to the gateway (instead
  of an in-process `Switchboard` call) — uniform with every other client, so a net
  simplification.
- **Rooms in the daemon as tasks, not threads** — chosen because room work is
  async coordination + small disk I/O; the runtime already offloads CPU-heavy
  inference to the blocking pool. Per-room OS threads would waste memory and add
  no isolation that supervised tasks + panic-catch don't already give.
- **One daemon socket + `rooms.join` routing** — chosen over a socket-per-room
  bound by the daemon; it makes discovery a live RPC and matches the proxy's
  existing join-then-forward flow. Rejected: per-room sockets (re-creates the
  directory-scan discovery we are removing).
- **Append-only JSONL log is canonical** — chosen (user's call) as the faithful
  evolution of today's JSONL persistence; greppable, human-readable, no new
  dependency. Rejected for now: embedded SQLite (richer queries + WAL, but a new
  dep and a break from `SPEC.md`'s no-DB default) — revisit only if cross-room
  search is needed.
- **Transcripts live in the project at `.rozum/room/`** — chosen (user's call):
  the room is co-located with the code it is about, travels with the project, and
  is trivially inspectable; project identity *is* room identity, which matches
  project-named rooms. Consequences accepted: discovery can no longer scan one
  dir, so the daemon keeps a `rooms.json` registry of room locations; the daemon
  writes into each project's `.rozum/` (fine on single-user local). An
  auto-written `.rozum/.gitignore` (`*`) keeps transcripts out of git by default.
  Non-project / ad-hoc rooms fall back to `$XDG_STATE_HOME/rozum/rooms/<name>/`.
- **Daily transcript files (`YYYY-MM-DD.jsonl`) with a per-day counter** — chosen
  (user's call): bounded per-file size, cheap startup (only the newest day is read
  to resume), and easy archival / GC / per-day grep. A message's address is
  `(date, n)` with `n` reset to 0 each day, so each day file is self-contained
  (line `n` == index `n`) and deleting an old day leaves **no gaps**. A
  rebuildable `index.json` maps date→`{count,bytes}` for the days list, totals,
  and locating which day file to read (within-day lookup scans the bounded file).
  Coordination signals carry `(date, n, end_offset)` so
  direct-read clients roll to the next day file at the boundary. Accepted
  tradeoff: no single global message id — `(date, n)` is the stable (compound) id
  instead. Rejected: a room-global monotonic `seq` (simpler one-int cursor, but
  leaves gaps under day-file GC and needs a cross-file offset index); size-based
  rotation (daily is simpler and matches the ask) — add later if a day's file
  grows unwieldy. Both consumers mirror this layout: the TUI renders and scrolls
  **by day** (loads the current day, lazy-loads older days, day separators), and
  the future REST read is **day-scoped** (`/days` + `/messages/<date>`) — neither
  walks a global sequence.
- **Clients read content from disk; daemon coordinates + is the single writer**
  — chosen (user's call) for scale and resilience: read bytes never fan out
  through the daemon (only the one small `submit` write does), so content fan-out
  is the OS page cache over the day files and the daemon's per-room memory is
  ~roster + counters; clients survive daemon downtime as readers. Single-writer (not direct client appends) is kept because
  JSONL lines exceed `PIPE_BUF`, so concurrent multi-process `O_APPEND` would
  interleave/corrupt; routing the small submit through the daemon also keeps
  `(date, n)` assignment authoritative and budget central. Rejected: daemon ships
  turn bytes to every client (re-introduces the daemon as a bandwidth/memory
  bottleneck); multi-writer direct-to-disk (corruption / no central ordering
  without file locks).
  Valid only because all clients are local on one filesystem — consistent with
  the single-user-local, no-remote constraint.
- **Session-lifetime stable identity, token in proxy memory, binding persisted
  in roster** — chosen (user's call); kills the `#N` reshuffle with the least
  machinery and survives daemon restart for a live proxy. Rejected: durable
  across full agent restarts (needs a persisted client token + a "same agent or
  new?" policy and risks collisions for two same-named agents in one project).
- **TUI becomes a daemon client** — chosen (user's call) because the daemon is a
  headless service; the human attaches/detaches without owning room lifetime.

## Results
<!-- Fill in after implementation: room count under load, idle RAM per room,
     transcript read latency tail vs cold, identity-stability test, regression
     of model-serving path with rooms active. -->
