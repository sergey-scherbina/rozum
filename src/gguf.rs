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

// ─── GgufBackend (feature = "gguf") ─────────────────────────────────────────

#[cfg(feature = "gguf")]
mod inner {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::{GgufOptions, format_qwen_prompt};
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

            let max_tokens = sampling.max_tokens.unwrap_or(2048) as usize;
            // Shared engine-agnostic sampler over the CPU logit slice (top-k/top-p/
            // repeat-penalty/seed) — see `crate::sampler`. Default temp 0.7 keeps GGUF's
            // historic behavior when the request omits it.
            let cfg = crate::sampler::SamplerConfig {
                temperature: sampling.temperature.unwrap_or(0.7),
                top_k: sampling.top_k.unwrap_or(0) as usize,
                top_p: sampling.top_p.unwrap_or(1.0),
                repeat_penalty: sampling.repeat_penalty.unwrap_or(1.0),
            };
            let eos = model.token_eos();

            // The token stream: sample from the current logits, stop on EOS/EOG, then
            // advance the model one step so the next logits are ready. The EOS/max-tokens/
            // cancel/runaway-guard + text & tool-call emission are owned by the shared
            // engine-SPI loop `crate::engine::consume_tokens` (one copy across MLX + GGUF).
            // Runs synchronously on this blocking thread, so the `!Send` `LlamaContext`
            // living in the closure is fine (we call `consume_tokens` directly, not the
            // `Send`-bounded `drive()`).
            let model_iter = Arc::clone(&model);
            let mut rng = crate::sampler::seeded_rng(sampling.seed);
            let mut recent: Vec<u32> = Vec::new();
            let mut n_cur = batch.n_tokens();
            // `get_logits_ith` indexes the LAST decoded batch, not the absolute position:
            // the prefill batch's last (only-logits) token is at `n_prompt-1`, but every
            // single-token decode batch has its token at index 0. Reading `n_cur-1` after
            // the first step read past the 1-token decode batch → garbage logits → an end
            // token sampled → generation stopped after one token (a pre-existing GGUF bug,
            // surfaced by the consume_tokens adoption's e2e). Track the right index instead.
            let mut logits_idx = n_cur - 1;
            let token_stream = std::iter::from_fn(move || -> Option<Result<u32, String>> {
                let id = crate::sampler::sample(
                    ctx.get_logits_ith(logits_idx),
                    &cfg,
                    crate::sampler::repeat_window(&recent),
                    &mut rng,
                );
                let token = LlamaToken(id as i32);
                if token == eos || model_iter.is_eog_token(token) {
                    return None; // end of generation → consume_tokens reports EndTurn
                }
                recent.push(id);
                // Advance: feed the sampled token so the next logits are ready.
                batch.clear();
                if batch.add(token, n_cur, &[0], true).is_err() {
                    return Some(Err("gguf: batch add failed during decode".to_owned()));
                }
                n_cur += 1;
                if ctx.decode(&mut batch).is_err() {
                    return Some(Err("gguf: decode failed".to_owned()));
                }
                logits_idx = 0; // the just-decoded batch holds exactly one token (index 0)
                Some(Ok(id))
            });

            // Detokenizer for `consume_tokens` (it diffs the running text to stream
            // deltas). Accumulate per-token `token_to_str` — byte-identical to GGUF's
            // prior text; `Special::Tokenize` keeps `<tool_call>` literal for the parser.
            let model_detok = Arc::clone(&model);
            let mut detok_acc = String::new();
            let mut detok_done = 0usize;
            let decode = move |ids: &[u32], _skip_special: bool| -> Option<String> {
                while detok_done < ids.len() {
                    let tok = LlamaToken(ids[detok_done] as i32);
                    #[allow(deprecated)]
                    let s = model_detok
                        .token_to_str(tok, Special::Tokenize)
                        .unwrap_or_default();
                    detok_acc.push_str(&s);
                    detok_done += 1;
                }
                Some(detok_acc.clone())
            };

            let meta = crate::engine::EngineMeta {
                n_ctx: opts.n_ctx,
                eos: Vec::new(), // EOS/EOG handled in the token iterator above
                model_type: String::new(),
                harmony: false, // GGUF serves Qwen-style `<tool_call>` models, not harmony
            };

            // `consume_tokens` streams TextDelta / ToolUse* / Done itself (incl. parsing
            // tool calls at finalize via `crate::serving::parse_tool_calls`, with
            // cross-turn-unique ids). `emit` returns false when the client dropped the
            // stream, so a vanished receiver stops generation early.
            crate::engine::consume_tokens(
                token_stream,
                &meta,
                n_prompt,
                max_tokens,
                true, // runaway-loop guard (parity with the MLX path)
                &cancel,
                decode,
                |ev| tx.blocking_send(ev).is_ok(),
            );
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

/// Register the in-process GGUF engine constructor with `rozum-core`'s backend
/// registry (inversion of control for the workspace split — core never depends on
/// this engine). Called once at binary startup. Behaviour matches the old
/// in-`backend.rs` `add_gguf_backend`: resolve the spec/path, build a
/// `GgufBackend`, fall back to a placeholder (return `None`) on any failure.
#[cfg(feature = "gguf")]
pub fn register_engine() {
    crate::backend::register_gguf_engine(|config| {
        let spec = config.model_spec.clone().or_else(|| {
            config
                .model_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
        });
        let model_path = match spec.as_deref().and_then(resolve_model_path) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    id = %config.id,
                    spec = ?spec,
                    "gguf backend: could not resolve model path; using placeholder. \
                     Specify an absolute path or a 'lmstudio:' / 'ollama:' prefix."
                );
                return None;
            }
        };
        match GgufBackend::new(model_path, GgufOptions::default()) {
            Ok(b) => Some(std::sync::Arc::new(b) as std::sync::Arc<dyn crate::backend::ChatBackend>),
            Err(e) => {
                tracing::warn!(id = %config.id, error = %e, "gguf backend: load failed; using placeholder");
                None
            }
        }
    });
}

/// No-op when the `gguf` feature is off — `BackendEngine::Gguf` then resolves to a
/// placeholder, exactly as before.
#[cfg(not(feature = "gguf"))]
pub fn register_engine() {}

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

    // Tool-call parsing + cross-turn-unique ids are now covered by the shared engine
    // loop (`crate::engine::consume_tokens` tests + `crate::serving::parse_tool_calls`
    // + `crate::engine::next_tool_call_id`), since GGUF routes through `consume_tokens`.

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
