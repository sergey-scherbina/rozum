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

- [ ] web-autosize-input - Replace web input with a Claude-style autosizing textarea.
  - `<textarea>` grows on `input` up to `30vh` desktop / `20vh` mobile.
  - `Enter` sends, `Shift+Enter` newline, `Esc` clears, no horizontal scroll.
  - Collapses back to 1 row after send.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `web-autosize-input`).

- [ ] web-scrollback-sticky - Make web scrollback usable while messages arrive.
  - `data-stick` heuristic keeps viewport put when user is scrolled away from bottom.
  - "↓ N new" pill appears and increments while not stuck; click snaps and re-enables stick.
  - Long messages (>6 lines or >600 chars) render collapsed with `[expand ▾]`.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `web-scrollback-sticky`).

- [ ] web-transcript-history - Replay transcript on connect and lazy-paginate older history.
  - `GET /transcript?from_seq=<n>&limit=<n>` REST endpoint on the bridge.
  - On WebSocket connect the bridge sends a `history` envelope with the last 200 entries.
  - Scrolling within 60 px of `#log` top fetches the next chunk and prepends without moving viewport.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `web-transcript-history`).

- [ ] tui-autosize-input - Make the TUI input area grow to fit multi-line composition.
  - Replace `Constraint::Length(3)` with `clamp(1, max(3, area.height/3))` based on textarea line count.
  - `Alt+Enter` newline, `Enter` sends, `Esc` cancels.
  - Long input lines wrap inside the input area and never scroll horizontally.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `tui-autosize-input`).

- [ ] mcp-proxy-auto-mark - Auto-emit `mark_responding` from mcp-proxy.
  - When the proxy returns `your_turn:true` from `wait_my_turn`, also call `meeting.mark_responding` on the agent's behalf.
  - Refresh every 15 s until next `meeting.submit` / `meeting.leave` / process exit.
  - Backwards compatible: explicit calls from the agent still work and refresh identically.
  - Spec: `docs/specs/web-ui-improvements.md` (slug `mcp-proxy-auto-mark`).

- [ ] web-transcript-persist - Persist transcript to disk for the web bridge.
  - Append every transcript entry to `$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl`.
  - `GET /transcript` reads from the file when the in-memory window is exhausted.
  - `--no-persist` CLI flag disables both the write and the read fallback.
  - blockedBy: `web-transcript-history` (requires the REST endpoint and the in-memory window contract it defines).
  - Spec: `docs/specs/web-ui-improvements.md` (slug `web-transcript-persist`).

## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are limited to meeting management.
- Specs for completed items have checked behavior boxes and results.
