# `rozum launch` Wrapper

## Goal

One command that starts the rozum LLM gateway, sets the env vars an agent CLI needs to use it, and runs that CLI as a child process. The user types `rozum launch --model <spec> <program> [args...]` and lands inside the agent already pointed at the local model.

Removes the multi-step ritual of: (1) start gateway in one terminal, (2) export 4 env vars in another, (3) launch agent, (4) pick model from `/model` picker.

## Scope

- `src/main.rs` — new `Launch` subcommand, `reorder_launch_args` pre-parser, `run_launch` function.
- `src/gateway.rs` — public `claude_model_alias(spec)` helper + public `serve_on(backend, listener, model_id)` so the launcher can bind the listener before spawning the child.

## Interface

### CLI

```
rozum launch [OPTIONS] <PROGRAM>...

Options:
  --model <SPEC>    Model spec (same as `gateway --model`)
  --port <PORT>     Gateway port (auto-picks a free port if not specified)
  --n-ctx <N>       Context window in tokens (default 32768)

Examples:
  rozum launch --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit claude
  rozum launch claude --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit           # same — known flags
                                                              # are reordered automatically
  rozum launch --model qwen2.5-coder:32b -- aider --no-auto-commits
                                                              # `--` passes the rest
                                                              # verbatim to the child
```

### Env vars set on the child

| Variable | Value | Purpose |
|----------|-------|---------|
| `ANTHROPIC_BASE_URL` | `http://127.0.0.1:<port>` | Route Anthropic requests to gateway |
| `ANTHROPIC_AUTH_TOKEN` | `rozum-local` | `Authorization: Bearer` — outranks OAuth without `claude /logout` |
| `ANTHROPIC_API_KEY` | (removed) | Avoids "Auth conflict" warning |
| `ANTHROPIC_MODEL` | `claude-rozum-<sanitized-spec>` | Pre-selects local model — no manual `/model` pick |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | same alias | Sub-agent / summariser routes still use local model |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | same alias | … |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | same alias | … |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | `1` | Claude Code queries `/v1/models` so the model appears in `/model` picker with `display_name` |
| `OPENAI_BASE_URL` | `http://127.0.0.1:<port>/v1` | Same gateway for Codex / aider / opencode |
| `OPENAI_API_KEY` | `rozum-local` | Required by OpenAI SDKs to authenticate at all |
| `ROZUM_GATEWAY_URL` | `http://127.0.0.1:<port>` | Self-identification for scripts |

### Gateway `/v1/models` response

The gateway advertises the loaded backend as:

```json
{
  "object": "list",
  "data": [{
    "id": "claude-rozum-<sanitized-spec>",
    "object": "model",
    "owned_by": "rozum",
    "display_name": "<original-model-spec>"
  }]
}
```

The `claude-` prefix is required because Claude Code's gateway-discovery filter only adds models whose id begins with `claude` or `anthropic`. The original spec is preserved in `display_name` for the picker UI.

## Behavior

- [x] `rozum launch --model X claude` starts the gateway on a free port and runs `claude` as a child with the env table above.
- [x] `rozum launch claude --model X` (flags after program) works too — the pre-parser pulls known flags (`--model`, `--port`, `--n-ctx`) ahead of the positional program.
- [x] Args after `--` are passed verbatim to the child, including known flag names, so `rozum launch --model X claude -- --model claude-internal-flag` is unambiguous.
- [x] If `--port` is omitted, the launcher picks a free port via `TcpListener::bind("127.0.0.1:0")` and reports it.
- [x] The TCP listener is bound *before* the child spawns, eliminating the connect race during gateway startup.
- [x] When the child exits, the gateway task is aborted and the launcher exits with the child's exit code (or `127` if spawn failed).
- [x] Claude Code shows `Auth token: ANTHROPIC_AUTH_TOKEN` and the local model id in `/status` — OAuth credentials in `~/.claude/.credentials.json` are not touched.
- [x] Claude Code starts on the local model: `/status` `Model:` field is `claude-rozum-<sanitized-spec>`.
- [x] The local model also appears in the `/model` picker labelled "From gateway" with the original spec as display name.
- [x] Works against any backend the underlying `gateway` subcommand supports: in-process GGUF (`--features gguf`), mlx_lm.server HTTP, or `ROZUM_BACKEND_URL`. If no real backend is reachable, `rozum launch` exits with code 1 instead of starting the child against a placeholder.

## Out of scope

- Persistent state between launches (no daemon mode).
- Watching the child process and restarting it.
- Detecting whether the child is interactive vs piped (no stdin/stdout handling beyond standard inheritance).
- Per-task model routing (Opus vs Sonnet vs Haiku) — all sub-agent slots point at the same local model.
- Stripping `ANTHROPIC_MODEL` if the user wants to keep using OAuth Opus for some commands — use plain `claude` without `rozum launch` for that.

## Design

### Why `ANTHROPIC_AUTH_TOKEN` instead of `ANTHROPIC_API_KEY`

Claude Code's authentication precedence (per `code.claude.com/docs/en/authentication`):

1. Cloud provider creds (`CLAUDE_CODE_USE_BEDROCK`, etc.)
2. **`ANTHROPIC_AUTH_TOKEN`** — `Authorization: Bearer` header, no approval required
3. `ANTHROPIC_API_KEY` — `X-Api-Key` header, **requires one-time interactive approval** (and triggers the "Auth conflict" warning when an OAuth login is also present)
4. `apiKeyHelper`
5. `CLAUDE_CODE_OAUTH_TOKEN`
6. Subscription OAuth from `/login`

`ANTHROPIC_AUTH_TOKEN` sits above OAuth (rank 2 vs rank 6), so we win without needing `claude /logout`. The user keeps their Claude Pro/Max login intact for plain `claude` invocations.

### Why explicitly `env_remove("ANTHROPIC_API_KEY")`

If the parent shell happens to have `ANTHROPIC_API_KEY` set (common for users who also code against the API directly), it would coexist with `ANTHROPIC_AUTH_TOKEN`. Per the precedence table the auth token wins anyway, but Claude Code still emits the "Auth conflict" warning. Clearing the var on the child silences the warning.

### Why all four model env vars

Claude Code uses different model slots internally:

- `ANTHROPIC_MODEL` — main inference model the user types at
- `ANTHROPIC_DEFAULT_OPUS_MODEL` / `_SONNET_MODEL` / `_HAIKU_MODEL` — fallback model ids when Claude Code asks for an Opus/Sonnet/Haiku tier explicitly (sub-agents, summarisation, plan mode)

Setting all four to the same local alias ensures every internal request lands on our gateway — no surprise paid OAuth calls from a sub-agent.

### Argument reordering

clap's `trailing_var_arg = true` on the `program: Vec<String>` field means the first non-flag positional argument captures all following arguments verbatim. So `rozum launch claude --model X` would clap-parse as program `claude` with args `["--model", "X"]`, and `--model` would be missing.

`reorder_launch_args` walks argv after the `launch` token, pulls each known launcher flag (`--model`, `--port`, `--n-ctx`) plus its value to the front, and stops at an explicit `--` separator so the user can still pass through identically-named child flags.

### Listener binding before child spawn

`tokio::net::TcpListener::bind` happens before `Command::spawn` so that:

1. The port is guaranteed free at the moment the child is launched.
2. The child's first request cannot arrive at a closed socket due to startup races.

The bound listener is then handed to `gateway::serve_on` (a new public function alongside `gateway::run` that accepts an already-bound listener instead of binding internally).

### Exit code propagation

The child runs synchronously via `tokio::task::spawn_blocking(move || cmd.status())`. When it exits, the gateway task is `abort()`'d and the launcher exits with the child's `status.code()`, so shell pipelines see the agent's exit status rather than always 0.

## Decisions

- **No daemon mode** — chosen because the typical agent session is bounded by the agent's own lifecycle. Persisting the gateway between launches would add restart, port-collision, and config-drift problems for no real win.
- **Reuse the same gateway code** — `rozum launch` calls into the same `build_gateway_backend` / `gateway::serve_on` as `rozum gateway`, with one extra wrapper. No new HTTP plumbing.
- **`claude-rozum-` prefix** instead of using model name verbatim — chosen because Claude Code's discovery filter explicitly drops non-`claude*`/`anthropic*` ids. Real spec is preserved in `display_name`.
- **Free-port auto-pick** — chosen because the launcher is interactive and one-shot; users should not have to think about port management. `--port` remains available for power users who want a fixed endpoint (e.g. for a debugger to attach to).
- **`--` separator over clever parsing** — chosen because it's the POSIX-standard way to pass identically-named flags through a wrapper. The reorder pre-parser is layered on top for ergonomics, not as a replacement.

## Risks / sharp edges

- A user with `ANTHROPIC_API_KEY` in `~/.claude/settings.json` (`"env"` block) cannot have it removed from the child by `env_remove` on `Command` — settings.json is read by Claude Code itself, after the launcher has set up the process. We rely on `ANTHROPIC_AUTH_TOKEN` outranking; the precedence is robust to this.
- If the user's `claude` binary is actually an alias or shell function, `Command::new("claude")` finds the binary on `PATH`, not the alias. Behaviour matches a non-alias terminal.
- `ANTHROPIC_MODEL` set without a corresponding entry in `/v1/models` causes Claude Code to fall back to its built-in default at startup. We mitigate by also setting `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` so the gateway-advertised alias is always present in the picker.

## Results

Implemented in `src/main.rs` (`Launch` subcommand + `run_launch` + `reorder_launch_args`) and `src/gateway.rs` (`claude_model_alias`, `serve_on`).

Verified manually with:

```
cargo run -- launch --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit claude
```

Claude Code starts on `claude-rozum-<sanitized-spec>`, `/status` reports `Auth token: ANTHROPIC_AUTH_TOKEN` and `Anthropic base URL: http://127.0.0.1:<port>`, requests appear in the launcher's stderr as `← POST /v1/messages`. No `Auth conflict` warning; user's OAuth login is preserved.
