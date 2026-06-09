# mistralrs Backend (Native MLX/Metal, In-Process)

## Goal

In-process backend that loads MLX-format safetensors directly into the rozum process via the [`mistralrs`](https://crates.io/crates/mistralrs) Rust crate. Removes Python (`mlx_lm.server`) from the runtime path for models distributed in MLX format. Targets equivalent or better throughput vs other Apple-Silicon MLX runtimes while keeping the existing `ChatBackend` SPI from `chat-backend-spi.md` unchanged.

## Scope

- `src/mistralrs_backend.rs` — new module under `#[cfg(feature = "mistralrs")]`.
- `Cargo.toml` — new optional feature `mistralrs` with `mistralrs` dep, `metal` feature enabled on Apple Silicon.
- `src/backend.rs` — new `BackendEngine::Mistralrs`, constructor `BackendConfig::mistralrs(id, model_spec)`, branch in `BackendRegistry::from_configs`.
- `src/main.rs` — model spec resolver: `mistralrs:<hf-repo>` and `mlx-community:<repo>` prefixes routed to this backend in `build_gateway_backend`.
- New unit tests for spec parsing and `ToolUseParser` reuse.

## Interface

```rust
// Feature gate: #[cfg(feature = "mistralrs")]

pub struct MistralrsOptions {
    pub n_ctx: u32,           // default 32_768; env ROZUM_MISTRALRS_N_CTX
    pub temperature: f32,     // default 0.7
    pub top_p: f32,           // default 0.9
    pub max_seq_len: usize,   // default 4096
}

pub struct MistralrsBackend { /* private */ }

impl MistralrsBackend {
    /// `model_spec` may be:
    ///   "/abs/path/to/mlx-model-dir/"    — local safetensors directory
    ///   "hf:<user>/<repo>"                — HuggingFace repo, auto-download via hf-hub
    ///   "mlx-community:<repo>"            — sugar for hf:mlx-community/<repo>
    pub fn new(model_spec: &str, opts: MistralrsOptions) -> ModelResult<Self>;
}

#[async_trait]
impl ChatBackend for MistralrsBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    fn context_window(&self) -> u32;
}
```

CLI integration:
```bash
# Direct gateway:
rozum gateway --model mlx-community:Qwen3-30B-A3B-Instruct-4bit

# launch wrapper:
rozum launch --model mlx-community:Qwen2.5-Coder-32B-Instruct-4bit claude
```

## Behavior

- [ ] `MistralrsBackend::new` loads weights exactly once per instance. Subsequent `chat()` calls reuse the loaded model.
- [ ] Auto-download via `hf-hub` if the model is specified as `hf:<repo>` or `mlx-community:<repo>` and not already cached.
- [ ] Streaming via mistralrs's native streaming API; events mapped to `ChatEvent::TextDelta` and `ChatEvent::Done` (`EndTurn`/`MaxTokens`/`Cancelled`).
- [ ] Per-token cancel via `req.cancel`: checked between decoded tokens; backend stops within one decode step.
- [ ] Tool-use: pass `req.tools` into the model prompt via the chat template; reuse `crate::gguf::ToolUseParser` to detect `<tool_call>` blocks for Qwen-family models. Emit `ToolUseStart`/`ToolUseDelta`/`ToolUseEnd` events.
- [ ] System prompt block from `ChatRequest.messages` is forwarded to mistralrs's chat template.
- [ ] Sampling params (`temperature`, `top_p`, `top_k`, `max_tokens`) map to mistralrs's sampling configuration.
- [ ] In `build_gateway_backend`, the priority chain becomes:
  1. In-process GGUF (`--features gguf`)
  2. **In-process mistralrs (`--features mistralrs`)** ← NEW
  3. mlx_lm.server HTTP
  4. `ROZUM_BACKEND_URL` env
- [ ] `cargo check` (default): unchanged, no new hard deps.
- [ ] `cargo check --features mistralrs`: builds with Metal feature on aarch64-apple-darwin.
- [ ] Unit tests pass: model-spec parsing, sampling param mapping, tool-use parser reuse.
- [ ] Integration smoke test under `#[ignore]`: loads a real MLX model on M-series, generates a short response, measures ≥ 30 tok/s.

## Out of scope

- LoRA / adapter weights (future work).
- Multi-GPU.
- Vision / multimodal MLX models.
- Custom samplers beyond what mistralrs exposes.
- Quantisation conversion (use pre-quantised MLX-community models).

## Design

`mistralrs` provides a high-level `MistralRs` builder with `add_model` + `chat_completion` async APIs that already speak roughly OpenAI-shaped messages. The adapter layer is thin: parse `ChatRequest.messages` into mistralrs's message format, drive its streaming response iterator, map each chunk to `ChatEvent`.

Tool-use handling reuses `crate::gguf::ToolUseParser` because the wire format on the model side is the same Qwen-hermes `<tool_call>…</tool_call>` convention regardless of which engine runs the weights.

Model spec resolution lives in `MistralrsBackend::new`:
- `/abs/path/to/dir`: directly pass to mistralrs's local-model loader.
- `hf:<repo>` or `mlx-community:<repo>`: use mistralrs's HF auto-download or call `hf-hub` directly to materialise files under `~/.cache/huggingface/hub/<repo>/`.

The `mistralrs` feature is **enabled by default** so `cargo build` produces a binary that can already run local MLX models on Apple Silicon. Users who only need the meeting-room runtime — or who don't have the Metal Toolchain installed — can drop the dependency with `cargo build --no-default-features`. Apple Silicon Metal kernels activate via mistralrs's `metal` feature on aarch64-apple-darwin.

## Decisions

- **mistralrs over hand-rolled mlx-rs port** — chosen because mistralrs already implements Qwen2/Qwen3/Llama/Mistral forward passes, KV-cache, chat templates, and sampling. A hand-rolled port via `mlx-rs` core ops would require thousands of lines per model family and ongoing maintenance.
- **In-process over subprocess** — chosen to eliminate Python from the runtime path for users who want a single-binary deployment.
- **Reuse `gguf::ToolUseParser`** — chosen because the model-side tool-call format is engine-independent; duplicating the parser would create drift.
- **On by default** — chosen because the primary product use case is now "rozum hosts a local LLM for Claude Code / Codex", and asking every user to remember `--features mistralrs` defeated that. The Metal Toolchain requirement is a one-time setup; users without it can opt out via `--no-default-features`.

## Risks / sharp edges

- mistralrs API surface evolves quickly; pin to a known-good minor version and bump intentionally.
- First-time model download via `hf-hub` can be tens of GB; cache directory and download progress need user-visible logging.
- Metal kernel compile time on first build adds ~30–60 s; document in README under the `mistralrs` feature section.
- Chat template handling differs per model family — Qwen3 templates ship with the repo's `tokenizer_config.json`; verify mistralrs picks them up correctly.

## Recommended Models (MLX-community Q4)

| Model | Spec | Size | Notes |
|-------|------|------|-------|
| Qwen2.5-Coder-32B-Instruct | `mlx-community:Qwen2.5-Coder-32B-Instruct-4bit` | ~19 GB | Best open coding model; native tool-use |
| Qwen3-30B-A3B-Instruct (MoE) | `mlx-community:Qwen3-30B-A3B-Instruct-4bit` | ~17 GB | MoE, ~3B active params/token |

## Results

Implemented in `src/mistralrs_backend.rs` with the minimum surface needed for
in-process MLX inference. Wired into `build_gateway_backend` priority chain
between in-process GGUF and mlx_lm.server HTTP.

- `cargo check` (default): ✓ — no new hard deps.
- `cargo test --lib mistralrs::tests`: 3/3 pass (`normalize_spec` helper).
- `cargo check --features mistralrs`: **requires full Xcode** (not just
  Command Line Tools) because `mistralrs-paged-attn-0.8.1/build.rs` shells out
  to `xcrun metal` to compile Metal kernels. The error message looks like:

  ```
  xcrun: error: unable to find utility "metal", not a developer tool or in PATH
  ```

  Install full Xcode from the App Store, then:
  ```bash
  sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
  cargo build --features mistralrs
  ```

### Known limitations (deliberate, follow-up items)

- ISQ (in-situ quantisation) options not exposed yet — uses mistralrs defaults
  per model. Add when the API surface stabilises across mistralrs minor
  versions.
- Sampling param customisation (`temperature`/`top_p`/`top_k`/`max_tokens`)
  not forwarded yet — uses mistralrs defaults.
- `Role::Tool` is mapped to `User` text content; proper tool-result wiring is
  a follow-up alongside the `gguf-tool-use-non-qwen` task.
- Tool-use parsing: not yet reused from `gguf::ToolUseParser`. When enabled,
  the same hermes-style `<tool_call>` detection will apply.

### Usage

```bash
cargo build --features mistralrs
rozum launch --model mlx-community:Qwen2.5-Coder-32B-Instruct-4bit claude
rozum launch --model hf:Qwen/Qwen3-4B claude
rozum launch --model /path/to/local/safetensors-dir claude
```

The backend is selected automatically when:
- the spec is not a filesystem path,
- the spec does not start with `lmstudio:`,
- the build was compiled with `--features mistralrs`.

If selection fails (e.g. download fails, model not supported), the chain falls
through to mlx_lm.server HTTP and ROZUM_BACKEND_URL.
