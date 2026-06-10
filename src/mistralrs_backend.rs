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

    use mistralrs::{
        AutoDeviceMapParams, CalledFunction, ChatCompletionChunkResponse, ChunkChoice,
        DeviceMapSetting, Function, MemoryGpuConfig, Model, ModelBuilder, PagedAttentionMetaBuilder,
        RequestBuilder, Response, TextMessageRole, Tool, ToolCallResponse, ToolCallType, ToolChoice,
        ToolType,
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
        /// command buffer. 1 serialises them, matching Ollama/LM Studio.
        pub max_num_seqs: usize,
    }

    impl Default for MistralrsOptions {
        fn default() -> Self {
            let n_ctx = std::env::var("ROZUM_MISTRALRS_N_CTX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32_768);
            let max_num_seqs = std::env::var("ROZUM_MISTRALRS_MAX_SEQS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n >= 1)
                .unwrap_or(1);
            Self {
                n_ctx,
                max_num_seqs,
            }
        }
    }

    pub struct MistralrsBackend {
        model: Arc<Model>,
        opts: MistralrsOptions,
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
                        let too_big = msg.contains("does not fit")
                            || msg.contains("exceeds total capacity");
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
            Ok(Self {
                model: Arc::new(model),
                opts,
            })
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
            let request = self.build_request(&req);
            let model = Arc::clone(&self.model);

            // Move `model` into the stream so the upstream borrow it holds is
            // bounded by the stream's lifetime, not by this function call.
            let chat_stream: ChatStream = Box::pin(async_stream::stream! {
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

                while let Some(item) = upstream.next().await {
                    if cancel.is_cancelled() {
                        yield Ok(ChatEvent::Done {
                            input_tokens: 0,
                            output_tokens,
                            stop_reason: StopReason::Cancelled,
                        });
                        done_sent = true;
                        break;
                    }

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

#[cfg(test)]
mod tests {
    use super::normalize_spec;

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
}
