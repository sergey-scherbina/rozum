# nadia — the coding agent

nadia reads and edits files, runs commands, checks its own work and stops when it is
done, driving a small local model through this gateway with no API key and no network.
It exists twice on purpose, and this document is the map of both halves:

| | Where | Language | Role |
|---|---|---|---|
| **in this repo** | `crates/nadia` | Rust | The executable reference. Carries subagents, the HTTP control surface and the Telegram front-end. |
| **its own repo** | [github.com/sergey-scherbina/nadia](https://github.com/sergey-scherbina/nadia) (`../nadia`, `REPOS.md`) | ScalaScript + Scala 3 | Two more implementations of the same spec, plus containers, Kubernetes and the non-local providers. |

**The contract lives in exactly one file: `nadia:SPEC.md`.** It was written before any
implementation and all three are reviewed against it. There is deliberately no
`docs/specs/nadia*.md` here — a second spec would be a second source of truth, and where
two implementations disagree the spec decides which one is wrong. What follows is
reference and operations, not contract.

## Position in the stack

```
nadia               tools · prompts · sandbox · approval · subagents · CLI/REPL/HTTP
   │ uses
rozum-agent         the loop, tool dispatch, budgets, the transcript   (Contracts 2–3)
   │ talks to
rozum gateway       stateless POST /v1/chat/completions (tools, SSE)   (Contract 1)
                    + per-family tool rendering/parsing + constrained decoding
   │
the model           native MLX, in process
```

The three contracts are specified in [`specs/integration.md`](specs/integration.md).

**The boundary that must not blur.** nadia emits **neutral OpenAI-form** tool JSON —
`name` / `description` / `parameters` — and nothing else. Rendering that into the syntax a
model family was trained on (Qwen `<tool_call>`, GLM `<arg_key>`, DeepSeek
`<｜tool▁sep｜>`, harmony) and parsing the reply back stays here, in
`crates/rozum-core/src/serving.rs` and the chat templates. A parser on the agent side
would be a second source of truth, and its failure mode is a gateway defect that reads as
a model defect — which this project has already paid for twice.

## The one in this repo — `crates/nadia`

### Build and run

The crate depends only on `rozum-agent`, `rozum-core` and `rozum-gateway`, none of which
link a model engine, so it builds in seconds without the MLX toolchain:

```bash
cargo build -p nadia --release          # → target/release/nadia
cargo install --path crates/nadia       # → ~/.cargo/bin/nadia, needed by the Telegram bridge
```

```bash
# a gateway with a tool-capable model
rozum gateway --model mlx-community:Qwen3.5-4B-MLX-4bit --port 8080

nadia run "add a --json flag to the CLI and a test for it"   # one task, headless, in cwd
nadia                                                        # or sit in it (chat is the default)
nadia serve                                                  # subagents over HTTP
nadia mcp list --probe                                       # MCP servers, and what they serve
nadia help                                                   # the same text as -h
```

### CLI

| Option | Default | Notes |
|---|---|---|
| `--workspace <DIR>` | current directory | The jail. Nothing outside it is writable. |
| `--gateway <URL>` | `$OPENAI_BASE_URL`, else `$ROZUM_GATEWAY_URL`, else `http://127.0.0.1:8080/v1` | Either spelling is normalized to one `/v1`. |
| `--model <ID>` | `$NADIA_MODEL`, else `local` | `local` is whatever the gateway has resident. |
| `--max-steps <N>` | 24 | Model round-trips per task. |
| `--allow-net` | off | Lets `bash` reach the network. |
| `--no-confine` | off | Skips `sandbox-exec` (macOS). |
| `--json` | off | Batch: the full result, including every tool call, as JSON. |
| `--mcp <NAME>` | none | Connect this MCP server's tools for the run; repeatable. `--mcp-all` takes every server in the config, `--mcp-config <PATH>` says where it is. |
| `--port` / `--bind` / `--token` | `8790` / `127.0.0.1` / `$NADIA_TOKEN` | `serve` only. |
| `--record <PATH\|auto>` | off | Journal this run. `auto` places it in `.rozum/runs/<id>.jsonl` and prints the id. Nothing records by default. |
| `--replay <ID\|PATH>` | off | Re-run a journal instead of calling a model. An id is resolved in `.rozum/runs`; a path is used as-is. |
| `--replay-live-tools` | off | With `--replay`: replay the plan, run the tools for real, stop at the first result that differs. |
| `--replay-fork <PATH\|auto>` | off | With `--replay`: instead of stopping, continue live from the divergence into a new journal. |

`NADIA_DEBUG=1` prints the gateway, model and workspace a run actually used — the first
question asked of any surprising matrix row, and not recoverable after the fact.

Batch exit codes are the contract `scripts/bench/agentic.sh` reads: **0** finished,
**1** budget exhausted, **2** gateway or transport failure. The harness treats 2 as
infrastructure rather than as a model failure; conflating the two is how a dead gateway
gets recorded as a bad model.

### The six tools

`read_file` · `write_file` · `edit_file` · `list_dir` · `grep` · `bash`

That is the whole surface, and the bar for a seventh is that it enables a task class
*impossible* with these six, not merely more convenient. Schemas are re-sent on every step
of every turn and compete for a small model's attention — measured here as ~33 tools /
~4.9K schema tokens for stock Claude Code, which `rozum launch --lean` cuts by 84%.
A test asserts the count, so the set cannot grow by accident.

Two behaviours are worth knowing because they are load-bearing rather than incidental:

- `edit_file` requires `old_string` to match **exactly once**. Zero or several matches is
  a refusal that changes nothing and tells the model to re-read with more context. A tool
  that replaced the first match would let a model "fix" one of five identical lines and
  report success.
- A tool error is not an exception — it is the next prompt. Every message is written as an
  instruction the model can act on, and output is clipped to 8 KB with the truncation
  *announced*, because silent truncation makes a model confidently wrong about what it saw.

### The seventh tool is not built, it is connected — MCP

The bar above is why nadia grows no tools. It borrows them instead: `--mcp <name>` connects
an **MCP server** for the run, from the ecosystem's `mcpServers` config (`--mcp-config`, else
`<workspace>/.mcp.json`, else `~/.config/nadia/mcp.json`), and `nadia mcp list --probe` shows
what each one serves. Contract: `nadia:SPEC.md` §2.1, identical in all three implementations.

```bash
nadia run "…" --mcp rozum        # {"mcpServers":{"rozum":{"command":"rozum","args":["mcp-proxy"]}}}
```

The rules that matter from this side:

- **Opt-in per run** — a config that merely exists adds nothing. One server can add a dozen
  tools to the six, which is the same schema-token tax `rozum launch --lean` exists to undo.
- **`mcp__<server>__<tool>`**, so the six can never be shadowed.
- **Gated exactly like `bash`**, and **outside the workspace jail** — a server is a separate
  process with its own access to the machine, and every connect says so.
- **A named server that will not start ends the run**, with its name; one that dies mid-run
  turns its tools into tool errors the model can act on instead of killing the run.

The Rust implementation gets the transport from `rozum-agent`'s `McpToolSource` (rmcp,
`docs/specs/mcp-toolsource.md`) and owns only the policy: config, selection, naming, gating.
What this buys the room, concretely: nadia connected to `rozum mcp-proxy` can *post* — the
launch-side bridge below carries presence for a nadia that has no MCP, and this is the other
half, where it answers.

### Commands: `help` and `?`

The REPL takes `help`, `?`, `/help` and `/?`, bare or with a command name — `help` lists
every command with its **format** (`/tell <id> <message>`; the arguments are what a user does
not know at that moment) and one line, `help tell` adds the paragraph. An unknown name gets
the names, not the page. `nadia help` from the shell prints the same usage as `-h`.

It is rendered from a command table (`crates/nadia/src/commands.rs`) rather than a string
literal beside the dispatcher, because those are two lists that must agree and therefore
eventually don't.

### Record and replay — a run you can run again

A run has exactly two nondeterministic inputs: what the model said, and what a
tool answered. Everything else in the loop is a pure fold over those, so
journaling both — in call order, as JSONL — is enough to re-run the whole thing
later.

```bash
nadia run "<task>" --record auto      # → .rozum/runs/1788422537-79d9f8.jsonl
nadia runs list                       # id, entries, task, and what forked from what
nadia runs rm <id>
```

Three modes, and they answer different questions:

| Mode | Model | Tools | Question |
|---|---|---|---|
| `--replay <id>` | journal | journal | Does the loop still behave the same, given identical inputs? |
| `--replay <id> --replay-live-tools` | journal | **real** | Does the plan that failed still fail against today's tree? |
| `--replay <id> --replay-fork auto` | journal, then **live** | **real** | Carry the run forward onto today's world, as a new journal. |

Strict replay touches nothing — no gateway, no network, no tools, no writes —
which is what makes it usable as a regression test in CI with no model at all.
Prove it by pointing `--gateway` somewhere unreachable: if the same answer comes
out, nothing called out.

Live-tools is the fix loop. It stops at the first tool result that differs,
naming the tool, what it returns now, what the recording had, and how far the
plan replayed identically. **The stop is the answer**, not a failure to finish:
"where did reality stop matching" is precisely what you were asking.

Fork is live-tools that does not stop — read it as *rebase for runs*. The prefix
that still matches costs no model calls, a note records why it stopped being a
replay, and a live model carries on from there into a new, self-contained
journal that replays strictly like any other.

**Divergence is loud, never silent.** Every entry carries a fingerprint of the
call that produced it; a call that does not match the next entry fails with both
fingerprints rather than answering a different question. Running past the end of
the journal is a divergence too.

Two consequences worth knowing before you rely on it:

- **Replay with the same `--workspace` and the same tools.** The workspace path
  is in the system prompt and tool names are in the request, so both are part of
  the fingerprint; changing either is correctly refused as a different run.
- **The verify gate is skipped under `--replay`**, and says so. The gate makes
  its own model calls, which the journal never recorded, so running it would
  reach for a gateway in the one mode whose promise is that it does not. A
  replay is not a verified run.

A journal holds everything — system prompt, your words, every model reply, every
tool result. It is exactly as sensitive as the session it recorded; recording is
always explicit, and the file is yours to keep or delete.

### Containment

Three independent mechanisms; the sandbox decides what is *possible*, the approval gate
what is *wanted*, and they are not the same question.

1. **Path jail** — every path argument is canonicalized (including the not-yet-existing
   tail) and checked against the workspace root. Escapes are **refused, never clamped**: a
   jail that stripped `..` would turn `../secrets` into `<root>/secrets` and write
   somewhere nobody asked for.
2. **Exec confinement** — `bash` runs with cwd pinned to the root, a 120 s default
   timeout, and on macOS a seatbelt profile that denies writes outside the root and denies
   network unless `--allow-net`. Writes only: confining *reads* aborts dyld before the
   child ever runs. `/tmp` is deliberately not on the allowlist — it is world-writable and
   shared, so allowing it hands the agent a free channel out of the workspace.
3. **Approval gate** — in chat, `write_file` / `edit_file` / `bash` ask first, with a diff
   for edits; reads are never gated, because prompting for them trains the operator to hit
   `y` without reading. `/approve auto` turns it off; batch runs start in auto because
   asking would deadlock on a stdin nobody is at. **Anything that is not an explicit yes
   is a refusal, including EOF** — the first version read a bare Enter as allow, and
   `read_line` also returns `""` at EOF, so a piped stdin auto-approved everything.

A denial reaches the model as a tool error, not a halt. "The user declined" is something
it can answer; a killed turn is not.

### The verify gate — success is not the model's to declare

Contract: `nadia:SPEC.md` §3.1; the shared primitives are specified in
[`specs/verify-gate.md`](specs/verify-gate.md). Before a task runs, the model is asked to formalize it into a
check (`{"checkable","cargo_test","run":[{"arg","expect"}]}`) and *rozum* builds the shell
command from that structure — so the model supplies values, never shell. After the agent stops,
the command runs in the workspace and its exit status decides. A failure comes back as the next
turn carrying the command and **what it actually printed**; bounded at
`NADIA_VERIFY_ROUNDS` (default 2). `NADIA_VERIFY=0` turns the gate off.

Why it exists, concretely: the RPN calculator an agent wrote from Telegram builds, runs, and
prints `4 + 4 = 7`. The model had verified what its prompt asks for — that the program builds and
runs — and nobody had written down what the right answer was. A derived check is that missing
sentence.

The guards matter as much as the check:

- **`checkable: false` is an answer.** A task with no machine-checkable criterion gets no
  invented one — "reply with the word pong" once became `cargo run -- pong == gnop`.
- **Unverified is reported as unverified**, never as a pass. In Telegram that is the
  `⚠ не проверено` line next to `✔ проверка прошла: <command>`.
- **A failed check means not done**, whatever the model says: `nadia run` exits 1.
- **The check describes the task, not how the task was spelled.** A quote that delimited the
  argument in the prompt is not part of the argument (`cargo run -- "3 4 + 2 *"` checks the value
  `3 4 + 2 *`), and a project the model put in a subdirectory is NAMED in the repair prompt rather
  than accommodated by moving the check into it — both measured, both having cost a run its whole
  repair budget.

The primitives are `rozum-agent`'s `verify` module and `rozum launch` uses the same ones — the
derive prompt, the shell builder, the hallucination guard and the judge parser exist once. That
prompt took measurement to word; two copies of it would drift, and the failure mode of a drifted
verifier is a run that reports success nobody checked.

### Budgets and the repetition guard

24 steps, 4096 tokens, 15 minutes, temperature 0. The step budget is the safety net rather
than the plan — the benchmark tasks land in 4–12 steps, and a run that reaches 24 has lost
the thread rather than found a hard problem.

The guard refuses a call that is going in circles: the same tool, the same arguments **and
the same result**, four times in the last twelve calls. Both halves matter. The first
version matched on the call alone and cut `multibug` off mid-repair, because re-running
`cargo test` is the verify half of fix → test → fix and its output changes as the files
do. The gateway's own loop breaker shipped with the identical defect the same week —
BUG-014 in [`BUGS.md`](../BUGS.md), where it fired on 11 of nadia's 16 cells.

### Subagents

An agent is a task with an identity, a mailbox, a workspace and a lifecycle somebody
outside it can drive. The control point is tool dispatch — a running agent is inside the
loop, which does not return until the turn is over, so the only place to reach it is where
it already yields.

| Verb | Meaning |
|---|---|
| `spawn` | Start one on a workspace; returns an id immediately. |
| `list` / `status` | What it is doing, counted from dispatch — not from the agent's own report, because an agent that has lost the thread reports progress happily. |
| `tell` | Something for its next turn. Delivered *between* turns; the loop owns its message list until it returns. |
| `pause` / `resume` | Parks at the next tool boundary and costs nothing while parked. |
| `stop` | Cooperative: reaches the model as a tool error, so it gets to write a closing summary. |
| `kill` | Aborts the task now and frees the slot. No last words. |

Phases: `running` · `paused` · `stopping` · `done` · `failed` · `killed`. `stopping` is a
real state rather than a flag, because a polite stop completes at the next tool boundary
and "did it stop yet" deserves an honest answer.

In the REPL: `/spawn`, `/agents`, `/status`, `/tell`, `/pause`, `/resume`, `/stop`,
`/kill`. Ids are small integers because they get typed by a human under time pressure,
and from a phone.

### `nadia serve` — the same verbs over HTTP

A Telegram bot, the UCC, a shell script and a cron job all want those verbs and none of
them want to be linked into nadia to get them.

| | |
|---|---|
| `GET /health` | |
| `GET /agents` · `POST /agents` | list · spawn (`{"task": …, "workspace"?: …}`) |
| `GET /agents/{id}` · `DELETE /agents/{id}` | status (with `result` once there is one) · kill |
| `POST /agents/{id}/tell` | `{"message": …}` |
| `POST /agents/{id}/pause` · `/resume` · `/stop` | |

```bash
nadia serve --port 8790
curl -s localhost:8790/agents -d '{"task":"fix the flaky test"}' -H 'content-type: application/json'
```

Auth is a shared secret in `x-nadia-token`, required unless the listener is on loopback —
and a tokenless bind to anything else is **refused, not warned about**: a control surface
that starts processes on a public port with no token is not a misconfiguration to log, it
is an open remote-execution endpoint. `no agent 7` is a 404; every other supervisor
refusal is a 409 the caller can show a human verbatim.

Subagents live inside the `serve` process, so `/status` and `/pause` only mean anything
while it is up. That is why it is started on demand rather than run as a service — a
service that restarted under them would silently lose their work.

### From Telegram

`crates/rozum-meeting/src/telegram/nadia.rs` maps the bot's commands onto that protocol.
No second bot and no second access list: the same per-room roster that governs the
assistant in a chat governs this. `/spawn`, `/tell`, `/pause`, `/resume`, `/stop`, `/kill`
need **`write` + `shell`**, because that is exactly what the agent will do on the caller's
behalf; `/agents` and `/status` need only `chat` or `read`. A refusal names the grant that
is missing (`/grant <id> write shell`).

The first command that needs it brings `nadia serve` up on loopback :8790 and every later
one reuses it, so an operator who never spawns an agent never pays for the process. It
requires `nadia` on `PATH` — which the UCC deploy now installs.

Three things turn that protocol into something usable from a phone:

- **The result comes to you.** A watcher in the bridge posts each agent's outcome into the
  chat that started it, once, when it finishes — done, failed or killed, with the summary.
  Without it the protocol was complete and the workflow was not: you would start an agent
  and then poll `/status 3` until it changed, which is a job for a machine and is exactly
  the machine you are talking to. The watch list is on disk (`nadia-telegram.json` in the
  state dir), so a bridge that re-execs — it does, on every group topology change — still
  delivers. An id reused by a restarted `nadia serve` is detected by comparing the task and
  dropped rather than delivered: a result posted into the wrong chat reads as a real one.
- **Where it works is yours to choose.** `/projects` lists what this machine knows (the
  meeting daemon's registered rooms plus the UCC's own additions — the same two sources the
  UCC project picker reads), `/project <name>` sets it per chat, and `/spawn` passes it as
  the agent's workspace. Unset still means nadia's own scratch dir, `$NADIA_WORKSPACE` or
  `~/.nadia` — its own directory rather than a corner of `~/.rozum`, because what collects
  there is the operator's work and not rozum's runtime.
- **`/nadia on` — plain text is the task.** Ordinary messages go to the agent instead of to
  the chat model: to the one already working (as `/tell`) if there is one, else starting a
  new one. Two live agents and it asks which, rather than guessing — a steering message
  handed to the wrong agent is worse than one extra tap. `/nadia off` gives the chat back to
  the assistant. Interception happens before the room, so a message is never answered twice.

Every verb is also in the bot's command menu, so typing `/` offers them instead of requiring
you to remember they exist.

### In the matrix and in the UCC

`AGENTS=nadia scripts/bench/agentic.sh` needs no harness change. The row is
`rozum launch … nadia run "$prompt"` with no provider flags and no `tool_hint`: nadia
reads `OPENAI_BASE_URL` / `ROZUM_GATEWAY_URL`, which `rozum launch` already exports to
every child, and its workspace defaults to the cwd that launch has already jailed.

The UCC offers it wherever it offers an agent: the matrix chips, **Coders**, **Sessions**
and the phone chat's Agent mode. Coders and chat spawn the headless form (`nadia run
<task>` — the `run` verb matters: a bare `nadia <prompt>` reads the prompt as the *mode*
and exits 2); a Session runs bare `nadia` in tmux, which is the REPL with its approval
gate, so the operator approves each write from the phone. A launch is refused up front if
`nadia` is not on PATH, rather than dying as a 127 inside a log file — `deploy-ucc-web.sh`
builds and installs it so the chip and the machine cannot disagree.

### In a meeting room

nadia has no MCP client, so it cannot join a room the way `claude` and `codex` do —
`rozum mcp add` has nothing to register, and there is no `wait_my_turn` for it to hold
open. `rozum launch` carries the presence instead
(`crates/rozum-meeting/src/meeting/launch_bridge.rs`, spec
[`rozum-native-channels.md`](specs/rozum-native-channels.md)): it joins the project's room,
posts `working: nadia — <task>` when the run starts and `done:`/`blocked:` with the
verify-gate verdict when it ends, and appends everything said by someone else to the
piggyback drop file the launch-local proxy folds into nadia's next request — `‹for you›`
first when the line addresses it. So the human gets **presence and steering**, not a
conversation: nadia can be told something mid-run, but it has no tool to answer with.

On by default for nadia and nothing else (`ROOM_BRIDGE_AGENTS`), and only while Tier-3
piggyback is live — which is what keeps `scripts/bench/agentic.sh` (it passes
`--no-piggyback`) from either spamming the room or having room chatter injected into a cell
it is scoring. `--no-room-bridge` forces it off, `ROZUM_ROOM_BRIDGE=1` forces it on for any
agent, and `$ROZUM_MEETING_AS` names the handle when the default `nadia` would be ambiguous.

On the 2026-07-31 run — 8 tasks × 2 reps, same resident Qwen3.5-4B for everyone:

| agent | passed |
|---|---:|
| claude | 15 / 16 |
| **nadia** | **14 / 16** |
| codex | 9 / 16 |
| opencode | 2 / 16 |

Read that as a pass rate, not a ranking: one cell separates the top two, which is noise at
two repetitions. The interesting number is the spread — the harness around a small model
matters more than the weights. Both nadia failures were `wordcount`, and both were the
same defect: the model answered `{"content": 4}` where the schema said string, and the
tool replied "missing required argument", which was false, so it re-sent the identical
call until the guard ended a task it had already solved. Fixed in `4f6746a`, after this
run; not re-measured since.

Current numbers, regenerated from the run CSVs rather than kept by hand, are in
[`scripts/bench/RESULTS.md`](../scripts/bench/RESULTS.md). The table above is one dated
run and stays here for its story — the failure it explains — not as a live figure.

### Where the code is

| File | |
|---|---|
| `src/main.rs` | argument parsing, batch, the REPL, the live renderer |
| `src/tools.rs` | the six tools |
| `src/mcp.rs` | MCP config, server selection, tool naming, the refusals |
| `src/commands.rs` | the REPL command table the help is rendered from |
| `src/sandbox.rs` | path jail, exec confinement, the seatbelt profile |
| `src/approval.rs` | the gate, the decision, the diff shown at the prompt |
| `src/session.rs` | system prompt, budgets, the repetition guard, the transcript |
| `src/supervisor.rs` | agents as supervised actors |
| `src/serve.rs` | the HTTP surface |
| `crates/rozum-meeting/src/telegram/nadia.rs` | the Telegram front-end |

## The one in its own repo

[github.com/sergey-scherbina/nadia](https://github.com/sergey-scherbina/nadia) — a sibling
checkout at `../nadia`, registered in [`REPOS.md`](../REPOS.md) and described in
[`repos/nadia.md`](../repos/nadia.md).

It holds two more implementations of the same spec, differing from the Rust one in exactly
one axis: how much sits underneath them.

| | Where | Underneath it |
|---|---|---|
| ScalaScript | `nadia:src/*.ssc` | `std.agent` — the thinnest; the SDK carries all three contracts |
| Scala 3 | `nadia:scala/` | its own 323-line SDK, and under that only the JDK |

The Scala 3 one exists to answer what the other two cannot — *how much of an agent is
framework?* The answer is that the generic half (330 lines) is **smaller** than the domain
half: an agent is mostly its tools and its policy, not its loop. That finding is why the
duplication is worth its cost.

It also ships deployable in ways this repo does not: a container image, Kubernetes / ECS /
Cloud Run manifests, and `--provider local|huggingface|openai|bedrock|vertex`. `local` —
this gateway, no credential — stays the default. The interesting part is what happens to
the safety model on the way out: `sandbox-exec` does not exist on Linux, so the agent
stops claiming it and names the mechanism actually in force, and `--allow-net` reports
itself as the no-op it becomes inside a container where the agent and its commands share
one network namespace.

Its documentation, all in that repo: `SPEC.md` (the contract) · `docs/architecture.md` ·
`docs/tools.md` · `docs/safety.md` · `docs/operations.md` · `docs/deployment.md` ·
`docs/development.md` · `BACKLOG.md`.

## Which side owns what

The recurring question when a change touches both. The short answer: if it is about *the
model*, it is rozum's; if it is about *the agent*, it is nadia's — and the Rust
implementation being physically here does not move that line.

| | Owner |
|---|---|
| Per-family tool syntax, constrained decoding, chat templates | **rozum** (`rozum-core/src/serving.rs`) |
| Model hosting, residency, admission, the gateway's own loop breaker | **rozum** |
| The loop, tool dispatch, budgets, the transcript | **rozum** (`rozum-agent`) |
| The six tools, prompts, sandbox, approval policy, subagents, front-ends | **nadia**, in all three implementations |
| The spec all three are reviewed against | **`nadia:SPEC.md`** |
| Containers, Kubernetes, non-local providers | **the nadia repo** |

A defect present in more than one implementation is a bug in each of them — `4f6746a` is
the worked example: the same argument-type refusal was fixed here and in the nadia repo,
and the reasoning was written down once, in `nadia:docs/tools.md`.
