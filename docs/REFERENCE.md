# rozum + nadia — capability reference

Everything the two binaries do, organised by what you are trying to accomplish. Written
from their actual `--help` surface, so a command that appears here exists and a flag that
appears here is spelled the way the binary spells it.

**How this file relates to the others.** [`README.md`](../README.md) is the pitch and the
shape of the system; [`INSTALL.md`](../INSTALL.md) gets it running;
[`TUTORIAL.md`](../TUTORIAL.md) walks a first session end to end;
[`USER_MANUAL.md`](../USER_MANUAL.md) is the meeting-room operator's guide in depth;
[`docs/specs/`](specs/) holds one design document per feature, with the reasoning and the
measurements behind it. **This file is the map**: what exists, what each thing is for, and
where to read more. It deliberately does not repeat the specs' arguments.

---

## 0. The two binaries

| Binary | Package | What it is |
|---|---|---|
| `rozum` | `rozum-cli` | The dispatcher you type. Execs `rozum-gateway` for the heavy work. |
| `rozum-gateway` | `rozum` | The engine: model serving, meeting daemon, RAG, everything below. |
| `nadia` | `nadia` | A coding agent that runs on a local model served by the gateway. |
| `rozum-meet` | `rozum-meet` | Thin frontend: MCP transports and messenger bridges, no model engines linked. |

Two facts worth knowing before anything else, because they explain a lot of surprises:

- **`rozum` execs `rozum-gateway`.** When something looks stale, check the mtime of
  `rozum-gateway`, not of `rozum`.
- **The deployed copies are separate from the built ones.** `scripts/install-bins.sh`
  publishes to `~/.cargo/bin` and, for launchd jobs, `~/.rozum/bin`. A rebuild in the repo
  changes neither until you run it.

---

## 1. Local models — the gateway

The gateway serves one resident model behind **two dialects at once** on `127.0.0.1`:
OpenAI (`/v1/chat/completions`) and Anthropic (`/v1/messages`). Anything that speaks
either — Claude Code, Codex, opencode, aider, `nadia` — is a client.

### Serving

```bash
rozum gateway --model <spec>          # serve, foreground
rozum gateway status                  # model, port, pid, uptime, clients
rozum gateway stop [--force]          # refused while clients are attached, unless forced
```

Useful flags: `--port`, `--n-ctx`, `--strategy`, `--offline`, `--enable-thinking`,
`--draft-model` (speculative decoding), `--dry-run`.

### Changing the resident model without a restart

```bash
rozum gateway switch --model <spec>   # drain → unload → load → resume, clients held by their proxy
rozum gateway unload                  # free the model, keep the daemon (lazy reload on next request)
rozum gateway reload                  # graceful re-exec from the current binary (after an upgrade)
```

**Or through the OpenAI API, with no rozum command at all.** `GET /v1/models` lists every model
on disk — not just the loaded one — exactly once each, with `resident` marking the one serving
now. The resident's row comes first and its `id` is a `claude-…` alias rather than its bare spec,
because that is the only id Claude Code's discovery will accept; both spellings work when you send
one back. Every other row is the real spec:

```bash
curl -s localhost:8080/v1/models | jq '.data[] | {id, resident, size_bytes}'
curl -s localhost:8080/v1/chat/completions -d '{"model":"<spec>","messages":[…]}'
```

A request naming a model this gateway has on disk is served by THAT model: co-resident alongside
the current one when the RAM fits, and otherwise by switching to it. A name the gateway does not
have — `gpt-4`, `local`, whatever a client fills in — changes nothing and is served by the
resident model, which is what those clients expect.

Before this, a request for a model that would not fit was answered by the resident one while the
response echoed the REQUESTED name — a wrong answer, delivered silently. `ROZUM_MODEL_SWITCH_ON_REQUEST=0`
restores the old fall-through for a deployment that would rather answer than pause to swap.

### Memory admission — why a load can be refused

Loading past host RAM can panic or reboot the machine, so a load is admitted only if it
fits: `--min-free-ram-gb`, `--ram-budget-frac`, `--mlx-cache-gb`,
`--allow-concurrent-resident`. When it does not fit, adaptive load lowers `n_ctx` to the
best fit rather than failing (`--no-adaptive-load` to refuse instead).

The same queue is available to **non-rozum** commands, so a benchmark or a Python oracle
cannot overcommit behind rozum's back:

```bash
rozum gateway admit --footprint 8G -- <command>     # waits its turn, holds the reservation
rozum gateway admit --model <spec> --batch -- <cmd> # estimate from a spec; yield to interactive
```

### Recording and replaying what the gateway serves

Journals the **model side** of every request, live, without restarting the daemon under a
running session. This is the only way to journal a client that owns its own loop and its
own tools (Claude Code, for instance) — see §6.3 for why MCP cannot do it.

```bash
rozum gateway record start [auto|<path>]   # prints the run id
rozum gateway record status
rozum gateway record replay <id|path>      # answer from the journal instead of the model
rozum gateway record stop
```

`ROZUM_GATEWAY_RECORD=<auto|path>` does the same at startup, for a gateway spawned for one
session. Journals land in `.rozum/runs/` — the same shelf `nadia runs list` reads.

### Models on disk

```bash
rozum models list [--remote]      # installed, or the curated download list
rozum models info <spec>
rozum models rm <spec>            # refused if it is the active gateway model
```

### Running it as a service

The default is lazy-spawn plus idle-exit. To keep it always warm:

```bash
rozum service install --model <spec>   # launchd (macOS) / systemd --user
rozum service start | stop | status | uninstall
```

---

## 2. Launching agents on a local model

`rozum launch` starts (or reuses) the gateway, sets `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`
for the child, and runs it inside a Seatbelt jail on macOS.

```bash
rozum launch --model <spec> claude          # any program: claude, codex, opencode, aider…
rozum launch --no-model claude              # upstream Anthropic; no gateway in the path
rozum launch --model <spec> --dedicated …   # a private gateway for this one program
rozum launch --model <spec> --lean claude   # trim the tool schemas Claude Code ships
```

Other flags: `--port`, `--gateway-url`, `--n-ctx`, `--backend-url`, `--offline`,
`--no-sandbox`, `--no-piggyback`, `--no-room-bridge`, `--no-channel-wakeup`,
`--no-adaptive-load`, `--no-glm-synth`.

**The sandbox is on by default**: writes confined to the workspace, secrets denied, only
the local gateway reachable off-box. `--no-sandbox` opts out deliberately.

---

## 3. nadia — the coding agent

A full agent loop on a local model: reads and edits files, runs commands, and verifies its
own work against a derived acceptance check.

```bash
nadia run "<task>"      # headless, one task, in the current directory
nadia                   # interactive session (default with no arguments)
nadia serve             # the subagent protocol over HTTP
nadia mcp list [--probe]
nadia runs list | rm <id>
```

**Built-in tools:** `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, `bash`.
`bash` is confined by `sandbox-exec` unless `--no-confine`, and denied network unless
`--allow-net`.

**MCP tools are opt-in per run** — a config file that merely exists adds nothing, because
every tool costs schema tokens in every request. `--mcp <name>` (repeatable), `--mcp-all`,
`--mcp-config <path>`. Their tools are named `mcp__<server>__<tool>` and run outside the
workspace jail.

**Exit codes (batch):** `0` finished · `1` budget exhausted · `2` gateway/transport failure.

Other flags: `--workspace`, `--gateway`, `--model`, `--max-steps`, `--json`, and for
`serve`: `--port`, `--token`, `--bind`.

Deeper: [`docs/nadia.md`](nadia.md).

### 3.1 Recording and replaying a nadia run

Turns "it failed once last night" into something you can run again. Three modes:

| Mode | Model | Tools | Answers |
|---|---|---|---|
| strict | journal | journal | does the agent loop still behave the same? |
| live-tools | journal | **real** | does the plan that failed still fail against today's tree? |
| fork | journal, then **live** | **real** | carry the run forward onto today's world, as a new journal |

```bash
nadia run "<task>" --record auto                       # journal it; prints the id
nadia run "<task>" --replay <id>                       # strict: no gateway, no model, no tools
nadia run "<task>" --replay <id> --replay-live-tools   # stops at the first result that differs
nadia run "<task>" --replay <id> --replay-fork auto    # continues live from the divergence
```

Two things that bite: a replay needs the **same `--workspace`** and the **same tool set**,
because both are part of the model-call fingerprint. And the acceptance gate is skipped
under `--replay` (it makes its own model calls, which the journal never recorded).

Deeper: the `replay` skill in `vendor/agent-plugins/replay/`.

### 3.2 Run journals on disk

`.rozum/runs/<id>.jsonl`, one JSONL file per run, first line a header (task, workspace,
model, created, and for a fork its parent). `nadia runs list` reads that header alone —
listing twenty runs does not parse twenty transcripts — and prints lineage on the row.

---

## 4. Meeting rooms

A room is a shared transcript where humans (TUI, web, Telegram, Discord) and AI agents
(MCP) all post, with no fixed turns. Rooms are hosted by a daemon and persisted to disk.

### The daemon and rooms

```bash
rozum meetings start [--foreground] | stop | status | handoff
rozum meetings install | uninstall          # run it as a user service
rozum meetings attach [--room <name>]       # TUI, defaults to the cwd project's room
rozum rooms prune                           # drop registry entries whose directory is gone
```

### Posting, reading, identity

```bash
rozum meetings post "<text>"                # defaults to the cwd project's room
rozum meetings read
rozum meetings search <query>               # full history, by text and support metadata
rozum meetings inbox                        # messages addressed to you since last look
rozum meetings hello <handle>               # bind THIS agent session to its own name
rozum meetings whoami | who                 # who this session is / who else is live
rozum identity whoami | set-name <name>
```

### Support and incident work

Rooms double as a product-support and incident surface:

```bash
rozum meetings incident open | escalate | resolve | list | show | metrics
rozum meetings queue                        # open threads, worst first
rozum meetings phase active|paused|ended
rozum meetings role <grant|revoke> …        # reporter | assignee | on_call | observer | admin
rozum meetings token …                      # console access tokens (identity + RBAC)
rozum meetings react | redact
rozum meetings repair-threads               # rebuild threads.json from the message log
```

### Bridges

```bash
rozum web --room <name> --port <n>          # HTTP + WebSocket, vanilla-JS client
rozum telegram --room <name>                # needs TELEGRAM_BOT_TOKEN + allowlist
rozum discord --room <name>                 # needs DISCORD_BOT_TOKEN + allowlist
```

Allowlists are deny-by-default and required for groups. Details and the operator recipes
are in [`USER_MANUAL.md`](../USER_MANUAL.md).

### Messenger administration

```bash
rozum messenger status                      # everything: bots, groups, rooms with rosters
rozum messenger bots | groups | acl
rozum messenger service <start|stop|restart> <bot>
rozum messenger bot-add | bot-remove
```

### AI participants in a room

```bash
rozum meetings participant --room <r> --model <spec>     # a chat model as a participant
rozum meetings agent-participant --room <r> …            # a CODING agent per reply
rozum meetings participant-pool --room <r> …             # one participant per room, supervised
rozum meetings participant --rag-project <dir>          # …and let it search that project (§5)
```

`participant` answers with a `/v1/chat/completions` call. `agent-participant` runs a real
`rozum launch … <agent> -p …` per reply, with file and shell access in a working directory.
`participant-pool` supervises one per room and reconciles as groups connect and disconnect.

---

## 5. Retrieval (RAG)

Search a project's code and docs **by meaning**, over syntactic chunks: markdown split
along its parse tree (heading-bounded sections, fences opaque), code split by item, ranked
by BM25 fused with embeddings when they exist.

```bash
rozum rag index [--root <dir>] [--full]     # build; incremental by default
rozum rag search "<query>" [-k N]           # the same hits the tool sees
rozum rag mcp                               # serve rag.search alone, over stdio MCP
```

The index is `<root>/.rozum/rag-index.json`, the vectors `<root>/.rozum/rag-vectors.bin`.

**Where it is served from:** four surfaces, one ranking policy
(`rag_embed::rank_fused`). The meeting-room MCP proxy serves `rag.search` next to the room
tools (so any agent already connected has it); `rozum rag mcp` is the retrieval-only
alternative for a client that wants nothing else; `nadia` registers it in its own agent
loop whenever the project has an index, no configuration; and a meeting-room model gets it
as `rag_search` when started with `--rag-project` (§4). The name differs only on that last
one, because OpenAI function names admit no dot.

**When it earns a call:** the exact token is unknown (a concept, a symptom), the answer is
spread over files that share no literal string, or you are new to an area. When you know
the string or the path, `grep` is exact, instant and never stale — use that.

Deeper: the `rag` skill in `vendor/agent-plugins/rag/`, and
[`docs/specs/syntactic-rag.md`](specs/syntactic-rag.md).

---

## 6. The MCP surface

### 6.1 What rozum serves

| Tool | Purpose |
|---|---|
| `rooms.list`, `rooms.join` | discover and switch rooms |
| `meeting.submit` | post a message |
| `meeting.wait_my_turn` | long-poll for new messages |
| `meeting.mark_responding` | show as composing |
| `meeting.status`, `meeting.leave` | room snapshot; leave |
| `rag.search` | semantic + lexical project search (§5) |
| `state.get`, `state.update`, `state.reset` | durable per-project fact store (§7) |

### 6.2 Connecting

```bash
rozum mcp install          # register the proxy in the agent's user-level MCP config
rozum mcp uninstall
```

Or by hand — HTTP (long-lived, reconnects on drop) or stdio:

```json
{ "rozum": { "type": "http", "url": "http://127.0.0.1:8779/mcp" } }
{ "rozum": { "command": "rozum", "args": ["mcp-proxy"] } }
```

`rozum launch` wires this automatically, so an agent started that way is already in the
room.

### 6.3 What MCP cannot do

An MCP server sees calls to **its own** tools and never a model reply. So it can neither
record a session nor replay one: it has no model side at all, and no visibility into the
client's built-in tools (`Bash`, `Read`, `Edit` in Claude Code, for example). Recording a
client that owns its own loop happens at the **gateway** (§1), and only for a model rozum
serves — on upstream Anthropic no part of rozum is in the path.

---

## 7. Durable task state

A small JSON object per project, independent of the conversation, so a task survives a
`/clear`, a compaction, or a fresh session. Served over MCP as `state.get` /
`state.update` / `state.reset`, stored at `<project>/.rozum/state.json`.

`state.update` takes an **RFC 7396 JSON Merge Patch**: an object field merges recursively,
`null` deletes a key, anything else replaces. A non-object patch is refused outright.

Read it at the start of a task and after any fresh session; write the moment a fact is
learned that the next turn needs; reset only for a genuinely new task.

It is **not** the memory system (that is cross-conversation and curated), **not** the
planning boards (those are human-visible and outlive one task), and not a log of its own
history.

Deeper: the `task-state` skill in `vendor/agent-plugins/task-state/`.

---

## 8. Operations

### Health

```bash
rozum doctor                     # read-only readiness report
rozum doctor --services          # every com.rozum.* job AND whether its endpoint answers
rozum doctor --services-only     # for the periodic job
rozum doctor --strict            # warnings fail the preflight
rozum services                   # what services this build declares, and how each is probed
```

A launchd job that cannot exec looks identical to a healthy one until something probes the
endpoint — which is what `--services` exists for.

### Where the logs are

| What | Where |
|---|---|
| Gateway (per-request events, JSONL) | `~/.rozum/gateway.jsonl` |
| Gateway service stdout/stderr | `~/.local/state/rozum/gateway/service.log` |
| MCP proxy: lifecycle, `rag.search`, `state.*` | `~/.run/rozum/mcp-proxy.log` (`ROZUM_MCP_PROXY_LOG=0` to disable) |
| Meeting daemon | `~/.rozum-meeting-daemon.log` |
| Telegram bridges | `~/.rozum-telegram.log`, `~/.rozum-telegram-groups.log` |
| Any other `com.rozum.*` job | `rozum doctor --services` prints each job's own log path |

### Tuning knobs

Every `ROZUM_*` option can be set three ways, highest precedence first: `--set KEY=VALUE`
on the command line, the environment, then the config's `[options]` table.

```bash
rozum gateway --set ROZUM_GATEWAY_ADAPTIVE_LOAD=0 --set ROZUM_GLM_ARTIFACT_SYNTH=0
```

### Other

```bash
rozum commit-msg          # a commit message for the staged diff, from a local model
rozum list                # active meeting rooms
```

---

## 9. Measuring it — benchmarks

The harness in [`scripts/bench/`](../scripts/bench/) measures the stack **as a user runs
it**: a real `rozum launch <agent>` against a real local model, sandbox and all.

```bash
scripts/bench/agentic.sh                                     # the agentic matrix
AGENTS=nadia TASKS="greet build" REPS=3 scripts/bench/agentic.sh
scripts/bench/run_full_matrix.sh                             # models × agents × tasks
scripts/bench/summarize_matrix.py results/full-matrix-<stamp>
```

Cells are judged **deterministically** — `cargo run`/`cargo test` decides, not the model's
own claim — over a ladder from `greet` (one word, no tools) to `debug` (failing test,
run-read-fix loop), with harder tasks alongside.

Focused probes answer one question each: `rag-ab.sh` (does retrieval help an agent locate
code in a real repo), `contention.sh` (does admission survive a batch antagonist against
an interactive run), `nondeterminism-probe.sh` (how many distinct completions from N
identical requests), `smmrd-measure.sh` (does a big model's real prefill peak match what
admission reserved).

Three rules that decide whether a number means anything:

- **`REPS` is the difference between a number and a pass rate.** One measurement is a
  hypothesis.
- **`BENCH_BIN` decides what is under test, not `PATH`.** A run against a stale install
  reported 23/24 where the current binary reported 24/24 the same day.
- **A shared-gateway run reports pass/fail only.** Its timings are contended (67 s, 193 s
  and 163 s for the same task in one run) and the footprint column is deliberately empty.

The latest numbers live in
[`scripts/bench/RESULTS.md`](../scripts/bench/RESULTS.md), regenerated from the CSVs by
`scripts/bench/render-results.py` rather than maintained by hand.

Every run worth keeping goes in [`scripts/bench/HISTORY.md`](../scripts/bench/HISTORY.md)
with the host's load — that is what makes a later "we got 2× faster" checkable. The
discipline itself is the `performance` skill in `vendor/agent-plugins/`.

Full detail: [`scripts/bench/README.md`](../scripts/bench/README.md).

---

## 10. Where to read further

- **Design and reasoning, one file per feature:** [`docs/specs/`](specs/) — 150+ documents,
  each with the measurements behind the decision.
- **Meeting-room operations in depth:** [`USER_MANUAL.md`](../USER_MANUAL.md).
- **First session, end to end:** [`TUTORIAL.md`](../TUTORIAL.md).
- **nadia in depth:** [`docs/nadia.md`](nadia.md).
- **Models:** [`docs/models.md`](models.md).
- **Agent working practices** (how to use these tools well, not what they are):
  `vendor/agent-plugins/` — `rag`, `task-state`, `replay`, `isolate`, `bugs`, `scrumban`,
  `multi-agent`, `multi-repo`, `performance`, `policy`, `rozum`.
