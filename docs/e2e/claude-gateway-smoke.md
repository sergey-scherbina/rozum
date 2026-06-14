# E2E: Claude Code ↔ rozum gateway ↔ local MLX model

A repeatable end-to-end smoke test that drives a **real, headless Claude Code** against
the **rozum gateway** backed by a **local MLX model** (no real Anthropic), has it create
and build a tiny Rust project, and verifies the result **independently of Claude** (the
files on disk + `cargo run`). It exercises the whole agentic path that matters for
"Rozum as a local LLM provider for CC/Codex":

```
claude (headless -p)  →  ANTHROPIC_BASE_URL=http://127.0.0.1:PORT  →  rozum gateway
                      →  in-process MLX backend  →  Qwen3.6-35B-A3B-4bit (Metal)
```

What it actually exercises in the gateway: the Anthropic `/v1/messages` streaming dialect,
**multi-step tool-use loops** (`write_file`, `bash`/cargo, read-back), **both Qwen3.6
tool-call formats** (JSON and `<function=>` XML — see CHANGELOG), tool-result round-trips,
and clean stop/termination.

## Runner

`scripts/e2e_claude_gateway.sh [build|test]`

| env | default | meaning |
|---|---|---|
| `ROZUM` | `target/release/rozum` (next to the script) | the rozum binary (must be built with `--features mlx-native` and current `master`) |
| `MODEL` | `mlx-community:Qwen3.6-35B-A3B-4bit` | model spec (any cached MLX model) |
| `PORT` | `8400` | gateway port |
| `E2E_MAX_TURNS` | `20` | Claude `--max-turns` bound (a local model can meander; this guarantees termination) |
| `E2E_TIMEOUT` | `600` | hard `timeout` (s) around the `claude -p` run |
| `KEEP` | unset | keep the temp workdir for inspection |

It exits non-zero on any failed check, and prints `✅ PASS` / `❌ FAIL`.

## Tasks (the scenario)

Each task runs in a fresh `mktemp -d` workdir; Claude is told to build there.

- **build** (default, simplest): *"Create a minimal Rust binary `reverse-cli` (Cargo.toml +
  src/main.rs, edition 2021, no deps) that reverses its first CLI argument; run
  `cargo run -- hello` and confirm it prints `olleh`."*
  - **Checks:** `Cargo.toml` exists · `src/*.rs` exists · `cargo run -- hello` ⇒ `olleh`.
- **test** (a step up — adds a unit test + `cargo test`): a **binary** `reverse-cli` with
  `fn reverse(&str)->String` and a `#[cfg(test)]` unit test for `reverse("hello") == "olleh"`,
  both in `src/main.rs`; `main` reverses its first CLI arg.
  - **Checks:** the above + `cargo test` is green + `cargo run -- hello` ⇒ `olleh`.

> **Why a binary (not a lib) for the `test` task.** An earlier `test` prompt asked for a
> `src/lib.rs` + `#[test]`. The model would `cargo init` a default crate (whose template test
> is `assert_eq!(add(2,2), 4)` — always green) and never implement `reverse`, yet the run
> reported **"cargo test green"** — a *false success*. Forcing a binary + an **un-fakeable
> behavior check** (`cargo run -- hello` must print `olleh`) closes that hole: a scaffold-only
> run fails the behavior check as it should.

The runner verifies the **artifacts on disk**, not Claude's prose — so the test is robust to
Claude's exact wording / the model's nondeterminism. The TASK + success criteria are fixed;
the steps are allowed to vary.

## Prerequisites

- `rozum` built: `cargo build --release --features mlx-native --bin rozum` (current master).
- `claude` (Claude Code CLI ≥ 2.1) on `PATH`.
- `cargo` on `PATH`.
- The model cached locally (HF cache): `mlx-community/Qwen3.6-35B-A3B-4bit` (~19 GB).
- **Kill the Bloop/ScalaCli Java daemon first** — it hogs ~18 GB and throttles MLX on the
  shared Apple-Silicon memory: `ps -A -o rss,comm | sort -rn | head` → `kill <java pid>`.

## How it wires Claude to the gateway

The runner sets the same env `rozum launch` would (`ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`,
`ANTHROPIC_MODEL` = the gateway's `/v1/models` id, the `claude-rozum-<spec>` alias), then runs
`claude -p "<task>" --dangerously-skip-permissions --max-turns N --output-format stream-json`.
`--dangerously-skip-permissions` lets Claude write files / run cargo without prompting (it's a
throwaway temp dir). Thinking is **off by default** in the gateway (clean output); set
`ROZUM_ENABLE_THINKING=1` before launching the gateway to compare with reasoning on.

## Observed behavior (2026-06-14, Qwen3.6-35B-A3B-4bit, M-series)

- **build task: ✅ PASS.** Claude created a correct minimal project
  (`fn main(){ … args[1].chars().rev().collect::<String>() … }`, minimal Cargo.toml) and
  `cargo run -- hello` printed `olleh`. The gateway carried the whole loop — streaming
  `/v1/messages`, multi-step tool-use, file writes, cargo via the bash tool.
- **Bounded run:** `result=success, num_turns=13, tool_uses=12, ~363 s` — clean termination.
- **Model meanders.** 12 tool uses for "create a reverse CLI" is a lot (a frontier model
  needs ~3–4). Qwen3.6-35B-A3B is a capable-but-not-frontier coder: it re-reads/re-runs and
  takes extra steps, but converges. This is model quality, **not** a gateway issue.
- **Termination caveat — always pass `--max-turns`.** A first, unbounded run ran the full
  600 s and was killed by `timeout` even though the work had finished (the local model kept
  the loop going / didn't emit a clean final turn — likely greedy-MoE nondeterminism landing
  in a longer loop). The bounded run terminated naturally at 13 turns. The runner defaults
  to `--max-turns 20`; keep it.
- **Speed:** ~6 min wall for a trivial task is dominated by the 12 agentic round-trips (each
  re-prefills the growing context), not raw decode (~96 t/s). Fewer-turn models finish faster.

- **test task: ✅ PASS after `runaway-stop` (❌ FAIL before it — two ways).** The `test` task
  (binary + a `#[cfg(test)]` test + `cargo test`) is one notch harder than `build`. Before the
  runaway guard, Qwen3.6-35B-A3B failed it intermittently in two distinct ways (below); after it,
  the same task passes its verification. In every run the gateway delivered each tool call +
  result correctly, so the pre-fix failures were **model behavior / a missing server guard, not
  gateway protocol bugs**:
  - **Run A — token-level runaway (a hang).** Hit the 600 s `timeout` with `result=None` after
    only **1 tool-use**: on the turn after the first tool round-trip the greedy decode **ran
    away** (looped / never emitted EOS) toward Claude Code's large `max_tokens`. `--max-turns`
    did **not** save it — that bounds the agentic loop, not a single generation's token count.
    Enabled by two gaps the gateway *can* fix: no `max_tokens` cap + no anti-repetition / no-
    progress stop on the greedy (temp 0) path → **BACKLOG `mlx-native-runaway-stop`** (server
    `max_tokens` ceiling + an n-gram repetition guard in `stream_generation`).
  - **Run B — tool-level loop + premature "success".** Completed at `num_turns=12,
    tool_uses=11, result=success, 363 s`, but the project was **broken**: the model issued the
    *identical* `mkdir -p reverse-cli/src` **five times**, created two duplicate `TaskCreate`s,
    wrote a root `Cargo.toml` with no `[[bin]]` / no `src`, said "I'll create the project
    structure…", and then **stopped, declaring success without ever writing `src/main.rs`**.
    This is small-model degeneracy (repeating no-op tool calls, then a false "done"); a server
    token-guard can't catch it (it's across separate requests). It's the model ceiling.
  - **After `mlx-native-runaway-stop` (2026-06-14): ✅ the `test` task now PASSES verification.**
    Re-run: `assistant=26, tool_uses=21`, the model created a correct binary (`fn reverse` +
    a `#[cfg(test)]` test), **`cargo test` green + `cargo run -- hello` → `olleh`** — the
    un-fakeable checks pass. Crucially there is **no single-generation hang** anymore: the
    session progresses through 26 turns of real work (vs. Run A's stall at 1 tool-use), so the
    runaway guard did its job. It still hit the 600 s `timeout` (`rc=124`, `result=None`) —
    Claude's loop kept going without a clean final turn — but the **deliverable is correct and
    verified**. The 605 s is now *slowness*, not a hang: ~26 hybrid turns, each re-prefilling the
    growing context (~20 s/turn). That per-turn re-prefill is exactly what
    `mlx-native-prefix-kv-cache` removes for dense — and (now shipped)
    `mlx-native-prefix-kv-cache-hybrid` removes for Qwen3.6 too.
  - **Takeaway:** `build` (the simplest task) is a reliable smoke; `test` sits at/above this
    model's reliable agentic ceiling and fails intermittently. Use `build` as the go/no-go
    gateway smoke; treat `test` as a model-capability stress, not a gateway regression.
  - (The `test` prompt now also says **"in the CURRENT directory (do NOT create a
    subdirectory)"** — Run B's `cargo new reverse-cli`-style subdir misread is otherwise a
    natural reading of "create a project reverse-cli", and the disk verify checks the workdir
    root.)

- **Eval-harness lesson (fixed).** An earlier `test` prompt asked for a `lib.rs` + `#[test]`;
  the model `cargo init`-ed a default crate whose template test (`add(2,2)==4`) is always green
  and never implemented `reverse`, yet the run reported **"cargo test green"** — a *false
  success*. The `test` task now forces a **binary** + an **un-fakeable** behavior check
  (`cargo run -- hello` must print `olleh`), so a scaffold-only run fails as it should. General
  rule for new scenarios: always include a behavior assertion the model can't satisfy by
  scaffolding.

## Codex (OpenAI dialect) — ✅ WORKS (since the `/v1/responses` endpoint)

`scripts/e2e_codex_gateway.sh [build|test]` is the Codex parallel of the Claude runner
(drives `codex exec` headless, same tasks, same independent disk verification).

- **`build` task: ✅ PASS** (2026-06-14, Qwen3.6-35B-A3B-4bit): `codex exec` created a correct
  `reverse-cli`, ran `cargo run -- hello` → `olleh`, `rc=0` in **~71 s** (notably fewer turns /
  faster than Claude on the same task). The gateway carried the whole loop through the new
  `POST /v1/responses` endpoint (typed Responses SSE: `response.created` → `output_item.added`
  → `output_text.delta` → … → `response.completed`).
- **What made it work** (both shipped):
  1. **`POST /v1/responses`** (`gateway.rs::responses_handler`). Codex CLI ≥ 0.137 dropped
     `wire_api="chat"` and *requires* the Responses API; the endpoint translates the Responses
     request ⇄ the internal `ChatBackend` and streams the typed Responses event protocol.
  2. **Single leading system message.** Codex sends a top-level `instructions` **and** a
     `developer` message — two system turns — which the Qwen3.6 chat template rejects
     (`raise_exception('System message must be at the beginning.')`, surfaced as a
     `response.failed` event → Codex retried 5× and died). The Responses→internal conversion
     now **folds all system/developer text into one leading system message**.
- **Wiring caveat:** Codex **ignores `OPENAI_BASE_URL`** — it needs an explicit model provider
  (`-c model_provider=rozum -c 'model_providers.rozum.base_url=…' -c '…wire_api="responses"'`),
  which the runner sets. `rozum launch`'s `OPENAI_BASE_URL`/`OPENAI_API_KEY` are **not enough**
  for current Codex; a launch integration should write a Codex `model_providers` config.
- The runner preflights `/v1/responses` (a 1-token request) and aborts only if it's 404 (an
  old gateway binary without the endpoint).

> Known cosmetic gap: Codex's `/v1/models` refresh wants `{"models":[…]}` (the gateway returns
> the OpenAI `{"data":[…],"object":"list"}` shape), so Codex logs one non-fatal
> "failed to refresh available models" warning and proceeds. Harmless; a `/v1/models` alias is
> a trivial future nicety.

## Ideas for harder scenarios (later)

- Edit an existing file (not just create): give a buggy `add.rs`, ask to fix it.
- Multi-file project + a failing test, ask Claude to make it pass (debug loop).
- Longer session (`--continue`) across several tasks — stress KV / context growth.
- Run the same task ×N and report PASS rate (greedy MoE is nondeterministic).
