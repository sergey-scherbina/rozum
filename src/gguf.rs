/// In-process GGUF inference via llama-cpp-2 (Metal on Apple Silicon).
///
/// Path resolvers (`resolve_model_path`, etc.) are always compiled.
/// `GgufBackend` and its helpers are gated on `#[cfg(feature = "gguf")]`.
use std::path::PathBuf;

// ─── GgufOptions ─────────────────────────────────────────────────────────────

/// Configuration for a `GgufBackend` instance.
#[derive(Clone, Debug)]
pub struct GgufOptions {
    /// Context size in tokens. Default 32 768.
    pub n_ctx: u32,
    /// Number of model layers to offload to GPU. `u32::MAX` means all layers (Metal default).
    pub n_gpu_layers: u32,
    /// Batch size for prompt prefill.
    pub n_batch: u32,
    /// Enable flash attention (reduces KV-cache memory).
    pub flash_attn: bool,
}

impl Default for GgufOptions {
    fn default() -> Self {
        let n_ctx = std::env::var("ROZUM_GGUF_N_CTX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(32_768u32);
        let n_gpu_layers = std::env::var("ROZUM_GGUF_GPU_LAYERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(u32::MAX);
        Self {
            n_ctx,
            n_gpu_layers,
            n_batch: 512,
            flash_attn: true,
        }
    }
}

// ─── Path resolvers ───────────────────────────────────────────────────────────

/// Resolve a model spec to a concrete GGUF file path.
///
/// Resolution order (first match wins):
/// 1. `lmstudio:<user>/<repo>` — search `~/.cache/lm-studio/models/<user>/<repo>/`.
/// 2. `ollama:<name>[:<tag>]` — the Ollama model whose blob already sits in
///    `~/.ollama/models/blobs/` (used directly — no HTTP to a running
///    `ollama serve`). The `ollama:` prefix is REQUIRED; a bare `name:tag` is not
///    interpreted as Ollama.
/// 3. Absolute / relative filesystem path that exists.
///
/// Returns `None` and logs a warning if no path can be determined.
pub fn resolve_model_path(spec: &str) -> Option<PathBuf> {
    if let Some(repo) = spec.strip_prefix("lmstudio:") {
        return resolve_lmstudio_model(repo);
    }
    // Ollama models must be requested explicitly with an `ollama:` prefix — a bare
    // `name:tag` is no longer silently treated as Ollama (it was ambiguous with HF
    // / MLX specs and surprised users).
    if let Some(name) = spec.strip_prefix("ollama:") {
        return resolve_ollama_blob(name);
    }
    let path = PathBuf::from(spec);
    if path.exists() {
        return Some(path);
    }
    tracing::warn!(spec = %spec, "gguf: could not resolve spec to a file path");
    None
}

fn lmstudio_home() -> PathBuf {
    std::env::var_os("ROZUM_LMSTUDIO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".cache/lm-studio")
        })
}

fn ollama_home() -> PathBuf {
    std::env::var_os("ROZUM_OLLAMA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".ollama")
        })
}

/// Read the Ollama manifest for `<name>[:<tag>]` (default tag `latest`) and
/// return the cached GGUF blob path. Ollama itself does not have to be running
/// — we only touch files on disk.
pub fn resolve_ollama_blob(spec: &str) -> Option<PathBuf> {
    let (name, tag) = match spec.rsplit_once(':') {
        Some((n, t)) if !n.is_empty() && !t.is_empty() => (n, t),
        _ => (spec, "latest"),
    };

    let base = ollama_home().join("models");
    // Ollama keeps manifests under registry.ollama.ai/library/<name>/<tag> for
    // models pulled from the default registry; namespaced models live under
    // registry.ollama.ai/<user>/<name>/<tag>.
    let library = base
        .join("manifests")
        .join("registry.ollama.ai")
        .join("library")
        .join(name)
        .join(tag);
    let manifest_path = if library.is_file() {
        library
    } else {
        // Try one extra layer of nesting for namespaced repos like
        // hf.co/<user>/<repo> that some tools rewrite into the manifest tree.
        let alt = base
            .join("manifests")
            .join("registry.ollama.ai")
            .join(name)
            .join(tag);
        if alt.is_file() {
            alt
        } else {
            return None;
        }
    };

    let manifest_text = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).ok()?;
    let layers = manifest["layers"].as_array()?;
    let digest = layers.iter().find_map(|layer| {
        if layer["mediaType"].as_str() == Some("application/vnd.ollama.image.model") {
            layer["digest"].as_str().map(str::to_owned)
        } else {
            None
        }
    })?;
    let blob_name = digest.replace(':', "-");
    let blob_path = base.join("blobs").join(blob_name);
    blob_path.is_file().then_some(blob_path)
}

/// Search `~/.cache/lm-studio/models/<repo>/` for a GGUF file.
///
/// Preference order: file whose name contains `ROZUM_GGUF_QUANT_PREF`
/// (default `Q4_K_M`), then largest K quantisation, then first found.
pub fn resolve_lmstudio_model(repo: &str) -> Option<PathBuf> {
    let dir = lmstudio_home().join("models").join(repo);
    if !dir.is_dir() {
        tracing::warn!(
            dir = %dir.display(),
            "lmstudio: model directory not found. \
             Download the model in LMStudio first."
        );
        return None;
    }
    let pref = std::env::var("ROZUM_GGUF_QUANT_PREF").unwrap_or_else(|_| "Q4_K_M".to_owned());
    let mut candidates: Vec<PathBuf> = walkdir_gguf(&dir);
    if candidates.is_empty() {
        tracing::warn!(dir = %dir.display(), "lmstudio: no .gguf files found");
        return None;
    }
    candidates.sort_by_key(|p| {
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        std::cmp::Reverse(quant_priority(&name, &pref))
    });
    candidates.into_iter().next()
}

fn walkdir_gguf(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(walkdir_gguf(&path));
            } else if path.extension().map_or(false, |e| e == "gguf") {
                results.push(path);
            }
        }
    }
    results
}

/// Higher value = better preference. Called to sort multiple GGUF candidates.
fn quant_priority(filename: &str, pref: &str) -> i32 {
    let upper = filename.to_uppercase();
    if upper.contains(&pref.to_uppercase()) {
        return 100;
    }
    // Prefer higher K quantisations (Q5 > Q4 > Q3 > Q2)
    for (tag, score) in [
        ("Q5_K_M", 50),
        ("Q5_K_S", 45),
        ("Q4_K_M", 40),
        ("Q4_K_S", 35),
        ("Q4_0", 30),
        ("Q3_K_M", 20),
        ("Q3_K_S", 15),
        ("Q2_K", 5),
    ] {
        if upper.contains(tag) {
            return score;
        }
    }
    0
}

// ─── Tool-use token parser ────────────────────────────────────────────────────

/// State machine that detects Qwen-hermes `<tool_call>…</tool_call>` blocks
/// as they arrive token-by-token.
///
/// Call `feed(token_text)` for each decoded token; it returns zero or more
/// events to forward to the caller.
pub struct ToolUseParser {
    state: ToolParseState,
    buffer: String,
    call_index: u32,
}

#[derive(Debug)]
enum ToolParseState {
    /// Normal text — pass through.
    Text,
    /// Inside a `<tool_call>…</tool_call>` block, accumulating JSON.
    InCall { id: String, json_buf: String },
}

/// Events emitted by the parser. Mirrors `ChatEvent` variants.
#[derive(Debug)]
pub enum ToolParseEvent {
    /// A piece of plain text to forward as `TextDelta`.
    Text(String),
    /// Start of a tool call (id + name extracted from JSON header).
    Start { id: String, name: String },
    /// Incremental JSON fragment of the arguments.
    Delta { id: String, json_fragment: String },
    /// End of the tool call JSON.
    End { id: String },
    /// Parsing error — emit remaining buffer as text and reset.
    ParseError(String),
}

impl ToolUseParser {
    pub fn new() -> Self {
        Self {
            state: ToolParseState::Text,
            buffer: String::new(),
            call_index: 0,
        }
    }

    pub fn feed(&mut self, token: &str) -> Vec<ToolParseEvent> {
        self.buffer.push_str(token);
        let mut events = Vec::new();

        loop {
            match &self.state {
                ToolParseState::Text => {
                    if let Some(pos) = self.buffer.find("<tool_call>") {
                        // Flush text before the tag.
                        if pos > 0 {
                            events.push(ToolParseEvent::Text(self.buffer[..pos].to_owned()));
                        }
                        self.buffer = self.buffer[pos + "<tool_call>".len()..].to_owned();
                        self.call_index += 1;
                        let id = format!("call_{}", self.call_index);
                        self.state = ToolParseState::InCall {
                            id,
                            json_buf: String::new(),
                        };
                    } else {
                        // Keep up to the last 20 chars as lookahead for split tags.
                        if self.buffer.len() > 20 {
                            let flush_len = self.buffer.len() - 20;
                            events.push(ToolParseEvent::Text(self.buffer[..flush_len].to_owned()));
                            self.buffer = self.buffer[flush_len..].to_owned();
                        }
                        break;
                    }
                }
                ToolParseState::InCall { .. } => {
                    if let Some(end) = self.buffer.find("</tool_call>") {
                        let json_fragment = self.buffer[..end].to_owned();
                        let after = self.buffer[end + "</tool_call>".len()..].to_owned();
                        self.buffer = after;

                        let (id, accumulated) = if let ToolParseState::InCall { id, json_buf } =
                            std::mem::replace(&mut self.state, ToolParseState::Text)
                        {
                            (id, json_buf + &json_fragment)
                        } else {
                            unreachable!()
                        };

                        let name =
                            extract_tool_name(&accumulated).unwrap_or_else(|| "unknown".to_owned());
                        events.push(ToolParseEvent::Start {
                            id: id.clone(),
                            name,
                        });
                        events.push(ToolParseEvent::Delta {
                            id: id.clone(),
                            json_fragment: accumulated,
                        });
                        events.push(ToolParseEvent::End { id });
                    } else {
                        // Accumulate into json_buf, emit incremental delta for what's safe.
                        if let ToolParseState::InCall { json_buf, .. } = &mut self.state {
                            // Keep last 15 chars as lookahead for split "</tool_call>".
                            if self.buffer.len() > 15 {
                                let safe = self.buffer.len() - 15;
                                json_buf.push_str(&self.buffer[..safe]);
                                self.buffer = self.buffer[safe..].to_owned();
                            }
                        }
                        break;
                    }
                }
            }
        }
        events
    }

    /// Flush any remaining text at end of generation.
    pub fn flush(&mut self) -> Vec<ToolParseEvent> {
        let remaining = std::mem::take(&mut self.buffer);
        let mut events = Vec::new();
        if !remaining.is_empty() {
            match &self.state {
                ToolParseState::Text => {
                    events.push(ToolParseEvent::Text(remaining));
                }
                ToolParseState::InCall { id, json_buf: _ } => {
                    // Incomplete tool call — emit as parse error.
                    events.push(ToolParseEvent::ParseError(format!(
                        "incomplete <tool_call> block for id {id}"
                    )));
                }
            }
        }
        self.state = ToolParseState::Text;
        events
    }
}

impl Default for ToolUseParser {
    fn default() -> Self {
        Self::new()
    }
}

fn extract_tool_name(json: &str) -> Option<String> {
    let trimmed = json.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    v.get("name").and_then(|n| n.as_str()).map(str::to_owned)
}

// ─── GgufBackend (feature = "gguf") ─────────────────────────────────────────

#[cfg(feature = "gguf")]
mod inner {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::{GgufOptions, ToolParseEvent, ToolUseParser, format_qwen_prompt};
    use crate::backend::{
        ChatBackend, ChatEvent, ChatRequest, ChatStream, ModelError, ModelResult, SamplingParams,
        StopReason,
    };

    #[allow(deprecated)]
    use llama_cpp_2::model::Special;
    use llama_cpp_2::{
        context::params::LlamaContextParams,
        llama_backend::LlamaBackend,
        llama_batch::LlamaBatch,
        model::{AddBos, LlamaModel, params::LlamaModelParams},
        token::LlamaToken,
    };

    // Global backend handle — llama_backend_init is idempotent in C++ but we
    // keep a single Rust wrapper alive so the destructor runs exactly once.
    static LLAMA_BACKEND: Mutex<Option<Arc<LlamaBackend>>> = Mutex::new(None);

    fn get_backend() -> ModelResult<Arc<LlamaBackend>> {
        let mut guard = LLAMA_BACKEND
            .lock()
            .map_err(|_| ModelError::BackendUnavailable("llama backend mutex poisoned".into()))?;
        if guard.is_none() {
            let b = LlamaBackend::init().map_err(|e| {
                ModelError::BackendUnavailable(format!("llama_backend_init failed: {e}"))
            })?;
            *guard = Some(Arc::new(b));
        }
        Ok(Arc::clone(guard.as_ref().unwrap()))
    }

    pub struct GgufBackend {
        model: Arc<LlamaModel>,
        backend: Arc<LlamaBackend>,
        opts: GgufOptions,
        pub model_id: String,
    }

    impl GgufBackend {
        pub fn new(model_path: PathBuf, opts: GgufOptions) -> ModelResult<Self> {
            let backend = get_backend()?;

            let model_params = LlamaModelParams::default().with_n_gpu_layers(opts.n_gpu_layers);

            let model =
                LlamaModel::load_from_file(&*backend, &model_path, &model_params).map_err(|e| {
                    ModelError::BackendUnavailable(format!(
                        "gguf: failed to load {}: {e}",
                        model_path.display()
                    ))
                })?;

            let model_id = model_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "model".to_owned());

            tracing::info!(
                model = %model_id,
                n_ctx = opts.n_ctx,
                n_gpu_layers = opts.n_gpu_layers,
                "GgufBackend loaded"
            );

            Ok(Self {
                model: Arc::new(model),
                backend,
                opts,
                model_id,
            })
        }

        fn generate_blocking(
            model: Arc<LlamaModel>,
            backend: Arc<LlamaBackend>,
            opts: GgufOptions,
            prompt: String,
            sampling: SamplingParams,
            cancel: CancellationToken,
            tx: tokio::sync::mpsc::Sender<ModelResult<ChatEvent>>,
        ) {
            let n_ctx = match NonZeroU32::new(opts.n_ctx) {
                Some(n) => n,
                None => {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(
                        "gguf: n_ctx must be > 0".to_owned(),
                    )));
                    return;
                }
            };

            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(Some(n_ctx))
                .with_n_batch(opts.n_batch)
                .with_flash_attention_policy(opts.flash_attn as i32);

            let mut ctx = match model.new_context(&*backend, ctx_params) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(format!(
                        "gguf: context creation failed: {e}"
                    ))));
                    return;
                }
            };

            // Tokenise the prompt. AddBos::Never because the chat template
            // already contains the model's BOS token (Qwen uses <|im_start|>).
            let tokens = match model.str_to_token(&prompt, AddBos::Never) {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(format!(
                        "gguf: tokenization failed: {e}"
                    ))));
                    return;
                }
            };

            if tokens.is_empty() {
                let _ = tx.blocking_send(Ok(ChatEvent::Done {
                    input_tokens: 0,
                    output_tokens: 0,
                    stop_reason: StopReason::EndTurn,
                }));
                return;
            }

            let n_prompt = tokens.len();
            let mut batch = LlamaBatch::new(opts.n_batch as usize, 1);
            for (i, &token) in tokens.iter().enumerate() {
                let is_last = i == tokens.len() - 1;
                if batch.add(token, i as i32, &[0], is_last).is_err() {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(
                        "gguf: batch add failed during prefill".to_owned(),
                    )));
                    return;
                }
            }
            if ctx.decode(&mut batch).is_err() {
                let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(
                    "gguf: prefill decode failed".to_owned(),
                )));
                return;
            }

            let max_tokens = sampling.max_tokens.unwrap_or(2048);
            let temperature = sampling.temperature.unwrap_or(0.7);
            let mut n_cur = batch.n_tokens();
            let mut n_generated: u32 = 0;
            let mut parser = ToolUseParser::new();
            let eos = model.token_eos();

            loop {
                if cancel.is_cancelled() {
                    emit_flush(&mut parser, &tx);
                    let _ = tx.blocking_send(Ok(ChatEvent::Done {
                        input_tokens: n_prompt as u32,
                        output_tokens: n_generated,
                        stop_reason: StopReason::Cancelled,
                    }));
                    return;
                }

                if n_generated >= max_tokens {
                    emit_flush(&mut parser, &tx);
                    let _ = tx.blocking_send(Ok(ChatEvent::Done {
                        input_tokens: n_prompt as u32,
                        output_tokens: n_generated,
                        stop_reason: StopReason::MaxTokens,
                    }));
                    return;
                }

                // Sample the next token using raw logits (no sampler feature needed).
                let logits = ctx.get_logits_ith(n_cur - 1);
                let token = sample_token(logits, temperature);

                if token == eos || model.is_eog_token(token) {
                    emit_flush(&mut parser, &tx);
                    let _ = tx.blocking_send(Ok(ChatEvent::Done {
                        input_tokens: n_prompt as u32,
                        output_tokens: n_generated,
                        stop_reason: StopReason::EndTurn,
                    }));
                    return;
                }

                #[allow(deprecated)]
                let token_text = model
                    .token_to_str(token, Special::Tokenize)
                    .unwrap_or_default();

                // Feed to tool-use parser; emit resulting events.
                let events = parser.feed(&token_text);
                let mut tool_call_ended = false;
                for ev in events {
                    if emit_event(ev, &tx) {
                        tool_call_ended = true;
                    }
                }
                if tool_call_ended {
                    let _ = tx.blocking_send(Ok(ChatEvent::Done {
                        input_tokens: n_prompt as u32,
                        output_tokens: n_generated,
                        stop_reason: StopReason::ToolUse,
                    }));
                    return;
                }

                n_generated += 1;

                batch.clear();
                if batch.add(token, n_cur, &[0], true).is_err() {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(
                        "gguf: batch add failed during decode".to_owned(),
                    )));
                    return;
                }
                n_cur += 1;
                if ctx.decode(&mut batch).is_err() {
                    let _ = tx.blocking_send(Err(ModelError::BackendUnavailable(
                        "gguf: decode failed".to_owned(),
                    )));
                    return;
                }
            }
        }
    }

    /// Sample the next token from raw logits.
    ///
    /// Uses greedy decoding when `temperature ≤ 1e-6`, otherwise applies
    /// temperature scaling + softmax + categorical sampling with a simple LCG.
    fn sample_token(logits: &[f32], temperature: f32) -> LlamaToken {
        if logits.is_empty() {
            return LlamaToken(0);
        }
        if temperature <= 1e-6 {
            // Greedy
            let best = logits
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0);
            return LlamaToken(best as i32);
        }
        // Temperature + softmax
        let max_l = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let mut probs: Vec<f32> = logits
            .iter()
            .map(|&l| ((l - max_l) / temperature).exp())
            .collect();
        let sum: f32 = probs.iter().sum();
        if sum > 0.0 {
            probs.iter_mut().for_each(|p| *p /= sum);
        }
        // Simple counter-based PRNG (no external dep, deterministic between calls).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r_bits = c
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407)
            >> 32;
        let r = (r_bits as f32) / (u32::MAX as f32);
        let mut cumulative = 0.0f32;
        for (i, &p) in probs.iter().enumerate() {
            cumulative += p;
            if r <= cumulative {
                return LlamaToken(i as i32);
            }
        }
        LlamaToken((probs.len() - 1) as i32)
    }

    fn emit_flush(
        parser: &mut ToolUseParser,
        tx: &tokio::sync::mpsc::Sender<ModelResult<ChatEvent>>,
    ) {
        for ev in parser.flush() {
            emit_event(ev, tx);
        }
    }

    /// Emit a parser event over the channel. Returns `true` on `ToolUseEnd`.
    fn emit_event(
        event: ToolParseEvent,
        tx: &tokio::sync::mpsc::Sender<ModelResult<ChatEvent>>,
    ) -> bool {
        match event {
            ToolParseEvent::Text(text) => {
                if !text.is_empty() {
                    let _ = tx.blocking_send(Ok(ChatEvent::TextDelta { text }));
                }
                false
            }
            ToolParseEvent::Start { id, name } => {
                let _ = tx.blocking_send(Ok(ChatEvent::ToolUseStart { id, name }));
                false
            }
            ToolParseEvent::Delta { id, json_fragment } => {
                let _ = tx.blocking_send(Ok(ChatEvent::ToolUseDelta {
                    id,
                    input_json_delta: json_fragment,
                }));
                false
            }
            ToolParseEvent::End { id } => {
                let _ = tx.blocking_send(Ok(ChatEvent::ToolUseEnd { id }));
                true
            }
            ToolParseEvent::ParseError(msg) => {
                tracing::warn!(error = %msg, "gguf tool-use parser error");
                false
            }
        }
    }

    #[async_trait]
    impl ChatBackend for GgufBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let prompt = format_qwen_prompt(&req.messages, &req.tools);
            let model = Arc::clone(&self.model);
            let backend = Arc::clone(&self.backend);
            let opts = self.opts.clone();
            let sampling = req.sampling.clone();
            let cancel = req.cancel.clone();

            let (tx, rx) = tokio::sync::mpsc::channel::<ModelResult<ChatEvent>>(128);

            tokio::task::spawn_blocking(move || {
                GgufBackend::generate_blocking(model, backend, opts, prompt, sampling, cancel, tx);
            });

            Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)) as ChatStream)
        }

        fn context_window(&self) -> u32 {
            self.opts.n_ctx
        }

        fn label(&self) -> &'static str {
            "gguf"
        }
    }

    pub use GgufBackend as Export;
}

#[cfg(feature = "gguf")]
pub use inner::Export as GgufBackend;

// ─── Chat template formatting (always compiled) ───────────────────────────────

/// Format messages and optional tool definitions into a Qwen-style chat prompt.
///
/// Uses `<|im_start|>` / `<|im_end|>` tokens (Qwen / ChatML format).
/// For non-Qwen models, fall back to a simpler concatenation.
pub fn format_qwen_prompt(
    messages: &[crate::backend::Message],
    tools: &[crate::backend::ToolDef],
) -> String {
    let mut out = String::new();

    // System turn: inject tool definitions if any.
    let tool_json: Option<String> = if tools.is_empty() {
        None
    } else {
        let defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        Some(serde_json::to_string_pretty(&defs).unwrap_or_default())
    };

    // If there's no system message in the request, synthesise one for tools.
    let has_system = messages
        .iter()
        .any(|m| m.role == crate::backend::Role::System);
    if !has_system {
        if let Some(ref defs) = tool_json {
            out.push_str("<|im_start|>system\n");
            out.push_str("You are a helpful assistant.\n\n");
            out.push_str("You have access to the following tools:\n");
            out.push_str("<tools>\n");
            out.push_str(defs);
            out.push_str("\n</tools>\n\n");
            out.push_str(
                "To call a tool, respond with:\n\
                 <tool_call>\n\
                 {\"name\": \"<tool_name>\", \"arguments\": <args_json>}\n\
                 </tool_call>\n",
            );
            out.push_str("<|im_end|>\n");
        }
    }

    for msg in messages {
        let role_str = match msg.role {
            crate::backend::Role::System => "system",
            crate::backend::Role::User => "user",
            crate::backend::Role::Assistant => "assistant",
            crate::backend::Role::Tool => "tool",
        };
        out.push_str("<|im_start|>");
        out.push_str(role_str);
        out.push('\n');

        // For system messages, append tool defs if the model has tools.
        if msg.role == crate::backend::Role::System {
            out.push_str(&content_to_text(&msg.content));
            if let Some(ref defs) = tool_json {
                out.push_str("\n\nYou have access to the following tools:\n<tools>\n");
                out.push_str(defs);
                out.push_str("\n</tools>\n\nTo call a tool, use the following format:\n");
                out.push_str("<tool_call>\n{\"name\": \"<tool_name>\", \"arguments\": <args_json>}\n</tool_call>\n");
            }
        } else {
            out.push_str(&content_to_text(&msg.content));
        }

        out.push_str("<|im_end|>\n");
    }

    // Prompt the assistant to continue.
    out.push_str("<|im_start|>assistant\n");
    out
}

fn content_to_text(content: &[crate::backend::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            crate::backend::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── Path resolver tests ──

    #[test]
    fn resolve_absolute_path_that_exists() {
        // Create a real temp file.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("model.gguf");
        std::fs::write(&path, b"fake").unwrap();
        assert_eq!(resolve_model_path(path.to_str().unwrap()), Some(path));
    }

    #[test]
    fn resolve_absolute_path_missing_returns_none() {
        assert!(resolve_model_path("/nonexistent/path/model.gguf").is_none());
    }

    #[test]
    fn resolve_lmstudio_finds_q4_k_m() {
        let dir = TempDir::new().unwrap();
        // LMStudio layout: <home>/models/<repo>/*.gguf
        let model_dir = dir
            .path()
            .join("models")
            .join("Qwen")
            .join("Qwen2.5-Coder-32B-Instruct-GGUF");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join("model-Q2_K.gguf"), b"q2").unwrap();
        std::fs::write(model_dir.join("model-Q4_K_M.gguf"), b"q4").unwrap();

        // Point ROZUM_LMSTUDIO_HOME at our temp dir.
        // SAFETY: single-threaded test, no other threads read these vars.
        unsafe {
            std::env::set_var("ROZUM_LMSTUDIO_HOME", dir.path());
        }
        let result = resolve_lmstudio_model("Qwen/Qwen2.5-Coder-32B-Instruct-GGUF");
        unsafe {
            std::env::remove_var("ROZUM_LMSTUDIO_HOME");
        }

        assert!(result.is_some());
        let name = result
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.contains("Q4_K_M"), "expected Q4_K_M, got {name}");
    }

    #[test]
    fn resolve_lmstudio_missing_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("ROZUM_LMSTUDIO_HOME", dir.path());
        }
        let result = resolve_lmstudio_model("Nonexistent/Model");
        unsafe {
            std::env::remove_var("ROZUM_LMSTUDIO_HOME");
        }
        assert!(result.is_none());
    }

    #[test]
    fn resolve_ollama_blob_finds_cached_file() {
        // Set up a fake Ollama tree: manifests/.../qwen3.5/9b-mlx + blobs/sha256-…
        let dir = TempDir::new().unwrap();
        let models = dir.path().join("models");
        let manifest_dir = models
            .join("manifests")
            .join("registry.ollama.ai")
            .join("library")
            .join("qwen3.5");
        let blobs_dir = models.join("blobs");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(&blobs_dir).unwrap();

        let digest = "sha256:deadbeef";
        let blob_name = digest.replace(':', "-");
        std::fs::write(blobs_dir.join(&blob_name), b"fake").unwrap();
        let manifest = serde_json::json!({
            "layers": [
                { "mediaType": "application/vnd.ollama.image.model", "digest": digest }
            ]
        });
        std::fs::write(
            manifest_dir.join("9b-mlx"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        unsafe {
            std::env::set_var("ROZUM_OLLAMA_HOME", dir.path());
        }
        // The `ollama:` prefix is REQUIRED; a bare `name:tag` no longer resolves.
        let prefixed = resolve_model_path("ollama:qwen3.5:9b-mlx");
        let bare = resolve_model_path("qwen3.5:9b-mlx");
        let default_tag = resolve_model_path("ollama:qwen3.5"); // no `latest` tag present
        unsafe {
            std::env::remove_var("ROZUM_OLLAMA_HOME");
        }

        assert!(
            prefixed.is_some(),
            "expected to resolve ollama:qwen3.5:9b-mlx, got None"
        );
        assert!(prefixed.unwrap().file_name().unwrap() == blob_name.as_str());
        assert!(
            bare.is_none(),
            "a bare name:tag must NOT resolve to Ollama (prefix required)"
        );
        assert!(
            default_tag.is_none(),
            "should not resolve to default :latest when not present"
        );
    }

    // ── Tool-use parser tests ──

    #[test]
    fn parser_passes_plain_text_through() {
        let mut p = ToolUseParser::new();
        let mut events = p.feed("hello world");
        // Short strings are kept as lookahead; flush releases them.
        events.extend(p.flush());
        let text: String = events
            .iter()
            .filter_map(|e| {
                if let ToolParseEvent::Text(t) = e {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn parser_detects_complete_tool_call() {
        let mut p = ToolUseParser::new();
        let input = concat!(
            "<tool_call>\n",
            "{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Kyiv\"}}\n",
            "</tool_call>"
        );
        let events = p.feed(input);
        // Expect: Start, Delta, End (possibly preceded by empty Text)
        let has_start = events
            .iter()
            .any(|e| matches!(e, ToolParseEvent::Start { name, .. } if name == "get_weather"));
        let has_end = events
            .iter()
            .any(|e| matches!(e, ToolParseEvent::End { .. }));
        assert!(has_start, "expected Start event, got: {events:?}");
        assert!(has_end, "expected End event, got: {events:?}");
    }

    #[test]
    fn parser_handles_text_before_tool_call() {
        let mut p = ToolUseParser::new();
        let input = "Sure, let me check. <tool_call>\n{\"name\": \"search\", \"arguments\": {}}\n</tool_call>";
        let events = p.feed(input);
        let has_text_before = events
            .iter()
            .any(|e| matches!(e, ToolParseEvent::Text(t) if t.contains("Sure")));
        let has_start = events
            .iter()
            .any(|e| matches!(e, ToolParseEvent::Start { .. }));
        assert!(
            has_text_before,
            "expected text before tool call: {events:?}"
        );
        assert!(has_start, "expected Start event: {events:?}");
    }

    // ── Qwen chat template tests ──

    #[test]
    fn format_qwen_prompt_single_user_message() {
        use crate::backend::Message;
        let messages = vec![Message::user("Hello!")];
        let prompt = format_qwen_prompt(&messages, &[]);
        assert!(prompt.contains("<|im_start|>user\nHello!<|im_end|>"));
        assert!(prompt.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn format_qwen_prompt_injects_tool_defs_into_system() {
        use crate::backend::{Message, ToolDef};
        let messages = vec![Message::user("What's the weather?")];
        let tools = vec![ToolDef {
            name: "get_weather".to_owned(),
            description: "Get weather for a city".to_owned(),
            input_schema: serde_json::json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }];
        let prompt = format_qwen_prompt(&messages, &tools);
        assert!(
            prompt.contains("<tools>"),
            "expected <tools> block: {prompt}"
        );
        assert!(
            prompt.contains("get_weather"),
            "expected tool name: {prompt}"
        );
        assert!(
            prompt.contains("<|im_start|>system"),
            "expected system turn: {prompt}"
        );
    }
}
