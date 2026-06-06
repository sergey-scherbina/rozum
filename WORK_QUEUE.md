# Work Queue

Current sprint focus: make Rozum a reliable local meeting room for live agents and a human operator. Model backends are optional adapters, not the default product path.

## Sprint

- [ ] remote-api-backends - Add configurable OpenAI and Anthropic API backends.
  - Add backend engines for OpenAI Responses API and Anthropic Messages API.
  - Configure provider, model id, base URL, max tokens, and credential source through config.
  - Keep API keys out of committed config; support env-variable references and/or ignored local secrets.
  - Include default model entries for OpenAI ChatGPT/GPT and Anthropic Claude.
  - Do not require live API calls in normal tests.
  - Spec first: `docs/specs/remote-api-backends.md`.

- [ ] agent-meetings - Let live Claude Code and Codex sessions join a moderated meeting room.
  - `rozum` is the meeting-room agent: one process = one named room.
  - `rozum mcp-proxy` (stdio) is added once to each agent's MCP config; agents discover rooms via `rooms.list` and join with `rooms.join(name)`.
  - Human participates directly through the TUI as a first-class participant.
  - Moderator modes: round-robin and manual/operator-selected.
  - Budget control: soft per-turn warning, hard total-chars limit.
  - Hotkeys and slash commands for pause, stop, rename, kick, mode-switch.
  - Spec: `docs/specs/agent-meetings.md` + `agent-meetings-mcp.md` + `agent-meetings-process.md` + `agent-meetings-tui.md`.

- [ ] meeting-cli-surface - Keep the binary focused on meeting management.
  - Supported commands: bare room launch, `list`, `mcp-proxy`.
  - Do not expose model diagnostics through user-facing CLI commands.
  - Spec: `docs/specs/optional-local-models.md`.

- [ ] runtime-config - Load backend policy and backend list from `rozum.toml`.
  - Support `single`, `fallback`, and `fanout` policies.
  - Support backend engines already defined in code: `hello`, `candle`, `llama-gguf`, `native-rust`, `external-command`.
  - Provide a default config equivalent to the current tiny fallback plan.
  - Spec first: `docs/specs/runtime-config.md`.

- [ ] eval-harness - Add a minimal local eval runner.
  - Add `evals/smoke.toml` or `evals/smoke.json`.
  - Include greeting, summary, sentiment, JSON extraction, and simple route-intent cases.
  - Report pass/fail and observed model output.
  - Keep tests deterministic where possible.
  - Spec first: `docs/specs/eval-harness.md`.

- [ ] smollm2-chat-template - Prompt SmolLM2-Instruct with an explicit chat template.
  - Add a prompt formatting layer before backend execution.
  - Keep raw prompt mode available for debugging.
  - Verify `Hello! How are you?` still produces a sensible response.
  - Spec first: `docs/specs/smollm2-chat-template.md`.

- [ ] tui-soft-wrap - Wrap long input lines inside the TUI input area instead of horizontal scroll.
  - `tui-textarea 0.7` has no `set_wrap`; pick approach (custom rendering via Paragraph::wrap, fork the crate, or auto-insert newlines on overflow).
  - Carved out from `tui-autosize-input` after the autosize part landed but the wrap part proved blocked by the upstream crate.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `tui-soft-wrap`).

- [ ] mcp-proxy-auto-mark - Auto-emit `mark_responding` from mcp-proxy.
  - When the proxy returns `your_turn:true` from `wait_my_turn`, also call `meeting.mark_responding` on the agent's behalf.
  - Refresh every 15 s until next `meeting.submit` / `meeting.leave` / process exit.
  - Backwards compatible: explicit calls from the agent still work and refresh identically.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `mcp-proxy-auto-mark`).


## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are limited to meeting management.
- Specs for completed items have checked behavior boxes and results.
