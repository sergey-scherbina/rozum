/// Native in-process MLX/Metal backend via the `mistralrs` crate.
///
/// Loads safetensors weights from HuggingFace (auto-download) or a local
/// directory and runs inference directly on Apple Silicon Metal kernels —
/// no Python, no Ollama, no separate server process.
///
/// Gated on `#[cfg(feature = "mistralrs")]`. Default build is unaffected.
#[cfg(feature = "mistralrs")]
mod inner {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::backend::{
        ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
        ModelResult, Role, StopReason,
    };
    use crate::mistralrs_admission::{AdmissionConfig, AdmissionScheduler, RequestCost};

    use mistralrs::{
        AutoDeviceMapParams, CalledFunction, ChatCompletionChunkResponse, ChunkChoice,
        DeviceMapSetting, Function, MemoryGpuConfig, Model, ModelBuilder,
        PagedAttentionMetaBuilder, RequestBuilder, Response, TextMessageRole, Tool,
        ToolCallResponse, ToolCallType, ToolChoice, ToolType,
    };

    /// Cap on generated tokens when the request does not specify one, so a
    /// reasoning model (e.g. Qwen3.6 with its `<think>` block) can't run away
    /// unbounded the way it did before sampler limits were wired through.
    const DEFAULT_MAX_TOKENS: usize = 4096;

    /// Knobs that map to mistralrs's `ModelBuilder` options.
    /// Sampling-detail knobs intentionally omitted for now — mistralrs's
    /// defaults are sensible and the API surface for sampler customisation
    /// has shifted between versions.
    #[derive(Clone, Debug)]
    pub struct MistralrsOptions {
        /// Context window (used by `context_window()`).
        pub n_ctx: u32,
        /// Max sequences the engine batches concurrently. mistralrs defaults to
        /// 32; on a memory-constrained Mac that lets two large prompt prefills
        /// (e.g. Claude Code's parallel requests) run at once and OOM the Metal
        /// command buffer. The load-time caller budgets this from the model
        /// footprint vs available memory (see [`super::budgeted_max_num_seqs`]
        /// and `main.rs`); `Default` is a safe serialised floor of `1`.
        pub max_num_seqs: usize,
    }

    impl Default for MistralrsOptions {
        fn default() -> Self {
            let n_ctx = std::env::var("ROZUM_MISTRALRS_N_CTX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32_768);
            // Serialised floor; the load-time caller (main.rs) sets the budgeted
            // value via `budgeted_max_num_seqs`.
            Self {
                n_ctx,
                max_num_seqs: 1,
            }
        }
    }

    pub struct MistralrsBackend {
        model: Arc<Model>,
        opts: MistralrsOptions,
        /// Admission gate in front of the engine (Phase B+C): runtime-adjustable
        /// concurrency limit, SJF + reserved fast lane.
        scheduler: AdmissionScheduler,
    }

    impl MistralrsBackend {
        /// `model_id` is anything `ModelBuilder::new` accepts: a HuggingFace
        /// repo (`<user>/<repo>`) or a local safetensors directory.
        pub async fn new(model_id: &str, mut opts: MistralrsOptions) -> ModelResult<Self> {
            eprintln!(
                "mistralrs: loading '{model_id}' (first run downloads weights from HuggingFace into ~/.cache/huggingface/hub/)"
            );
            // Smallest context to retry down to before giving up.
            const N_CTX_FLOOR: u32 = 8_192;
            // The device-map fit check runs before weights load, so a too-big
            // context fails fast and we can retry smaller. Step down by this much.
            const N_CTX_STEP: u32 = 4_096;
            let paged = std::env::var("ROZUM_MISTRALRS_PAGED").as_deref() != Ok("0");
            let model = loop {
                let mut builder = ModelBuilder::new(model_id)
                    .with_logging() // enables hf-hub progress bars during download
                    .with_max_num_seqs(opts.max_num_seqs)
                    // The auto device-map defaults max_seq_len to 4096, which caps
                    // the KV cache (and the PagedAttention pool) regardless of
                    // n_ctx — long prompts get rejected as "too long". Raise it.
                    .with_device_mapping(DeviceMapSetting::Auto(AutoDeviceMapParams::Text {
                        max_seq_len: opts.n_ctx as usize,
                        max_batch_size: opts.max_num_seqs.max(1),
                    }));
                // PagedAttention computes attention block-wise (no full seq*seq
                // score matrix) and pools the KV cache, bounding the prefill memory
                // peak for long prompts. Without it a single ~2.4k-token prompt on
                // a 27B OOMs the Metal command buffer. Disable with ROZUM_MISTRALRS_PAGED=0.
                if paged {
                    if let Ok(cfg) = PagedAttentionMetaBuilder::default()
                        .with_gpu_memory(MemoryGpuConfig::ContextSize(opts.n_ctx as usize))
                        .build()
                    {
                        eprintln!("mistralrs: PagedAttention enabled (ctx {})", opts.n_ctx);
                        builder = builder.with_paged_attn(cfg);
                    }
                }
                match builder.build().await {
                    Ok(model) => break model,
                    Err(e) => {
                        // The device mapper refuses (before loading weights) when
                        // model + KV cache exceeds Metal's working-set budget;
                        // retry with a smaller context instead of failing outright.
                        let msg = e.to_string();
                        let too_big =
                            msg.contains("does not fit") || msg.contains("exceeds total capacity");
                        if too_big && opts.n_ctx > N_CTX_FLOOR {
                            let reduced = opts.n_ctx.saturating_sub(N_CTX_STEP).max(N_CTX_FLOOR);
                            eprintln!(
                                "mistralrs: context {} exceeds device memory, retrying at {}",
                                opts.n_ctx, reduced
                            );
                            opts.n_ctx = reduced;
                            continue;
                        }
                        return Err(ModelError::BackendUnavailable(format!(
                            "mistralrs: failed to load {model_id}: {e}"
                        )));
                    }
                }
            };
            eprintln!("mistralrs: '{model_id}' ready (context {})", opts.n_ctx);
            // The engine capacity (opts.max_num_seqs) was budgeted by the caller;
            // the admission limit defaults to it (ROZUM_MISTRALRS_ADMIT overrides).
            let scheduler =
                AdmissionScheduler::new(AdmissionConfig::from_engine_capacity(opts.max_num_seqs));
            Ok(Self {
                model: Arc::new(model),
                opts,
                scheduler,
            })
        }

        /// Cheap cost estimate for admission ordering: ~4 chars/token over the
        /// rendered messages plus the requested generation budget. Only needs to
        /// separate "quick follow-up" from "large context read".
        fn estimate_cost(&self, req: &ChatRequest) -> RequestCost {
            let chars: usize = req
                .messages
                .iter()
                .map(|m| Self::message_text(m).len())
                .sum();
            let max_tokens = req
                .sampling
                .max_tokens
                .map(|m| m as usize)
                .unwrap_or(DEFAULT_MAX_TOKENS);
            RequestCost {
                prompt_tokens: chars / 4,
                max_tokens,
            }
        }

        fn message_text(msg: &Message) -> String {
            msg.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        }

        /// Assistant `tool_use` blocks -> mistralrs `ToolCallResponse`s so the
        /// model sees its own prior calls in conversation history.
        fn message_tool_calls(msg: &Message) -> Vec<ToolCallResponse> {
            msg.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(ToolCallResponse {
                        index: 0,
                        id: id.clone(),
                        tp: ToolCallType::Function,
                        function: CalledFunction {
                            name: name.clone(),
                            arguments: input.to_string(),
                        },
                    }),
                    _ => None,
                })
                .collect()
        }

        fn tool_def_to_mistralrs(def: &crate::backend::ToolDef) -> Tool {
            let parameters = def
                .input_schema
                .as_object()
                .map(|m| m.clone().into_iter().collect());
            Tool {
                tp: ToolType::Function,
                function: Function {
                    name: def.name.clone(),
                    description: Some(def.description.clone()),
                    parameters,
                    strict: None,
                },
            }
        }

        fn build_request(&self, req: &ChatRequest) -> RequestBuilder {
            let mut rb = RequestBuilder::new();
            for msg in &req.messages {
                match msg.role {
                    Role::System => {
                        rb = rb.add_message(TextMessageRole::System, Self::message_text(msg))
                    }
                    Role::User => {
                        rb = rb.add_message(TextMessageRole::User, Self::message_text(msg))
                    }
                    Role::Assistant => {
                        let tool_calls = Self::message_tool_calls(msg);
                        let text = Self::message_text(msg);
                        rb = if tool_calls.is_empty() {
                            rb.add_message(TextMessageRole::Assistant, text)
                        } else {
                            rb.add_message_with_tool_call(
                                TextMessageRole::Assistant,
                                text,
                                tool_calls,
                            )
                        };
                    }
                    Role::Tool => {
                        for b in &msg.content {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } = b
                            {
                                rb = rb.add_tool_message(content.clone(), tool_use_id.clone());
                            }
                        }
                    }
                }
            }

            if !req.tools.is_empty() {
                let tools = req.tools.iter().map(Self::tool_def_to_mistralrs).collect();
                rb = rb.set_tools(tools).set_tool_choice(ToolChoice::Auto);
            }

            let s = &req.sampling;
            let max_len = s
                .max_tokens
                .map(|m| m as usize)
                .unwrap_or(DEFAULT_MAX_TOKENS);
            rb = rb.set_sampler_max_len(max_len);
            if let Some(t) = s.temperature {
                rb = rb.set_sampler_temperature(t as f64);
            }
            if let Some(p) = s.top_p {
                rb = rb.set_sampler_topp(p as f64);
            }
            if let Some(k) = s.top_k {
                rb = rb.set_sampler_topk(k as usize);
            }
            rb
        }
    }

    #[async_trait]
    impl ChatBackend for MistralrsBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let cancel = req.cancel.clone();
            let cost = self.estimate_cost(&req);
            let scheduler = self.scheduler.clone();
            let request = self.build_request(&req);
            let model = Arc::clone(&self.model);

            // Move `model` into the stream so the upstream borrow it holds is
            // bounded by the stream's lifetime, not by this function call.
            let chat_stream: ChatStream = Box::pin(async_stream::stream! {
                // Admission control: wait for a slot (fast lane / SJF). Race it
                // against cancellation so a client that disconnects while queued
                // never holds a phantom slot. The guard is held for the whole
                // stream; dropping it on completion/disconnect frees the slot and
                // wakes the next waiter.
                let _admit = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        yield Ok(ChatEvent::Done {
                            input_tokens: 0,
                            output_tokens: 0,
                            stop_reason: StopReason::Cancelled,
                        });
                        return;
                    }
                    guard = scheduler.admit(cost) => guard,
                };

                let mut upstream = match model.stream_chat_request(request).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield Err(ModelError::BackendUnavailable(format!(
                            "mistralrs: stream_chat_request: {e}"
                        )));
                        return;
                    }
                };

                let mut output_tokens: u32 = 0;
                let mut done_sent = false;

                loop {
                    // Race the next chunk against cancellation so a client disconnect is
                    // honored immediately, even mid-prefill (when upstream yields nothing
                    // for a long time). Dropping `upstream` on break tears down the
                    // mistralrs request so it stops holding the single sequence slot.
                    let item = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            yield Ok(ChatEvent::Done {
                                input_tokens: 0,
                                output_tokens,
                                stop_reason: StopReason::Cancelled,
                            });
                            done_sent = true;
                            break;
                        }
                        item = upstream.next() => item,
                    };
                    let Some(item) = item else { break };

                    if let Response::Chunk(ChatCompletionChunkResponse { choices, .. }) = item {
                        if let Some(ChunkChoice { delta, finish_reason, .. }) = choices.first() {
                            // Stream the model's `<think>` reasoning as text too,
                            // otherwise a reasoning-only chunk yields nothing and
                            // the client sees an empty response.
                            if let Some(reasoning) = &delta.reasoning_content {
                                if !reasoning.is_empty() {
                                    yield Ok(ChatEvent::TextDelta { text: reasoning.clone() });
                                }
                            }
                            if let Some(content) = &delta.content {
                                if !content.is_empty() {
                                    output_tokens += 1;
                                    yield Ok(ChatEvent::TextDelta { text: content.clone() });
                                }
                            }
                            // mistralrs emits the parsed tool call(s) whole, with
                            // the full argument JSON already assembled.
                            if let Some(tool_calls) = &delta.tool_calls {
                                for tc in tool_calls {
                                    yield Ok(ChatEvent::ToolUseStart {
                                        id: tc.id.clone(),
                                        name: tc.function.name.clone(),
                                    });
                                    yield Ok(ChatEvent::ToolUseDelta {
                                        id: tc.id.clone(),
                                        input_json_delta: tc.function.arguments.clone(),
                                    });
                                    yield Ok(ChatEvent::ToolUseEnd { id: tc.id.clone() });
                                }
                            }
                            if let Some(reason) = finish_reason {
                                let stop_reason = match reason.as_str() {
                                    "length" => StopReason::MaxTokens,
                                    "tool_calls" => StopReason::ToolUse,
                                    _ => StopReason::EndTurn,
                                };
                                yield Ok(ChatEvent::Done {
                                    input_tokens: 0,
                                    output_tokens,
                                    stop_reason,
                                });
                                done_sent = true;
                                break;
                            }
                        }
                    }
                    // Other Response variants (Done, Errors, etc.) ignored —
                    // the final Done is emitted by the natural loop end.
                }

                if !done_sent {
                    yield Ok(ChatEvent::Done {
                        input_tokens: 0,
                        output_tokens,
                        stop_reason: StopReason::EndTurn,
                    });
                }

                // Keep `model` alive for the entire stream lifetime.
                drop(model);
            });

            Ok(chat_stream)
        }

        fn context_window(&self) -> u32 {
            self.opts.n_ctx
        }

        fn label(&self) -> &'static str {
            "mistralrs"
        }
    }

    pub use MistralrsBackend as Export;
    pub use MistralrsOptions as ExportOptions;
}

#[cfg(feature = "mistralrs")]
pub use inner::Export as MistralrsBackend;
#[cfg(feature = "mistralrs")]
pub use inner::ExportOptions as MistralrsOptions;

/// Translate user-friendly model specs into a HuggingFace id or local path
/// that `mistralrs::ModelBuilder::new` understands.
///
/// - `mlx-community:<repo>` → `mlx-community/<repo>`
/// - `hf:<user>/<repo>`     → `<user>/<repo>`
/// - `/abs/path/`           → as-is (local safetensors directory)
/// - anything else          → as-is (passed through, ModelBuilder may parse it)
pub fn normalize_spec(spec: &str) -> String {
    if let Some(repo) = spec.strip_prefix("mlx-community:") {
        format!("mlx-community/{repo}")
    } else if let Some(rest) = spec.strip_prefix("hf:") {
        rest.to_owned()
    } else {
        spec.to_owned()
    }
}

// ── Phase A: budgeted engine concurrency ───────────────────────────────────────
// Spec: docs/specs/mistralrs-concurrency-scheduling.md

/// Prefill activation peak per token (`mistralrs-chunked-prefill.md`: ~465 KB,
/// one hidden-state-sized tensor per token). With chunked prefill the peak is
/// bounded by the chunk size, so each concurrent prefill slot costs a *constant*
/// `chunk * this`, regardless of prompt length — which is what makes a memory
/// budget over slots meaningful.
pub const PREFILL_PEAK_BYTES_PER_TOKEN: u64 = 465 * 1024;

/// Compute sweet-spot ceiling on budgeted concurrency. Metal is a single device:
/// past a handful of concurrent prefills the GPU saturates, so extra slots only
/// add tail latency, not throughput. Override via `ROZUM_MISTRALRS_SEQS_CEILING`.
pub const DEFAULT_SEQS_CEILING: usize = 8;

/// Fraction of currently-available RAM we commit, leaving slack for the OS and
/// for transient spikes the per-seq estimate doesn't capture.
const BUDGET_SAFETY_FRAC: f64 = 0.8;

/// Transient memory of one concurrent prefill slot: `chunk_tokens` × the
/// per-token peak.
pub fn per_seq_prefill_peak(chunk_tokens: usize) -> u64 {
    chunk_tokens as u64 * PREFILL_PEAK_BYTES_PER_TOKEN
}

/// Inputs to the concurrency budget, gathered at load time from the actual model
/// (see `main.rs` footprint helpers). All memory terms in bytes.
#[derive(Clone, Copy, Debug)]
pub struct ConcurrencyBudget {
    /// RAM free right now, before weights load.
    pub available_ram: Option<u64>,
    /// Model weights that become resident on load.
    pub weights: Option<u64>,
    /// Paged KV pool, sized from `n_ctx`.
    pub kv_pool: Option<u64>,
    /// Transient cost of one concurrent prefill ([`per_seq_prefill_peak`]).
    pub per_seq_peak: u64,
    /// Compute sweet-spot cap.
    pub ceiling: usize,
}

/// Budgeted engine `max_num_seqs`: how many concurrent prefills fit in the RAM
/// left after the resident model, clamped to `[1, ceiling]`.
///
/// `slots = floor((safety·available − weights − kv_pool) / per_seq_peak)`.
/// The floor is `1` (one request must always run; whether it fits at all is the
/// preflight's job). The value only reaches `≥2` when there is headroom for a
/// second concurrent prefill — i.e. when a reserved fast lane (Phase B+C) is
/// physically possible. Falls back to the `1` floor when any memory term is
/// unknown (e.g. weights not cached yet on first run).
pub fn budgeted_max_num_seqs(b: &ConcurrencyBudget) -> usize {
    let (Some(available), Some(weights), Some(kv_pool)) = (b.available_ram, b.weights, b.kv_pool)
    else {
        return 1;
    };
    if b.per_seq_peak == 0 || b.ceiling == 0 {
        return 1;
    }
    let headroom = available as f64 * BUDGET_SAFETY_FRAC - weights as f64 - kv_pool as f64;
    if headroom <= 0.0 {
        return 1;
    }
    let slots = (headroom / b.per_seq_peak as f64).floor() as usize;
    slots.clamp(1, b.ceiling)
}

#[cfg(test)]
mod tests {
    use super::{ConcurrencyBudget, budgeted_max_num_seqs, normalize_spec, per_seq_prefill_peak};

    #[test]
    fn normalize_mlx_community_prefix() {
        assert_eq!(
            normalize_spec("mlx-community:Qwen2.5-Coder-32B-Instruct-4bit"),
            "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"
        );
    }

    #[test]
    fn normalize_hf_prefix() {
        assert_eq!(normalize_spec("hf:Qwen/Qwen3-4B"), "Qwen/Qwen3-4B");
    }

    #[test]
    fn normalize_bare_passthrough() {
        assert_eq!(normalize_spec("Qwen/Qwen3-4B"), "Qwen/Qwen3-4B");
        assert_eq!(normalize_spec("/abs/path"), "/abs/path");
    }

    const GB: u64 = 1 << 30;

    /// A 4-bit ~20 GB model with a ~4 GB KV pool and a 4096-token chunk
    /// (~1.9 GB/slot) on machines of varying size.
    fn budget(available_gb: u64) -> ConcurrencyBudget {
        ConcurrencyBudget {
            available_ram: Some(available_gb * GB),
            weights: Some(20 * GB),
            kv_pool: Some(4 * GB),
            per_seq_peak: per_seq_prefill_peak(4096), // ~1.9 GB
            ceiling: super::DEFAULT_SEQS_CEILING,
        }
    }

    // All GiB (1<<30); one slot at chunk 4096 ≈ 1.82 GiB.
    #[test]
    fn budget_serialises_when_no_headroom_for_a_second_prefill() {
        // 32 GiB: 0.8·32 − 24 ≈ 1.7 GiB headroom < one slot → floor 1.
        assert_eq!(budgeted_max_num_seqs(&budget(32)), 1);
        // 36 GiB: 0.8·36 − 24 ≈ 4.8 GiB ≈ 2 slots.
        assert_eq!(budgeted_max_num_seqs(&budget(36)), 2);
    }

    #[test]
    fn budget_scales_with_memory_up_to_the_ceiling() {
        // 48 GiB: 0.8·48 − 24 ≈ 14.4 GiB / 1.82 ≈ 7 slots.
        assert_eq!(budgeted_max_num_seqs(&budget(48)), 7);
        // 64 GiB: ≈ 27 GiB / 1.82 ≈ 14 → clamped to the ceiling (8).
        assert_eq!(
            budgeted_max_num_seqs(&budget(64)),
            super::DEFAULT_SEQS_CEILING
        );
    }

    #[test]
    fn budget_floors_to_one_on_unknown_or_overcommitted_memory() {
        let mut b = budget(64);
        b.weights = None; // first run, weights not cached → can't budget
        assert_eq!(budgeted_max_num_seqs(&b), 1);

        // Model bigger than available → no headroom → floor 1, not 0.
        assert_eq!(budgeted_max_num_seqs(&budget(16)), 1);
    }
}
