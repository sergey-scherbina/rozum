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
    use futures::StreamExt as _;

    use crate::backend::{
        ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
        ModelResult, Role, StopReason,
    };

    use mistralrs::{
        ChatCompletionChunkResponse, ChunkChoice, Delta, Model, ModelBuilder, RequestBuilder,
        Response, TextMessageRole,
    };

    /// Knobs that map to mistralrs's `ModelBuilder` options.
    /// Sampling-detail knobs intentionally omitted for now — mistralrs's
    /// defaults are sensible and the API surface for sampler customisation
    /// has shifted between versions.
    #[derive(Clone, Debug)]
    pub struct MistralrsOptions {
        /// Context window (used by `context_window()`).
        pub n_ctx: u32,
    }

    impl Default for MistralrsOptions {
        fn default() -> Self {
            let n_ctx = std::env::var("ROZUM_MISTRALRS_N_CTX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32_768);
            Self { n_ctx }
        }
    }

    pub struct MistralrsBackend {
        model: Arc<Model>,
        opts: MistralrsOptions,
    }

    impl MistralrsBackend {
        /// `model_id` is anything `ModelBuilder::new` accepts: a HuggingFace
        /// repo (`<user>/<repo>`) or a local safetensors directory.
        pub async fn new(model_id: &str, opts: MistralrsOptions) -> ModelResult<Self> {
            let model = ModelBuilder::new(model_id).build().await.map_err(|e| {
                ModelError::BackendUnavailable(format!("mistralrs: failed to load {model_id}: {e}"))
            })?;
            tracing::info!(model = %model_id, n_ctx = opts.n_ctx, "MistralrsBackend loaded");
            Ok(Self {
                model: Arc::new(model),
                opts,
            })
        }

        fn role_to_mistralrs(role: &Role) -> TextMessageRole {
            match role {
                Role::System => TextMessageRole::System,
                Role::User => TextMessageRole::User,
                Role::Assistant => TextMessageRole::Assistant,
                // mistralrs may name this differently; map Tool to User as a
                // safe fallback (a follow-up issue tracks proper tool support).
                Role::Tool => TextMessageRole::User,
            }
        }

        fn message_to_text(msg: &Message) -> String {
            msg.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        }

        fn build_request(&self, req: &ChatRequest) -> RequestBuilder {
            let mut rb = RequestBuilder::new();
            for msg in &req.messages {
                let role = Self::role_to_mistralrs(&msg.role);
                let text = Self::message_to_text(msg);
                if !text.is_empty() {
                    rb = rb.add_message(role, text);
                }
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

            let mut stream = model.stream_chat_request(request).await.map_err(|e| {
                ModelError::BackendUnavailable(format!("mistralrs: stream_chat_request: {e}"))
            })?;

            let chat_stream: ChatStream = Box::pin(async_stream::stream! {
                let mut output_tokens: u32 = 0;
                let mut done_sent = false;

                while let Some(item) = stream.next().await {
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
                        if let Some(ChunkChoice {
                            delta: Delta { content: Some(content), .. },
                            finish_reason,
                            ..
                        }) = choices.first()
                        {
                            if !content.is_empty() {
                                output_tokens += 1;
                                yield Ok(ChatEvent::TextDelta { text: content.clone() });
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
                    // Other Response variants (Done, Errors, etc.) are
                    // intentionally ignored — the final Done is emitted
                    // by the loop's natural end below.
                }

                if !done_sent {
                    yield Ok(ChatEvent::Done {
                        input_tokens: 0,
                        output_tokens,
                        stop_reason: StopReason::EndTurn,
                    });
                }
            });

            Ok(chat_stream)
        }

        fn context_window(&self) -> u32 {
            self.opts.n_ctx
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
