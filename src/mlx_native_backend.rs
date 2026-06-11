/// Pure-Rust native MLX backend (no Python, no subprocess, no candle).
///
/// Runs a full MLX forward on Apple Silicon Metal via the vendored `mlx-lm`
/// fork (`.vendor/mlx-lm`). MLX `Array`s and the model are `!Send` (one Metal
/// stream, single-threaded), so the model is owned for life by ONE dedicated
/// worker thread: it loads the weights itself (they can never cross a thread
/// boundary) and serves jobs off a channel, streaming token events back. The
/// `ChatBackend` is a thin Send+Sync handle over that worker's job queue.
///
/// Gated on `#[cfg(feature = "mlx-native")]`. Default build is unaffected.
#[cfg(feature = "mlx-native")]
mod inner {
    use std::path::{Path, PathBuf};
    use std::thread;

    use async_trait::async_trait;
    use tokio::sync::{mpsc, oneshot};
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use crate::backend::{
        ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
        ModelResult, Role, SamplingParams, StopReason,
    };

    use mlx_lm::cache::ConcatKeyValueCache;
    use mlx_lm::models::{qwen3, qwen3_5, qwen3_5_moe, qwen3_moe};
    use mlx_lm_utils::tokenizer::{
        ApplyChatTemplateArgs, Chat, Conversation, Tokenizer, load_model_chat_template_from_file,
    };
    use mlx_rs::Array;
    use mlx_rs::error::Exception;
    use mlx_rs::ops::indexing::{IndexOp, NewAxis};
    use mlx_rs::transforms::eval;

    /// Cap on generated tokens when the request does not specify one, so a
    /// reasoning model can't run away unbounded.
    const DEFAULT_MAX_TOKENS: usize = 4096;
    /// Qwen3 `<|im_end|>`, used when the checkpoint config omits `eos_token_id`.
    const QWEN3_EOS: u32 = 151645;
    /// Fallback context window when config lacks `max_position_embeddings`.
    const DEFAULT_N_CTX: u32 = 32_768;

    /// A loaded MLX model, dispatched by `config.json`'s `model_type`. Each
    /// variant exposes the same `Generate` token iterator, so the streaming
    /// loop is architecture-agnostic.
    enum LoadedModel {
        Qwen3(qwen3::Model),
        Qwen3Moe(qwen3_moe::Model),
        Qwen35(qwen3_5::Model),
        Qwen35Moe(qwen3_5_moe::Model),
    }

    impl LoadedModel {
        fn load(model_type: &str, dir: &Path) -> Result<Self, String> {
            match model_type {
                "qwen3" => qwen3::load_qwen3_model(dir)
                    .map(LoadedModel::Qwen3)
                    .map_err(|e| format!("mlx: load qwen3 {}: {e}", dir.display())),
                "qwen3_moe" => qwen3_moe::load_qwen3_moe_model(dir)
                    .map(LoadedModel::Qwen3Moe)
                    .map_err(|e| format!("mlx: load qwen3_moe {}: {e}", dir.display())),
                // Qwen3.6 dense (the config wrapper is `qwen3_5`, text is `qwen3_5_text`).
                "qwen3_5" | "qwen3_5_text" => qwen3_5::load_qwen3_5_model(dir)
                    .map(LoadedModel::Qwen35)
                    .map_err(|e| format!("mlx: load qwen3_5 {}: {e}", dir.display())),
                // Qwen3.6 MoE (wrapper `qwen3_5_moe`, text `qwen3_5_moe_text`).
                "qwen3_5_moe" | "qwen3_5_moe_text" => qwen3_5_moe::load_qwen3_5_moe_model(dir)
                    .map(LoadedModel::Qwen35Moe)
                    .map_err(|e| format!("mlx: load qwen3_5_moe {}: {e}", dir.display())),
                other => Err(format!("mlx: unsupported model_type '{other}'")),
            }
        }
    }

    /// One inference request handed to the worker thread. All fields are `Send`;
    /// the `!Send` MLX work happens entirely on the worker.
    struct Job {
        messages: Vec<Message>,
        sampling: SamplingParams,
        model_id: String,
        cancel: tokio_util::sync::CancellationToken,
        events: mpsc::UnboundedSender<ModelResult<ChatEvent>>,
    }

    /// Minimal slice of `config.json` we read on the calling thread (plain JSON,
    /// no MLX), so the worker only ever touches the `!Send` model.
    fn read_config(dir: &Path) -> (u32, Vec<u32>, String) {
        let mut n_ctx = DEFAULT_N_CTX;
        let mut eos: Vec<u32> = Vec::new();
        let mut model_type = "qwen3".to_string();
        if let Ok(text) = std::fs::read_to_string(dir.join("config.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(n) = v.get("max_position_embeddings").and_then(|x| x.as_u64()) {
                    n_ctx = n as u32;
                }
                if let Some(t) = v.get("model_type").and_then(|x| x.as_str()) {
                    model_type = t.to_string();
                }
                // eos_token_id is an int or a list; stop on ALL of them (Qwen3
                // ships <|im_end|> 151645 + <|endoftext|> 151643).
                match v.get("eos_token_id") {
                    Some(serde_json::Value::Number(n)) => {
                        eos.extend(n.as_u64().map(|id| id as u32));
                    }
                    Some(serde_json::Value::Array(a)) => {
                        eos.extend(a.iter().filter_map(|x| x.as_u64()).map(|id| id as u32));
                    }
                    _ => {}
                }
            }
        }
        if eos.is_empty() {
            eos.push(QWEN3_EOS);
        }
        (n_ctx, eos, model_type)
    }

    pub struct MlxNativeBackend {
        jobs: mpsc::UnboundedSender<Job>,
        model_id: String,
        n_ctx: u32,
    }

    impl MlxNativeBackend {
        /// `model_dir` is a local directory of safetensors + tokenizer.json +
        /// tokenizer_config.json (the layout HuggingFace / mlx-community ship).
        /// `model_id` selects the chat-template variant and labels logs.
        pub async fn new(model_dir: PathBuf, model_id: String) -> ModelResult<Self> {
            let (n_ctx, eos, model_type) = read_config(&model_dir);
            let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
            let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
            let label = model_id.clone();

            // The worker loads the model on its own thread and owns it for life;
            // the `!Send` model never crosses a thread boundary.
            thread::Builder::new()
                .name("mlx-native".into())
                .spawn(move || worker_main(model_dir, model_type, eos, jobs_rx, ready_tx))
                .map_err(|e| ModelError::BackendUnavailable(format!("mlx: spawn worker: {e}")))?;

            match ready_rx.await {
                Ok(Ok(())) => {
                    eprintln!("mlx-native: '{label}' ready (context {n_ctx})");
                    Ok(Self {
                        jobs: jobs_tx,
                        model_id,
                        n_ctx,
                    })
                }
                Ok(Err(e)) => Err(ModelError::BackendUnavailable(e)),
                Err(_) => Err(ModelError::BackendUnavailable(
                    "mlx: worker died during load".into(),
                )),
            }
        }
    }

    #[async_trait]
    impl ChatBackend for MlxNativeBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let (events_tx, events_rx) = mpsc::unbounded_channel();
            let job = Job {
                messages: req.messages,
                sampling: req.sampling,
                model_id: self.model_id.clone(),
                cancel: req.cancel,
                events: events_tx,
            };
            self.jobs
                .send(job)
                .map_err(|_| ModelError::BackendUnavailable("mlx: worker gone".into()))?;
            Ok(Box::pin(UnboundedReceiverStream::new(events_rx)))
        }

        fn context_window(&self) -> u32 {
            self.n_ctx
        }

        fn label(&self) -> &'static str {
            "mlx-native"
        }

        /// MLX serialises on a single worker thread / Metal stream, so a safe
        /// in-process capacity is 1 — `concurrency::admit_wrap` gates at that.
        fn concurrency_capacity(&self) -> Option<usize> {
            Some(1)
        }
    }

    /// Map our role to the static string the chat template expects. We pass
    /// these to `Conversation` directly, covering "system"/"tool" that the
    /// helper's own `Role` enum omits.
    fn role_str(role: &Role) -> &'static str {
        match role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    fn message_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Worker thread entry point. Loads the model (reporting load result over
    /// `ready`), then serves jobs until the queue closes.
    fn worker_main(
        model_dir: PathBuf,
        model_type: String,
        eos: Vec<u32>,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        ready: oneshot::Sender<Result<(), String>>,
    ) {
        let mut model = match LoadedModel::load(&model_type, &model_dir) {
            Ok(m) => m,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut tokenizer = match Tokenizer::from_file(model_dir.join("tokenizer.json")) {
            Ok(t) => t,
            Err(e) => {
                let _ = ready.send(Err(format!("mlx: tokenizer: {e:?}")));
                return;
            }
        };
        // Chat template: the `chat_template` field of tokenizer_config.json, or
        // a raw `chat_template.jinja` (multimodal snapshots ship the latter).
        let template =
            match load_model_chat_template_from_file(model_dir.join("tokenizer_config.json"))
                .ok()
                .flatten()
                .or_else(|| std::fs::read_to_string(model_dir.join("chat_template.jinja")).ok())
            {
                Some(t) => t,
                None => {
                    let _ = ready.send(Err(
                        "mlx: no chat template (tokenizer_config.json / chat_template.jinja)"
                            .into(),
                    ));
                    return;
                }
            };
        if ready.send(Ok(())).is_err() {
            return; // caller gave up before load finished
        }

        // `blocking_recv` is correct here: this is a plain OS thread with no
        // tokio runtime, so it parks until the next job (or the queue closes).
        while let Some(job) = jobs.blocking_recv() {
            run_job(&mut model, &mut tokenizer, &template, &eos, job);
        }
    }

    /// Render the prompt and stream token events. Dispatches on the model
    /// architecture; each `Generate` iterator feeds the shared streaming loop.
    fn run_job(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        job: Job,
    ) {
        let prompt_ids = match render_prompt(tokenizer, template, &job.model_id, &job.messages) {
            Ok(ids) => ids,
            Err(e) => {
                let _ = job.events.send(Err(ModelError::BackendUnavailable(e)));
                return;
            }
        };
        if std::env::var("ROZUM_MLX_DEBUG").is_ok() {
            eprintln!("PROMPT_IDS len={} {:?}", prompt_ids.len(), prompt_ids);
        }
        let prompt_tokens = Array::from(&prompt_ids[..]).index(NewAxis);
        let temp = job.sampling.temperature.unwrap_or(0.0);
        // top_k <= 0 / top_p >= 1.0 disable those filters (the sampler's defaults).
        let top_p = job.sampling.top_p.unwrap_or(1.0);
        let top_k = job.sampling.top_k.map(|k| k as i32).unwrap_or(0);
        if let Some(s) = job.sampling.seed {
            let _ = mlx_rs::random::seed(s);
        }
        let max_tokens = job
            .sampling
            .max_tokens
            .map(|m| m as usize)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        match model {
            LoadedModel::Qwen3(m) => {
                let mut generator = qwen3::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                );
            }
            LoadedModel::Qwen3Moe(m) => {
                let mut generator = qwen3_moe::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                );
            }
            LoadedModel::Qwen35(m) => {
                // Owns its heterogeneous (KV + conv/recurrent) cache internally.
                let mut generator = qwen3_5::Generate::new(m, temp, &prompt_tokens);
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                );
            }
            LoadedModel::Qwen35Moe(m) => {
                let mut generator = qwen3_5_moe::Generate::new(m, temp, &prompt_tokens);
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                );
            }
        }
    }

    /// Architecture-agnostic streaming loop: pull tokens off a `Generate`
    /// iterator, force per-token compute, stop on EOS / max-tokens / cancel,
    /// and emit UTF-8-safe text deltas. A send error means the client dropped
    /// the stream -> stop early.
    fn stream_generation<I>(
        generate: I,
        tokenizer: &mut Tokenizer,
        eos: &[u32],
        prompt_len: usize,
        max_tokens: usize,
        job: &Job,
    ) where
        I: Iterator<Item = Result<Array, Exception>>,
    {
        let mut out_ids: Vec<u32> = Vec::new();
        let mut emitted = String::new();
        let mut output_tokens: u32 = 0;

        for token in generate {
            if job.cancel.is_cancelled() {
                let _ = job.events.send(Ok(ChatEvent::Done {
                    input_tokens: prompt_len as u32,
                    output_tokens,
                    stop_reason: StopReason::Cancelled,
                }));
                return;
            }
            let token = match token {
                Ok(t) => t,
                Err(e) => {
                    let _ = job
                        .events
                        .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                    return;
                }
            };
            if eval([&token]).is_err() {
                let _ = job.events.send(Err(ModelError::BackendUnavailable(
                    "mlx: eval failed".into(),
                )));
                return;
            }
            let id = token.item::<u32>();
            if output_tokens == 0 && std::env::var("ROZUM_MLX_DEBUG").is_ok() {
                eprintln!("FIRST_TOK {id} (eos={eos:?})");
            }
            if eos.contains(&id) {
                let _ = job.events.send(Ok(ChatEvent::Done {
                    input_tokens: prompt_len as u32,
                    output_tokens,
                    stop_reason: StopReason::EndTurn,
                }));
                return;
            }
            out_ids.push(id);
            output_tokens += 1;

            // Incremental detokenize: re-decode the run and emit the new suffix.
            // Hold back a trailing replacement char (an incomplete multi-byte
            // sequence, e.g. mid-Cyrillic) until the next token completes it.
            if let Ok(text) = tokenizer.decode(&out_ids, true) {
                let stable = text.trim_end_matches('\u{FFFD}');
                if stable.len() > emitted.len() && stable.starts_with(&emitted) {
                    let delta = stable[emitted.len()..].to_string();
                    emitted = stable.to_string();
                    if job
                        .events
                        .send(Ok(ChatEvent::TextDelta { text: delta }))
                        .is_err()
                    {
                        return; // client dropped the stream
                    }
                }
            }

            if output_tokens as usize >= max_tokens {
                let _ = job.events.send(Ok(ChatEvent::Done {
                    input_tokens: prompt_len as u32,
                    output_tokens,
                    stop_reason: StopReason::MaxTokens,
                }));
                return;
            }
        }

        // The iterator ended without an internal stop — the hybrid `Generate`
        // returns `None` when cancelled mid-prefill (chunk boundary). Report it.
        let stop_reason = if job.cancel.is_cancelled() {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        };
        let _ = job.events.send(Ok(ChatEvent::Done {
            input_tokens: prompt_len as u32,
            output_tokens,
            stop_reason,
        }));
    }

    /// Apply the model's chat template to the messages and tokenize, returning
    /// the prompt token ids (with the generation prompt appended).
    fn render_prompt(
        tokenizer: &mut Tokenizer,
        template: &str,
        model_id: &str,
        messages: &[Message],
    ) -> Result<Vec<u32>, String> {
        let convo: Vec<Conversation<&'static str, String>> = messages
            .iter()
            .map(|m| Conversation {
                role: role_str(&m.role),
                content: message_text(m),
            })
            .collect();
        let args = ApplyChatTemplateArgs {
            conversations: vec![Chat::from(convo)],
            documents: None,
            model_id,
            chat_template_id: None,
            add_generation_prompt: Some(true),
            continue_final_message: None,
        };
        let encodings = tokenizer
            .apply_chat_template_and_encode(template.to_string(), args)
            .map_err(|e| format!("mlx: chat template render: {e}"))?;
        Ok(encodings
            .iter()
            .flat_map(|e| e.get_ids())
            .copied()
            .collect())
    }

    pub use MlxNativeBackend as Export;
}

#[cfg(feature = "mlx-native")]
pub use inner::Export as MlxNativeBackend;

/// Resolve a model spec to a local directory of safetensors + tokenizer files.
///
/// - an existing directory path -> as-is
/// - `mlx-community:<repo>` / `hf:<user>/<repo>` / `<user>/<repo>` -> the
///   already-downloaded HuggingFace cache snapshot, if present
///   (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<rev>/`).
///
/// Auto-download is deliberately out of scope for now: this reuses weights a
/// prior mistralrs / mlx_lm run already fetched. Returns `None` when nothing
/// local matches, so the gateway falls through to the next backend.
pub fn resolve_model_dir(spec: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let direct = PathBuf::from(spec);
    if direct.is_dir() && direct.join("config.json").is_file() {
        return Some(direct);
    }

    // Normalize the spec's `org/name`, mirroring mistralrs_backend::normalize_spec.
    let repo = if let Some(r) = spec.strip_prefix("mlx-community:") {
        format!("mlx-community/{r}")
    } else if let Some(r) = spec.strip_prefix("hf:") {
        r.to_owned()
    } else {
        spec.to_owned()
    };
    let (org, name) = repo.split_once('/')?;

    let home = std::env::var_os("HOME")?;
    let cache = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{name}"))
        .join("snapshots");
    let snapshots = std::fs::read_dir(&cache).ok()?;
    for entry in snapshots.flatten() {
        let dir = entry.path();
        if dir.join("config.json").is_file() {
            return Some(dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::resolve_model_dir;

    #[test]
    fn resolve_missing_spec_is_none() {
        assert!(resolve_model_dir("definitely/not-a-real-model-xyzzy").is_none());
    }

    #[test]
    fn resolve_existing_dir_passthrough() {
        // A temp dir with a config.json resolves to itself.
        let tmp = std::env::temp_dir().join("rozum_mlx_resolve_test");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("config.json"), "{}").unwrap();
        assert_eq!(resolve_model_dir(tmp.to_str().unwrap()), Some(tmp.clone()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // End-to-end through the real ChatBackend (template -> worker -> stream).
    // Needs the local Qwen3-4B-4bit snapshot; run with:
    //   cargo test --features mlx-native -- --ignored mlx_chat_capital_of_france
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_chat_capital_of_france() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into())
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // MoE path (qwen3_moe). Needs the local Qwen3-30B-A3B-4bit snapshot; run:
    //   cargo test --features mlx-native -- --ignored mlx_moe_chat_capital
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-30B-A3B-4bit"]
    async fn mlx_moe_chat_capital() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let dir = resolve_model_dir("mlx-community:Qwen3-30B-A3B-4bit")
            .expect("Qwen3-30B-A3B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-30B-A3B-4bit".into())
            .await
            .expect("backend load");
        let req = ChatRequest::simple(
            "What is the capital of France? Reply in one short sentence. /no_think",
        );
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX MoE OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Qwen3.6 dense (qwen3_5: hybrid full-attn + GatedDeltaNet). Needs the local
    // Qwen3.6-27B-4bit snapshot; run:
    //   cargo test --features mlx-native -- --ignored mlx_qwen35_chat
    // Greedy prefix must match Python mlx_lm (deterministic; it "thinks" first).
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3.6-27B-4bit"]
    async fn mlx_qwen35_chat() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-27B-4bit".into())
            .await
            .expect("backend load");
        let mut req = ChatRequest::simple(
            "What is the capital of France? Reply in one short sentence. /no_think",
        );
        req.sampling = SamplingParams {
            max_tokens: Some(24),
            ..Default::default()
        };
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX Q3.6 OUTPUT: {text}");
        assert!(
            text.starts_with("Here's a thinking process"),
            "greedy prefix diverged from Python oracle; got: {text}"
        );
    }

    // Qwen3.6 MoE (qwen3_5_moe: hybrid backbone + sparse MoE w/ shared expert).
    // Needs the local Qwen3.6-35B-A3B-4bit snapshot; run:
    //   cargo test --features mlx-native -- --ignored mlx_qwen35_moe_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3.6-35B-A3B-4bit"]
    async fn mlx_qwen35_moe_chat() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};

        let dir = resolve_model_dir("mlx-community:Qwen3.6-35B-A3B-4bit")
            .expect("Qwen3.6-35B-A3B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-35B-A3B-4bit".into())
            .await
            .expect("backend load");
        let mut req = ChatRequest::simple(
            "What is the capital of France? Reply in one short sentence. /no_think",
        );
        req.sampling = SamplingParams {
            max_tokens: Some(24),
            ..Default::default()
        };
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX Q3.6-MoE OUTPUT: {text}");
        assert!(
            text.starts_with("Thinking Process:"),
            "greedy prefix diverged from Python oracle; got: {text}"
        );
    }

    // Perf benchmark for the Qwen3.6 GatedDeltaNet prefill (the ops-path scan).
    // Drives the fork model directly with synthetic tokens. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_qwen35_prefill_bench
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf bench; requires local mlx-community/Qwen3.6-27B-4bit"]
    fn mlx_qwen35_prefill_bench() {
        use mlx_lm::models::qwen3_5::load_qwen3_5_model;
        use mlx_rs::Array;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::eval;
        use std::time::Instant;

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let mut model = load_qwen3_5_model(&dir).expect("load");

        // Synthetic prompt token ids (values don't matter for timing).
        let synth = |n: usize| -> Vec<u32> { (0..n).map(|i| (1000 + i % 5000) as u32).collect() };

        for &n in &[128usize, 512, 1024] {
            let ids = synth(n);
            let prompt = Array::from(&ids[..]).index(NewAxis);
            let mut cache = model.init_cache();
            let t = Instant::now();
            let logits = model.forward(&prompt, &mut cache).expect("prefill");
            eval([&logits]).unwrap();
            let prefill = t.elapsed().as_secs_f64();
            // Decode rate: time 16 single-token steps.
            let mut y = logits.index((.., -1, ..));
            let td = Instant::now();
            let steps = 16;
            for _ in 0..steps {
                let inp = mlx_rs::ops::indexing::argmax_axis(&y, -1, false)
                    .unwrap()
                    .index((.., NewAxis));
                let l = model.forward(&inp, &mut cache).expect("decode");
                y = l.index((.., -1, ..));
                eval([&y]).unwrap();
            }
            let dps = steps as f64 / td.elapsed().as_secs_f64();
            eprintln!(
                "BENCH n={n:>4}  prefill={prefill:>7.3}s ({:>7.1} tok/s)  decode={dps:>6.1} tok/s",
                n as f64 / prefill
            );
        }
    }

    // Chunked prefill must be byte-identical to a single pass: the per-position
    // attention (causal mask + KV cache) and the sequential GatedDeltaNet scan are
    // position-local, so splitting the prompt only changes WHEN intermediates are
    // freed, not the math. Drives the 27B model: one ~3000-tok synthetic prompt
    // prefilled single-pass (chunk huge) vs chunked (chunk 512); compares the
    // last-position logits exactly. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_qwen35_chunked_prefill
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "requires local mlx-community/Qwen3.6-27B-4bit"]
    fn mlx_qwen35_chunked_prefill_matches_single_pass() {
        use mlx_lm::models::qwen3_5::load_qwen3_5_model;
        use mlx_rs::Array;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::eval;

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let mut model = load_qwen3_5_model(&dir).expect("load");

        let ids: Vec<u32> = (0..3000).map(|i| (1000 + i % 5000) as u32).collect();
        let prompt = Array::from(&ids[..]).index(NewAxis);

        // Single pass (chunk > T) vs chunked (512) — fresh cache each.
        let mut cache_a = model.init_cache();
        let single = model
            .prefill_chunked(&prompt, &mut cache_a, 8192)
            .expect("single-pass prefill");
        let mut cache_b = model.init_cache();
        let chunked = model
            .prefill_chunked(&prompt, &mut cache_b, 512)
            .expect("chunked prefill");
        eval([&single, &chunked]).unwrap();

        let a = single.reshape(&[-1]).unwrap();
        let b = chunked.reshape(&[-1]).unwrap();
        let max_abs = a
            .subtract(&b)
            .unwrap()
            .abs()
            .unwrap()
            .max(None)
            .unwrap()
            .item::<f32>();
        let am = |x: &Array| {
            mlx_rs::ops::indexing::argmax_axis(x, 0, false)
                .unwrap()
                .item::<u32>()
        };
        eprintln!(
            "CHUNKED-PREFILL max|Δlogit|={max_abs:.3e}  argmax single={} chunked={}",
            am(&a),
            am(&b)
        );
        assert_eq!(am(&a), am(&b), "chunked prefill changed the sampled token");
        assert!(
            max_abs < 1e-2,
            "chunked prefill logits diverged: max|Δ|={max_abs}"
        );
    }

    // Go/no-go probe for the decode-compile lever: does `mx.compile` actually fuse
    // and speed up a real forward on this hardware? Compiles the dense Qwen3-4B
    // forward (no custom kernel, no MoE — cleanest) at FIXED shapes (fresh cache
    // each call, so shapes stay constant and the compiled graph is reused) and
    // times compiled vs uncompiled at T=1 (decode-representative: same dispatch
    // count as a real decode step) and T=16. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_compile_probe
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf probe; requires local mlx-community/Qwen3-4B-4bit"]
    fn mlx_compile_probe() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3::{load_qwen3_model, Model, ModelInput};
        use mlx_rs::Array;
        use mlx_rs::module::Module;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::compile::compile_with_state;
        use mlx_rs::transforms::eval;
        use std::time::Instant;

        let dir =
            resolve_model_dir("mlx-community:Qwen3-4B-4bit").expect("Qwen3-4B-4bit not in HF cache");
        let mut model = load_qwen3_model(&dir).expect("load");

        // A fresh-cache single forward over [1, T]. Shapes fixed across calls, so
        // the compiled graph is reused. Captures nothing -> Copy + 'static.
        let step = |model: &mut Model, args: &[Array]| -> Result<Vec<Array>, mlx_rs::error::Exception> {
            let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
            let input = ModelInput {
                inputs: &args[0],
                mask: None,
                cache: &mut cache,
            };
            let logits = <Model as Module<ModelInput<'_, ConcatKeyValueCache>>>::forward(
                model, input,
            )?;
            Ok(vec![logits])
        };

        for &t in &[1i32, 16] {
            let ids: Vec<u32> = (0..t).map(|i| (1000 + i) as u32).collect();
            let input = Array::from(&ids[..]).index(NewAxis);
            let args = [input];
            let iters = 64;

            // Uncompiled baseline.
            let _ = eval([&step(&mut model, &args).unwrap()[0]]);
            let t0 = Instant::now();
            for _ in 0..iters {
                let o = step(&mut model, &args).unwrap();
                eval([&o[0]]).unwrap();
            }
            let uncompiled = t0.elapsed().as_secs_f64() / iters as f64;

            // Compiled.
            let mut compiled = compile_with_state(step, None);
            let _ = eval([&compiled(&mut model, &args).unwrap()[0]]); // warm/trace
            let t1 = Instant::now();
            for _ in 0..iters {
                let o = compiled(&mut model, &args).unwrap();
                eval([&o[0]]).unwrap();
            }
            let comp = t1.elapsed().as_secs_f64() / iters as f64;

            eprintln!(
                "COMPILE-PROBE T={t:>2}  uncompiled={:.3}ms  compiled={:.3}ms  speedup={:.2}x",
                uncompiled * 1e3,
                comp * 1e3,
                uncompiled / comp
            );
        }
    }

    // Mid-prefill cancellation: `Model::prefill_cancellable` must bail (return
    // None) at a chunk boundary as soon as `should_cancel()` fires, so a cancel on
    // a long prompt is honored DURING prefill (not only after it). Deterministic
    // (no timing): cancel immediately, and cancel after N chunks. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_qwen35_prefill_cancel
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "requires local mlx-community/Qwen3.6-27B-4bit"]
    fn mlx_qwen35_prefill_cancels_mid_prefill() {
        use mlx_lm::models::qwen3_5::load_qwen3_5_model;
        use mlx_rs::Array;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use std::cell::Cell;

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let mut model = load_qwen3_5_model(&dir).expect("load");
        // ~3000 tokens, chunk 512 -> ~6 chunks.
        let ids: Vec<u32> = (0..3000).map(|i| (1000 + i % 5000) as u32).collect();
        let prompt = Array::from(&ids[..]).index(NewAxis);

        // Cancel before the first chunk -> immediate None.
        let mut cache = model.init_cache();
        let out = model
            .prefill_cancellable(&prompt, &mut cache, 512, &|| true)
            .expect("prefill");
        assert!(out.is_none(), "immediate cancel must bail before any chunk");

        // Cancel after 2 chunks -> still bails before completing all ~6.
        let calls = Cell::new(0usize);
        let mut cache2 = model.init_cache();
        let out2 = model
            .prefill_cancellable(&prompt, &mut cache2, 512, &|| {
                calls.set(calls.get() + 1);
                calls.get() > 2
            })
            .expect("prefill");
        assert!(out2.is_none(), "cancel after 2 chunks must bail");
        assert!(
            calls.get() <= 4,
            "should have stopped early, not run all chunks (calls={})",
            calls.get()
        );

        // No cancel -> completes (Some).
        let mut cache3 = model.init_cache();
        let out3 = model
            .prefill_cancellable(&prompt, &mut cache3, 512, &|| false)
            .expect("prefill");
        assert!(out3.is_some(), "no cancel must complete prefill");
        eprintln!("PREFILL-CANCEL ok (calls until bail={})", calls.get());
    }
}
