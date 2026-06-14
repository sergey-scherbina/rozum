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

## Codex (OpenAI dialect) — currently BLOCKED ⛔

`scripts/e2e_codex_gateway.sh [build|test]` is the Codex parallel of the Claude runner
(drives `codex exec` headless, same tasks, same independent disk verification). **But it
cannot run today:**

- **Codex CLI ≥ 0.137 requires the OpenAI _Responses_ API** (`POST /v1/responses`) and
  **dropped `wire_api="chat"`** (config error: *"`wire_api = "chat"` is no longer supported …
  set `wire_api = "responses"`"*).
- The rozum gateway only implements `/v1/chat/completions` (OpenAI) and `/v1/messages`
  (Anthropic). `POST /v1/responses` → **HTTP 404**, so Codex retries and dies:
  *"unexpected status 404 Not Found … /v1/responses"*.
- Also note: Codex **ignores `OPENAI_BASE_URL`** — it needs an explicit model provider
  (`-c model_provider=rozum -c 'model_providers.rozum.base_url=…' -c '…wire_api="responses"'`),
  which the runner sets. `rozum launch`'s `OPENAI_BASE_URL`/`OPENAI_API_KEY` are therefore
  **not enough** for current Codex.

The runner preflights `/v1/responses` and exits 3 with a clear message if it's 404.

**Improvement to unblock Codex → BACKLOG `gateway-openai-responses-api`:** add a
`POST /v1/responses` endpoint that translates the Responses request (`input` items, `tools`,
`instructions`, `stream`) to the internal `ChatBackend` and streams back the Responses event
protocol (`response.created` / `response.output_text.delta` /
`response.function_call_arguments.delta` / `response.completed`, …). This is the single
biggest thing missing for "Rozum as a local provider for **Codex**".

## Ideas for harder scenarios (later)

- Edit an existing file (not just create): give a buggy `add.rs`, ask to fix it.
- Multi-file project + a failing test, ask Claude to make it pass (debug loop).
- Longer session (`--continue`) across several tasks — stress KV / context growth.
- Run the same task ×N and report PASS rate (greedy MoE is nondeterministic).
