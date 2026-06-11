# Sprint

(Formerly `WORK_QUEUE.md`; renamed to `SPRINT.md` per `AGENTS.md` / the
multi-agent skill.)

Current sprint focus: (1) make Rozum a reliable local meeting room for live agents and a human operator; (2) make Rozum a local LLM provider for Claude Code and Codex via an outward OpenAI/Anthropic-compatible gateway backed by an in-process MLX / GGUF engine on Apple Silicon Metal.

## Sprint

### Top priority (P0): mistralrs Qwen3.6 finish-the-forward — RESOLVED (day 6)

**Root cause found and fixed.** The residual divergence was NOT the
weight-row-ordering hypothesis from days 1-5. It was the **RMSNorm `+1`
convention**: `GemmaRmsNorm::new` bakes `weight = on_disk_weight + 1.0`, but
the sanitized `mlx-community/Qwen3.6-35B-A3B-4bit` checkpoint uses raw
RMSNorm weights (MLX's `should_shift_norm_weights` is False for sanitized
checkpoints: conv1d `(8192,4,1)`, no MTP). The `+1` over-scaled every norm
(~2.1x) and the silu MoE compounded it to a ~14x experts blow-up + an
over-peaked router. Fix: `GemmaRmsNorm::new_unshifted` for the five norm
sites in `vision_models/qwen3_5_moe/text.rs`.

Full writeup + the one-pass diagnostic methodology that localized it:
`docs/specs/mlx-weight-layout-and-afq.md` section 13. Oracle:
`scripts/mlx_ref.py` (--layers/--attn/--mlp/--router) +
`scripts/mlx_ref.qwen36-hello.txt`. Patch:
`patches/mistralrs-qwen36-afq-wip.patch`.

- [x] qwen36-fullattention-split - VERIFIED ALREADY CORRECT (red herring).
  - `qwen3_5_moe/text.rs` FullAttention already loads separate
    `q_proj/k_proj/v_proj` AFQ layers and its gate-interleave split matches
    `mlx_lm/models/qwen3_next.py::Qwen3NextAttention` exactly. Confirmed
    byte-for-byte: layer-3 (first FullAttention) `||x||` 1.4432 vs Python
    1.4460 once the RMSNorm fix is in. No layout change was needed.

- [x] qwen36-moe-switchmlp-layout - VERIFIED ALREADY CORRECT (red herring).
  - The MLX checkpoint ships experts **pre-fused** as
    `switch_mlp.{gate,up,down}_proj` `(num_experts, out, in)` (no per-expert
    `experts.<i>`). On Metal the `MoEExperts` Fast path -> quant
    `FusedExperts::new` loads them via `AfqLayer::afq_packed_linear_b` and
    the forward applies router weights correctly. The 14x magnitude was the
    RMSNorm bug feeding a 10x input, not a layout/dequant error. Confirmed:
    layer-0 per-expert outputs now match Python's `[0.78,0.25,0.57,...]`.

- [x] qwen36-numerical-parity-gate - PASSED.
  - `"Hello"` (11 tokens) renders identically in both runs; embedding
    byte-for-byte; all 40 layer last-position `||x||` match within bf16
    rounding; top-1 logit `id=8160 'Here' logit=22.0` identical to `mlx_lm`.
    Greedy generation begins identically.
  - Remaining (follow-up, not blocking): thread the fix into the upstream PR
    (`docs/specs/mistralrs-qwen36-pr.md`) and decide whether rozum's
    crates.io `mistralrs` 0.8.1 dep gets a `[patch]` to `.vendor/mistral-rs`
    or waits for an upstream release. NOTE: rozum currently has NO `[patch]`,
    so the fix lives only in `.vendor` + `patches/` until that is wired —
    `rozum`'s own binary still loads unpatched 0.8.1 and Qwen3.6 should keep
    routing through the LM Studio HTTP backend until then.

### Active

#### P0 (current): mistralrs-mlx-direct — native direct MLX quant-ops in mistral.rs

Replace `mistralrs-quant`'s candle-Metal AFQ kernels with real Apple MLX ops
(`mx.quantized_matmul` / `quantize` / `dequantize` / `gather_qmm`) via `mlx-rs`,
so quantized MLX-community checkpoints compute byte-for-byte with `mlx_lm` and
the candle-AFQ bug class (RMSNorm +1, zero-buffer, nibble audits) is retired.
Targeted to the AFQ quant ops only; candle keeps the rest of the graph.
Generic over AFQ → covers dense/MoE/hybrid + all bit-widths with no per-model
code. Lives in the `.vendor/mistral-rs` fork (feature `mlx-direct`, off by
default), upstreamable.

Spec: `docs/specs/mistralrs-mlx-direct.md`. Branch: `feature/mistralrs-mlx-direct`.
Decisions locked: targeted quant-ops · `mlx-rs` bindings · in the fork, generic.

- [ ] mlx-direct-p0 - Phase 0: bridge prototype + single-op parity.
  - Add `mlx-direct` feature + pinned `mlx-rs` dep (Apple-Silicon-gated).
  - `afq/mlx_bridge.rs`: candle Metal tensor ↔ `mlx_rs::Array`, **copy
    baseline** (API probe showed true zero-copy is not reachable: candle uses
    Private storage, mlx-c adopt wants a `void*` not an MTLBuffer; copy is
    negligible in decode). Settle contiguity/dtype/staging; record in spec.
  - `afq/mlx_direct.rs`: `dequantize` + `quantized_matmul`.
  - Gate: one real AFQ weight dequant + one quantized matmul, byte-for-byte vs
    `mx.*` AND vs the legacy candle op. Blocks all later phases.

- [ ] mlx-direct-p1 - Phase 1: dense model parity.
  - Switch `afq_quantize_op` / `afq_dequantize_op` / `afq_mm_op` behind
    `MISTRALRS_MLX_DIRECT`.
  - Gate: Qwen3-4B-4bit byte-for-byte tokens vs `mlx_lm.generate --temp 0`;
    exercise Qwen3-30B.

- [ ] mlx-direct-p2 - Phase 2: MoE gather path.
  - Wire `afq_gather_qmm_rhs_sorted{,_gate_up}` → `mx.gather_qmm`.
  - Gate: Qwen3-30B-A3B-4bit (MoE) and Qwen3.6-35B-A3B-4bit (hybrid) both
    token-for-token vs `mlx_lm`. Qwen3.6 green = headline result.

- [ ] mlx-direct-p3 - Phase 3: generalize bit-widths & variants.
  - AFQ 2/3/6/8-bit, MXFP4, group sizes 32/128, DWQ checkpoints; parametric
    parity test over (bits, group_size).

- [ ] mlx-direct-p4 - Phase 4: perf, optional zero-copy spike & default flip.
  - Benchmark copy baseline vs legacy (tok/s prefill+decode, peak RSS) on
    Qwen3.6. Decode expected to already win; watch prefill (boundary copy +
    lost cross-runtime overlap). Only if prefill regresses, time-box a zero-copy
    spike (custom FFI `mlx_array_new_data_managed` + MTLBuffer adoption) or widen
    the MLX region. Flip build-time default once MLX-direct meets-or-beats legacy
    and all parity gates are green; env switch stays as escape hatch.

#### mistralrs-concurrency-scheduling — responsive, memory-budgeted concurrency

Replace the blunt `max_num_seqs` 1/2 ladder with a layered model: budgeted
engine capacity (A), a rozum-side admission scheduler decoupled from the static
engine knob (B), priority + a reserved fast lane so small interactive requests
never queue behind big ones (C), and bounded-queue backpressure + an OOM circuit
breaker (D). Memory sets the upper bound; the Metal single-GPU compute sweet spot
sets the ceiling. Deliver synergistically in the order A → B+C → D (A lifts the
floor to 2 so a fast lane is physically possible).

Spec: `docs/specs/mistralrs-concurrency-scheduling.md`. Builds on the constant
per-prefill cost from `mistralrs-chunked-prefill.md` (~465 KB/token × chunk).

- [x] concurrency-budget - Phase A: load-time budgeted engine `max_num_seqs`. **DONE.**
  - `budgeted_max_num_seqs(ConcurrencyBudget)` = `clamp(headroom/per_seq, 1, ceiling)`,
    `headroom = safety_frac*available - weights - kv_pool`, `per_seq = prefill_chunk * ~465KB`.
  - Reuse `main.rs` footprint helpers (weights, kv_cache_bytes, available_ram_bytes).
  - Floor 1; lift to ≥2 only when headroom covers one extra `per_seq` (fast-lane room).
  - `ROZUM_MISTRALRS_MAX_SEQS` forces exact; `ROZUM_MISTRALRS_SEQS_CEILING` caps (default 8).
  - Replaces the 24-36 GB→1 / ≥48 GB→2 ladder. Pure fn unit-tested without Xcode.

- [x] concurrency-admission - Phase B+C: admission scheduler + fast lane. **DONE.**
  - `AdmissionScheduler` semaphore ≤ engine capacity, limit via `ROZUM_MISTRALRS_ADMIT`.
  - `chat()` acquires `AdmitGuard` before the engine; releases on done/cancel/drop.
  - SJF ordering by `RequestCost (prompt+max_tokens)`; reserved fast-lane slot for
    cost < `ROZUM_MISTRALRS_FASTLANE_TOKENS` (default 1024, 0 disables).
  - Finding: fork does NOT yield between prefill chunks (chunk loop is inside
    `pipeline::step`) → admission-order responsiveness only; mid-prefill preempt
    deferred to backlog `concurrency-engine-yield`.
  - Disconnect cancel/reap preserved for queued + admitted requests.

- [x] concurrency-load-shedding - Phase D: backpressure + circuit breaker. **DONE.**
  - Bounded queue `ROZUM_MISTRALRS_QUEUE_MAX` (default 32) → `Overloaded` → gateway 429 + Retry-After.
  - Metal alloc failure → `trip()` drops limit (floor 1), cooldown `recover_step()` raises back. No auto-retry (avoids re-OOM); best-effort substring detection.
  - Per-class `max_tokens` dropped (redundant with cost weighting). Invariants covered by scheduler tests.

**`mistralrs-concurrency-scheduling` complete (A + B+C + D).** Follow-ups in BACKLOG;
the big one is `concurrency-engine-yield` (true mid-prefill interleaving).

- [x] concurrency-backend-abstraction - Lift the admission machinery out of mistralrs into a generic `src/concurrency` module + `AdmittingBackend` decorator. **DONE.**
  - `ChatBackend::concurrency_capacity() -> Option<usize>` (default None); `admit_wrap` gates iff `Some`, passthrough otherwise (safe default for remote backends).
  - mistralrs reports `Some(max_num_seqs)`; its `chat()` is plain inference again. Generic `ROZUM_ADMIT*` env. The new mlx-rs backend gets admission/fast-lane/backpressure/breaker for free by returning a capacity.
  - Spec: `docs/specs/concurrency-backend-abstraction.md`.

#### shared-gateway — one shared model process, many launch clients

Make the model-serving gateway a shared, single-instance detached process that
`rozum launch` clients discover & reuse, so two launches don't load two models
and OOM. Single-owner election (TCP-port bind + advisory flock), transparent
failover on a stable port, idle shutdown via client leases. `--model` becomes
optional (reuse running / interactive picker). Adds `rozum models rm`.
Composes with `concurrency` (sharing = one model; AdmittingBackend = N clients).

Spec: `docs/specs/shared-gateway.md`.

- [x] shared-gateway-mvp - Detached `rozum gateway` daemon (registers `active.json`,
  stable port, idle-timeout exit). `rozum launch` discovers a healthy compatible
  gateway and reuses it, else spawns one (port-bind dedup; flock deferred to
  failover) and waits for health, then execs the agent. `--dedicated` keeps the
  old in-process behaviour. **DONE.**
- [x] shared-gateway-failover - Launch-side watchdog respawns the daemon on death
  (same port), anti-stampede via `share::try_spawn_lock` (O_EXCL stale-steal),
  port-bind backstop. Agent reconnects over the brief gap via its own retry. **DONE.**
- [x] shared-gateway-leases - Lease-refcount lifetime (`leases/<pid>` heartbeat,
  mtime-reap) keeps the daemon up while clients are live; `rozum gateway status`/`stop`. **DONE.**
- [x] launch-model-picker - `--model` optional: omitted+running → reuse (print
  model); omitted+none on a TTY → interactive picker (cached first with
  `(cached, size)` / `(not cached, ~size)` annotations; non-cached → download
  confirm); non-TTY → error. Mismatch policy: takeover-if-idle else reuse-with-warning. **DONE.**
- [x] models-rm - `rozum models rm <spec>`: confirm, refuse if it is the active
  model, delete HF/LMStudio dirs directly (Ollama via `ollama rm`), report freed size. **DONE.**
- [x] shared-gateway-proxy - Launch-local model-free reverse proxy in the request
  path (agent → proxy → daemon), mirroring `mcp-proxy`. Foundation for replay /
  poison / transparent swap. Re-points the agent at the proxy's local port. **DONE.**
- [x] shared-gateway-replay-retry - Buffer + replay a request when the daemon dies
  **before the first streamed token**; mid-stream failures surface. Smart retry:
  backoff + jitter, attempt cap, wait-for-health. **Two-tier admission**: daemon
  advertises room (`GET /v1/admit`); each proxy holds its client's requests in its
  own `concurrency::AdmissionScheduler` (SJF + fast lane) and only forwards within
  the daemon's window — prompts wait at the edge, not bounced. **DONE.**
- [x] shared-gateway-poison - Soft/graduated: per-fingerprint crash count;
  degrade-then-retry (serialize) first; refuse 422 only after `ROZUM_POISON_MAX`
  (default 3); share to TTL'd `poison.json` (default 1 h, decay-on-success) only
  on sole-in-flight high confidence — ambiguous stays local to the proxy.
  Crash-attribution = established-connection death (`!is_connect`), so a failover
  gap isn't blamed on the prompt; degrade = exclusive `lane` write-lock serializes
  the retry prefill; proxy fast-refuses confirmed entries before forwarding and the
  daemon's `poison_layer` re-checks before running the model. **DONE.**
- [x] gateway-switch - `rozum gateway switch --model Y [--backend B] [--n-ctx N]`
  / `reload` / `unload`: in-place drain → drop old model (never two resident) →
  load new → bump `generation` → resume; proxies hold across the gap (`/v1/admit`
  closes its window). Held by a `Switchboard` swap cell + injected `BackendBuilder`
  closure; chat handlers `enter()` (park while draining, lazy-reload if unloaded)
  and hold a `ChatLease` for the whole stream so a switch waits for streaming to
  finish. Drain uses a separate `generating` counter (not the idle `in_flight`,
  which would deadlock). `reload` re-execs the binary; `unload` drops the model and
  lazily rebuilds on the next chat. Control plane: auth-gated localhost
  `POST /control/{switch,unload,reload}`. `--dedicated` (no builder) refuses all
  three. `--backend` forces gguf/mistralrs/lmstudio/mlx/url. **DONE.**

#### channel-wakeup — push room events into idle agent sessions

Turn `rozum mcp-proxy` into a one-way Claude Code **channel** so a joined-but-idle
agent gets woken when a message lands in its room, instead of relying on the agent
to keep long-polling `meeting.wait_my_turn`. The proxy already holds a
`Peer<RoleServer>` to the agent session (`upstream_peer`); it declares the
`claude/channel` capability and a background task pushes `notifications/claude/channel`
with the new transcript delta. `wait_my_turn` stays as the authoritative pull path.

Empirically verified (CC 2.1.172): channels register fine under rozum's
local-gateway env (auth gate is Bedrock/Vertex/Foundry-only, not custom base URL),
but **only in the interactive `claude` CLI** — headless `-p`/Agent-SDK gets no
channel. `rozum launch … claude` is interactive, so it's on the right path.

Spec: `docs/specs/channel-wakeup.md`. rmcp 1.7 confirmed to support both pieces
(`ServerCapabilities.experimental` map + `ServerNotification::CustomNotification`).

- [ ] channel-wakeup-capability - Declare `experimental:{"claude/channel":{}}` in the
  proxy `InitializeResult` + extend `instructions` to teach the agent to read
  `<channel source="rozum" …>` events as a wakeup (authoritative delta via `wait_my_turn`).
- [ ] channel-wakeup-pusher - Per-joined-room background task (modeled on `heartbeat_task`)
  that runs its own room long-poll and emits `notifications/claude/channel`
  (`content` = rendered delta, `meta` = `{room,from,seq,your_turn}`) on `upstream_peer`.
  Fire-and-forget; never crash the proxy/room conn on send failure.
- [ ] channel-wakeup-lifecycle - Abort the task on leave / room-switch / teardown
  (same points as `heartbeat_task`/`RoomConn`); de-dup own-authored turns; advance
  `since_seq` past delivered entries so reconnect doesn't replay a notification storm.
- [ ] channel-wakeup-launch-flag - `rozum launch` injects
  `--dangerously-load-development-channels server:rozum` for Claude Code agents
  (suppressible; CC ≥ 2.1.80; non-`claude` programs untouched).

- [ ] runtime-config - Load backend policy and backend list from `rozum.toml`.
  - Support `single`, `fallback`, and `fanout` policies.
  - Support every `BackendEngine` defined in code (`Hello`, `Candle`, `LlamaGguf`, `NativeRust`, `ExternalCommand`, `Gguf`, plus the `mistralrs`/`openai-http` shapes once we settle on their config schema).
  - Provide a sensible default config that mirrors the gateway/launch auto-detect chain (in-process GGUF → mistralrs → mlx_lm.server → ROZUM_BACKEND_URL).
  - Useful when a user routinely switches between multiple local + remote backends in different sessions without re-typing `--model`.
  - Spec first: `docs/specs/runtime-config.md`.

### Qwen3.6 unblocking track (three escalating upstream fixes)

Ordered cheapest → most strategic. Pick up the first one that lands; downstream
ones still pay off long-term but the user-facing Qwen3.6 problem is solved as
soon as any single track succeeds.

- [ ] llamacpp-qwen36-patch - Upstream PR to llama.cpp accepting `qwen35moe.rope.dimension_sections` length 3.
  - Single hyperparam loader fix (~50 LoC). Concrete error logged with Qwen3.6 GGUF from `unsloth/Qwen3.6-35B-A3B-GGUF`.
  - Patched llama.cpp → patched llama-cpp-2 version bump → `cargo update` in rozum and `--features gguf` works for Qwen3.6.
  - Estimated effort: ~1 week active + upstream review cycle.
  - Spec: `docs/specs/llamacpp-qwen36-patch.md`.

- [ ] mistralrs-qwen36-pr - Upstream PR to mistralrs registering Qwen3.5/3.6 as an alias of the existing `qwen3_next` model.
  - Discovery: mistralrs already has all the hybrid linear-attention layer code in `qwen3_next.rs` (GatedDeltaNet, full-attention, SparseMoeBlock, MoE routing). mlx-lm's `qwen3_5.py` re-uses `qwen3_next.py` classes verbatim — same architecture.
  - The PR is therefore not new layer code; it's: (a) register `model_type: "qwen3_5_moe"` and `architectures: ["Qwen3_5MoeForConditionalGeneration"]` to dispatch to the existing `Qwen3NextLoader`; (b) tolerate the nested `text_config` block + explicit `layer_types` array in the config parser; (c) handle `attn_output_gate` if it changes behaviour.
  - Correctness gate: byte-for-byte token match against `mlx_lm.generate --temp 0`.
  - Highest-leverage: every Rust project that uses mistralrs picks up Qwen3.5/3.6.
  - Estimated effort: ~1 week active (down from 2-3 weeks after the qwen3_next discovery).
  - Spec: `docs/specs/mistralrs-qwen36-pr.md`.

- [ ] mlx-native-port - Native MLX runtime in rozum on top of `mlx-rs`, porting `mlx_lm` Python piece by piece.
  - Phased: Phase 0 (bootstrap) → Phase 1 (Qwen3-4B dense) → Phase 2 (Qwen3 MoE) → Phase 3 (Qwen3.6 hybrid). Each phase has a numerical-match exit criterion.
  - Removes our dependency on mistralrs / llama-cpp-2 release cycles entirely; new model families become ~3-5 day port tasks instead of "wait for upstream".
  - New crate feature `mlx-native` (off by default — heavy compile, big code surface).
  - Estimated effort: ~5-8 calendar weeks for parity with current mistralrs scope.
  - Spec: `docs/specs/mlx-native-port.md`.

### Done

- [x] lmstudio-http-backend - Auto-detect LM Studio's local OpenAI-compatible server at `http://localhost:1234/v1`.
  - Unlocks Qwen3.6 (and any LM Studio MLX model) on Apple Silicon today, ahead of in-process mistralrs AFQ work.
  - Inserts above `mlx_lm.server` in the `build_gateway_backend` priority chain.
  - Reuses the existing `OpenAiHttpBackend` SSE parser; no new dependencies.
  - Env: `ROZUM_LMSTUDIO_HTTP=http://host:port/v1` to override the default endpoint.
  - Spec: `docs/specs/lmstudio-http-backend.md`.

- [x] idle-cpu-reduction - Event-driven TUI / room loops; ~0% CPU when idle.
  - Spec: `docs/specs/idle-cpu-reduction.md`.

- [x] chat-backend-spi - Async streaming `ChatBackend` trait with tool-use, sampling params, cancel; replaces the old sync `InferenceBackend`.
  - Content blocks (`Text` / `ToolUse` / `ToolResult`) in the SPI from day 1.
  - Helper `collect_to_string` for meeting call-sites that still need a final `String`.
  - `BackendOrchestrator` (Single / Fallback / FanOut) rewritten on async streams.
  - Spec: `docs/specs/chat-backend-spi.md`.

- [x] gguf-backend - In-process GGUF inference on Metal via llama-cpp-2.
  - Crate feature `gguf`. Path resolvers for absolute paths, `lmstudio:<repo>`, and Ollama-cached tags (`<name>[:<tag>]`, reading `~/.ollama/models/blobs/` without a running daemon).
  - Streaming, per-token cancel, prompt-cache by `session_id`, Qwen-hermes tool-use parser.
  - Spec: `docs/specs/gguf-backend.md`.

- [x] mistralrs-backend - In-process native-MLX backend via the `mistralrs` crate (on by default).
  - Loads MLX safetensors directly: `mlx-community:<repo>`, `hf:<user>/<repo>`, or local directory. Auto-download via `hf-hub`.
  - Streaming token-by-token; per-token cancel; reuses `crate::gguf::ToolUseParser` for tool calls.
  - Spec: `docs/specs/mistralrs-backend.md`.

- [x] api-gateway - Outward HTTP gateway exposing both OpenAI and Anthropic dialects on `127.0.0.1`.
  - `GET /v1/models`, `POST /v1/chat/completions` (OpenAI SSE with `tool_calls`), `POST /v1/messages` (Anthropic event-stream with `tool_use` blocks).
  - Context-overflow → HTTP 400 with a clear error. Cancel propagates from client disconnect.
  - Optional bearer auth via `ROZUM_GATEWAY_TOKEN`. Bind always `127.0.0.1`.
  - Spec: `docs/specs/api-gateway.md`.

- [x] launch-wrapper - `rozum launch --model X <program>` starts the gateway and execs the agent CLI with `ANTHROPIC_*` / `OPENAI_*` env vars pre-set.
  - Uses `ANTHROPIC_AUTH_TOKEN` (rank-2 in Claude Code auth precedence) so the local model wins without `claude /logout`.
  - Sets `ANTHROPIC_MODEL` + the four `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` slots so Claude Code starts on the local model without a manual `/model` pick.
  - Enables `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` so the model shows up in the `/model` picker with `display_name`.
  - Argument reordering pre-parser accepts both `--model X claude` and `claude --model X`; `--` separator forwards remaining args verbatim.
  - Spec: `docs/specs/launch-wrapper.md`.

- [x] launch-no-model - `rozum launch --no-model <program>` runs the agent with no
  local model against upstream Anthropic: no gateway/lease/proxy, no `ANTHROPIC_*`/
  `OPENAI_*` overrides, operator's own auth preserved; only rozum agent-context
  defaults applied. Picker lists "Anthropic (cloud — no local model)" first.
  `LaunchTarget::{Local,Anthropic}`; `--no-model` conflicts with `--model`/
  `--dedicated`/`--n-ctx`/`--port`, reordered like value flags. Unlocks channels
  (real Anthropic auth) for rozum-launched agents. Spec: `docs/specs/launch-wrapper.md`. **DONE.**

- [x] models-cli - `rozum models {list, list --remote, info <spec>}` for discovering and inspecting local LLM models.
  - Scans HuggingFace hub, Ollama (both monolithic GGUF and per-tensor MLX layouts), and LMStudio caches without needing those runtimes running.
  - `list --remote` prints a curated download list optimised for 24-36 GB Apple Silicon unified memory.
  - `info <spec>` fetches HuggingFace metadata for not-installed models (author, downloads, license, total size, tags) and prints the install command.

### Cancelled / Superseded

These were in the queue earlier but either landed as part of larger work or no longer match the current product direction.

- [x] meeting-cli-surface — done as part of the current CLI shape: bare `rozum` launches a meeting, `rozum list` / `rozum mcp-proxy` are present, and the only user-facing model commands are `rozum gateway / launch / models`. No standalone "model diagnostics" CLI was ever shipped. Spec: `docs/specs/optional-local-models.md`.

- [x] agent-meetings — implemented as the default `rozum` runtime + `rozum mcp-proxy`. Claude Code / Codex sessions join via the MCP proxy and a human participates through the TUI. Moderator modes, budget, and hotkeys live in `src/meeting/`. Spec: `docs/specs/agent-meetings*.md`.

- [x] remote-api-backends — superseded by two newer pieces of work: `OpenAiHttpBackend` already speaks the OpenAI Chat Completions dialect against any compatible server (Ollama, mlx_lm.server, vLLM, OpenAI itself) via `ROZUM_BACKEND_URL`, and `api-gateway` exposes both OpenAI and Anthropic dialects locally. A symmetric `AnthropicHttpClient` backend (so rozum can call out to api.anthropic.com) is captured separately under `anthropic-http-client-backend` in `BACKLOG.md`.

- [x] smollm2-chat-template — superseded by per-backend chat templating: `gguf::format_qwen_prompt` for GGUF backends (Qwen / ChatML format with tool defs); mistralrs's own template applier for MLX backends; the gateway forwards chat templates upstream for OpenAI-HTTP backends. No standalone SmolLM2-specific layer is needed.

- [x] eval-harness — no longer in scope while the product focus is "local LLM provider for Claude Code / Codex". Evals matter when we are choosing between local models for accuracy; right now we are choosing for "does it run at all on M-series with the target architecture", which is best answered by trying the model in `rozum launch`. Will reopen as `local-llm-eval-harness` in `BACKLOG.md` if/when we need it.

## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- `cargo build --no-default-features` produces a meeting-room-only binary.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are `gateway`, `launch`, `models`, `list`, `mcp-proxy`, `web`, `discord`, `telegram`.
- Specs for completed items have checked behavior boxes and results.
