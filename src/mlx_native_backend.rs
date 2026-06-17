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
    use mlx_lm::models::{gemma3, gpt_oss, llama, qwen2, qwen3, qwen3_5, qwen3_5_moe, qwen3_moe};
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
    /// Hard ceiling on generated tokens regardless of what the client asks for, so
    /// one runaway generation can't tie up the cap-1 worker for many minutes. A
    /// coding agent's single turn rarely needs more; override via
    /// `ROZUM_MAX_OUTPUT_TOKENS` (0 disables the cap). Backstop to the repetition
    /// guard below — that catches the common case (a loop) far sooner.
    const DEFAULT_OUTPUT_CEILING: usize = 8192;
    // Runaway-loop guard is shared: `crate::engine::is_runaway_loop`.
    /// Qwen3 `<|im_end|>`, used when the checkpoint config omits `eos_token_id`.
    const QWEN3_EOS: u32 = 151645;
    /// Fallback context window when config lacks `max_position_embeddings`.
    const DEFAULT_N_CTX: u32 = 32_768;

    /// Count of batched-decode runs (`run_batch` calls with ≥2 rows). Lets a test
    /// prove the scheduler actually batched concurrent requests rather than quietly
    /// falling back to serial. Process-global; only read in tests.
    pub(crate) static BATCH_RUN_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Count of rows ADMITTED into a live batch mid-decode (continuous batching). Lets a
    /// test prove a queued job was pulled into a freed slot rather than run serially after.
    pub(crate) static BATCH_ADMIT_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Total rows ever served via batched decode (initial batch members + mid-decode admits).
    /// `BATCH_ROWS_TOTAL / BATCH_RUN_COUNT` ≈ average batch occupancy. Observability only.
    pub(crate) static BATCH_ROWS_TOTAL: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    /// Peak rows in a single batch seen so far (high-water mark). Observability only.
    pub(crate) static BATCH_MAX: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Record `added` new rows entering a batch and the resulting `peak` occupancy into the
    /// global counters (initial assembly: `added == peak == B`; a mid-decode admit:
    /// `added == 1`, `peak ==` the batch size after the admit).
    fn note_batch_rows(added: usize, peak: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        BATCH_ROWS_TOTAL.fetch_add(added, Relaxed);
        BATCH_MAX.fetch_max(peak, Relaxed);
    }

    /// Hybrid (GatedDeltaNet) archs that need MLX retained command-buffer refs
    /// (`ROZUM_MLX_RETAIN`) for correctness — the custom kernel's input buffer is
    /// otherwise freed before the in-flight GPU dispatch reads it (docs/mlx-gd-bug/).
    /// This is also the bf16/retain fast decode path; keep this list in sync with the
    /// `qwen3_5` / `qwen3_5_moe` loaders or the +2.7× decode win silently regresses.
    pub(crate) fn is_hybrid_model(model_type: &str) -> bool {
        matches!(
            model_type,
            "qwen3_5" | "qwen3_5_text" | "qwen3_5_moe" | "qwen3_5_moe_text"
        )
    }

    /// Set/clear `ROZUM_MLX_RETAIN` for `model_type` (hybrid → retained refs; dense →
    /// the faster unretained path). Must run before the worker thread's first MLX op.
    fn apply_retain_env(model_type: &str) {
        // SAFETY: single-threaded MLX worker; see `LoadedModel::load`.
        unsafe {
            if is_hybrid_model(model_type) {
                std::env::set_var("ROZUM_MLX_RETAIN", "1");
            } else {
                std::env::remove_var("ROZUM_MLX_RETAIN");
            }
        }
    }

    /// A loaded MLX model, dispatched by `config.json`'s `model_type`. Each
    /// variant exposes the same `Generate` token iterator, so the streaming
    /// loop is architecture-agnostic.
    enum LoadedModel {
        Qwen3(qwen3::Model),
        Qwen3Moe(qwen3_moe::Model),
        Qwen35(qwen3_5::Model),
        Qwen35Moe(qwen3_5_moe::Model),
        Llama(llama::Model),
        Qwen2(qwen2::Model),
        Gemma3(gemma3::Model),
        GptOss(gpt_oss::Model),
    }

    impl LoadedModel {
        fn load(model_type: &str, dir: &Path) -> Result<Self, String> {
            // Hybrid models (Qwen3.6 GatedDeltaNet custom kernel) are only correct
            // without a per-call eval when MLX command buffers RETAIN their referenced
            // buffers (otherwise an upstream kernel-input buffer is freed before the
            // in-flight GPU dispatch reads it — see docs/mlx-gd-bug/). Enable retained
            // refs BEFORE the first MLX op (model load), so MLX's command-buffer
            // creation reads it; this drops ~48 syncs/token (~12 -> ~16-17 t/s).
            // Dense models keep the faster unretained path.
            // SAFETY: the native backend runs all MLX work on one dedicated worker
            // thread; set/clear before that thread touches MLX. Cleared for dense so a
            // gateway dense<->hybrid switch in the same process stays correct (the MLX
            // patch reads the env per command buffer).
            apply_retain_env(model_type);
            match model_type {
                "qwen3" => qwen3::load_qwen3_model(dir)
                    .map(LoadedModel::Qwen3)
                    .map_err(|e| format!("mlx: load qwen3 {}: {e}", dir.display())),
                "qwen3_moe" => qwen3_moe::load_qwen3_moe_model(dir)
                    .map(LoadedModel::Qwen3Moe)
                    .map_err(|e| format!("mlx: load qwen3_moe {}: {e}", dir.display())),
                // OpenAI gpt-oss (MXFP4 MoE + attention sinks + sliding-window + YaRN).
                "gpt_oss" => gpt_oss::load_gpt_oss_model(dir)
                    .map(LoadedModel::GptOss)
                    .map_err(|e| format!("mlx: load gpt_oss {}: {e}", dir.display())),
                // Qwen3.6 dense (the config wrapper is `qwen3_5`, text is `qwen3_5_text`).
                "qwen3_5" | "qwen3_5_text" => qwen3_5::load_qwen3_5_model(dir)
                    .map(LoadedModel::Qwen35)
                    .map_err(|e| format!("mlx: load qwen3_5 {}: {e}", dir.display())),
                // Qwen3.6 MoE (wrapper `qwen3_5_moe`, text `qwen3_5_moe_text`).
                "qwen3_5_moe" | "qwen3_5_moe_text" => qwen3_5_moe::load_qwen3_5_moe_model(dir)
                    .map(LoadedModel::Qwen35Moe)
                    .map_err(|e| format!("mlx: load qwen3_5_moe {}: {e}", dir.display())),
                // Llama family (Llama 3.x, and other `model_type: llama` checkpoints), plus
                // Mistral / Mistral-Nemo (`model_type: "mistral"`) — architecturally Llama
                // (GQA, no qkv-bias, SwiGLU, RoPE) and served by the *llama* class upstream in
                // `mlx_lm`. The only delta is Mistral's sliding-window attention; the llama
                // path runs full attention, so it matches the reference except for contexts
                // beyond the window (4096), which the KV preflight already bounds.
                "llama" | "mistral" => llama::load_llama_model(dir)
                    .map(LoadedModel::Llama)
                    .map_err(|e| format!("mlx: load llama/mistral {}: {e}", dir.display())),
                // Phi-3 (`model_type: "phi3"`) — the Llama arch with FUSED qkv / gate_up
                // projections; the loader splits them and returns a `llama::Model`, so it runs
                // on the existing Llama path (Generate, batched decode, etc.).
                "phi3" => llama::load_phi3_model(dir)
                    .map(LoadedModel::Llama)
                    .map_err(|e| format!("mlx: load phi3 {}: {e}", dir.display())),
                // Gemma 3 (text). The wrapper config maps `text_config.model_type`, so both the
                // text-only `gemma3_text` and the multimodal `gemma3` wrapper land here.
                "gemma3_text" | "gemma3" => gemma3::load_gemma3_model(dir)
                    .map(LoadedModel::Gemma3)
                    .map_err(|e| format!("mlx: load gemma3 {}: {e}", dir.display())),
                // Qwen2 / Qwen2.5 / Qwen2.5-Coder (dense; qkv-bias, no q/k-norm).
                "qwen2" => qwen2::load_qwen2_model(dir)
                    .map(LoadedModel::Qwen2)
                    .map_err(|e| format!("mlx: load qwen2 {}: {e}", dir.display())),
                other => Err(format!("mlx: unsupported model_type '{other}'")),
            }
        }
    }

    /// One inference request handed to the worker thread. All fields are `Send`;
    /// the `!Send` MLX work happens entirely on the worker.
    struct Job {
        messages: Vec<Message>,
        tools: Vec<crate::backend::ToolDef>,
        sampling: SamplingParams,
        model_id: String,
        cancel: tokio_util::sync::CancellationToken,
        events: mpsc::UnboundedSender<ModelResult<ChatEvent>>,
    }

    /// Fraction of currently-available RAM we let the KV cache occupy, leaving
    /// slack for weights, activations and the OS.
    const KV_SAFETY_FRAC: f64 = 0.75;
    /// KV cache element size (bf16 compute dtype).
    const KV_DTYPE_BYTES: u64 = 2;

    /// Bytes the KV cache grows per context position: `2 (k+v) * full_attn_layers
    /// * n_kv_heads * head_dim * dtype`. Only the full-attention layers hold KV;
    /// GatedDeltaNet conv/recurrent state is O(1) in context. Reads `text_config`
    /// (the multimodal hybrid wrapper) if present, else the top level. `None` if
    /// the config lacks the needed fields.
    pub(crate) fn kv_bytes_per_position(cfg: &serde_json::Value) -> Option<u64> {
        let c = cfg.get("text_config").unwrap_or(cfg);
        let n_layers = c.get("num_hidden_layers")?.as_u64()?;
        // Hybrid: every `full_attention_interval`-th layer is full attention.
        // Dense models omit it -> all layers hold KV.
        let interval = c
            .get("full_attention_interval")
            .and_then(|v| v.as_u64())
            .filter(|&i| i > 0)
            .unwrap_or(1);
        let full_attn_layers = if interval > 1 {
            n_layers / interval
        } else {
            n_layers
        };
        let n_kv = c.get("num_key_value_heads")?.as_u64()?;
        let head_dim = c
            .get("head_dim")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                let hidden = c.get("hidden_size")?.as_u64()?;
                let heads = c.get("num_attention_heads")?.as_u64()?;
                (heads > 0).then(|| hidden / heads)
            })?;
        Some(2 * full_attn_layers * n_kv * head_dim * KV_DTYPE_BYTES)
    }

    /// RAM available right now (macOS `vm_stat`: free + inactive + speculative +
    /// purgeable pages). `None` if it can't be read.
    fn available_ram_bytes() -> Option<u64> {
        let out = std::process::Command::new("vm_stat").output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let page_size = s
            .lines()
            .next()
            .and_then(|l| l.split("page size of ").nth(1))
            .and_then(|r| r.split(' ').next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(16384);
        let mut pages = 0u64;
        for line in s.lines() {
            for label in [
                "Pages free:",
                "Pages inactive:",
                "Pages speculative:",
                "Pages purgeable:",
            ] {
                if let Some(rest) = line.strip_prefix(label) {
                    if let Ok(v) = rest.trim().trim_end_matches('.').parse::<u64>() {
                        pages += v;
                    }
                }
            }
        }
        (pages > 0).then_some(pages * page_size)
    }

    /// Minimal slice of `config.json` we read on the calling thread (plain JSON,
    /// no MLX), so the worker only ever touches the `!Send` model. The last field
    /// is the KV bytes-per-position for the large-context preflight.
    fn read_config(dir: &Path) -> (u32, Vec<u32>, String, Option<u64>) {
        let mut n_ctx = DEFAULT_N_CTX;
        let mut eos: Vec<u32> = Vec::new();
        let mut model_type = "qwen3".to_string();
        let mut kv_per_pos = None;
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
                kv_per_pos = kv_bytes_per_position(&v);
            }
        }
        if eos.is_empty() {
            eos.push(QWEN3_EOS);
        }
        // gpt-oss (harmony) also stops on <|call|> 200012 (a tool call) and
        // <|endoftext|> 199999, per generation_config.json's eos list; config.json
        // lists only <|return|> 200002, so without these a tool call never stops
        // and the model hallucinates the tool's result + a final answer.
        if model_type == "gpt_oss" {
            for id in [199999u32, 200012] {
                if !eos.contains(&id) {
                    eos.push(id);
                }
            }
        }
        (n_ctx, eos, model_type, kv_per_pos)
    }

    pub struct MlxNativeBackend {
        /// `Option` so [`Drop`] can close the channel (drop the sender) BEFORE joining the
        /// worker — otherwise the live sender keeps `blocking_recv` parked and join deadlocks.
        jobs: Option<mpsc::UnboundedSender<Job>>,
        /// The worker thread, joined on drop so the model's MLX buffers are fully freed
        /// before this returns (clean unload/swap; no teardown↔next-load race on the shared
        /// Metal context).
        worker: Option<thread::JoinHandle<()>>,
        model_id: String,
        n_ctx: u32,
    }

    impl Drop for MlxNativeBackend {
        fn drop(&mut self) {
            // Close the job channel so the worker's `blocking_recv` returns `None` and it
            // exits its loop + drops the model, THEN join it. Without the join, ~8-15 GB of
            // MLX buffers free asynchronously and race a subsequent model load on the
            // single-stream Metal context (corruption / SIGSEGV) — and an unload wouldn't
            // deterministically reclaim the RAM. Joining blocks until teardown completes.
            drop(self.jobs.take());
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
        }
    }

    impl MlxNativeBackend {
        /// `model_dir` is a local directory of safetensors + tokenizer.json +
        /// tokenizer_config.json (the layout HuggingFace / mlx-community ship).
        /// `model_id` selects the chat-template variant and labels logs.
        /// `max_ctx` caps the declared context window (the model's own max from config is
        /// the ceiling): `Some(n)` honors a user `--n-ctx` (the window becomes
        /// `min(model_max, n)`); `None` uses the model's full max. The KV cache grows lazily
        /// per token + a per-request RAM preflight guards memory, so the full max is safe.
        pub async fn new(
            model_dir: PathBuf,
            model_id: String,
            max_ctx: Option<u32>,
        ) -> ModelResult<Self> {
            cap_mlx_memory();
            let (mut n_ctx, eos, model_type, kv_per_pos) = read_config(&model_dir);
            if let Some(cap) = max_ctx {
                n_ctx = n_ctx.min(cap);
            }
            let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
            let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
            let label = model_id.clone();

            // The worker loads the model on its own thread and owns it for life;
            // the `!Send` model never crosses a thread boundary. Keep the handle so the
            // backend can join it on drop (clean teardown).
            let worker = thread::Builder::new()
                .name("mlx-native".into())
                .spawn(move || {
                    worker_main(model_dir, model_type, eos, kv_per_pos, jobs_rx, ready_tx)
                })
                .map_err(|e| ModelError::BackendUnavailable(format!("mlx: spawn worker: {e}")))?;

            match ready_rx.await {
                Ok(Ok(())) => {
                    eprintln!("mlx-native: '{label}' ready (context {n_ctx})");
                    Ok(Self {
                        jobs: Some(jobs_tx),
                        worker: Some(worker),
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

    /// Cap MLX's unified-memory use so a resident model doesn't hoard all of RAM and
    /// starve the agent / other processes on the host. `set_cache_limit` is the key
    /// lever: MLX otherwise keeps freed Metal buffers cached, so the footprint grows to
    /// a RAM fraction (~28 GB observed) regardless of model size; capping it returns
    /// those buffers to the OS, keeping the footprint near the live (weights + KV)
    /// memory. `ROZUM_MLX_CACHE_GB` (default 4) and `ROZUM_MLX_MEM_GB` (default total
    /// RAM − 8) override; `0` for either disables that cap. Process-global, idempotent.
    fn cap_mlx_memory() {
        let gb = |g: u64| -> usize { (g as usize).saturating_mul(1usize << 30) };
        let total_gb = crate::concurrency::total_ram_bytes().unwrap_or(16u64 << 30) / (1u64 << 30);
        let cache_gb = std::env::var("ROZUM_MLX_CACHE_GB")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(4);
        let mem_gb = std::env::var("ROZUM_MLX_MEM_GB")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or_else(|| total_gb.saturating_sub(8).max(8));
        if mem_gb > 0 {
            mlx_rs::memory::set_memory_limit(gb(mem_gb));
        }
        if cache_gb > 0 {
            mlx_rs::memory::set_cache_limit(gb(cache_gb));
        }
        if std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
            eprintln!("mlx-native: memory cap mem={mem_gb}GB cache={cache_gb}GB (total {total_gb}GB)");
        }
    }

    #[async_trait]
    impl ChatBackend for MlxNativeBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let (events_tx, events_rx) = mpsc::unbounded_channel();
            let job = Job {
                messages: req.messages,
                tools: req.tools,
                sampling: req.sampling,
                model_id: self.model_id.clone(),
                cancel: req.cancel,
                events: events_tx,
            };
            self.jobs
                .as_ref()
                .ok_or_else(|| ModelError::BackendUnavailable("mlx: worker shut down".into()))?
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
            // `ROZUM_BATCH=N` (default 1) admits up to N concurrent requests so the
            // worker can batch them (continuous batched decode, dense Qwen3 + greedy);
            // 1 = the proven serial path. Non-batchable requests still run serially.
            Some(batch_cap())
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

    /// Flatten a message's blocks into the string the chat template renders.
    /// Assistant `ToolUse` blocks are rendered back as Qwen3 `<tool_call>{…}</tool_call>`
    /// markup (the inverse of `parse_tool_calls`) so a prior assistant tool call
    /// survives in multi-turn history — without it, a tool-result follow-up has no
    /// preceding call and the model loses the trained tool-loop format. `ToolResult`
    /// blocks pass their content through (rendered under the `tool` role).
    pub(crate) fn message_text(msg: &Message) -> String {
        let mut out = String::new();
        for b in &msg.content {
            match b {
                ContentBlock::Text { text } => out.push_str(text),
                ContentBlock::ToolResult { content, .. } => out.push_str(content),
                ContentBlock::ToolUse { name, input, .. } => {
                    let call = serde_json::json!({ "name": name, "arguments": input });
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("<tool_call>\n");
                    out.push_str(&call.to_string());
                    out.push_str("\n</tool_call>");
                }
            }
        }
        out
    }

    /// Render a message for the gpt-oss **harmony** template: assistant `ToolUse`
    /// blocks become structured `tool_calls` (so the template emits native
    /// `commentary to=functions.X` markup and a later `tool` result has a matching
    /// preceding call — else it `raise_exception`s); `Text`/`ToolResult` pass
    /// through as content. The template does `tool_call.arguments|tojson`, so the
    /// arguments are passed as the parsed object, not a JSON string.
    fn harmony_conversation(msg: &Message) -> Conversation<&'static str, String> {
        let mut text = String::new();
        let mut calls: Vec<serde_json::Value> = Vec::new();
        for b in &msg.content {
            match b {
                ContentBlock::Text { text: t } => text.push_str(t),
                ContentBlock::ToolResult { content, .. } => text.push_str(content),
                ContentBlock::ToolUse { name, input, .. } => {
                    calls.push(serde_json::json!({
                        "type": "function",
                        "function": { "name": name, "arguments": input },
                    }));
                }
            }
        }
        Conversation {
            role: role_str(&msg.role),
            content: text,
            tool_calls: (!calls.is_empty()).then(|| serde_json::Value::Array(calls)),
        }
    }

    /// Worker thread entry point. Loads the model (reporting load result over
    /// `ready`), then serves jobs until the queue closes.
    fn worker_main(
        model_dir: PathBuf,
        model_type: String,
        mut eos: Vec<u32>,
        kv_per_pos: Option<u64>,
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
        // Chat turn-end tokens that end an assistant turn but aren't in the raw config
        // `eos_token_id` (Gemma's instruct models emit `<end_of_turn>` (106), but config eos
        // is only `<eos>` (1) → the model over-runs into garbage past its answer). Add any
        // such token the tokenizer knows; harmless for models without it (None).
        for t in ["<end_of_turn>"] {
            if let Some(id) = tokenizer.token_to_id(t) {
                if !eos.contains(&id) {
                    eos.push(id);
                }
            }
        }
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
        // Expose the model's BOS token to templates that emit it themselves (Gemma).
        MODEL_BOS_TOKEN
            .with(|c| *c.borrow_mut() = read_bos_token(&model_dir.join("tokenizer_config.json")));
        if ready.send(Ok(())).is_err() {
            return; // caller gave up before load finished
        }

        // `blocking_recv` is correct here: this is a plain OS thread with no
        // tokio runtime, so it parks until the next job (or the queue closes).
        // `prefix` carries the previous dense request's KV across jobs for prefix
        // reuse (the worker is cap-1 serial, so no locking is needed).
        let mut store = PrefixStore::new();
        let cap = batch_cap();
        let batchable_arch = is_batchable_arch(&model);
        while let Some(first) = jobs.blocking_recv() {
            // Fast path: batching off, or this model's arch isn't batchable → serial
            // (keeps the prefix-KV LRU).
            if cap <= 1 || !batchable_arch {
                run_job(&mut model, &mut tokenizer, &template, &eos, kv_per_pos, &mut store, first);
                continue;
            }
            // Gather more already-admitted jobs (up to `cap`), waiting a small window
            // (`ROZUM_BATCH_WINDOW_MS`, default 10) for near-simultaneous requests to
            // arrive so they batch together instead of racing the worker.
            let mut pending = vec![first];
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(batch_window_ms());
            while pending.len() < cap {
                match jobs.try_recv() {
                    Ok(j) => pending.push(j),
                    Err(_) => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
            // Batch the batchable ones together (per-row sampling); run the rest serially.
            // The batch runs CONTINUOUSLY — it keeps pulling more batchable jobs from `jobs`
            // into freed slots and returns any it can't batch (`returned`) to run serially
            // alongside `other`. A lone batchable job stays serial (keeps prefix-KV).
            let (batchable, mut serial): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(|j| is_batchable(j));
            if batchable.len() >= 2 {
                let returned = if is_hybrid_arch(&model) {
                    run_batch_hybrid(
                        &mut model, &mut tokenizer, &template, &eos, batchable, &mut jobs, cap,
                    )
                } else {
                    run_batch(
                        &mut model, &mut tokenizer, &template, &eos, batchable, &mut jobs, cap,
                    )
                };
                serial.extend(returned);
            } else {
                serial.extend(batchable);
            }
            for job in serial {
                run_job(&mut model, &mut tokenizer, &template, &eos, kv_per_pos, &mut store, job);
            }
        }
    }

    /// Batched-decode capacity from `ROZUM_BATCH` (default 1 = serial).
    fn batch_cap() -> usize {
        std::env::var("ROZUM_BATCH")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .max(1)
    }

    /// How long (ms) the worker waits for near-simultaneous requests to fill a
    /// batch before starting decode. `ROZUM_BATCH_WINDOW_MS` (default 10).
    fn batch_window_ms() -> u64 {
        std::env::var("ROZUM_BATCH_WINDOW_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(10)
    }

    /// Arches that support batched decode. Dense Qwen3 / Qwen3-MoE use `qwen3::Attention`'s
    /// per-row-rope path; hybrid Qwen3.6 (`Qwen35`/`Qwen35Moe`) batches its full-attention
    /// layers the same way and STACKS the fixed-size GatedDeltaNet state per row. (Llama /
    /// Qwen2 have their own attention with no per-row-rope path yet.)
    fn is_batchable_arch(model: &LoadedModel) -> bool {
        matches!(
            model,
            LoadedModel::Qwen3(_)
                | LoadedModel::Qwen3Moe(_)
                | LoadedModel::Qwen35(_)
                | LoadedModel::Qwen35Moe(_)
                | LoadedModel::Llama(_)
                | LoadedModel::Qwen2(_)
                | LoadedModel::Gemma3(_)
        )
    }

    /// Hybrid (Qwen3.6) arches — routed to [`run_batch_hybrid`] (heterogeneous cache)
    /// instead of the dense [`run_batch`].
    fn is_hybrid_arch(model: &LoadedModel) -> bool {
        matches!(model, LoadedModel::Qwen35(_) | LoadedModel::Qwen35Moe(_))
    }

    /// A request joins a batch if it needs neither a repetition penalty nor a fixed seed —
    /// `qwen3::sample_rows` does per-row temp/top_k/top_p (greedy = temp 0 → argmax override),
    /// so temperature/top-p/top-k requests batch too. Repetition penalty (needs each row's
    /// history scattered into the logits) and explicit seeds (need per-row RNG keys) are not
    /// supported in the batched path yet, so those rows run serially.
    fn is_batchable(job: &Job) -> bool {
        let s = &job.sampling;
        // Constrained tool jobs need the B=1 masked loop (`run_constrained_dense`), so keep
        // them out of the batched path.
        let constrained = constrain_enabled() && !job.tools.is_empty();
        !constrained && s.repeat_penalty.unwrap_or(1.0) == 1.0 && s.seed.is_none()
    }

    /// Per-row sampling params `(temp, top_k, top_p)` for `qwen3::sample_rows`, with the
    /// runtime defaults (temp 0 = greedy, no top-k/top-p filtering).
    fn sampling_of(job: &Job) -> (f32, i32, f32) {
        let s = &job.sampling;
        (
            s.temperature.unwrap_or(0.0),
            s.top_k.unwrap_or(0) as i32,
            s.top_p.unwrap_or(1.0),
        )
    }

    /// Sample one token per row of `[B, vocab]` logits, each row honoring its own
    /// `(temp, top_k, top_p)`. Returns `B` token ids. The batched analog of the serial
    /// path's `qwen3::sample_with`.
    fn sample_rows_vec(logits: &Array, temps: &[f32], topks: &[i32], topps: &[f32]) -> Vec<u32> {
        use mlx_rs::ops::indexing::IndexOp;
        let n = temps.len() as i32;
        let toks = qwen3::sample_rows(
            logits,
            &Array::from_slice(temps, &[n]),
            &Array::from_slice(topks, &[n]),
            &Array::from_slice(topps, &[n]),
        )
        .expect("sample_rows");
        let _ = mlx_rs::transforms::eval([&toks]);
        (0..temps.len()).map(|r| toks.index(r as i32).item::<u32>()).collect()
    }

    /// Persisted dense KV cache from the previous request on this worker, for
    /// prefix reuse. `ids` is the prompt the cache represents (its KV covers
    /// `[0, ids.len())`); when the next prompt extends `ids`, the cache is
    /// truncated to that length and only the new suffix is prefilled.
    struct PrefixCache {
        ids: Vec<u32>,
        cache: Vec<Option<ConcatKeyValueCache>>,
    }

    /// Persisted hybrid (Qwen3.6) cache from the previous request, for prefix reuse.
    /// `cache` is the live heterogeneous cache (advanced past `ids` by generation);
    /// `snap` is the per-layer `Linear` recurrent state snapshotted at the END of the
    /// previous prefill (offset == `ids.len()`). On reuse the `Full` layers are
    /// truncated to `ids.len()` and the `Linear` layers restored from `snap`.
    struct HybridPrefix {
        ids: Vec<u32>,
        cache: Vec<qwen3_5::LayerCache>,
        snap: Vec<qwen3_5::LinearSnap>,
    }

    /// Default number of prefix-cache slots (distinct conversations whose KV is kept
    /// resident). 1 covers a single agent; >1 lets *interleaved* sessions (several
    /// room agents, or Claude Code + Codex at once) each keep their prefix instead of
    /// thrashing one slot. Each slot holds a conversation's KV, so it costs memory —
    /// override via `ROZUM_PREFIX_CACHE_SLOTS` (lower it for very long contexts).
    const DEFAULT_PREFIX_SLOTS: usize = 4;

    /// Small LRU of prefix caches on the (cap-1) worker, so concurrent/interleaved
    /// conversations each reuse their own KV. A worker serves one model, so only one
    /// of `dense`/`hybrid` is ever populated. Front = most-recently-used.
    pub(crate) struct PrefixStore {
        dense: Vec<PrefixCache>,
        hybrid: Vec<HybridPrefix>,
        cap: usize,
    }

    impl PrefixStore {
        fn new() -> Self {
            let cap = std::env::var("ROZUM_PREFIX_CACHE_SLOTS")
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(DEFAULT_PREFIX_SLOTS)
                .max(1);
            Self { dense: Vec::new(), hybrid: Vec::new(), cap }
        }

        /// Index of the entry whose `ids` is the LONGEST strict prefix of `ids` (the
        /// conversation this request extends), among entries — or `None`.
        pub(crate) fn best_match<T>(
            entries: &[T],
            ids: &[u32],
            get: impl Fn(&T) -> &[u32],
        ) -> Option<usize> {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    let eids = get(e);
                    eids.len() < ids.len() && ids.starts_with(eids)
                })
                .max_by_key(|(_, e)| get(e).len())
                .map(|(i, _)| i)
        }

        /// Take the dense entry this prompt extends (removed; the advanced cache is
        /// re-inserted by `put_dense` after generation). Returns `(reuse_len, cache)`.
        fn take_dense(&mut self, ids: &[u32]) -> Option<(usize, Vec<Option<ConcatKeyValueCache>>)> {
            let i = Self::best_match(&self.dense, ids, |e| &e.ids)?;
            let e = self.dense.remove(i);
            Some((e.ids.len(), e.cache))
        }

        fn put_dense(&mut self, ids: Vec<u32>, cache: Vec<Option<ConcatKeyValueCache>>) {
            self.dense.insert(0, PrefixCache { ids, cache });
            self.dense.truncate(self.cap);
        }

        /// Take the hybrid entry this prompt extends. Returns `(reuse_len, cache, snap)`.
        fn take_hybrid(
            &mut self,
            ids: &[u32],
        ) -> Option<(usize, Vec<qwen3_5::LayerCache>, Vec<qwen3_5::LinearSnap>)> {
            let i = Self::best_match(&self.hybrid, ids, |e| &e.ids)?;
            let e = self.hybrid.remove(i);
            Some((e.ids.len(), e.cache, e.snap))
        }

        fn put_hybrid(
            &mut self,
            ids: Vec<u32>,
            cache: Vec<qwen3_5::LayerCache>,
            snap: Vec<qwen3_5::LinearSnap>,
        ) {
            self.hybrid.insert(0, HybridPrefix { ids, cache, snap });
            self.hybrid.truncate(self.cap);
        }
    }

    /// Dense arches own their KV cache externally (`Vec<Option<ConcatKeyValueCache>>`)
    /// and support prefix reuse via plain truncation. Hybrid (Qwen3.6) owns its cache
    /// internally and carries a non-truncatable GatedDeltaNet recurrent state, so it
    /// reuses via truncate-`Full` + restore-`Linear`-from-snapshot (`HybridPrefix`).
    fn is_dense(model: &LoadedModel) -> bool {
        matches!(
            model,
            LoadedModel::Qwen3(_)
                | LoadedModel::Qwen3Moe(_)
                | LoadedModel::Llama(_)
                | LoadedModel::Qwen2(_)
        )
    }

    // Process-unique tool-call id is shared: `crate::engine::next_tool_call_id`.

    /// Render the prompt and stream token events. Dispatches on the model
    /// architecture; each `Generate` iterator feeds the shared streaming loop.
    fn run_job(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        kv_per_pos: Option<u64>,
        store: &mut PrefixStore,
        job: Job,
    ) {
        // Schema-constrained decode: a separate B=1 loop that masks the sampler. Hybrid Qwen3.6
        // takes the `LayerCache` path; every dense arch takes the KV-cache path. Two triggers:
        //   1. tool-call constraining — ON by default (opt out with `ROZUM_MLX_CONSTRAIN=0`).
        //   2. `response_format` / structured output — ALWAYS honored when the client asks
        //      (it's an explicit correctness request, not an opt-in).
        if should_constrain(&job, model) {
            let driver = ToolConstraint::from_job(&job).expect("constrained job has tools");
            return if is_hybrid_arch(model) {
                run_constrained_hybrid(model, tokenizer, template, eos, job, driver)
            } else {
                run_constrained_dense(model, tokenizer, template, eos, job, driver)
            };
        }
        if (is_dense(model) || is_hybrid_arch(model)) && job.sampling.response_schema.is_some() {
            let driver = ResponseConstraint::from_job(&job).expect("response_schema present");
            return if is_hybrid_arch(model) {
                run_constrained_hybrid(model, tokenizer, template, eos, job, driver)
            } else {
                run_constrained_dense(model, tokenizer, template, eos, job, driver)
            };
        }
        let prompt_ids = match render_prompt(
            tokenizer,
            template,
            &job.model_id,
            &job.messages,
            &job.tools,
        ) {
            Ok(ids) => ids,
            Err(e) => {
                let _ = job.events.send(Err(ModelError::BackendUnavailable(e)));
                return;
            }
        };
        if std::env::var("ROZUM_MLX_DEBUG").is_ok() {
            eprintln!("PROMPT_IDS len={} {:?}", prompt_ids.len(), prompt_ids);
        }
        // Conversation boundary (prompt WITHOUT the trailing generation prompt): the
        // prefix that recurs across agentic turns and that prefix reuse keys on. The
        // generation prompt — esp. the thinking-off `<think></think>` prefill — does
        // NOT recur (next turn renders this turn as a completed message), so reusing
        // up to the full prompt would never match. Falls back to the full length if
        // the no-gen render fails (then reuse just won't fire).
        let conv_len = render_prompt_opt(
            tokenizer,
            template,
            &job.model_id,
            &job.messages,
            &job.tools,
            false,
        )
        .map(|c| c.len().min(prompt_ids.len()))
        .unwrap_or(prompt_ids.len());
        let gen_prompt_len = prompt_ids.len().saturating_sub(conv_len);
        // Effective output budget: the client's `max_tokens` (or our default), then
        // clamped to a hard ceiling so one runaway generation can't pin the cap-1
        // worker for minutes. Clients like Claude Code send a very large `max_tokens`;
        // the ceiling bounds the worst case (the repetition guard below catches an
        // actual loop far sooner). `ROZUM_MAX_OUTPUT_TOKENS=0` disables the ceiling.
        let ceiling = std::env::var("ROZUM_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_OUTPUT_CEILING);
        let max_tokens = {
            let want = job
                .sampling
                .max_tokens
                .map(|m| m as usize)
                .unwrap_or(DEFAULT_MAX_TOKENS);
            if ceiling == 0 { want } else { want.min(ceiling) }
        };

        // Large-context KV preflight: the `ConcatKeyValueCache` grows ~`kv_per_pos`
        // bytes per position (prompt + generation), so reject a request that would
        // not fit in available unified memory with a clear message instead of
        // letting Metal OOM mid-run. Skipped when either term is unknown.
        if let (Some(kv), Some(avail)) = (kv_per_pos, available_ram_bytes()) {
            let positions = (prompt_ids.len() + max_tokens) as u64;
            let needed = kv.saturating_mul(positions);
            let budget = (avail as f64 * KV_SAFETY_FRAC) as u64;
            if needed > budget {
                let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
                let fit = (budget / kv.max(1)) as usize;
                let _ = job.events.send(Err(ModelError::BackendUnavailable(format!(
                    "mlx: context too large for available memory: {} prompt + {} gen tokens \
                     need ~{:.1} GB of KV cache but only ~{:.1} GB is free. Lower --n-ctx / \
                     max_tokens or shorten the prompt (fits ~{} tokens now).",
                    prompt_ids.len(),
                    max_tokens,
                    gb(needed),
                    gb(budget),
                    fit,
                ))));
                return;
            }
        }

        let mut temp = job.sampling.temperature.unwrap_or(0.0);
        // top_k <= 0 / top_p >= 1.0 disable those filters (the sampler's defaults).
        let top_p = job.sampling.top_p.unwrap_or(1.0);
        let top_k = job.sampling.top_k.map(|k| k as i32).unwrap_or(0);
        let repeat_penalty = job.sampling.repeat_penalty.unwrap_or(1.0);
        // gpt-oss is a reasoning model built for SAMPLING (generation_config:
        // do_sample=true, temperature 1.0). Under greedy / near-greedy decoding its
        // long analysis CoT collapses into verbatim repetition loops ("We need. We
        // need…") that emit no tool call, so the agent stalls. Floor the temperature
        // to gpt-oss's intended ~1.0 (clients asking for MORE keep it). A repetition
        // penalty does NOT help and in fact breaks it — harmony output must repeat
        // structural tokens (<|channel|>, <|message|>, "functions", JSON punctuation),
        // so penalizing repeats corrupts the tool-call format. Verified: temp 1.0 +
        // no penalty completes 6/6 where greedy completes 0/6. Floor tunable via env.
        if matches!(model, LoadedModel::GptOss(_)) {
            let min_temp = std::env::var("ROZUM_GPTOSS_MIN_TEMP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0);
            temp = temp.max(min_temp);
        }
        if let Some(s) = job.sampling.seed {
            let _ = mlx_rs::random::seed(s);
        }

        // Prefix-KV reuse: find the stored conversation that this prompt extends
        // (longest-prefix match in the LRU — so interleaved sessions each reuse their
        // own), keep its KV, truncate to the shared length, and prefill only the new
        // suffix. Byte-exact: the kept `[0, reuse)` KV is exactly what a fresh prefill
        // computes, and `create_attention_mask` builds the causal mask from the cache
        // offset. Reuse keys on the CONVERSATION boundary (`conv_len` below), since the
        // generation-prompt tail doesn't recur. `ROZUM_PREFIX_CACHE=0` disables.
        let prefix_enabled =
            !matches!(std::env::var("ROZUM_PREFIX_CACHE").as_deref(), Ok("0"));
        // gpt-oss owns an external `ConcatKeyValueCache` like the dense arches, so it
        // supports prefix reuse (truncate to the shared prefix, prefill only the new
        // suffix) — byte-exact since the sliding-window mask is recomputed from the
        // (post-truncation) offset each forward. It is deliberately NOT in `is_dense`
        // (which also gates the Qwen `<tool_call>` constraints that harmony must
        // avoid), so include it here explicitly. Without reuse every turn re-prefills
        // the whole growing conversation — brutally slow for multi-turn agents.
        let dense = is_dense(model) || matches!(model, LoadedModel::GptOss(_));
        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        // Hybrid: a pre-populated heterogeneous cache when reusing (else None → the
        // hybrid `Generate` builds a fresh one via `init_cache`).
        let mut hcache: Option<Vec<qwen3_5::LayerCache>> = None;
        let mut reuse_len = 0usize;
        if prefix_enabled && dense {
            if let Some((rl, c)) = store.take_dense(&prompt_ids) {
                reuse_len = rl;
                cache = c;
                for c in cache.iter_mut().flatten() {
                    c.truncate(reuse_len as i32);
                }
            }
        } else if prefix_enabled {
            // Hybrid (Qwen3.6): truncate the `Full` (KV) layers to the shared prefix
            // and restore the `Linear` recurrent layers from the conversation-boundary
            // snapshot taken last turn — byte-exact for the append-only case.
            if let Some((rl, mut c, snap)) = store.take_hybrid(&prompt_ids) {
                reuse_len = rl;
                for (layer, s) in c.iter_mut().zip(snap.iter()) {
                    layer.truncate(reuse_len as i32);
                    layer.restore(s);
                }
                hcache = Some(c);
            }
        }
        if reuse_len > 0 && std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
            eprintln!(
                "PREFIX_REUSE reuse={reuse_len}/{} (prefill {} new tokens)",
                prompt_ids.len(),
                prompt_ids.len() - reuse_len
            );
        }
        // Prefill the full prompt fresh, or just the new suffix when reusing.
        let prompt_tokens = if reuse_len > 0 {
            Array::from(&prompt_ids[reuse_len..]).index(NewAxis)
        } else {
            Array::from(&prompt_ids[..]).index(NewAxis)
        };
        // Hybrid arms hand back their (advanced) cache + end-of-prefill snapshot here
        // so they can be persisted for next-turn reuse (dense persists `cache` below).
        let mut hybrid_result: Option<(
            Vec<qwen3_5::LayerCache>,
            Option<Vec<qwen3_5::LinearSnap>>,
        )> = None;
        match model {
            LoadedModel::Qwen3(m) => {
                let mut generator = qwen3::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true,  // pipeline: dense overlaps; hybrid kernel-eval blocks
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
            }
            LoadedModel::Qwen3Moe(m) => {
                let mut generator = qwen3_moe::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true,  // pipeline: dense overlaps; hybrid kernel-eval blocks
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
            }
            LoadedModel::GptOss(m) => {
                let mut generator = gpt_oss::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true,  // pipeline: dense overlaps (gpt-oss is single-stream)
                    true,  // harmony: parse the channel format into final/tool_calls
                );
            }
            LoadedModel::Llama(m) => {
                // Dense transformer like Qwen3: external KV cache + the shared sampler.
                let mut generator = llama::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true,  // pipeline: dense overlaps; hybrid kernel-eval blocks
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
            }
            LoadedModel::Qwen2(m) => {
                let mut generator = qwen2::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true,  // pipeline: dense overlaps; hybrid kernel-eval blocks
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
            }
            LoadedModel::Gemma3(m) => {
                let mut generator = gemma3::Generate::new(m, &mut cache, temp, &prompt_tokens);
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true, // pipeline: dense overlaps; hybrid kernel-eval blocks
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
            }
            LoadedModel::Qwen35(m) => {
                // Owns its heterogeneous (KV + conv/recurrent) cache internally; on
                // reuse it's seeded with the truncated+restored cache via `with_cache`.
                let mut generator = match hcache.take() {
                    Some(c) => qwen3_5::Generate::with_cache(m, temp, &prompt_tokens, c),
                    None => qwen3_5::Generate::new(m, temp, &prompt_tokens),
                };
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k, repeat_penalty);
                // Snapshot the Linear state at the conversation boundary (before the
                // generation-prompt tail), so it matches the next turn's reuse offset.
                generator.set_gen_prompt_len(gen_prompt_len as i32);
                let generator = stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true, // hybrid now pipelines too: the retain fix (ROZUM_MLX_RETAIN)
                          // dropped the per-call kernel eval, so the next token's graph
                          // can async_eval while we read the current's id (byte-exact;
                          // see mlx_qwen35_moe_decode_bench serial==pipe MATCH)
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
                hybrid_result = Some(generator.into_cache_and_snapshot());
            }
            LoadedModel::Qwen35Moe(m) => {
                let mut generator = match hcache.take() {
                    Some(c) => qwen3_5_moe::Generate::with_cache(m, temp, &prompt_tokens, c),
                    None => qwen3_5_moe::Generate::new(m, temp, &prompt_tokens),
                };
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k, repeat_penalty);
                // Snapshot the Linear state at the conversation boundary (before the
                // generation-prompt tail), so it matches the next turn's reuse offset.
                generator.set_gen_prompt_len(gen_prompt_len as i32);
                let generator = stream_generation(
                    generator,
                    tokenizer,
                    eos,
                    prompt_ids.len(),
                    max_tokens,
                    &job,
                    true, // hybrid now pipelines too: the retain fix (ROZUM_MLX_RETAIN)
                          // dropped the per-call kernel eval, so the next token's graph
                          // can async_eval while we read the current's id (byte-exact;
                          // see mlx_qwen35_moe_decode_bench serial==pipe MATCH)
                    false, // harmony: Qwen-style <tool_call>, not the channel format
                );
                hybrid_result = Some(generator.into_cache_and_snapshot());
            }
        }

        // Persist the (now advanced) cache into the LRU so the next request that
        // extends this conversation can reuse it. Keyed by the CONVERSATION boundary
        // (`conv_len`), not the full prompt: the generation-prompt tail doesn't recur,
        // so the next prompt only starts_with the conversation prefix. The matched
        // entry (if any) was removed on reuse above, so this re-inserts the extended
        // conversation at MRU; an unmatched (new) conversation inserts + evicts LRU.
        let conv_ids = prompt_ids.get(..conv_len).map(<[u32]>::to_vec);
        if !prefix_enabled {
            // leave the store untouched
        } else if let Some(ids) = conv_ids {
            if dense && !cache.is_empty() {
                store.put_dense(ids, cache);
            } else if let Some((hcache, Some(snap))) = hybrid_result {
                // Only persist when prefill ran (snapshot present); a mid-prefill
                // cancel yields no snapshot, so we don't poison the cache.
                store.put_hybrid(ids, hcache, snap);
            }
        }
    }

    /// Forward for the batchable dense arches (Qwen3 / Qwen3-MoE — both `qwen3::Attention`).
    fn dense_forward(
        model: &mut LoadedModel,
        inp: &Array,
        mask: Option<&Array>,
        cache: &mut Vec<Option<ConcatKeyValueCache>>,
    ) -> Result<Array, mlx_rs::error::Exception> {
        use mlx_rs::module::Module;
        match model {
            LoadedModel::Qwen3(m) => {
                let input = qwen3::ModelInput { inputs: inp, mask, cache };
                <qwen3::Model as Module<qwen3::ModelInput<'_, ConcatKeyValueCache>>>::forward(m, input)
            }
            LoadedModel::Qwen3Moe(m) => {
                let input = qwen3::ModelInput { inputs: inp, mask, cache };
                <qwen3_moe::Model as Module<qwen3::ModelInput<'_, ConcatKeyValueCache>>>::forward(
                    m, input,
                )
            }
            // gpt-oss reuses qwen3's `ModelInput`; it ignores the external `mask` and
            // builds its own per-layer full/sliding masks internally.
            LoadedModel::GptOss(m) => {
                let input = qwen3::ModelInput { inputs: inp, mask, cache };
                <gpt_oss::Model as Module<qwen3::ModelInput<'_, ConcatKeyValueCache>>>::forward(
                    m, input,
                )
            }
            // Llama family (Llama 3.x, Mistral, Phi-3, SmolLM …) — its `Attention` reads
            // `llama::BATCH_PAD_OFFSETS` for per-row rope and takes the key-pad mask via
            // `ModelInput.mask`, so the same ragged batched cache + masks drive it.
            LoadedModel::Llama(m) => {
                let input = llama::ModelInput { inputs: inp, mask, cache };
                <llama::Model as Module<llama::ModelInput<'_, ConcatKeyValueCache>>>::forward(
                    m, input,
                )
            }
            LoadedModel::Qwen2(m) => {
                let input = qwen2::ModelInput { inputs: inp, mask, cache };
                <qwen2::Model as Module<qwen2::ModelInput<'_, ConcatKeyValueCache>>>::forward(
                    m, input,
                )
            }
            // Gemma 3: per-row rope (BATCH_PAD_OFFSETS) + it derives per-layer local masks from
            // the pad mask we pass (global) + its sliding window.
            LoadedModel::Gemma3(m) => {
                let input = gemma3::ModelInput { inputs: inp, mask, cache };
                <gemma3::Model as Module<gemma3::ModelInput<'_, ConcatKeyValueCache>>>::forward(
                    m, input,
                )
            }
            _ => Err(mlx_rs::error::Exception::custom("dense_forward: non-batchable arch")),
        }
    }

    // ===================================================================
    // Speculative decoding — MLX dense target + draft.
    //
    // The accept-longest-greedy-prefix loop is the engine-agnostic
    // orchestrator (`crate::specdecode::decode`); here we implement only the
    // two token-level capabilities it drives over an MLX KV cache. The
    // orchestrator emits ONLY the target's greedy tokens, so the output is
    // byte-identical to plain greedy decode of the target — the draft just
    // changes how many tokens the target commits per forward (a latency win).
    // See `docs/specs/speculative-decoding.md`.
    //
    // Dense arches only (Qwen3 / Qwen3-MoE / Llama / Qwen2 / Gemma3): they own
    // an external `ConcatKeyValueCache` that truncates freely, which is exactly
    // what KV rollback on a rejected draft needs. Hybrid (Qwen3.6
    // GatedDeltaNet) is deferred (non-truncatable recurrent state) and falls
    // back to plain greedy.
    // ===================================================================

    /// Length of the longest common prefix of two token slices.
    #[allow(dead_code)]
    fn common_prefix_len(a: &[u32], b: &[u32]) -> usize {
        a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
    }

    /// Per-row argmax of `[1, n, vocab]` logits → the `n` greedy token ids.
    #[allow(dead_code)]
    fn argmax_rows(logits: &Array) -> Vec<u32> {
        let a = mlx_rs::ops::indexing::argmax_axis(logits, -1, false).expect("argmax rows"); // [1, n]
        let flat = a.reshape(&[-1]).expect("reshape argmax");
        let _ = eval([&flat]);
        flat.as_slice::<u32>().to_vec()
    }

    /// MLX dense **target**: verifies `k` draft tokens in ONE forward over its KV.
    /// Holds the model + its external KV cache; `kv_len` is how many context
    /// tokens the cache currently covers — always a true prefix of `ctx`, since we
    /// only ever keep accepted tokens (which are by construction in `ctx`).
    #[allow(dead_code)]
    struct MlxDenseTarget<'a> {
        model: &'a mut LoadedModel,
        cache: Vec<Option<ConcatKeyValueCache>>,
        kv_len: usize,
        eos: Vec<u32>,
        forwards: usize,
    }

    impl crate::specdecode::Target for MlxDenseTarget<'_> {
        fn verify(&mut self, ctx: &[u32], draft: &[u32]) -> crate::specdecode::Verify {
            // Feed the context tail not yet in the KV (`delta`, ≥1 token) plus the
            // `k` draft tokens in one forward. `delta` is the corrected/bonus token
            // from the previous round (or the whole prompt on the first call).
            let d = ctx.len() - self.kv_len;
            let mut feed: Vec<u32> = Vec::with_capacity(d + draft.len());
            feed.extend_from_slice(&ctx[self.kv_len..]);
            feed.extend_from_slice(draft);
            let inp = Array::from(&feed[..]).index(NewAxis);
            let logits = dense_forward(&mut self.model, &inp, None, &mut self.cache)
                .expect("spec-decode target forward");
            self.forwards += 1;
            // Row j predicts the token at KV position kv_len+j+1, so the target's
            // greedy token for ctx position ctx.len()+i is row (d-1+i): row d-1 is
            // the last `delta` row (predicts the first new token), then one per draft.
            let preds = argmax_rows(&logits);
            let mut emit: Vec<u32> = Vec::new();
            let mut accepted = 0usize;
            let mut eos = false;
            for i in 0..draft.len() {
                let t_i = preds[d - 1 + i];
                emit.push(t_i); // always the target's greedy token (byte-identical)
                if draft[i] == t_i {
                    accepted += 1;
                    if self.eos.contains(&t_i) {
                        eos = true;
                        break;
                    }
                } else {
                    // First divergence: emit the target's correction, accept no more.
                    if self.eos.contains(&t_i) {
                        eos = true;
                    }
                    break;
                }
            }
            if accepted == draft.len() && !eos {
                // All `k` accepted → append the target's free bonus token.
                let bonus = preds[d - 1 + draft.len()];
                emit.push(bonus);
                if self.eos.contains(&bonus) {
                    eos = true;
                }
            }
            // Roll the KV back to the accepted prefix (drop rejected draft tokens);
            // the correction/bonus token is NOT in the KV (it's the prediction, not a
            // fed token) and arrives as next round's `delta`.
            let keep = ctx.len() + accepted;
            for c in self.cache.iter_mut().flatten() {
                c.truncate(keep as i32);
            }
            self.kv_len = keep;
            crate::specdecode::Verify { emit, eos }
        }
    }

    /// MLX dense **draft**: greedily proposes the next `k` tokens, reusing its KV
    /// across rounds. `fed` mirrors exactly the tokens in its KV (a prefix of some
    /// past `ctx`); each call first rolls the KV back to the longest prefix still
    /// shared with the live `ctx`, undoing tokens the target rejected last round.
    #[allow(dead_code)]
    struct MlxDenseDraft<'a> {
        model: &'a mut LoadedModel,
        cache: Vec<Option<ConcatKeyValueCache>>,
        fed: Vec<u32>,
        eos: Vec<u32>,
    }

    impl crate::specdecode::Draft for MlxDenseDraft<'_> {
        fn propose(&mut self, ctx: &[u32], k: usize) -> Vec<u32> {
            // Reconcile the draft KV to what `ctx` still agrees with (rejected
            // speculative tokens from last round fall away).
            let cp = common_prefix_len(&self.fed, ctx);
            if cp < self.fed.len() {
                for c in self.cache.iter_mut().flatten() {
                    c.truncate(cp as i32);
                }
                self.fed.truncate(cp);
            }
            // Feed the new context tail, then greedily extend `k` tokens, one forward
            // each. An empty tail (KV already covers all of `ctx`) → propose nothing;
            // the target then emits its plain-greedy token (still correct, no speedup).
            let mut step_in: Vec<u32> = ctx[self.fed.len()..].to_vec();
            if step_in.is_empty() {
                return Vec::new();
            }
            let mut proposed: Vec<u32> = Vec::with_capacity(k);
            for _ in 0..k {
                let inp = Array::from(&step_in[..]).index(NewAxis);
                let logits = dense_forward(&mut self.model, &inp, None, &mut self.cache)
                    .expect("spec-decode draft forward");
                self.fed.extend_from_slice(&step_in); // now covered by the KV
                let next = argmax_u32(&logits.index((.., -1, ..)));
                proposed.push(next);
                if self.eos.contains(&next) {
                    break;
                }
                step_in = vec![next];
            }
            proposed
        }
    }

    /// Plain greedy decode of one dense model — the canonical sequence speculative
    /// decoding must reproduce byte-for-byte. Returns the emitted token ids.
    #[allow(dead_code)]
    fn greedy_decode_dense(
        mut model: LoadedModel,
        prompt_ids: &[u32],
        eos: &[u32],
        max_new: usize,
    ) -> Vec<u32> {
        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let mut step_in: Vec<u32> = prompt_ids.to_vec();
        let mut out: Vec<u32> = Vec::with_capacity(max_new);
        for _ in 0..max_new {
            let inp = Array::from(&step_in[..]).index(NewAxis);
            let logits = dense_forward(&mut model, &inp, None, &mut cache).expect("greedy forward");
            let next = argmax_u32(&logits.index((.., -1, ..)));
            out.push(next);
            if eos.contains(&next) {
                break;
            }
            step_in = vec![next];
        }
        out
    }

    /// Drive dense speculative decoding via the engine-agnostic orchestrator.
    /// Consumes both models (target + draft are independent residents — same arch
    /// family, shared tokenizer, enforced by the caller). Returns the emitted token
    /// ids (== the target's greedy decode) and the number of target forwards it took
    /// (the speedup metric: plain greedy is one forward per token).
    #[allow(dead_code)]
    fn run_spec_decode_dense(
        target_model: &mut LoadedModel,
        draft_model: &mut LoadedModel,
        prompt_ids: &[u32],
        eos: &[u32],
        k: usize,
        max_new: usize,
    ) -> (Vec<u32>, usize) {
        let mut target = MlxDenseTarget {
            model: target_model,
            cache: Vec::new(),
            kv_len: 0,
            eos: eos.to_vec(),
            forwards: 0,
        };
        let mut draft = MlxDenseDraft {
            model: draft_model,
            cache: Vec::new(),
            fed: Vec::new(),
            eos: eos.to_vec(),
        };
        let out = crate::specdecode::decode(prompt_ids, &mut draft, &mut target, k, max_new);
        (out, target.forwards)
    }

    /// Speculative lookahead `k` (draft tokens proposed per target forward).
    /// `ROZUM_SPECDECODE_K` (default 4); a bigger `k` helps when the draft is
    /// accurate (more accepted per forward) and hurts when it isn't (wasted draft
    /// forwards). Clamped to ≥1.
    fn spec_lookahead_k() -> usize {
        std::env::var("ROZUM_SPECDECODE_K")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(4)
    }

    /// Dense MLX arches that support spec-decode (truncatable external KV). Hybrid
    /// (Qwen3.6) and gpt-oss (harmony) are excluded — the former has
    /// non-truncatable recurrent state, the latter a different streaming format.
    fn model_type_is_dense(model_type: &str) -> bool {
        matches!(
            model_type,
            "qwen3" | "qwen3_moe" | "llama" | "mistral" | "phi3" | "gemma3_text" | "gemma3" | "qwen2"
        )
    }

    /// A request decodes via spec-decode only if it is pure greedy (spec-decode
    /// verifies the target's *argmax*, so any sampling / penalty / seed would change
    /// the output) and not schema-constrained (the masked B=1 path owns those).
    /// Everything else falls back to plain target decode (`run_job`).
    fn is_greedy_request(job: &Job) -> bool {
        let s = &job.sampling;
        s.temperature.unwrap_or(0.0) == 0.0
            && s.top_p.unwrap_or(1.0) >= 1.0
            && s.top_k.unwrap_or(0) <= 0
            && s.repeat_penalty.unwrap_or(1.0) == 1.0
            && s.seed.is_none()
    }

    fn spec_job_eligible(job: &Job, target: &LoadedModel, draft: &LoadedModel) -> bool {
        is_dense(target) && is_dense(draft) && is_greedy_request(job) && !should_constrain(job, target)
    }

    /// Decode one greedy job via speculative decoding (draft proposes, target
    /// verifies) and stream the result with the same detok + tool-call parsing the
    /// normal path uses (`BatchSeq` + `check_finish`). Fresh KV per job (no
    /// cross-turn prefix reuse yet — a follow-up). `target`/`draft` are reused
    /// across jobs (owned by the worker); only their per-job KV caches are fresh.
    fn run_spec_job(
        target: &mut LoadedModel,
        draft: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        job: Job,
    ) {
        let prompt_ids =
            match render_prompt(tokenizer, template, &job.model_id, &job.messages, &job.tools) {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = job.events.send(Err(ModelError::BackendUnavailable(e)));
                    return;
                }
            };
        let ceiling = output_ceiling();
        let max_tokens = {
            let want = job.sampling.max_tokens.map(|m| m as usize).unwrap_or(DEFAULT_MAX_TOKENS);
            if ceiling == 0 { want } else { want.min(ceiling) }
        };
        let k = spec_lookahead_k();
        let mut tgt = MlxDenseTarget {
            model: target,
            cache: Vec::new(),
            kv_len: 0,
            eos: eos.to_vec(),
            forwards: 0,
        };
        let mut drf = MlxDenseDraft {
            model: draft,
            cache: Vec::new(),
            fed: Vec::new(),
            eos: eos.to_vec(),
        };
        let mut seq = BatchSeq {
            job,
            out_ids: Vec::new(),
            emitted: String::new(),
            full_text: String::new(),
            tool_seen: false,
            output_tokens: 0,
            prompt_len: prompt_ids.len() as i32,
            max_tokens,
            finished: false,
            stop: StopReason::EndTurn,
        };
        {
            // `check_finish` streams the token (text + tool-markup suppression) and
            // reports EOS / cancel / max-tokens / runaway — returning `false` here
            // stops the orchestrator. EOS is consumed by `check_finish` (not shown).
            let mut on_token = |tok: u32| -> bool { !check_finish(&mut seq, tok, eos, tokenizer) };
            crate::specdecode::decode_streaming(
                &prompt_ids,
                &mut drf,
                &mut tgt,
                k,
                max_tokens,
                &mut on_token,
            );
        }
        if std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
            eprintln!(
                "spec-decode: {} target forwards for {} output tokens (k={k}) — \
                 plain greedy would be {} forwards",
                tgt.forwards, seq.output_tokens, seq.output_tokens
            );
        }
        seq.finalize();
    }

    /// Worker thread entry point for a spec-decode pair: loads the target + draft
    /// (both dense, sharing a tokenizer family), then serves jobs — greedy jobs via
    /// [`run_spec_job`], everything else via plain target [`run_job`].
    #[allow(clippy::too_many_arguments)]
    fn worker_main_spec(
        target_dir: PathBuf,
        target_type: String,
        draft_dir: PathBuf,
        draft_type: String,
        mut eos: Vec<u32>,
        kv_per_pos: Option<u64>,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        ready: oneshot::Sender<Result<(), String>>,
    ) {
        let mut target = match LoadedModel::load(&target_type, &target_dir) {
            Ok(m) => m,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut draft = match LoadedModel::load(&draft_type, &draft_dir) {
            Ok(m) => m,
            Err(e) => {
                let _ = ready.send(Err(format!("spec-decode draft: {e}")));
                return;
            }
        };
        let mut tokenizer = match Tokenizer::from_file(target_dir.join("tokenizer.json")) {
            Ok(t) => t,
            Err(e) => {
                let _ = ready.send(Err(format!("mlx: tokenizer: {e:?}")));
                return;
            }
        };
        // Same-family guard: well-known tokens must map to the same id in both
        // tokenizers (a different tokenizer would silently corrupt the shared token
        // stream, since the draft proposes ids the target verifies directly).
        if let Ok(dtok) = Tokenizer::from_file(draft_dir.join("tokenizer.json")) {
            let mismatch = ["<|im_end|>", "<|endoftext|>"].iter().any(|t| {
                let (a, b) = (tokenizer.token_to_id(t), dtok.token_to_id(t));
                a.is_some() && b.is_some() && a != b
            });
            if mismatch {
                let _ = ready.send(Err(
                    "spec-decode: draft and target tokenizers differ (need the same family)".into(),
                ));
                return;
            }
        }
        for t in ["<end_of_turn>"] {
            if let Some(id) = tokenizer.token_to_id(t) {
                if !eos.contains(&id) {
                    eos.push(id);
                }
            }
        }
        let template =
            match load_model_chat_template_from_file(target_dir.join("tokenizer_config.json"))
                .ok()
                .flatten()
                .or_else(|| std::fs::read_to_string(target_dir.join("chat_template.jinja")).ok())
            {
                Some(t) => t,
                None => {
                    let _ = ready.send(Err("mlx: no chat template".into()));
                    return;
                }
            };
        MODEL_BOS_TOKEN
            .with(|c| *c.borrow_mut() = read_bos_token(&target_dir.join("tokenizer_config.json")));
        if ready.send(Ok(())).is_err() {
            return;
        }

        // Serial worker (cap-1): a `PrefixStore` backs the target-only fallback path.
        let mut store = PrefixStore::new();
        while let Some(job) = jobs.blocking_recv() {
            if spec_job_eligible(&job, &target, &draft) {
                run_spec_job(&mut target, &mut draft, &mut tokenizer, &template, eos.as_slice(), job);
            } else {
                run_job(&mut target, &mut tokenizer, &template, &eos, kv_per_pos, &mut store, job);
            }
        }
    }

    impl MlxNativeBackend {
        /// Build a spec-decode backend: a `target` model accelerated by a small
        /// `draft` (same tokenizer family), BOTH resident in one worker thread. On
        /// the single-device Apple-Silicon box this is the canonical single-stream
        /// co-use; the engine-agnostic orchestrator runs inside the worker, so the
        /// `!Send` models never cross a thread boundary. Greedy requests use the
        /// speculative loop; sampled / constrained requests fall back to plain
        /// target decode. Dense target + draft only (truncatable KV).
        pub async fn new_spec_decode(
            target_dir: PathBuf,
            target_id: String,
            draft_dir: PathBuf,
            max_ctx: Option<u32>,
        ) -> ModelResult<Self> {
            cap_mlx_memory();
            let (mut n_ctx, eos, target_type, kv_per_pos) = read_config(&target_dir);
            let (_dn, _de, draft_type, _dk) = read_config(&draft_dir);
            if !model_type_is_dense(&target_type) {
                return Err(ModelError::BackendUnavailable(format!(
                    "spec-decode: target arch '{target_type}' is not a supported dense MLX arch \
                     (hybrid Qwen3.6 / gpt-oss not supported yet)"
                )));
            }
            if !model_type_is_dense(&draft_type) {
                return Err(ModelError::BackendUnavailable(format!(
                    "spec-decode: draft arch '{draft_type}' is not a supported dense MLX arch"
                )));
            }
            if let Some(cap) = max_ctx {
                n_ctx = n_ctx.min(cap);
            }
            let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
            let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
            let label = target_id.clone();
            let worker = thread::Builder::new()
                .name("mlx-spec".into())
                .spawn(move || {
                    worker_main_spec(
                        target_dir,
                        target_type,
                        draft_dir,
                        draft_type,
                        eos,
                        kv_per_pos,
                        jobs_rx,
                        ready_tx,
                    )
                })
                .map_err(|e| {
                    ModelError::BackendUnavailable(format!("mlx: spawn spec worker: {e}"))
                })?;
            match ready_rx.await {
                Ok(Ok(())) => {
                    eprintln!("mlx-native: spec-decode '{label}' + draft ready (context {n_ctx})");
                    Ok(Self {
                        jobs: Some(jobs_tx),
                        worker: Some(worker),
                        model_id: target_id,
                        n_ctx,
                    })
                }
                Ok(Err(e)) => Err(ModelError::BackendUnavailable(e)),
                Err(_) => Err(ModelError::BackendUnavailable(
                    "mlx: spec worker died during load".into(),
                )),
            }
        }
    }

    /// Per-sequence streaming state inside a batch (mirrors `stream_generation`'s
    /// per-token emit + finalize for one row).
    struct BatchSeq {
        job: Job,
        out_ids: Vec<u32>,
        emitted: String,
        full_text: String,
        tool_seen: bool,
        output_tokens: u32,
        prompt_len: i32,
        max_tokens: usize,
        finished: bool,
        stop: StopReason,
    }

    impl BatchSeq {
        /// Append `tok`, detok, and stream the new text suffix (suppressing `<tool_call>`
        /// markup). Returns false if the client dropped the stream.
        fn push(&mut self, tok: u32, tokenizer: &mut Tokenizer) -> bool {
            self.out_ids.push(tok);
            self.output_tokens += 1;
            if let Ok(text) = tokenizer.decode(&self.out_ids, true) {
                let stable = text.trim_end_matches('\u{FFFD}');
                self.full_text = stable.to_string();
                if !self.tool_seen {
                    let tools = !self.job.tools.is_empty();
                    if let Some(pos) = tool_markup_at(stable, tools) {
                        if pos > self.emitted.len() && stable.starts_with(&self.emitted) {
                            let delta = stable[self.emitted.len()..pos].to_string();
                            self.emitted = stable[..pos].to_string();
                            let _ = self.job.events.send(Ok(ChatEvent::TextDelta { text: delta }));
                        }
                        self.tool_seen = true;
                    } else if stable.len() > self.emitted.len() && stable.starts_with(&self.emitted)
                    {
                        // Hold back a trailing ``` fence in a tool request — it may open a
                        // loose tool call the finalizer turns into a tool_use; leaking it
                        // duplicates the call as text. Flushed at finalize if it wasn't one.
                        let cut = if tools {
                            stable
                                .rfind("```")
                                .filter(|&p| p >= self.emitted.len())
                                .unwrap_or(stable.len())
                        } else {
                            stable.len()
                        };
                        if cut > self.emitted.len() {
                            let delta = stable[self.emitted.len()..cut].to_string();
                            self.emitted = stable[..cut].to_string();
                            if self
                                .job
                                .events
                                .send(Ok(ChatEvent::TextDelta { text: delta }))
                                .is_err()
                            {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        }

        /// Parse any tool calls, emit them (or the held-back text), then `Done`.
        fn finalize(&mut self) {
            let tool_calls = if matches!(self.stop, StopReason::Cancelled) {
                Vec::new()
            } else {
                crate::serving::parse_tool_calls(&self.full_text)
            };
            if !tool_calls.is_empty() {
                for (name, args) in tool_calls.iter() {
                    let id = crate::engine::next_tool_call_id();
                    let _ = self.job.events.send(Ok(ChatEvent::ToolUseStart {
                        id: id.clone(),
                        name: name.clone(),
                    }));
                    let _ = self.job.events.send(Ok(ChatEvent::ToolUseDelta {
                        id: id.clone(),
                        input_json_delta: args.clone(),
                    }));
                    let _ = self.job.events.send(Ok(ChatEvent::ToolUseEnd { id }));
                }
                self.stop = StopReason::ToolUse;
            } else if self.full_text.len() > self.emitted.len() {
                // Flush held-back text: suppressed tool markup that wasn't a parseable
                // call, or a held-back ``` fence that turned out not to open one.
                let _ = self.job.events.send(Ok(ChatEvent::TextDelta {
                    text: self.full_text[self.emitted.len()..].to_string(),
                }));
            }
            let _ = self.job.events.send(Ok(ChatEvent::Done {
                input_tokens: self.prompt_len as u32,
                output_tokens: self.output_tokens,
                stop_reason: self.stop,
            }));
        }
    }

    /// Left-pad a `[B,H,L,D]` KV tensor with `pad` zero positions at the FRONT of the
    /// sequence axis (axis 2) — right-aligns the real tokens so a batch of different-length
    /// rows shares one cache width. The pad slots are masked out of attention by the per-row
    /// pad mask; their RoPE never applies (cached keys keep their prefill rope).
    fn lpad_seq(x: &Array, pad: i32) -> Array {
        use mlx_rs::ops::{concatenate_axis, zeros_dtype};
        if pad == 0 {
            return x.clone();
        }
        let s = x.shape();
        let z = zeros_dtype(&[s[0], s[1], pad, s[3]], x.dtype()).unwrap();
        concatenate_axis(&[&z, x], 2).unwrap()
    }

    /// Apply one sampled token to a batch row and report whether the row is now FINISHED
    /// (EOS / cancelled / detok failure / max-tokens / runaway loop). On a continuing token
    /// it pushes (emits) the token; the caller streams via `BatchSeq`. Shared by the main
    /// per-row loop and continuous mid-decode admission so both honor the same stop rules.
    fn check_finish(seq: &mut BatchSeq, tok: u32, eos: &[u32], tokenizer: &mut Tokenizer) -> bool {
        if eos.contains(&tok) {
            seq.stop = StopReason::EndTurn;
            return true;
        }
        if seq.job.cancel.is_cancelled() {
            seq.stop = StopReason::Cancelled;
            return true;
        }
        if !seq.push(tok, tokenizer) {
            seq.stop = StopReason::Cancelled;
            return true;
        }
        if seq.output_tokens as usize >= seq.max_tokens {
            seq.stop = StopReason::MaxTokens;
            return true;
        }
        if crate::engine::is_runaway_loop(&seq.out_ids) {
            seq.stop = StopReason::EndTurn;
            return true;
        }
        false
    }

    // ─── Constrained tool-argument decode (schema-masked, B=1, dense) ─────────────

    /// `true` when this job should decode under the JSON-schema constraint: constraints are
    /// enabled (default on, unless `ROZUM_MLX_CONSTRAIN=0`), the request carries tools, and the
    /// model is a dense arch (`run_constrained_dense`) or the Qwen3.6 hybrid (`run_constrained_hybrid`).
    fn should_constrain(job: &Job, model: &LoadedModel) -> bool {
        constrain_enabled()
            && !job.tools.is_empty()
            && (is_dense(model) || is_hybrid_arch(model))
    }

    /// Whether constrained tool decoding is enabled. **On by default**; opt out with
    /// `ROZUM_MLX_CONSTRAIN=0`. The B=1 masked path forces valid tool-call JSON but is
    /// ~2-3× slower per token; `serving`'s JSON-repair recovers *common* malformations
    /// (unescaped quotes) on the fast path, but NOT the structurally-broken Qwen3.6 XML
    /// form a local model emits under a foreign (Codex/Claude) tool schema — e.g.
    /// `<tool_call>{"function=exec_command">{…}}` — which parses to nothing, so the agent
    /// silently drops the call and loops. Measured 2026-06-16 on Qwen3.6-35B-A3B: with
    /// constraints OFF, Codex `fix`/`debug` both fail (malformed `<tool_call>` never
    /// executes); ON, both pass. Correctness on the agentic tool path outweighs the
    /// latency, so it's the default; set `=0` for perf-sensitive serving that doesn't
    /// rely on tool calls landing.
    fn constrain_enabled() -> bool {
        !matches!(std::env::var("ROZUM_MLX_CONSTRAIN").ok().as_deref(), Some("0" | "false" | "off"))
    }

    /// Runtime driver that constrains a tool call to the tool schemas. Built from a job's
    /// `tools`; once the model opens a `<tool_call>` the body is constrained — JSON Hermes
    /// (`{"name":…,"arguments":…}`) or the Qwen3.6 XML form (`<function=…>`), whichever the
    /// model emits (chosen from the first body char). For JSON, `arguments` is resolved to the
    /// chosen tool's schema as soon as the `name` literal is read; the XML form resolves the
    /// tool internally.
    struct ToolConstraint {
        names: Vec<String>,
        arg_schemas: Vec<crate::constrain::Schema>,
        cons: crate::constrain::Constraint,
        active: bool,
        done: bool,
        body_start: usize,
    }

    /// Byte offset of a `{` that opens a tool-call-shaped JSON (`{ "name": … }`),
    /// for constraining a loose markdown/bare-json tool call that lacks the
    /// `<tool_call>` envelope. Requires the first key to be `name`, so a `{` in
    /// ordinary prose (or a `{}` in a code example) isn't mistaken for a tool call.
    pub(crate) fn find_loose_tool_json(text: &str) -> Option<usize> {
        let mut i = 0;
        while let Some(rel) = text[i..].find('{') {
            let pos = i + rel;
            if text[pos + 1..].trim_start().starts_with("\"name\"") {
                return Some(pos);
            }
            i = pos + 1;
        }
        None
    }

    /// Byte offset where tool-call markup begins, to suppress it from the streamed
    /// text: a `<tool_call>` envelope, or — when `tools` are offered — a loose
    /// ```json fence / bare `{"name":…}` the finalizer turns into a `tool_use`.
    /// Without suppression the call leaks as raw text AND a tool_use, which makes
    /// weaker models re-emit it (an agentic loop).
    pub(crate) fn tool_markup_at(text: &str, tools: bool) -> Option<usize> {
        if let Some(p) = text.find(TOOL_OPEN) {
            return Some(p);
        }
        if !tools {
            return None;
        }
        let brace = find_loose_tool_json(text)?;
        // Suppress an enclosing ```json fence too, if it sits just before the `{`.
        let head = &text[..brace];
        match head.rfind("```") {
            Some(f) if head[f + 3..].chars().all(|c| c.is_alphanumeric() || c.is_whitespace()) => {
                Some(f)
            }
            _ => Some(brace),
        }
    }

    impl ToolConstraint {
        fn from_job(job: &Job) -> Option<Self> {
            if job.tools.is_empty() {
                return None;
            }
            let names: Vec<String> = job.tools.iter().map(|t| t.name.clone()).collect();
            let arg_schemas: Vec<crate::constrain::Schema> = job
                .tools
                .iter()
                .map(|t| crate::constrain::Schema::parse(&t.input_schema))
                .collect();
            // Placeholder until the format is picked at activation.
            let cons = crate::constrain::Constraint::Json(crate::constrain::Schema::Any);
            Some(Self { names, arg_schemas, cons, active: false, done: false, body_start: 0 })
        }

        fn tools(&self) -> Vec<(String, crate::constrain::Schema)> {
            self.names.iter().cloned().zip(self.arg_schemas.iter().cloned()).collect()
        }

        /// The tool-call body slice to constrain right now, or `None` for free decode (before
        /// the `<tool_call>` body opens, or after it completes).
        fn json_region<'a>(&mut self, full_text: &'a str) -> Option<&'a str> {
            use crate::constrain::{envelope, Constraint, Schema};
            if self.done {
                return None;
            }
            if !self.active {
                // Native Qwen `<tool_call>` envelope (preferred); first body char picks
                // the format (`{` JSON, `<` XML). If the model emits NO envelope —
                // common for 4B–7B models driven by a foreign (Claude/OpenAI) tool
                // schema, which fall back to a bare or ```json `{"name":…,"arguments":…}`
                // — constrain that instead, so the masked sampler still forces VALID
                // JSON (escaped quotes, schema-conforming) and the call isn't dropped.
                let (off, lead) = match full_text.find(TOOL_OPEN) {
                    Some(op) => {
                        let after = &full_text[op + TOOL_OPEN.len()..];
                        let trimmed = after.trim_start();
                        let lead = trimmed.chars().next()?;
                        (op + TOOL_OPEN.len() + (after.len() - trimmed.len()), lead)
                    }
                    None => (find_loose_tool_json(full_text)?, '{'),
                };
                match lead {
                    '{' => self.cons = Constraint::Json(envelope(&self.names, Schema::Any)),
                    '<' => self.cons = Constraint::Xml(self.tools()),
                    _ => return None, // not a body we constrain (yet)
                }
                self.active = true;
                self.body_start = off;
            }
            let json = &full_text[self.body_start..];
            // JSON only: resolve `arguments` to the chosen tool's schema once `name` is read.
            if let Constraint::Json(_) = self.cons {
                if let Some(name) = extract_tool_name(json) {
                    if let Some(i) = self.names.iter().position(|n| *n == name) {
                        self.cons = Constraint::Json(envelope(
                            &self.names,
                            self.arg_schemas[i].clone(),
                        ));
                    }
                }
            }
            if self.cons.is_complete(json) {
                self.done = true;
                return None;
            }
            Some(json)
        }
    }

    /// Drives a constrained decode: given the decoded text so far, returns the text region to
    /// constrain right now + the active `Constraint`, or `None` for free decode. Lets the same
    /// masked loop serve both tool-call constraining ([`ToolConstraint`]) and whole-response
    /// structured output ([`ResponseConstraint`]).
    trait ConstraintDriver {
        fn region<'a>(
            &mut self,
            full_text: &'a str,
        ) -> Option<(&'a str, &crate::constrain::Constraint)>;
    }

    impl ConstraintDriver for ToolConstraint {
        fn region<'a>(
            &mut self,
            full_text: &'a str,
        ) -> Option<(&'a str, &crate::constrain::Constraint)> {
            let region = self.json_region(full_text)?;
            Some((region, &self.cons))
        }
    }

    /// Constrains the ENTIRE response to a fixed JSON Schema — the `response_format` /
    /// structured-output path. Active from the first generated token; releases once the value
    /// completes (then the model is free to emit EOS). Unlike [`ToolConstraint`] there's no
    /// `<tool_call>` envelope: the whole output IS the schema's value.
    struct ResponseConstraint {
        cons: crate::constrain::Constraint,
        done: bool,
    }

    impl ResponseConstraint {
        /// Build from a job's `response_schema` (the parsed `response_format`), or `None`.
        fn from_job(job: &Job) -> Option<Self> {
            let schema = job.sampling.response_schema.as_ref()?;
            Some(Self {
                cons: crate::constrain::Constraint::Json(crate::constrain::Schema::parse(schema)),
                done: false,
            })
        }
    }

    impl ConstraintDriver for ResponseConstraint {
        fn region<'a>(
            &mut self,
            full_text: &'a str,
        ) -> Option<(&'a str, &crate::constrain::Constraint)> {
            if self.done {
                return None;
            }
            if self.cons.is_complete(full_text) {
                self.done = true;
                return None;
            }
            Some((full_text, &self.cons))
        }
    }

    /// Pull a completed `"name": "X"` value out of a partial Hermes envelope, if present.
    /// Tool names carry no JSON escapes, so a plain quote scan is exact.
    pub(crate) fn extract_tool_name(json: &str) -> Option<String> {
        let k = json.find("\"name\"")?;
        let after = &json[k + 6..];
        let colon = after.find(':')?;
        let rest = &after[colon + 1..];
        let q = rest.find('"')?;
        let val = &rest[q + 1..];
        let end = val.find('"')?;
        Some(val[..end].to_string())
    }

    /// Sample one token from `[1, vocab]` logits under the schema mask: keep only the
    /// top-K candidates whose decoded piece keeps the JSON a valid (in)complete prefix,
    /// forbid all others (−∞), then run the normal sampler among the allowed. Widens K
    /// (256 → 4096 → full vocab) until ≥1 candidate is valid — a satisfiable prefix always
    /// has one — and falls back to the unconstrained argmax only if truly nothing matches.
    fn sample_constrained(
        logits: &Array,
        json: &str,
        cons: &crate::constrain::Constraint,
        tokenizer: &Tokenizer,
        opts: &qwen3::SamplerOpts,
        recent: &[u32],
    ) -> u32 {
        use crate::constrain::Prefix;
        // Materialize a contiguous CPU copy of the row (the `+0` forces it).
        let dense = logits.add(Array::from_f32(0.0)).expect("dense logits");
        let _ = mlx_rs::transforms::eval([&dense]);
        let row: Vec<f32> = dense.as_slice::<f32>().to_vec();
        let vocab = row.len();

        let piece_ok = |id: usize| -> bool {
            let Ok(text) = tokenizer.decode(&[id as u32], false) else {
                return false;
            };
            if text.is_empty() {
                return false; // special/empty token — never a body char
            }
            let mut probe = String::with_capacity(json.len() + text.len());
            probe.push_str(json);
            probe.push_str(&text);
            cons.prefix(&probe) != Prefix::Invalid
        };
        let topk = |k: usize| -> Vec<usize> {
            let mut idx: Vec<usize> = (0..vocab).collect();
            if k < vocab {
                idx.select_nth_unstable_by(k, |&a, &b| row[b].total_cmp(&row[a]));
                idx.truncate(k);
            }
            idx
        };

        let mut allowed: Vec<i32> = Vec::new();
        for &k in &[256usize, 4096, vocab] {
            allowed = topk(k.min(vocab))
                .into_iter()
                .filter(|&i| piece_ok(i))
                .map(|i| i as i32)
                .collect();
            if !allowed.is_empty() {
                break;
            }
        }
        if allowed.is_empty() {
            return argmax_u32(&dense);
        }

        let mut bias = vec![f32::NEG_INFINITY; vocab];
        for &i in &allowed {
            bias[i as usize] = 0.0;
        }
        let bias = Array::from_slice(&bias, &[1, vocab as i32]);
        let masked = dense.add(&bias).expect("mask add");
        let tok = qwen3::sample_with(&masked, opts, recent).expect("sample_with");
        let _ = mlx_rs::transforms::eval([&tok]);
        tok.reshape(&[-1]).unwrap().index(0).item::<u32>()
    }

    /// Argmax token id of `[1, vocab]` logits.
    fn argmax_u32(logits: &Array) -> u32 {
        use mlx_rs::ops::indexing::IndexOp;
        let a = mlx_rs::ops::indexing::argmax_axis(logits, -1, false).expect("argmax");
        let _ = mlx_rs::transforms::eval([&a]);
        a.reshape(&[-1]).unwrap().index(0).item::<u32>()
    }

    /// B=1 schema-constrained decode for dense arches. Mirrors `run_job`'s decode but masks
    /// the sampler to the tool schemas inside the `<tool_call>` JSON. Fresh prefill (no
    /// prefix-KV reuse) — a tool turn is short, so that is fine for v1.
    /// `qwen3::SamplerOpts` from a job's sampling params (the constrained loop's defaults).
    fn sampler_opts_of(job: &Job) -> qwen3::SamplerOpts {
        qwen3::SamplerOpts {
            temp: job.sampling.temperature.unwrap_or(0.0),
            top_p: job.sampling.top_p.unwrap_or(1.0),
            top_k: job.sampling.top_k.map(|k| k as i32).unwrap_or(0),
            repeat_penalty: job.sampling.repeat_penalty.unwrap_or(1.0),
        }
    }

    /// Shared B=1 constrained decode loop over an already-prefilled `(seq, cache, logits)`.
    /// Each step masks the sampler to the tool schema inside the `<tool_call>` JSON (free
    /// otherwise), then advances the cache via `forward`. Generic over the cache type so the
    /// dense (`ConcatKeyValueCache`) and hybrid (`qwen3_5::LayerCache`) paths share it.
    fn constrained_decode_loop<C, D: ConstraintDriver>(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        eos: &[u32],
        mut driver: D,
        opts: qwen3::SamplerOpts,
        mut seq: BatchSeq,
        mut cache: Vec<C>,
        mut logits: Array,
        forward: fn(&mut LoadedModel, &Array, &mut Vec<C>) -> Result<Array, Exception>,
    ) {
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        loop {
            let recent: &[u32] = if opts.repeat_penalty != 1.0 {
                qwen3::repeat_window(&seq.out_ids)
            } else {
                &[]
            };
            // Pick the next token (masked inside the constrained region, free otherwise). The
            // block bounds the immutable borrows of `seq`/`driver` so `check_finish` can `&mut`.
            let tok = {
                match driver.region(&seq.full_text) {
                    Some((region, cons)) => {
                        sample_constrained(&logits, region, cons, tokenizer, &opts, recent)
                    }
                    None => {
                        let t = qwen3::sample_with(&logits, &opts, recent).expect("sample_with");
                        let _ = mlx_rs::transforms::eval([&t]);
                        t.reshape(&[-1]).unwrap().index(0).item::<u32>()
                    }
                }
            };
            if check_finish(&mut seq, tok, eos, tokenizer) {
                break;
            }
            let inp = Array::from(&[tok][..]).index(NewAxis);
            logits = match forward(model, &inp, &mut cache) {
                Ok(l) => l.index((.., -1, ..)),
                Err(e) => {
                    let _ = seq
                        .job
                        .events
                        .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                    return;
                }
            };
        }
        seq.finalize();
    }

    /// Effective output ceiling (`ROZUM_MAX_OUTPUT_TOKENS`, 0 = off).
    fn output_ceiling() -> usize {
        std::env::var("ROZUM_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_OUTPUT_CEILING)
    }

    /// Single-token dense forward with the uniform `forward` signature the loop expects.
    fn dense_step(
        model: &mut LoadedModel,
        inp: &Array,
        cache: &mut Vec<Option<ConcatKeyValueCache>>,
    ) -> Result<Array, Exception> {
        dense_forward(model, inp, None, cache)
    }

    /// Single-token hybrid forward (B=1, no batch-pad thread-locals → sequential decode).
    fn hybrid_step(
        model: &mut LoadedModel,
        inp: &Array,
        cache: &mut Vec<qwen3_5::LayerCache>,
    ) -> Result<Array, Exception> {
        hybrid_forward(model, inp, cache)
    }

    fn run_constrained_dense<D: ConstraintDriver>(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        job: Job,
        driver: D,
    ) {
        let opts = sampler_opts_of(&job);
        if let Some(s) = job.sampling.seed {
            let _ = mlx_rs::random::seed(s);
        }
        let (seq, cache, logits) =
            match prefill_job_dense(model, tokenizer, template, output_ceiling(), job) {
                Some(x) => x,
                None => return,
            };
        constrained_decode_loop(model, tokenizer, eos, driver, opts, seq, cache, logits, dense_step);
    }

    /// Hybrid (Qwen3.6) constrained decode — same masked loop as the dense path, over the
    /// heterogeneous `LayerCache`. This is what makes constrained decode work on the Qwen3.6
    /// hybrid (the user's primary model) for both tool-args and structured output.
    fn run_constrained_hybrid<D: ConstraintDriver>(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        job: Job,
        driver: D,
    ) {
        let opts = sampler_opts_of(&job);
        if let Some(s) = job.sampling.seed {
            let _ = mlx_rs::random::seed(s);
        }
        let (seq, cache, logits) =
            match prefill_job_hybrid(model, tokenizer, template, output_ceiling(), job) {
                Some(x) => x,
                None => return,
            };
        constrained_decode_loop(model, tokenizer, eos, driver, opts, seq, cache, logits, hybrid_step);
    }

    /// Render + prefill ONE job on the dense path, building its `BatchSeq` + per-layer KV
    /// cache + last-position logits `[1, vocab]`. Returns `None` after sending an error
    /// event if rendering or the forward fails. Shared by the initial batch fill and
    /// continuous mid-decode admission.
    fn prefill_job_dense(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        ceiling: usize,
        job: Job,
    ) -> Option<(BatchSeq, Vec<Option<ConcatKeyValueCache>>, Array)> {
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        let prompt_ids =
            match render_prompt(tokenizer, template, &job.model_id, &job.messages, &job.tools) {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = job.events.send(Err(ModelError::BackendUnavailable(e)));
                    return None;
                }
            };
        let max_tokens = {
            let want = job.sampling.max_tokens.map(|m| m as usize).unwrap_or(DEFAULT_MAX_TOKENS);
            if ceiling == 0 { want } else { want.min(ceiling) }
        };
        let prompt = Array::from(&prompt_ids[..]).index(NewAxis);
        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let logits = match dense_forward(model, &prompt, None, &mut cache) {
            Ok(l) => l,
            Err(e) => {
                let _ = job
                    .events
                    .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                return None;
            }
        };
        let row = logits.index((.., -1, ..)); // [1, vocab]
        let seq = BatchSeq {
            job,
            out_ids: Vec::new(),
            emitted: String::new(),
            full_text: String::new(),
            tool_seen: false,
            output_tokens: 0,
            prompt_len: prompt_ids.len() as i32,
            max_tokens,
            finished: false,
            stop: StopReason::EndTurn,
        };
        Some((seq, cache, row))
    }

    /// Batched (continuous) decode for a wave of greedy dense requests: prefill each
    /// separately (own cache), assemble one left-padded batched cache, then decode all
    /// together (per-row argmax + per-row rope + per-row pad mask), streaming each
    /// sequence independently and retiring a row from the batch on EOS/max-tokens. While
    /// decoding it ADMITS queued greedy jobs from `jobs` into freed/spare slots (up to
    /// `cap`) — continuous batching — so a finished short row's slot is refilled instead of
    /// idling. Non-greedy jobs pulled from the queue are returned for the caller to run
    /// serially. Dense Qwen3/Qwen3-MoE only (the per-row-rope `qwen3::Attention`).
    fn run_batch(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        initial: Vec<Job>,
        jobs: &mut mpsc::UnboundedReceiver<Job>,
        cap: usize,
    ) -> Vec<Job> {
        use mlx_lm::models::qwen3::set_batch_pad_offsets;
        use mlx_rs::ops::indexing::{take_axis, IndexOp, NewAxis};
        use mlx_rs::ops::{arange, concatenate_axis};

        BATCH_RUN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
            eprintln!("mlx batched decode: B={}", initial.len());
        }

        let ceiling = std::env::var("ROZUM_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_OUTPUT_CEILING);
        let mut deferred: Vec<Job> = Vec::new();

        // 1. Prefill each request separately (correct per-sequence KV, no padding).
        let mut seqs: Vec<BatchSeq> = Vec::new();
        let mut caches: Vec<Vec<Option<ConcatKeyValueCache>>> = Vec::new();
        let mut logit_rows: Vec<Array> = Vec::new();
        for job in initial {
            if let Some((seq, cache, row)) =
                prefill_job_dense(model, tokenizer, template, ceiling, job)
            {
                logit_rows.push(row);
                caches.push(cache);
                seqs.push(seq);
            }
        }
        if seqs.is_empty() {
            return deferred;
        }

        // 2. Assemble one left-padded batched cache (rows right-aligned at `width`). Each
        //    row's pad `width − len_i` stays invariant as decode advances (both grow by 1).
        let max_l = seqs.iter().map(|s| s.prompt_len).max().unwrap();
        let mut width = max_l;
        let mut pads: Vec<i32> = seqs.iter().map(|s| max_l - s.prompt_len).collect();
        let n_layers = caches[0].len();
        let mut bcache: Vec<Option<ConcatKeyValueCache>> = Vec::with_capacity(n_layers);
        for l in 0..n_layers {
            let mut ks: Vec<Array> = Vec::with_capacity(seqs.len());
            let mut vs: Vec<Array> = Vec::with_capacity(seqs.len());
            for (c, &pad) in caches.iter().zip(&pads) {
                let (k, v, _) = c[l].as_ref().unwrap().kv_used().unwrap();
                ks.push(lpad_seq(&k, pad));
                vs.push(lpad_seq(&v, pad));
            }
            let kr: Vec<&Array> = ks.iter().collect();
            let vr: Vec<&Array> = vs.iter().collect();
            bcache.push(Some(ConcatKeyValueCache::from_kv(
                concatenate_axis(&kr, 0).unwrap(),
                concatenate_axis(&vr, 0).unwrap(),
                width,
            )));
        }
        drop(caches);
        let lr: Vec<&Array> = logit_rows.iter().collect();
        let mut logits = concatenate_axis(&lr, 0).unwrap(); // [B, vocab]
        drop(logit_rows);

        // 3. Continuous decode loop: sample per row, stream, retire finished rows, ADMIT
        //    queued greedy jobs into freed/spare slots, then forward the live rows.
        let mut batch_seq: Vec<usize> = (0..seqs.len()).collect(); // seq index per live row
        note_batch_rows(batch_seq.len(), batch_seq.len());
        loop {
            // Sample one token per row, each honoring its own temp/top_k/top_p.
            let mut temps: Vec<f32> = Vec::with_capacity(batch_seq.len());
            let mut topks: Vec<i32> = Vec::with_capacity(batch_seq.len());
            let mut topps: Vec<f32> = Vec::with_capacity(batch_seq.len());
            for &si in &batch_seq {
                let (t, k, p) = sampling_of(&seqs[si].job);
                temps.push(t);
                topks.push(k);
                topps.push(p);
            }
            let toks = sample_rows_vec(&logits, &temps, &topks, &topps);
            let mut keep_rows: Vec<usize> = Vec::new();
            let mut next_toks: Vec<u32> = Vec::new();
            for (row, &si) in batch_seq.iter().enumerate() {
                let tok = toks[row];
                if check_finish(&mut seqs[si], tok, eos, tokenizer) {
                    seqs[si].finished = true;
                    seqs[si].finalize();
                } else {
                    keep_rows.push(row);
                    next_toks.push(tok);
                }
            }
            // Retire finished rows: slice the batched cache + the row→seq map + pads.
            if keep_rows.len() != batch_seq.len() {
                let idx =
                    Array::from(&keep_rows.iter().map(|&r| r as i32).collect::<Vec<_>>()[..]);
                for c in bcache.iter_mut() {
                    let (k, v, off) = c.as_ref().unwrap().kv_used().unwrap();
                    *c = Some(ConcatKeyValueCache::from_kv(
                        take_axis(&k, &idx, 0).unwrap(),
                        take_axis(&v, &idx, 0).unwrap(),
                        off,
                    ));
                }
                batch_seq = keep_rows.iter().map(|&r| batch_seq[r]).collect();
                pads = keep_rows.iter().map(|&r| pads[r]).collect();
            }
            // Admit queued greedy jobs into free slots (continuous batching). Each new row
            // is prefilled (B=1), padded to `width` (growing it + re-padding existing rows
            // if the new prompt is longer), and stacked on the batch axis. Its first token
            // is sampled here and fed by the forward below — byte-exact to running alone.
            while batch_seq.len() < cap {
                let job = match jobs.try_recv() {
                    Ok(j) => j,
                    Err(_) => break,
                };
                if !is_batchable(&job) {
                    deferred.push(job);
                    continue;
                }
                let (mut nseq, ncache, nrow) =
                    match prefill_job_dense(model, tokenizer, template, ceiling, job) {
                        Some(t) => t,
                        None => continue,
                    };
                let (t, k, p) = sampling_of(&nseq.job);
                let ntok = sample_rows_vec(&nrow, &[t], &[k], &[p])[0];
                if check_finish(&mut nseq, ntok, eos, tokenizer) {
                    // First token already ends it — finalize, never enters the batch.
                    nseq.finished = true;
                    nseq.finalize();
                    continue;
                }
                let l_new = nseq.prompt_len;
                if l_new > width {
                    // New prompt is longer than the frontier: left-pad every existing row.
                    let extra = l_new - width;
                    for c in bcache.iter_mut() {
                        let (k, v, _) = c.as_ref().unwrap().kv_used().unwrap();
                        *c = Some(ConcatKeyValueCache::from_kv(
                            lpad_seq(&k, extra),
                            lpad_seq(&v, extra),
                            l_new,
                        ));
                    }
                    for p in pads.iter_mut() {
                        *p += extra;
                    }
                    width = l_new;
                }
                let pad_new = width - l_new;
                for (l, c) in bcache.iter_mut().enumerate() {
                    let (kn, vn, _) = ncache[l].as_ref().unwrap().kv_used().unwrap();
                    let (k, v, _) = c.as_ref().unwrap().kv_used().unwrap();
                    *c = Some(ConcatKeyValueCache::from_kv(
                        concatenate_axis(&[&k, &lpad_seq(&kn, pad_new)], 0).unwrap(),
                        concatenate_axis(&[&v, &lpad_seq(&vn, pad_new)], 0).unwrap(),
                        width,
                    ));
                }
                BATCH_ADMIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                seqs.push(nseq);
                batch_seq.push(seqs.len() - 1);
                pads.push(pad_new);
                next_toks.push(ntok);
                note_batch_rows(1, batch_seq.len());
            }
            if batch_seq.is_empty() {
                break;
            }
            // Forward the live tokens with per-row rope offset + per-row pad mask.
            let pad_off = Array::from(&pads[..]);
            let y = Array::from(&next_toks[..])
                .reshape(&[next_toks.len() as i32, 1])
                .unwrap();
            let k_cur = width + 1;
            let kidx = arange::<_, i32>(0, k_cur, None).unwrap().index((NewAxis, ..));
            let padd = pad_off.index((.., NewAxis));
            let dec_mask = kidx.ge(&padd).unwrap().index((.., NewAxis, NewAxis, ..));
            // Set the per-row offsets on every dense arch's thread-local — only the loaded
            // model's attention reads its own, so the extra setters are harmless no-ops.
            llama::set_batch_pad_offsets(Some(pad_off.clone()));
            qwen2::set_batch_pad_offsets(Some(pad_off.clone()));
            gemma3::set_batch_pad_offsets(Some(pad_off.clone()));
            set_batch_pad_offsets(Some(pad_off));
            let out = dense_forward(model, &y, Some(&dec_mask), &mut bcache);
            set_batch_pad_offsets(None);
            llama::set_batch_pad_offsets(None);
            qwen2::set_batch_pad_offsets(None);
            gemma3::set_batch_pad_offsets(None);
            logits = match out {
                Ok(l) => l.index((.., -1, ..)),
                Err(e) => {
                    for &si in &batch_seq {
                        let _ = seqs[si].job.events.send(Err(ModelError::BackendUnavailable(
                            format!("mlx: {e}"),
                        )));
                    }
                    return deferred;
                }
            };
            width += 1;
        }
        deferred
    }

    /// Hybrid (Qwen3.6) batched-decode dispatch. `Qwen35` and `Qwen35Moe` share
    /// `qwen3_5::LayerCache` and the same `Model::{init_cache, prefill, forward}` API,
    /// so one batched path serves both — only the concrete model call differs.
    fn hybrid_init_cache(model: &LoadedModel) -> Vec<qwen3_5::LayerCache> {
        match model {
            LoadedModel::Qwen35(m) => m.init_cache(),
            LoadedModel::Qwen35Moe(m) => m.init_cache(),
            _ => Vec::new(),
        }
    }

    fn hybrid_prefill(
        model: &mut LoadedModel,
        prompt: &Array,
        cache: &mut [qwen3_5::LayerCache],
    ) -> Result<Array, Exception> {
        match model {
            LoadedModel::Qwen35(m) => m.prefill(prompt, cache),
            LoadedModel::Qwen35Moe(m) => m.prefill(prompt, cache),
            _ => Err(Exception::custom("hybrid_prefill: non-hybrid arch")),
        }
    }

    fn hybrid_forward(
        model: &mut LoadedModel,
        inp: &Array,
        cache: &mut [qwen3_5::LayerCache],
    ) -> Result<Array, Exception> {
        match model {
            LoadedModel::Qwen35(m) => m.forward(inp, cache),
            LoadedModel::Qwen35Moe(m) => m.forward(inp, cache),
            _ => Err(Exception::custom("hybrid_forward: non-hybrid arch")),
        }
    }

    /// Render + prefill ONE job on the hybrid path, building its `BatchSeq` + heterogeneous
    /// `LayerCache` + last-position logits `[1, vocab]`. Materializes the cache (eval) so the
    /// deferred graph doesn't span sequences. Shared by the initial batch fill and
    /// continuous mid-decode admission. `None` (after an error event) on render/forward error.
    fn prefill_job_hybrid(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        ceiling: usize,
        job: Job,
    ) -> Option<(BatchSeq, Vec<qwen3_5::LayerCache>, Array)> {
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        let prompt_ids =
            match render_prompt(tokenizer, template, &job.model_id, &job.messages, &job.tools) {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = job.events.send(Err(ModelError::BackendUnavailable(e)));
                    return None;
                }
            };
        let max_tokens = {
            let want = job.sampling.max_tokens.map(|m| m as usize).unwrap_or(DEFAULT_MAX_TOKENS);
            if ceiling == 0 { want } else { want.min(ceiling) }
        };
        let prompt = Array::from(&prompt_ids[..]).index(NewAxis);
        let mut cache = hybrid_init_cache(model);
        let logits = match hybrid_prefill(model, &prompt, &mut cache) {
            Ok(l) => l,
            Err(e) => {
                let _ = job
                    .events
                    .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                return None;
            }
        };
        let row = logits.index((.., -1, ..)); // [1, vocab]
        let mut to_eval: Vec<&Array> = vec![&row];
        for c in cache.iter() {
            c.collect_eval(&mut to_eval);
        }
        let _ = mlx_rs::transforms::eval(to_eval);
        let seq = BatchSeq {
            job,
            out_ids: Vec::new(),
            emitted: String::new(),
            full_text: String::new(),
            tool_seen: false,
            output_tokens: 0,
            prompt_len: prompt_ids.len() as i32,
            max_tokens,
            finished: false,
            stop: StopReason::EndTurn,
        };
        Some((seq, cache, row))
    }

    /// Hybrid (Qwen3.6 dense + MoE) batched decode. Mirror of [`run_batch`], but over the
    /// heterogeneous `qwen3_5::LayerCache`: `Full` (attention) layers batch exactly like
    /// the dense path (left-pad each row's KV to `maxL`, per-row RoPE + key-pad mask),
    /// while `Linear` (GatedDeltaNet) layers just STACK each row's fixed-size conv +
    /// recurrent state on the batch axis — no padding/rope/mask, because the recurrence
    /// is row-independent (proven byte-exact in `gated_delta_batches_row_independent`).
    /// Each sequence is prefilled SEPARATELY so no pad token ever advances the recurrence.
    fn run_batch_hybrid(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        initial: Vec<Job>,
        jobs: &mut mpsc::UnboundedReceiver<Job>,
        cap: usize,
    ) -> Vec<Job> {
        use mlx_lm::models::qwen3_5::{set_batch_pad_mask, set_batch_pad_offsets, LayerCache};
        use mlx_rs::ops::indexing::{take_axis, IndexOp, NewAxis};
        use mlx_rs::ops::{arange, concatenate_axis};

        BATCH_RUN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
            eprintln!("mlx batched decode (hybrid): B={}", initial.len());
        }

        let ceiling = std::env::var("ROZUM_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_OUTPUT_CEILING);
        let mut deferred: Vec<Job> = Vec::new();

        // 1. Prefill each request separately (correct per-sequence KV + recurrent state,
        //    no padding through the GatedDeltaNet).
        let mut seqs: Vec<BatchSeq> = Vec::new();
        let mut caches: Vec<Vec<LayerCache>> = Vec::new();
        let mut logit_rows: Vec<Array> = Vec::new();
        for job in initial {
            if let Some((seq, cache, row)) =
                prefill_job_hybrid(model, tokenizer, template, ceiling, job)
            {
                logit_rows.push(row);
                caches.push(cache);
                seqs.push(seq);
            }
        }
        if seqs.is_empty() {
            return deferred;
        }

        // Stack a new row's per-layer cache onto the batch axis of `bcache`, growing the
        // shared width (left-padding the `Full` KV of existing rows) when the new prompt is
        // longer. `Linear` layers just concat the fixed-size conv/recurrent state — no pad.
        fn insert_hybrid_row(
            bcache: &mut [LayerCache],
            ncache: &[LayerCache],
            width: &mut i32,
            pads: &mut [i32],
            l_new: i32,
        ) {
            if l_new > *width {
                let extra = l_new - *width;
                for c in bcache.iter_mut() {
                    if let LayerCache::Full(kv) = c {
                        let (k, v, _) = kv.kv_used().unwrap();
                        *kv = ConcatKeyValueCache::from_kv(
                            lpad_seq(&k, extra),
                            lpad_seq(&v, extra),
                            l_new,
                        );
                    }
                }
                for p in pads.iter_mut() {
                    *p += extra;
                }
                *width = l_new;
            }
            let pad_new = *width - l_new;
            for (l, c) in bcache.iter_mut().enumerate() {
                match c {
                    LayerCache::Full(kv) => {
                        if let LayerCache::Full(nkv) = &ncache[l] {
                            let (kn, vn, _) = nkv.kv_used().unwrap();
                            let (k, v, _) = kv.kv_used().unwrap();
                            *kv = ConcatKeyValueCache::from_kv(
                                concatenate_axis(&[&k, &lpad_seq(&kn, pad_new)], 0).unwrap(),
                                concatenate_axis(&[&v, &lpad_seq(&vn, pad_new)], 0).unwrap(),
                                *width,
                            );
                        }
                    }
                    LayerCache::Linear { conv, state } => {
                        if let LayerCache::Linear { conv: nc, state: ns } = &ncache[l] {
                            *conv = Some(
                                concatenate_axis(&[conv.as_ref().unwrap(), nc.as_ref().unwrap()], 0)
                                    .unwrap(),
                            );
                            *state = Some(
                                concatenate_axis(
                                    &[state.as_ref().unwrap(), ns.as_ref().unwrap()],
                                    0,
                                )
                                .unwrap(),
                            );
                        }
                    }
                }
            }
        }

        // 2. Assemble one batched heterogeneous cache. Full → left-pad+stack KV (rows
        //    right-aligned at `width`); Linear → stack the fixed-size conv + recurrent state.
        let max_l = seqs.iter().map(|s| s.prompt_len).max().unwrap();
        let mut width = max_l;
        let mut pads: Vec<i32> = seqs.iter().map(|s| max_l - s.prompt_len).collect();
        let stack0 = |arrs: &[Array]| -> Array {
            let r: Vec<&Array> = arrs.iter().collect();
            concatenate_axis(&r, 0).unwrap()
        };
        let n_layers = caches[0].len();
        let mut bcache: Vec<LayerCache> = Vec::with_capacity(n_layers);
        for l in 0..n_layers {
            match &caches[0][l] {
                LayerCache::Full(_) => {
                    let mut ks: Vec<Array> = Vec::with_capacity(seqs.len());
                    let mut vs: Vec<Array> = Vec::with_capacity(seqs.len());
                    for (c, &pad) in caches.iter().zip(&pads) {
                        if let LayerCache::Full(kv) = &c[l] {
                            let (k, v, _) = kv.kv_used().unwrap();
                            ks.push(lpad_seq(&k, pad));
                            vs.push(lpad_seq(&v, pad));
                        }
                    }
                    bcache.push(LayerCache::Full(ConcatKeyValueCache::from_kv(
                        stack0(&ks),
                        stack0(&vs),
                        width,
                    )));
                }
                LayerCache::Linear { .. } => {
                    let mut convs: Vec<Array> = Vec::with_capacity(seqs.len());
                    let mut states: Vec<Array> = Vec::with_capacity(seqs.len());
                    for c in caches.iter() {
                        if let LayerCache::Linear { conv, state } = &c[l] {
                            convs.push(conv.clone().expect("prefilled conv state"));
                            states.push(state.clone().expect("prefilled recurrent state"));
                        }
                    }
                    bcache.push(LayerCache::Linear {
                        conv: Some(stack0(&convs)),
                        state: Some(stack0(&states)),
                    });
                }
            }
        }
        drop(caches);
        let lr: Vec<&Array> = logit_rows.iter().collect();
        let mut logits = concatenate_axis(&lr, 0).unwrap(); // [B, vocab]
        drop(logit_rows);

        // 3. Continuous decode loop: per-row argmax, stream, retire finished rows, ADMIT
        //    queued greedy jobs into freed/spare slots, then forward the live rows.
        let mut batch_seq: Vec<usize> = (0..seqs.len()).collect();
        note_batch_rows(batch_seq.len(), batch_seq.len());
        loop {
            // Sample one token per row, each honoring its own temp/top_k/top_p.
            let mut temps: Vec<f32> = Vec::with_capacity(batch_seq.len());
            let mut topks: Vec<i32> = Vec::with_capacity(batch_seq.len());
            let mut topps: Vec<f32> = Vec::with_capacity(batch_seq.len());
            for &si in &batch_seq {
                let (t, k, p) = sampling_of(&seqs[si].job);
                temps.push(t);
                topks.push(k);
                topps.push(p);
            }
            let toks = sample_rows_vec(&logits, &temps, &topks, &topps);
            let mut keep_rows: Vec<usize> = Vec::new();
            let mut next_toks: Vec<u32> = Vec::new();
            for (row, &si) in batch_seq.iter().enumerate() {
                let tok = toks[row];
                if check_finish(&mut seqs[si], tok, eos, tokenizer) {
                    seqs[si].finished = true;
                    seqs[si].finalize();
                } else {
                    keep_rows.push(row);
                    next_toks.push(tok);
                }
            }
            // Retire finished rows: slice both cache kinds on the batch axis + pads.
            if keep_rows.len() != batch_seq.len() {
                let idx =
                    Array::from(&keep_rows.iter().map(|&r| r as i32).collect::<Vec<_>>()[..]);
                for c in bcache.iter_mut() {
                    match c {
                        LayerCache::Full(kv) => {
                            let (k, v, off) = kv.kv_used().unwrap();
                            *kv = ConcatKeyValueCache::from_kv(
                                take_axis(&k, &idx, 0).unwrap(),
                                take_axis(&v, &idx, 0).unwrap(),
                                off,
                            );
                        }
                        LayerCache::Linear { conv, state } => {
                            *conv = conv.as_ref().map(|a| take_axis(a, &idx, 0).unwrap());
                            *state = state.as_ref().map(|a| take_axis(a, &idx, 0).unwrap());
                        }
                    }
                }
                batch_seq = keep_rows.iter().map(|&r| batch_seq[r]).collect();
                pads = keep_rows.iter().map(|&r| pads[r]).collect();
            }
            // Admit queued greedy jobs into free slots (continuous batching).
            while batch_seq.len() < cap {
                let job = match jobs.try_recv() {
                    Ok(j) => j,
                    Err(_) => break,
                };
                if !is_batchable(&job) {
                    deferred.push(job);
                    continue;
                }
                let (mut nseq, ncache, nrow) =
                    match prefill_job_hybrid(model, tokenizer, template, ceiling, job) {
                        Some(t) => t,
                        None => continue,
                    };
                let (t, k, p) = sampling_of(&nseq.job);
                let ntok = sample_rows_vec(&nrow, &[t], &[k], &[p])[0];
                if check_finish(&mut nseq, ntok, eos, tokenizer) {
                    nseq.finished = true;
                    nseq.finalize();
                    continue;
                }
                let l_new = nseq.prompt_len;
                insert_hybrid_row(&mut bcache, &ncache, &mut width, &mut pads, l_new);
                BATCH_ADMIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                seqs.push(nseq);
                batch_seq.push(seqs.len() - 1);
                next_toks.push(ntok);
                note_batch_rows(1, batch_seq.len());
            }
            if batch_seq.is_empty() {
                break;
            }
            // Forward the live tokens with per-row rope offset + per-row key-pad mask
            // (the GatedDeltaNet layers ignore both — fixed-size per-row state).
            let pad_off = Array::from(&pads[..]);
            let y = Array::from(&next_toks[..])
                .reshape(&[next_toks.len() as i32, 1])
                .unwrap();
            let k_cur = width + 1;
            let kidx = arange::<_, i32>(0, k_cur, None).unwrap().index((NewAxis, ..));
            let padd = pad_off.index((.., NewAxis));
            let dec_mask = kidx.ge(&padd).unwrap().index((.., NewAxis, NewAxis, ..));
            set_batch_pad_offsets(Some(pad_off));
            set_batch_pad_mask(Some(dec_mask));
            let out = hybrid_forward(model, &y, &mut bcache);
            set_batch_pad_offsets(None);
            set_batch_pad_mask(None);
            logits = match out {
                Ok(l) => l.index((.., -1, ..)),
                Err(e) => {
                    for &si in &batch_seq {
                        let _ = seqs[si].job.events.send(Err(ModelError::BackendUnavailable(
                            format!("mlx: {e}"),
                        )));
                    }
                    return deferred;
                }
            };
            width += 1;
        }
        deferred
    }

    const TOOL_OPEN: &str = "<tool_call>";
    const TOOL_CLOSE: &str = "</tool_call>";

    // Tool-call parsing (`<tool_call>` JSON/XML + the bare/```json fallback for
    // models that emit no envelope) lives in the shared `crate::serving` module;
    // call sites use `crate::serving::parse_tool_calls` directly. `TOOL_OPEN` /
    // `TOOL_CLOSE` stay here for the streaming suppression + constrained-decode paths.

    /// Architecture-agnostic streaming loop: pull tokens off a `Generate`
    /// iterator, force per-token compute, stop on EOS / max-tokens / cancel,
    /// and emit UTF-8-safe text deltas. Once a `<tool_call>` opener appears, text
    /// streaming stops and the run is parsed into `ToolUse*` events at the end.
    /// A send error means the client dropped the stream -> stop early.
    /// Drive the token iterator, streaming `ChatEvent`s to the client. Returns the
    /// (now exhausted) iterator so a hybrid caller can reclaim its internal cache +
    /// prefill snapshot for prefix reuse (`into_cache_and_snapshot`); dense callers
    /// drop it (releasing the external-cache borrow).
    /// Wraps an MLX token iterator (`Generate`) as a `u32` producer for the shared
    /// [`crate::engine::consume_tokens`], preserving the `async_eval`-lookahead
    /// pipelining: build step n+1's graph + `async_eval` it BEFORE blocking on n's
    /// readback, so the GPU never idles (byte-identical output; only eval timing).
    /// `pipeline=false` (hybrid custom-kernel arches whose per-call `eval` already
    /// blocks the forward) fetches the next token **lazily** — only when the consumer
    /// asks — so a stop (EOS) never computes an extra forward (which would desync the
    /// hybrid prefix-reuse cache). Errors surface as `Err(String)`; [`into_inner`]
    /// returns the drained iterator for hybrid cache/snapshot reclaim.
    struct PipelinedIds<I> {
        iter: I,
        cur: Option<Array>,
        pending_err: Option<String>,
        pipeline: bool,
        needs_fetch: bool,
    }

    impl<I> PipelinedIds<I>
    where
        I: Iterator<Item = Result<Array, Exception>>,
    {
        fn new(mut iter: I, pipeline: bool) -> Self {
            // Prime `cur` with the first token + `async_eval` it (as the old loop did,
            // unconditionally). Hybrid `Generate` returns None when cancelled mid-prefill.
            let (cur, pending_err) = match iter.next() {
                Some(Ok(t)) => {
                    let _ = mlx_rs::transforms::async_eval([&t]);
                    (Some(t), None)
                }
                Some(Err(e)) => (None, Some(format!("mlx: {e}"))),
                None => (None, None),
            };
            Self { iter, cur, pending_err, pipeline, needs_fetch: false }
        }

        fn into_inner(self) -> I {
            self.iter
        }

        /// Pull the next token; `prefetch` kicks off its GPU work now so the GPU stays
        /// fed while we block reading the current one. Streams errors via `pending_err`.
        fn pull(&mut self, prefetch: bool) -> Option<Array> {
            match self.iter.next() {
                Some(Ok(t)) => {
                    if prefetch {
                        let _ = mlx_rs::transforms::async_eval([&t]);
                    }
                    Some(t)
                }
                Some(Err(e)) => {
                    self.pending_err = Some(format!("mlx: {e}"));
                    None
                }
                None => None,
            }
        }
    }

    impl<I> Iterator for PipelinedIds<I>
    where
        I: Iterator<Item = Result<Array, Exception>>,
    {
        type Item = Result<u32, String>;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(e) = self.pending_err.take() {
                return Some(Err(e));
            }
            // Serial: fetch the current token lazily (deferred from the previous call),
            // so a stop on the previous token never computed this one.
            if !self.pipeline && self.needs_fetch {
                self.cur = self.pull(false);
                self.needs_fetch = false;
                if let Some(e) = self.pending_err.take() {
                    return Some(Err(e));
                }
            }
            let token = self.cur.take()?;
            if self.pipeline {
                // Pre-build + async_eval the NEXT token before blocking on the current.
                let next = self.pull(true);
                if eval([&token]).is_err() {
                    return Some(Err("mlx: eval failed".into()));
                }
                let id = token.item::<u32>();
                self.cur = next;
                Some(Ok(id))
            } else {
                if eval([&token]).is_err() {
                    return Some(Err("mlx: eval failed".into()));
                }
                let id = token.item::<u32>();
                self.needs_fetch = true; // fetch n+1 on the NEXT call, not now
                Some(Ok(id))
            }
        }
    }

    fn stream_generation<I>(
        generate: I,
        tokenizer: &mut Tokenizer,
        eos: &[u32],
        prompt_len: usize,
        max_tokens: usize,
        job: &Job,
        pipeline: bool,
        harmony: bool,
    ) -> I
    where
        I: Iterator<Item = Result<Array, Exception>>,
    {
        // A2b (native-engine-spi): the engine-agnostic detok->event + finalize half
        // now lives in `crate::engine::consume_tokens`. Here we only PRODUCE token
        // ids -- `PipelinedIds` wraps the MLX `Generate` iterator, keeping the
        // `async_eval`-lookahead pipelining and yielding `u32`s -- and return the
        // (drained) iterator so a hybrid caller can reclaim its cache + snapshot.
        let repeat_guard = !matches!(std::env::var("ROZUM_REPEAT_GUARD").as_deref(), Ok("0"));
        let meta = crate::engine::EngineMeta {
            n_ctx: 0,
            eos: eos.to_vec(),
            model_type: String::new(),
            harmony,
        };
        let mut ids = PipelinedIds::new(generate, pipeline);
        crate::engine::consume_tokens(
            &mut ids,
            &meta,
            prompt_len,
            max_tokens,
            repeat_guard,
            &job.cancel,
            |slice, skip| tokenizer.decode(slice, skip).ok(),
            |ev| job.events.send(ev).is_ok(),
        );
        ids.into_inner()
    }

    /// Apply the model's chat template to the messages and tokenize, returning
    /// the prompt token ids (with the generation prompt appended).
    /// OpenAI-style tool schemas (`{type:"function", function:{name, description,
    /// parameters}}`) for the chat template's `tools` variable. `None` if empty.
    fn tools_json(tools: &[crate::backend::ToolDef]) -> Option<serde_json::Value> {
        // An EMPTY list, not `None`, for no tools — matching transformers. Some chat
        // templates (Qwen3-Coder) do `tools | length` / `tools is defined` and break
        // on a null; truthiness-guarded templates (`{% if tools %}`) treat `[]` as
        // no-tools identically.
        if tools.is_empty() {
            return Some(serde_json::Value::Array(vec![]));
        }
        Some(serde_json::Value::Array(
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        ))
    }

    thread_local! {
        /// The model's BOS token string (from tokenizer_config.json), exposed to chat
        /// templates that emit it themselves via `{{ bos_token }}` (Gemma). Without it the
        /// template renders an empty BOS and a BOS-sensitive model produces garbage. Set
        /// once at worker startup (single worker thread → thread-local is fine).
        static MODEL_BOS_TOKEN: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Read `bos_token` from a tokenizer_config.json (a plain string or a `{content}` object).
    fn read_bos_token(path: &Path) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        match v.get("bos_token")? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => {
                o.get("content").and_then(|c| c.as_str()).map(String::from)
            }
            _ => None,
        }
    }

    fn render_prompt(
        tokenizer: &mut Tokenizer,
        template: &str,
        model_id: &str,
        messages: &[Message],
        tools: &[crate::backend::ToolDef],
    ) -> Result<Vec<u32>, String> {
        render_prompt_opt(tokenizer, template, model_id, messages, tools, true)
    }

    /// `render_prompt` with control over the trailing generation prompt. Rendering
    /// with `add_gen=false` gives the **conversation boundary** length — the prefix
    /// that recurs across agentic turns — which prefix reuse keys on (the generation
    /// prompt, esp. the thinking-off `<think></think>` prefill, differs from how the
    /// same turn is later rendered as a completed message, so it must be excluded).
    fn render_prompt_opt(
        tokenizer: &mut Tokenizer,
        template: &str,
        model_id: &str,
        messages: &[Message],
        tools: &[crate::backend::ToolDef],
        add_gen: bool,
    ) -> Result<Vec<u32>, String> {
        // gpt-oss's harmony template renders tool calls/results natively from
        // `message.tool_calls` (assistant) + the `tool` role, and raises if a tool
        // result has no preceding structured call. Detect it by its channel markers
        // and pass tool calls structurally instead of as Qwen `<tool_call>` text.
        let harmony = template.contains("<|channel|>");
        let convo: Vec<Conversation<&'static str, String>> = messages
            .iter()
            .map(|m| {
                if harmony {
                    harmony_conversation(m)
                } else {
                    Conversation {
                        role: role_str(&m.role),
                        content: message_text(m),
                        tool_calls: None,
                    }
                }
            })
            .collect();
        // Thinking is OFF by default (clean output for CC/Codex); the gateway's
        // `--enable-thinking` flag (or `ROZUM_ENABLE_THINKING`) turns it back on.
        // For a reasoning model this passes `enable_thinking=false` to the chat
        // template, which prefills a closed `<think></think>` so the OUTPUT is clean
        // (vs `/no_think`, which leaves an empty `<think></think>` in the output).
        let enable_thinking = std::env::var_os("ROZUM_ENABLE_THINKING").is_some();
        let args = ApplyChatTemplateArgs {
            conversations: vec![Chat::from(convo)],
            tools: tools_json(tools),
            documents: None,
            model_id,
            chat_template_id: None,
            add_generation_prompt: Some(add_gen),
            continue_final_message: None,
            enable_thinking: Some(enable_thinking),
            bos_token: MODEL_BOS_TOKEN.with(|c| c.borrow().clone()),
            eos_token: None,
        };
        let encodings = tokenizer
            .apply_chat_template_and_encode(template.to_string(), args)
            .map_err(|e| format!("mlx: chat template render: {e}"))?;
        let ids: Vec<u32> = encodings.iter().flat_map(|e| e.get_ids()).copied().collect();
        if harmony && add_gen && std::env::var_os("ROZUM_PROMPT_DUMP").is_some() {
            if let Ok(txt) = tokenizer.decode(&ids, false) {
                eprintln!("─── PROMPT_DUMP ({} tokens) ───\n{txt}\n─── /PROMPT_DUMP ───", ids.len());
            }
        }
        Ok(ids)
    }

    /// Test-only self-speculation harness for the dense spec-decode core. Loads
    /// `model_dir` three times — the SAME dense weights as (1) a plain-greedy
    /// reference, (2) a spec-decode target, (3) a spec-decode draft — so the draft
    /// is a perfect oracle. Renders `prompt`, then returns the reference greedy
    /// sequence, the spec-decode output, and the target forward count. The
    /// byte-identical contract holds iff `reference == spec_out` (proven against a
    /// real Metal forward: multi-token verify vs one-token-per-step greedy); the
    /// oracle drives `forwards` ≪ `reference.len()`. Loads are sequential (each
    /// model dropped before the next) so peak RAM stays near one model.
    #[cfg(test)]
    pub(crate) fn spec_decode_selftest(
        model_dir: &Path,
        model_id: &str,
        prompt: &str,
        k: usize,
        max_new: usize,
    ) -> (Vec<u32>, Vec<u32>, usize) {
        let (_n_ctx, eos, model_type, _kv) = read_config(model_dir);
        let mut tokenizer =
            Tokenizer::from_file(model_dir.join("tokenizer.json")).expect("tokenizer");
        let template = load_model_chat_template_from_file(model_dir.join("tokenizer_config.json"))
            .ok()
            .flatten()
            .or_else(|| std::fs::read_to_string(model_dir.join("chat_template.jinja")).ok())
            .expect("chat template");
        let req = ChatRequest::simple(prompt);
        let prompt_ids =
            render_prompt(&mut tokenizer, &template, model_id, &req.messages, &req.tools)
                .expect("render prompt");

        // Reference first (load → run → drop), then target + draft for the spec run.
        let reference = {
            let m = LoadedModel::load(&model_type, model_dir).expect("load reference");
            greedy_decode_dense(m, &prompt_ids, &eos, max_new)
        };
        let mut target = LoadedModel::load(&model_type, model_dir).expect("load target");
        let mut draft = LoadedModel::load(&model_type, model_dir).expect("load draft");
        let (spec_out, forwards) =
            run_spec_decode_dense(&mut target, &mut draft, &prompt_ids, &eos, k, max_new);
        (reference, spec_out, forwards)
    }

    pub use MlxNativeBackend as Export;
}

#[cfg(feature = "mlx-native")]
pub use inner::Export as MlxNativeBackend;

/// Process-wide batched-decode counters, for the gateway `/stats` endpoint. `runs` is the
/// number of batched-decode invocations (≥2 rows), `rows` the total rows they served (initial
/// members + mid-decode admits), `admits` the continuous mid-decode admissions, and `max` the
/// peak rows in a single batch. `rows / runs` ≈ average batch occupancy.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct BatchStats {
    pub runs: u64,
    pub rows: u64,
    pub admits: u64,
    pub max: u64,
}

/// Snapshot the global batched-decode counters. Returns `None` when nothing has batched yet
/// (so `/stats` can omit the section), else the running totals.
#[cfg(feature = "mlx-native")]
pub fn batch_stats() -> Option<BatchStats> {
    use std::sync::atomic::Ordering::Relaxed;
    let runs = inner::BATCH_RUN_COUNT.load(Relaxed) as u64;
    if runs == 0 {
        return None;
    }
    Some(BatchStats {
        runs,
        rows: inner::BATCH_ROWS_TOTAL.load(Relaxed) as u64,
        admits: inner::BATCH_ADMIT_COUNT.load(Relaxed) as u64,
        max: inner::BATCH_MAX.load(Relaxed) as u64,
    })
}

/// Without the `mlx-native` feature there is no batched decode → no stats.
#[cfg(not(feature = "mlx-native"))]
pub fn batch_stats() -> Option<BatchStats> {
    None
}

/// MLX Metal memory `(active, peak, cache)` in MB — the resident model's unified-memory
/// footprint (which process RSS does not capture). `active` drops to ~0 when the model is
/// unloaded. For the gateway `/stats` endpoint. `None` without the `mlx-native` feature.
#[cfg(feature = "mlx-native")]
pub fn mlx_memory_mb() -> Option<(u64, u64, u64)> {
    let mb = |b: usize| (b / (1024 * 1024)) as u64;
    Some((
        mb(mlx_rs::memory::get_active_memory()),
        mb(mlx_rs::memory::get_peak_memory()),
        mb(mlx_rs::memory::get_cache_memory()),
    ))
}

/// No MLX runtime without the feature → no Metal memory stats.
#[cfg(not(feature = "mlx-native"))]
pub fn mlx_memory_mb() -> Option<(u64, u64, u64)> {
    None
}

/// Map a model spec to its HuggingFace `org/name` repo id, or `None` if the spec
/// isn't an HF reference (a filesystem path, `lmstudio:`/`ollama:` spec, …).
pub fn spec_to_hf_repo(spec: &str) -> Option<String> {
    if std::path::Path::new(spec).exists() {
        return None;
    }
    if let Some(r) = spec.strip_prefix("mlx-community:") {
        Some(format!("mlx-community/{r}"))
    } else if let Some(r) = spec.strip_prefix("hf:") {
        Some(r.to_owned())
    } else if spec.contains('/') && !spec.starts_with('/') && !spec.contains(':') {
        // Bare `owner/repo`.
        Some(spec.to_owned())
    } else {
        None
    }
}

/// `model_type` values the native runtime can load (matches `LoadedModel::load`).
/// Used to reject an unsupported repo after its `config.json` is fetched, before
/// the multi-GB weights.
pub fn supported_model_type(model_type: &str) -> bool {
    matches!(
        model_type,
        "qwen3"
            | "qwen3_moe"
            | "qwen3_5"
            | "qwen3_5_text"
            | "qwen3_5_moe"
            | "qwen3_5_moe_text"
            | "gpt_oss"
            | "llama"
            | "mistral"
            | "phi3"
            | "gemma3"
            | "gemma3_text"
            | "qwen2"
    )
}

/// The effective `model_type` of a parsed `config.json` — top-level, or the
/// `text_config.model_type` of a multimodal wrapper (Qwen3.6 ships the latter).
fn config_model_type(cfg: &serde_json::Value) -> Option<&str> {
    cfg.get("model_type")
        .and_then(|v| v.as_str())
        .or_else(|| {
            cfg.get("text_config")
                .and_then(|t| t.get("model_type"))
                .and_then(|v| v.as_str())
        })
}

/// Download gate: accept only a `config.json` whose `model_type` the native
/// runtime can load (rejects an unsupported repo before its multi-GB weights).
fn model_type_gate(cfg: &serde_json::Value) -> Result<(), String> {
    match config_model_type(cfg) {
        Some(mt) if supported_model_type(mt) => Ok(()),
        Some(mt) => Err(format!(
            "native MLX does not support model_type '{mt}' (Qwen2/Qwen3/Qwen3.6/Llama)"
        )),
        None => Err("config.json has no model_type".to_owned()),
    }
}

/// Resolve a model spec to a local model dir, **downloading it if absent**.
///
/// Tries the local cache first (`resolve_model_dir`); on a miss, fetches the
/// snapshot from the matching hub — `modelscope:<owner>/<repo>` → ModelScope,
/// otherwise `mlx-community:` / `hf:` / `owner/repo` → HuggingFace — but only
/// after `config.json` confirms a supported `model_type`. Each writes the hub's
/// native cache layout so the download is shared with that hub's own tools.
/// Returns `None` (chain falls through) when the spec isn't a hub repo or the
/// download fails.
pub async fn ensure_model_dir(spec: &str) -> Option<std::path::PathBuf> {
    if let Some(dir) = resolve_model_dir(spec) {
        return Some(dir);
    }
    let result = if let Some(repo) = spec.strip_prefix("modelscope:") {
        crate::modelscope::ensure_snapshot(repo, model_type_gate).await
    } else {
        let repo = spec_to_hf_repo(spec)?;
        crate::hf_hub::ensure_snapshot(&repo, model_type_gate).await
    };
    match result {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("rozum mlx: auto-download of '{spec}' skipped: {e}");
            None
        }
    }
}

/// Resolve a model spec to a local directory of safetensors + tokenizer files
/// **already present** on disk (no download — see [`ensure_model_dir`]).
///
/// - an existing directory path -> as-is
/// - `mlx-community:<repo>` / `hf:<user>/<repo>` / `<user>/<repo>` -> the
///   downloaded HuggingFace cache snapshot, if present
///   (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<rev>/`).
///
/// Returns `None` when nothing local matches.
pub fn resolve_model_dir(spec: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let direct = PathBuf::from(spec);
    if direct.is_dir() && direct.join("config.json").is_file() {
        return Some(direct);
    }

    // ModelScope specs resolve to ModelScope's own (flat) cache dir.
    if let Some(r) = spec.strip_prefix("modelscope:") {
        let (owner, name) = r.split_once('/')?;
        let dir = crate::modelscope::model_cache_dir(owner, name)?;
        return dir.join("config.json").is_file().then_some(dir);
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

// Gated on the feature: these tests reach into the `mlx-native`-only `inner` module, so they only
// build when the runtime is compiled (keeps `--no-default-features` — the portable core — green).
#[cfg(all(test, feature = "mlx-native"))]
mod tests {
    use super::resolve_model_dir;

    #[test]
    fn resolve_missing_spec_is_none() {
        assert!(resolve_model_dir("definitely/not-a-real-model-xyzzy").is_none());
    }

    // Mistral / Mistral-Nemo route to the native MLX runtime via the Llama path (they ARE
    // the llama arch). Guards the catalog alias so `model_type: "mistral"` is admitted (and
    // doesn't regress to "unsupported", which would silently fall through to another backend).
    #[test]
    fn mistral_is_a_supported_model_type() {
        assert!(super::supported_model_type("mistral"));
        assert!(super::supported_model_type("llama"));
        assert!(super::supported_model_type("phi3"), "Phi-3 routes via the fused-split llama loader");
        assert!(!super::supported_model_type("mixtral"), "sparse MoE Mistral is a separate port");
    }

    // Regression guard: the hybrid (GatedDeltaNet) archs MUST map to retained MLX
    // command-buffer refs (`ROZUM_MLX_RETAIN`). Dropping one from the list silently
    // reverts the +2.7× decode win (and risks the token-2 garbage the retain fixes).
    #[test]
    fn hybrid_models_need_retain() {
        for t in ["qwen3_5", "qwen3_5_text", "qwen3_5_moe", "qwen3_5_moe_text"] {
            assert!(
                super::inner::is_hybrid_model(t),
                "{t} must be hybrid (needs ROZUM_MLX_RETAIN for correctness + speed)"
            );
        }
        for t in ["qwen3", "qwen3_moe", "llama", "mistral", "phi3", "gemma3_text", "qwen2", "qwen2_5"] {
            assert!(!super::inner::is_hybrid_model(t), "{t} is dense (unretained path)");
        }
    }


    // KV bytes/position: only full-attention layers count (hybrid uses
    // full_attention_interval), head_dim derived from hidden/heads if absent.
    #[cfg(feature = "mlx-native")]
    #[test]
    fn kv_bytes_per_position_estimate() {
        use super::inner::kv_bytes_per_position;
        // Hybrid wrapper: 64 layers, interval 4 -> 16 full-attn; kv heads 4,
        // head_dim 256, bf16. 2*16*4*256*2 = 65536.
        let hybrid = serde_json::json!({
            "text_config": {
                "num_hidden_layers": 64, "full_attention_interval": 4,
                "num_key_value_heads": 4, "head_dim": 256
            }
        });
        assert_eq!(kv_bytes_per_position(&hybrid), Some(65_536));
        // Dense (no interval -> all 28 layers), head_dim from hidden/heads.
        let dense = serde_json::json!({
            "num_hidden_layers": 28, "num_key_value_heads": 8,
            "hidden_size": 4096, "num_attention_heads": 32
        });
        // head_dim = 4096/32 = 128; 2*28*8*128*2 = 114688.
        assert_eq!(kv_bytes_per_position(&dense), Some(114_688));
        // Missing fields -> None.
        assert_eq!(kv_bytes_per_position(&serde_json::json!({})), None);
    }

    // (The deterministic `<tool_call>` parser checks now live in `serving.rs`.)

    // An assistant ToolUse block must survive `message_text` as `<tool_call>`
    // markup so multi-turn tool loops keep the prior call in history. This is
    // the inverse of `crate::serving::parse_tool_calls` — render then re-parse round-trips.
    #[test]
    fn tool_use_round_trips_into_history() {
        use super::inner::message_text;
        use crate::serving::parse_tool_calls;
        use crate::backend::{ContentBlock, Message, Role};

        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({ "city": "Paris" }),
            }],
        };
        let rendered = message_text(&msg);
        assert!(rendered.contains("<tool_call>"), "rendered: {rendered}");

        let calls = parse_tool_calls(&rendered);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "get_weather");
        assert!(calls[0].1.contains("Paris"), "args: {}", calls[0].1);
    }

    #[test]
    fn find_loose_tool_json_locates_envelope() {
        use super::inner::find_loose_tool_json;
        // Bare and ```json-fenced tool-call JSON → offset of the `{`.
        assert_eq!(find_loose_tool_json(r#"{"name":"Write","arguments":{}}"#), Some(0));
        let fenced = "I'll write it.\n```json\n{\n  \"name\": \"Write\"\n}\n```";
        let off = find_loose_tool_json(fenced).unwrap();
        assert_eq!(&fenced[off..off + 1], "{");
        // A `{` in ordinary prose / a code example (no leading "name") is NOT a tool call.
        assert_eq!(find_loose_tool_json("the struct is { x: 1 }"), None);
        assert_eq!(find_loose_tool_json(r#"{"file_path":"a","content":"b"}"#), None);
    }

    #[test]
    fn tool_markup_suppression_points() {
        use super::inner::tool_markup_at;
        // Native <tool_call> — suppressed regardless of tools.
        assert_eq!(tool_markup_at("hi <tool_call>{}", false), Some(3));
        // Loose ```json fence (tools offered) — suppress from the fence.
        let fenced = "I'll write it.\n```json\n{\"name\":\"Write\"}";
        assert_eq!(tool_markup_at(fenced, true), fenced.find("```"));
        // Bare {"name" — suppress from the brace.
        assert_eq!(tool_markup_at("ok {\"name\":\"x\"}", true), Some(3));
        // No tools offered → a loose json is NOT treated as a call.
        assert_eq!(tool_markup_at("```json\n{\"name\":\"x\"}", false), None);
        // A `{` in prose / a code example is not a call.
        assert_eq!(tool_markup_at("returns { x: 1 }", true), None);
    }

    #[test]
    fn extract_tool_name_from_partial_envelope() {
        use super::inner::extract_tool_name;
        // Complete name literal → extracted.
        assert_eq!(
            extract_tool_name(r#"{"name": "get_weather", "arguments": {"#).as_deref(),
            Some("get_weather")
        );
        // Name not finished yet → None (don't resolve the args schema early).
        assert_eq!(extract_tool_name(r#"{"name": "get_wea"#), None);
        // Object opened but no name key yet → None.
        assert_eq!(extract_tool_name(r#"{"#), None);
        // Tight spacing.
        assert_eq!(
            extract_tool_name(r#"{"name":"x","arguments":{}}"#).as_deref(),
            Some("x")
        );
    }

    // End-to-end proof the schema MASK bites: with `ROZUM_MLX_CONSTRAIN=1`, the tool's `unit`
    // enum is deliberately `["kelvin","rankine"]` — values a model would never volunteer for a
    // normal weather query (it wants celsius/fahrenheit, both forbidden here). The constrained
    // arguments must still be valid JSON, carry the required keys, AND use one of the odd enum
    // literals — which can only happen if the sampler was actually masked off the model's
    // preferred (invalid) tokens. Qwen3-4B (Hermes `<tool_call>` format). Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_constrained_tool_call_conforms
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: uses mlx-community/Qwen3-4B-4bit"]
    async fn mlx_constrained_tool_call_conforms() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatEvent, ChatRequest, SamplingParams, ToolDef};
        use futures::StreamExt;

        unsafe {
            std::env::set_var("ROZUM_MLX_CONSTRAIN", "1");
        }
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("qwen3 resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load");

        let tool = ToolDef {
            name: "get_weather".into(),
            description: "Get the current weather for a city.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"},
                    "unit": {"enum": ["kelvin", "rankine"]}
                },
                "required": ["location", "unit"]
            }),
        };
        let mut req =
            ChatRequest::simple("What is the weather in Paris in celsius? Call the get_weather tool.");
        req.tools = vec![tool];
        req.sampling = SamplingParams { max_tokens: Some(128), ..Default::default() };

        let mut stream = backend.chat(req).await.expect("chat");
        let (mut name, mut args) = (String::new(), String::new());
        while let Some(ev) = stream.next().await {
            match ev.expect("event") {
                ChatEvent::ToolUseStart { name: n, .. } => name = n,
                ChatEvent::ToolUseDelta { input_json_delta, .. } => args.push_str(&input_json_delta),
                ChatEvent::Done { .. } => break,
                _ => {}
            }
        }
        unsafe {
            std::env::remove_var("ROZUM_MLX_CONSTRAIN");
        }
        eprintln!("CONSTRAINED TOOL  name={name:?}  args={args:?}");
        assert_eq!(name, "get_weather", "the model must call the constrained tool");
        // The constraint guarantees: valid JSON, required keys present, enum honored.
        let v: serde_json::Value =
            serde_json::from_str(&args).expect("constrained args must be valid JSON");
        assert!(
            v.get("location").and_then(|x| x.as_str()).is_some(),
            "required string `location` missing: {v}"
        );
        let unit = v.get("unit").and_then(|x| x.as_str());
        assert!(
            matches!(unit, Some("kelvin") | Some("rankine")),
            "`unit` must be redirected to an enum literal (proves the mask bites): {v}"
        );
    }

    // Same constraint, but on the Qwen3.6 HYBRID path (`run_constrained_hybrid`, `LayerCache`) —
    // the user's primary tool-use model, which the dense constrained loop doesn't reach. Proves
    // the mask works over the GatedDeltaNet+attention cache too: the schema enum is again
    // `["kelvin","rankine"]` against a "celsius" prompt, so a conforming `unit` proves the mask
    // bit on the hybrid forward. Uses the cached 35B-A3B MoE (fast decode). Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_constrained_tool_call_hybrid
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "heavy: loads mlx-community/Qwen3.6-35B-A3B-4bit (~17GB)"]
    async fn mlx_constrained_tool_call_hybrid() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatEvent, ChatRequest, SamplingParams, ToolDef};
        use futures::StreamExt;

        unsafe {
            std::env::set_var("ROZUM_MLX_CONSTRAIN", "1");
        }
        let spec = "mlx-community:Qwen3.6-35B-A3B-4bit";
        let dir = ensure_model_dir(spec).await.expect("qwen3.6 resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load");

        let tool = ToolDef {
            name: "get_weather".into(),
            description: "Get the current weather for a city.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"},
                    "unit": {"enum": ["kelvin", "rankine"]}
                },
                "required": ["location", "unit"]
            }),
        };
        let mut req =
            ChatRequest::simple("What is the weather in Paris in celsius? Call the get_weather tool.");
        req.tools = vec![tool];
        req.sampling = SamplingParams { max_tokens: Some(128), ..Default::default() };

        let mut stream = backend.chat(req).await.expect("chat");
        let (mut name, mut args) = (String::new(), String::new());
        while let Some(ev) = stream.next().await {
            match ev.expect("event") {
                ChatEvent::ToolUseStart { name: n, .. } => name = n,
                ChatEvent::ToolUseDelta { input_json_delta, .. } => args.push_str(&input_json_delta),
                ChatEvent::Done { .. } => break,
                _ => {}
            }
        }
        unsafe {
            std::env::remove_var("ROZUM_MLX_CONSTRAIN");
        }
        eprintln!("CONSTRAINED TOOL (hybrid)  name={name:?}  args={args:?}");
        assert_eq!(name, "get_weather", "the hybrid model must call the constrained tool");
        let v: serde_json::Value =
            serde_json::from_str(&args).expect("constrained args must be valid JSON");
        assert!(
            v.get("location").and_then(|x| x.as_str()).is_some(),
            "required string `location` missing: {v}"
        );
        let unit = v.get("unit").and_then(|x| x.as_str());
        assert!(
            matches!(unit, Some("kelvin") | Some("rankine")),
            "`unit` must be redirected to an enum literal on the hybrid path: {v}"
        );
    }

    // Structured output (`response_format: json_schema`, no tools): the WHOLE response is
    // constrained to the schema during decode, so it parses + conforms. ALWAYS honored when
    // the request carries a `response_schema` (no env flag). Qwen3-4B. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_response_format_json_schema
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: uses mlx-community/Qwen3-4B-4bit"]
    async fn mlx_response_format_json_schema() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};

        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load");

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "country": {"type": "string"}
            },
            "required": ["city", "country"]
        });
        let mut req = ChatRequest::simple("Return the city Paris and its country as JSON.");
        req.sampling = SamplingParams {
            max_tokens: Some(64),
            response_schema: Some(schema),
            ..Default::default()
        };

        let text = collect_to_string(backend.chat(req).await.unwrap()).await.unwrap();
        eprintln!("RESPONSE_FORMAT OUTPUT: {text:?}");
        // Parse the first JSON value (tolerates any trailing tokens after completion).
        let v: serde_json::Value = serde_json::Deserializer::from_str(text.trim())
            .into_iter::<serde_json::Value>()
            .next()
            .expect("at least one JSON value")
            .expect("the constrained output must be valid JSON");
        assert!(
            v.get("city").and_then(|x| x.as_str()).is_some(),
            "required string `city` missing: {v}"
        );
        assert!(
            v.get("country").and_then(|x| x.as_str()).is_some(),
            "required string `country` missing: {v}"
        );
    }

    // Speculative decoding — the byte-identical contract on a REAL MLX dense
    // model. Self-speculation: target == draft == Qwen3-4B weights, so the draft
    // is a perfect oracle (maximal acceptance) AND the orchestrator's "emit only
    // the target's greedy tokens" invariant is exercised against a real Metal
    // forward (multi-token verify vs the reference's one-token-per-step greedy).
    // Proves the MLX verify/propose numerics before the agentic-matrix gate. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_spec_decode_byte_identical
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit three times (ref+target+draft)"]
    async fn mlx_spec_decode_byte_identical() {
        use super::ensure_model_dir;
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
        let (k, max_new) = (4usize, 64usize);
        let (reference, spec_out, forwards) = super::inner::spec_decode_selftest(
            &dir,
            &spec.replace(':', "/"),
            "Write a short haiku about the sea.",
            k,
            max_new,
        );
        // Longest common prefix with the sequential reference: the spec output
        // tracks the target's greedy decode.
        let lcp = reference.iter().zip(&spec_out).take_while(|(a, b)| a == b).count();
        eprintln!(
            "SPEC-DECODE  ref_len={} spec_len={} lcp={} target_forwards={} (plain-greedy = {})",
            reference.len(),
            spec_out.len(),
            lcp,
            forwards,
            reference.len()
        );
        // It is NOT bit-identical to the sequential reference on finite-precision
        // Metal: the verify forward batches `k+1` positions, so the target's KV is
        // built in a different shape than the reference's one-token-per-step decode,
        // and that float difference occasionally flips an argmax at a near-tie (the
        // same batched-vs-sequential class as chunked prefill). The orchestrator
        // emits ONLY the target's greedy tokens, so each output IS a valid greedy
        // decode of the target — identical in exact arithmetic (the mock unit test
        // proves that invariant). Here we assert the mechanism tracks the target on
        // a long prefix; a large early divergence would mean a real verify bug, not
        // a float tie. The functional gate is the agentic matrix (pass/fail
        // unchanged + tok/s up), per `docs/specs/speculative-decoding.md`.
        assert!(
            lcp * 2 >= reference.len(),
            "spec output diverges too early ({lcp} of {} tokens) — a verify bug, not a float tie",
            reference.len()
        );
        // Oracle draft ⇒ ≈len/(k+1) target forwards — the real speedup (+slack for
        // a tie-induced re-forward).
        assert!(
            forwards >= 1 && forwards <= reference.len().div_ceil(k + 1) + 2,
            "oracle draft should need ≈len/(k+1) target forwards: got {forwards} for {} tokens",
            reference.len()
        );
    }

    #[test]
    fn runaway_loop_detection() {
        use crate::engine::is_runaway_loop;
        // Below the window: never a loop yet.
        assert!(!is_runaway_loop(&[7u32; 10]));
        // Single-token spam (period 1) over the full window.
        assert!(is_runaway_loop(&[7u32; 80]));
        // A short repeating phrase (period 3) tiled past the window.
        let cycle: Vec<u32> = (0..80).map(|i| [11u32, 22, 33][i % 3]).collect();
        assert!(is_runaway_loop(&cycle));
        // Period 16 (the max we catch), repeated 5× = 80 tokens.
        let p16: Vec<u32> = (0..80).map(|i| (i % 16) as u32).collect();
        assert!(is_runaway_loop(&p16));
        // Real-looking, non-periodic text must NOT trigger (no false positive).
        let varied: Vec<u32> = (0..200).map(|i| (i * 2654435761u64 % 5003) as u32).collect();
        assert!(!is_runaway_loop(&varied));
        // A long non-repeating run that ends in a brief repeat (< window) is fine.
        let mut tail = varied.clone();
        tail.extend([9u32; 20]); // only 20 repeats, < REPEAT_WINDOW(64)
        assert!(!is_runaway_loop(&tail));
    }

    #[test]
    fn prefix_store_best_match() {
        use super::inner::PrefixStore;
        let m = |entries: &[Vec<u32>], ids: &[u32]| {
            PrefixStore::best_match(entries, ids, |v: &Vec<u32>| v.as_slice())
        };
        // Two interleaved "sessions": A = [1,2,3...], B = [1,9,...] (diverge at idx 1).
        let entries: Vec<Vec<u32>> = vec![vec![1, 2, 3], vec![1, 9]];
        // A's next turn extends A → matches entry 0, not B.
        assert_eq!(m(&entries, &[1, 2, 3, 4, 5]), Some(0));
        // B's next turn extends B → matches entry 1 (B is a prefix; A is not).
        assert_eq!(m(&entries, &[1, 9, 8, 7]), Some(1));
        // A different conversation that shares only [1] matches neither (no entry is
        // a full prefix) — it's a new session.
        assert_eq!(m(&entries, &[1, 0, 0]), None);
        // Longest-prefix wins when several entries match.
        let nested: Vec<Vec<u32>> = vec![vec![1], vec![1, 2], vec![1, 2, 3]];
        assert_eq!(m(&nested, &[1, 2, 3, 4]), Some(2));
        // An exact-length match is NOT reused (need a strict prefix to have a suffix).
        assert_eq!(m(&entries, &[1, 2, 3]), None);
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

    // Proves the worker-join `Drop` actually RECLAIMS the model's Metal memory (the
    // deterministic-unload guarantee). MLX weights live in unified-memory Metal buffers that
    // process RSS does NOT capture, so this measures MLX's OWN active-memory counter
    // (`mlx_rs::memory::get_active_memory`) before load, after load+chat, and after the
    // backend drops (Drop closes the channel + joins the worker → the model's arrays free).
    // The drop must give back most of what the load added — an idle-unload genuinely returns
    // the memory, not just marks the model gone. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_drop_reclaims_memory
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_drop_reclaims_memory() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};
        let active_mb = || (mlx_rs::memory::get_active_memory() / (1024 * 1024)) as u64;

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let before = active_mb();
        let after_load;
        {
            let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
                .await
                .expect("backend load");
            let _ = collect_to_string(
                backend
                    .chat(ChatRequest::simple("Hi. /no_think"))
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
            after_load = active_mb();
        } // backend drops here → Drop joins worker → MLX arrays freed
        let after_drop = active_mb();
        let added = after_load.saturating_sub(before);
        let freed = after_load.saturating_sub(after_drop);
        eprintln!(
            "MLX ACTIVE  before={before}MB  after_load={after_load}MB (+{added})  after_drop={after_drop}MB (freed {freed})"
        );
        // The 4-bit model is ~2.4 GB of Metal buffers; the load must be visible and the drop
        // must give back most of it (allocator may retain a little in its buffer cache).
        assert!(added > 1000, "model load should add >1GB active Metal memory (got +{added}MB)");
        assert!(
            freed as f64 > added as f64 * 0.8,
            "drop must reclaim most of the model's Metal memory: freed {freed}MB of +{added}MB"
        );
    }

    // Diagnostic: load a backend, FULLY drop it (Drop joins the worker), then load a
    // SECOND backend in the same process and chat. Characterizes whether sequential model
    // loads survive in one process — the model-unload/swap scenario (and why running two
    // model e2e tests together used to crash). Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_sequential_backend_loads
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_sequential_backend_loads() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};
        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let q = || ChatRequest::simple("What is the capital of France? Answer in one word. /no_think");
        // SAFETY: test process, set before the worker thread starts. Exercise a BATCHED run
        // on A (model-swap after batched decode is the real busy-gateway-then-unload path).
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        {
            let a = MlxNativeBackend::new(dir.clone(), "mlx-community/Qwen3-4B-4bit".into(), None)
                .await
                .expect("load A");
            let s1 = a.chat(q()).await.unwrap();
            let s2 = a.chat(q()).await.unwrap();
            let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
            let (t1, t2) = (t1.unwrap(), t2.unwrap());
            eprintln!("SEQ A (batched): {t1:?} {t2:?}");
            assert!(t1.contains("Paris") && t2.contains("Paris"), "A: {t1:?} {t2:?}");
        } // a dropped here → Drop joins the worker, frees the model
        eprintln!("SEQ: backend A dropped+joined; loading B...");
        let b = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("load B");
        let t = collect_to_string(b.chat(q()).await.unwrap()).await.unwrap();
        eprintln!("SEQ B: {t:?}");
        assert!(t.contains("Paris"), "B (second load in same process): {t:?}");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        eprintln!("SEQ: model swap after a batched run OK ✓");
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Prefix-KV reuse must be byte-exact: a turn-2 request that reuses turn-1's KV
    // prefix (same backend → same worker) must produce the IDENTICAL output to the
    // same turn-2 conversation prefilled fresh (a second, history-less backend).
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_prefix_reuse_byte_exact
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_prefix_reuse_byte_exact() {
        use super::MlxNativeBackend;
        use crate::backend::{
            ChatBackend, ChatRequest, ContentBlock, Message, Role, SamplingParams,
            collect_to_string,
        };
        use tokio_util::sync::CancellationToken;

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let asst = |s: &str| Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: s.into() }],
        };
        let req = |msgs: Vec<Message>| ChatRequest {
            messages: msgs,
            tools: vec![],
            sampling: SamplingParams { max_tokens: Some(24), ..Default::default() },
            cancel: CancellationToken::new(),
            session_id: None,
        };
        let q1 = "Name three primary colors, comma separated.";
        let q2 = "Now name three farm animals, comma separated.";

        // Backend A: turn 1 (populates the prefix), then turn 2 (REUSES it).
        let a = MlxNativeBackend::new(dir.clone(), "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("load A");
        let t1 = collect_to_string(a.chat(req(vec![Message::user(q1)])).await.unwrap())
            .await
            .unwrap();
        let convo2 = vec![Message::user(q1), asst(&t1), Message::user(q2)];
        let reuse = collect_to_string(a.chat(req(convo2.clone())).await.unwrap())
            .await
            .unwrap();

        // Backend B: same turn-2 conversation, no history → full fresh prefill.
        let b = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("load B");
        let fresh = collect_to_string(b.chat(req(convo2)).await.unwrap())
            .await
            .unwrap();

        eprintln!("REUSE: {reuse:?}\nFRESH: {fresh:?}");
        assert_eq!(reuse, fresh, "prefix-reuse output must byte-match a fresh prefill");
    }

    // Hybrid (Qwen3.6) prefix reuse must be byte-exact too: the recurrent GatedDeltaNet
    // `Linear` state restored from the end-of-prefill snapshot + the truncated `Full`
    // KV must reproduce a fresh prefill exactly. Uses the DENSE hybrid Qwen3.6-27B
    // (deterministic; the 35B-A3B MoE has greedy float-reduction nondeterminism that
    // would make a byte comparison flaky — the MoE shares this exact reuse logic). One
    // backend + `ROZUM_PREFIX_CACHE` toggle to keep memory to a single model load.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_prefix_reuse_byte_exact_hybrid
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3.6-27B-4bit"]
    async fn mlx_prefix_reuse_byte_exact_hybrid() {
        use super::MlxNativeBackend;
        use crate::backend::{
            ChatBackend, ChatRequest, ContentBlock, Message, Role, SamplingParams,
            collect_to_string,
        };
        use tokio_util::sync::CancellationToken;

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let asst = |s: &str| Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: s.into() }],
        };
        let req = |msgs: Vec<Message>| ChatRequest {
            messages: msgs,
            tools: vec![],
            sampling: SamplingParams { max_tokens: Some(24), ..Default::default() },
            cancel: CancellationToken::new(),
            session_id: None,
        };
        let q1 = "Name three primary colors, comma separated.";
        let q2 = "Now name three farm animals, comma separated.";

        let b = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-27B-4bit".into(), None)
            .await
            .expect("load");

        // Turn 1 populates the hybrid prefix; turn 2 REUSES it (truncate Full +
        // restore Linear from snapshot + prefill the suffix).
        unsafe { std::env::set_var("ROZUM_PREFIX_CACHE", "1") };
        let t1 = collect_to_string(b.chat(req(vec![Message::user(q1)])).await.unwrap())
            .await
            .unwrap();
        let convo2 = vec![Message::user(q1), asst(&t1), Message::user(q2)];
        let reuse = collect_to_string(b.chat(req(convo2.clone())).await.unwrap())
            .await
            .unwrap();

        // Same turn-2 conversation, prefix OFF → full fresh prefill (no reuse).
        unsafe { std::env::set_var("ROZUM_PREFIX_CACHE", "0") };
        let fresh = collect_to_string(b.chat(req(convo2)).await.unwrap())
            .await
            .unwrap();
        unsafe { std::env::remove_var("ROZUM_PREFIX_CACHE") };

        eprintln!("REUSE: {reuse:?}\nFRESH: {fresh:?}");
        assert_eq!(reuse, fresh, "hybrid prefix-reuse output must byte-match a fresh prefill");
    }

    // Prod-path perf through the FULL MlxNativeBackend.chat (tokenizer detok +
    // ChatEvent streaming) — the gateway's real path, not the raw model.forward
    // bench. Reports TTFT (prefill latency → first streamed token) and steady-state
    // decode t/s. `prompt_repeat` × ~13 tok sizes the prefill for a realistic TTFT.
    #[cfg(feature = "mlx-native")]
    async fn run_backend_chat_perf(
        spec: &str,
        model_id: &str,
        prompt_repeat: usize,
        max_tokens: u32,
    ) -> (f64, f64, u32) {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatEvent, ChatRequest};
        use futures::StreamExt;
        use std::time::Instant;

        let dir = resolve_model_dir(spec).unwrap_or_else(|| panic!("{spec} not in HF cache"));
        let backend = MlxNativeBackend::new(dir, model_id.into(), None)
            .await
            .expect("backend load");
        let sentence = "Explain in detail how transformer neural networks process a sequence. ";
        let mut req = ChatRequest::simple(sentence.repeat(prompt_repeat));
        req.sampling.max_tokens = Some(max_tokens);

        let t0 = Instant::now();
        let mut stream = backend.chat(req).await.expect("chat");
        let (mut first, mut last, mut out_tokens) = (None::<Instant>, Instant::now(), 0u32);
        while let Some(ev) = stream.next().await {
            match ev.expect("event") {
                ChatEvent::TextDelta { .. } => {
                    first.get_or_insert_with(Instant::now);
                    last = Instant::now();
                }
                ChatEvent::Done { output_tokens, .. } => out_tokens = output_tokens,
                _ => {}
            }
        }
        let ttft = first.map_or(0.0, |f| (f - t0).as_secs_f64());
        let decode_s = first.map_or(0.0, |f| (last - f).as_secs_f64());
        let tps = if decode_s > 0.0 {
            out_tokens as f64 / decode_s
        } else {
            0.0
        };
        eprintln!(
            "BACKEND.CHAT {model_id}: TTFT={ttft:.2}s (~{prompt_repeat} prompt-repeats)  decode={tps:.1} t/s  ({out_tokens} tok)"
        );
        (ttft, tps, out_tokens)
    }

    // MoE prod path (Qwen3.6-35B-A3B): ~96 t/s decode with hybrid pipelining.
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "perf; requires local mlx-community/Qwen3.6-35B-A3B-4bit + ROZUM_MLX_RETAIN"]
    async fn mlx_moe_backend_chat_tps() {
        let (_ttft, tps, _n) = run_backend_chat_perf(
            "mlx-community:Qwen3.6-35B-A3B-4bit",
            "mlx-community/Qwen3.6-35B-A3B-4bit",
            40,
            128,
        )
        .await;
        assert!(
            tps > 80.0,
            "MoE prod decode {tps:.1} t/s — hybrid pipelining off? (expect ~96)"
        );
    }

    // Dense prod path (Qwen3.6-27B): confirms the dense hybrid model reaches its
    // engine decode (~19-20 t/s) through the full gateway path (it pipelines too).
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "perf; requires local mlx-community/Qwen3.6-27B-4bit + ROZUM_MLX_RETAIN"]
    async fn mlx_dense_backend_chat_tps() {
        let (_ttft, tps, _n) = run_backend_chat_perf(
            "mlx-community:Qwen3.6-27B-4bit",
            "mlx-community/Qwen3.6-27B-4bit",
            40,
            96,
        )
        .await;
        assert!(tps > 16.0, "dense prod decode {tps:.1} t/s (expect ~19-20)");
    }

    // End-to-end Llama through native MLX (auto-downloads the model). Greedy.
    // Run: cargo test --features mlx-native -- --ignored --nocapture mlx_llama_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/Llama-3.2-1B-Instruct-4bit"]
    async fn mlx_llama_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:Llama-3.2-1B-Instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("llama download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX LLAMA OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Two catalog verifies in one tiny model: SmolLM2-1.7B-Instruct is `model_type: "llama"`
    // (llama-aliases) AND a NON-quantized bf16 checkpoint (fp16-verify — the AFQ loader's
    // `quantization = None` branch). Confirms a non-Llama-3, full-precision model runs on the
    // shared llama path. ~3.4 GB bf16. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_smollm_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/SmolLM2-1.7B-Instruct (bf16, ~3.4GB)"]
    async fn mlx_smollm_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:SmolLM2-1.7B-Instruct";
        let dir = ensure_model_dir(spec).await.expect("smollm download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load (SmolLM2 — llama path, non-quantized)");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX SMOLLM OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Gemma 3 (text) — proves the dedicated port works end to end (+1 RMSNorm, embed scaling,
    // q/k norm, GELU MLP, 4 norms/layer, per-layer local/global RoPE, tied embeddings). Short
    // prompt (within the 512 sliding window → full-attn approximation is exact). ~1 GB. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_gemma3_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/gemma-3-1b-it-4bit (~1GB)"]
    async fn mlx_gemma3_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:gemma-3-1b-it-4bit";
        let dir = ensure_model_dir(spec).await.expect("gemma3 download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load (Gemma 3)");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX GEMMA3 OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Gemma 3 MULTIMODAL-WRAPPER checkpoint (`model_type: "gemma3"`, the 4B/12B/27B) — a different
    // load path from the flat 1B (`gemma3_text`) above: text params live under `text_config` (with
    // most head fields omitted → Gemma3 defaults), `quantization` is top-level, the language-model
    // weights are prefixed `language_model.` next to vision/projector tensors we drop, the lm_head is
    // tied, the index.json is stale (names 2 shards that don't exist → fall back to the consolidated
    // `model.safetensors`), and the GLOBAL layers carry linear RoPE scaling (factor 8). Auto-downloads
    // ~3.4 GB. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_gemma3_wrapper_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/gemma-3-4b-it-4bit (~3.4GB)"]
    async fn mlx_gemma3_wrapper_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:gemma-3-4b-it-4bit";
        let dir = ensure_model_dir(spec).await.expect("gemma3-4b download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load (Gemma 3 multimodal wrapper → text model)");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX GEMMA3-4B WRAPPER OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Phi-3 (`model_type: "phi3"`) — proves the FUSED-projection split loader works end to end
    // (qkv_proj / gate_up_proj split into q/k/v + gate/up at load, then run on the Llama path).
    // Auto-downloads ~2 GB. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_phi3_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/Phi-3-mini-4k-instruct-4bit (~2GB)"]
    async fn mlx_phi3_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:Phi-3-mini-4k-instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("phi3 download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load (Phi-3 fused-split → llama path)");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX PHI3 OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // Mistral (`model_type: "mistral"`) on the Llama path — proves the alias works end to
    // end (Mistral is architecturally Llama; the llama loader reads its config). Greedy;
    // short prompt (well within the sliding window, so the full-attn approximation is exact).
    // Auto-downloads ~4 GB. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_mistral_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/Mistral-7B-Instruct-v0.3-4bit (~4GB)"]
    async fn mlx_mistral_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:Mistral-7B-Instruct-v0.3-4bit";
        let dir = ensure_model_dir(spec).await.expect("mistral download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load (mistral routes to the llama path)");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX MISTRAL OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // End-to-end via ModelScope: auto-download an MLX model from modelscope.cn
    // (not HF) + load + generate. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_modelscope_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads from modelscope.cn"]
    async fn mlx_modelscope_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "modelscope:mlx-community/Qwen2.5-0.5B-Instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("modelscope download/resolve");
        let backend = MlxNativeBackend::new(dir, "Qwen2.5-0.5B-Instruct-4bit".into(), None)
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX MODELSCOPE OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // End-to-end Qwen2.5 through native MLX (auto-downloads). Greedy.
    // Run: cargo test --features mlx-native -- --ignored --nocapture mlx_qwen2_chat
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: auto-downloads mlx-community/Qwen2.5-0.5B-Instruct-4bit"]
    async fn mlx_qwen2_chat() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use crate::backend::{ChatBackend, ChatRequest, collect_to_string};

        let spec = "mlx-community:Qwen2.5-0.5B-Instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("qwen2 download/resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX QWEN2 OUTPUT: {text}");
        assert!(text.contains("Paris"), "expected Paris, got: {text}");
    }

    // End-to-end tool use: render a tool into the chat template, let the model
    // emit `<tool_call>`, and parse it into ToolUse events. Model-dependent, so
    // ignored. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_tool_use_weather
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit; model-dependent"]
    async fn mlx_tool_use_weather() {
        use super::MlxNativeBackend;
        use crate::backend::{ChatBackend, ChatEvent, ChatRequest, ToolDef};
        use futures::StreamExt;

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");
        let mut req = ChatRequest::simple(
            "What is the weather in Paris right now? Call the tool to find out. /no_think",
        );
        req.tools = vec![ToolDef {
            name: "get_weather".into(),
            description: "Get the current weather for a city.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string", "description": "City name" } },
                "required": ["city"],
            }),
        }];
        let mut stream = backend.chat(req).await.expect("chat");
        let mut tool_names: Vec<String> = Vec::new();
        let mut stop = None;
        while let Some(ev) = stream.next().await {
            match ev.expect("event") {
                ChatEvent::ToolUseStart { name, .. } => tool_names.push(name),
                ChatEvent::Done { stop_reason, .. } => stop = Some(stop_reason),
                _ => {}
            }
        }
        eprintln!("TOOL CALLS: {tool_names:?} stop={stop:?}");
        assert!(
            tool_names.iter().any(|n| n == "get_weather"),
            "expected a get_weather tool call, got {tool_names:?}"
        );
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-30B-A3B-4bit".into(), None)
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-27B-4bit".into(), None)
            .await
            .expect("backend load");
        let mut req = ChatRequest::simple(
            "What is the capital of France? Reply in one short sentence. /no_think",
        );
        req.sampling = SamplingParams {
            max_tokens: Some(64),
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-35B-A3B-4bit".into(), None)
            .await
            .expect("backend load");
        let mut req = ChatRequest::simple(
            "What is the capital of France? Reply in one short sentence. /no_think",
        );
        req.sampling = SamplingParams {
            max_tokens: Some(64),
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

        let argmax_next = |y: &Array| {
            mlx_rs::ops::indexing::argmax_axis(y, -1, false)
                .unwrap()
                .index((.., NewAxis))
        };
        let steps = 64;
        let no_eval = std::env::var_os("ROZUM_GD_NO_EVAL").is_some();
        let gd_ops = std::env::var_os("ROZUM_GD_OPS").is_some();
        eprintln!("BENCH config: steps={steps} GD_NO_EVAL={no_eval} GD_OPS={gd_ops}");
        // n=1024 single-pass prefill omitted: model.forward (not chunked) peaks
        // too high for a 15GB model in tight RAM. Decode t/s is ~flat across ctx.
        for &n in &[128usize, 512] {
            let ids = synth(n);
            let prompt = Array::from(&ids[..]).index(NewAxis);

            // Prefill (timed once).
            let mut cache = model.init_cache();
            let t = Instant::now();
            let logits = model.forward(&prompt, &mut cache).expect("prefill");
            eval([&logits]).unwrap();
            let prefill = t.elapsed().as_secs_f64();

            // Helper: eval the lazily-collected input arrays and pull their ids,
            // so the hot loop stays sync-free (timing) but we still capture the
            // greedy id sequence (correctness).
            let ids_of = |arrs: &[Array]| -> Vec<u32> {
                let refs: Vec<&Array> = arrs.iter().collect();
                eval(refs).unwrap();
                arrs.iter().map(|a| a.item::<u32>()).collect()
            };

            // Serial decode (our old pattern): forward, eval (block), repeat.
            let mut y = logits.index((.., -1, ..));
            let mut serial_inps: Vec<Array> = Vec::with_capacity(steps);
            let td = Instant::now();
            for _ in 0..steps {
                let inp = argmax_next(&y);
                serial_inps.push(inp.clone());
                y = model.forward(&inp, &mut cache).expect("decode").index((.., -1, ..));
                eval([&y]).unwrap();
            }
            let serial = steps as f64 / td.elapsed().as_secs_f64();
            let serial_ids = ids_of(&serial_inps);

            // Pipelined decode: async_eval the next step before blocking on current.
            // Building step n+1's graph (which reads the cache state written by n)
            // BEFORE eval'ing n keeps that state referenced — the same retention
            // Python relies on, so the GatedDeltaNet state_out is not pool-reused.
            let mut cache2 = model.init_cache();
            let logits2 = model.forward(&prompt, &mut cache2).expect("prefill2");
            let mut cur = logits2.index((.., -1, ..));
            let _ = mlx_rs::transforms::async_eval([&cur]);
            let mut pipe_inps: Vec<Array> = Vec::with_capacity(steps);
            let tp = Instant::now();
            for _ in 0..steps {
                let inp = argmax_next(&cur);
                pipe_inps.push(inp.clone());
                let next = model.forward(&inp, &mut cache2).expect("decode2").index((.., -1, ..));
                let _ = mlx_rs::transforms::async_eval([&next]);
                eval([&cur]).unwrap();
                cur = next;
            }
            let pipelined = steps as f64 / tp.elapsed().as_secs_f64();
            let pipe_ids = ids_of(&pipe_inps);

            let match_sp = if serial_ids == pipe_ids { "MATCH" } else { "DIVERGE" };
            eprintln!(
                "BENCH n={n:>4}  prefill={prefill:>6.2}s ({:>6.1} tok/s)  decode serial={serial:>5.1}  pipelined={pipelined:>5.1} t/s  ({:.2}x)  serial==pipe:{match_sp}",
                n as f64 / prefill,
                pipelined / serial
            );
            eprintln!("SERIAL_IDS   n={n}: {:?}", &serial_ids[..serial_ids.len().min(24)]);
            eprintln!("PIPELINE_IDS n={n}: {:?}", &pipe_ids[..pipe_ids.len().min(24)]);
        }
    }

    // MoE sibling of mlx_qwen35_prefill_bench: same loop on Qwen3.6-35B-A3B-4bit
    // (3B active). Python mlx_lm.generate does ~100 t/s on this; this measures
    // whether our native runtime is at parity on the MoE too. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_qwen35_moe_decode_bench
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf bench; requires local mlx-community/Qwen3.6-35B-A3B-4bit"]
    fn mlx_qwen35_moe_decode_bench() {
        use mlx_lm::models::qwen3_5_moe::load_qwen3_5_moe_model;
        use mlx_rs::Array;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::eval;
        use std::time::Instant;

        let dir = resolve_model_dir("mlx-community:Qwen3.6-35B-A3B-4bit")
            .expect("Qwen3.6-35B-A3B-4bit not in HF cache");
        let mut model = load_qwen3_5_moe_model(&dir).expect("load");

        let synth = |n: usize| -> Vec<u32> { (0..n).map(|i| (1000 + i % 5000) as u32).collect() };
        let argmax_next = |y: &Array| {
            mlx_rs::ops::indexing::argmax_axis(y, -1, false)
                .unwrap()
                .index((.., NewAxis))
        };

        // Dump the single-decode-step (T=1) op graph to DOT, so its primitive count
        // can be compared to Python mlx_lm (`docs/mlx-gd-bug/py/count_prims.py`):
        // does mlx-rs emit more / less-fused primitives per token?
        if let Some(path) = std::env::var_os("ROZUM_DUMP_DOT") {
            let ids = synth(8);
            let prompt = Array::from(&ids[..]).index(NewAxis);
            let mut cache = model.init_cache();
            let y = model.forward(&prompt, &mut cache).expect("prefill");
            // Make prefill + all cache state concrete so the next step's graph is T=1.
            eval([&y]).unwrap();
            // One decode step, lazy; export its graph (matches the Python script).
            let inp = argmax_next(&y.index((.., -1, ..)));
            let y2 = model.forward(&inp, &mut cache).expect("decode").index((.., -1, ..));
            let p = path.to_string_lossy().to_string();
            mlx_rs::transforms::export_to_dot(&p, [&y2]).expect("export_to_dot");
            eprintln!("DUMPED decode-step graph to {p}");
            return;
        }

        // Context-growth probe: small prefill, then decode many steps, timing
        // per-window. If KV-concat (O(context) copy/token) is the cost, t/s drops
        // as context grows; a pre-allocated cache would stay flat.
        if std::env::var_os("ROZUM_CTXSWEEP").is_some() {
            let ids = synth(8);
            let prompt = Array::from(&ids[..]).index(NewAxis);
            let mut cache = model.init_cache();
            let mut y = model.forward(&prompt, &mut cache).expect("prefill").index((.., -1, ..));
            eval([&y]).unwrap();
            let total = 1024usize;
            let win = 64usize;
            let mut wstart = Instant::now();
            for s in 0..total {
                let inp = argmax_next(&y);
                y = model.forward(&inp, &mut cache).expect("decode").index((.., -1, ..));
                eval([&y]).unwrap();
                if (s + 1) % win == 0 {
                    let tps = win as f64 / wstart.elapsed().as_secs_f64();
                    eprintln!("CTX ~{:>5}: {:>6.1} t/s", s + 1, tps);
                    wstart = Instant::now();
                }
            }
            return;
        }
        let steps = 64;
        let ids_of = |arrs: &[Array]| -> Vec<u32> {
            let refs: Vec<&Array> = arrs.iter().collect();
            eval(refs).unwrap();
            arrs.iter().map(|a| a.item::<u32>()).collect()
        };
        for &n in &[128usize, 512] {
            let ids = synth(n);
            let prompt = Array::from(&ids[..]).index(NewAxis);

            let mut cache = model.init_cache();
            let t = Instant::now();
            let logits = model.forward(&prompt, &mut cache).expect("prefill");
            eval([&logits]).unwrap();
            let prefill = t.elapsed().as_secs_f64();

            // Serial decode, splitting CPU build (FFI node construction) from eval
            // (graph traverse + Metal dispatch + GPU) to locate the bottleneck.
            let mut y = logits.index((.., -1, ..));
            let mut serial_inps: Vec<Array> = Vec::with_capacity(steps);
            let (mut build_ns, mut eval_ns) = (0u128, 0u128);
            let td = Instant::now();
            for _ in 0..steps {
                let inp = argmax_next(&y);
                serial_inps.push(inp.clone());
                let tb = Instant::now();
                let raw = model.forward(&inp, &mut cache).expect("decode");
                build_ns += tb.elapsed().as_nanos();
                y = raw.index((.., -1, ..));
                let te = Instant::now();
                eval([&y]).unwrap();
                eval_ns += te.elapsed().as_nanos();
            }
            let serial = steps as f64 / td.elapsed().as_secs_f64();
            let serial_ids = ids_of(&serial_inps);
            eprintln!(
                "SPLIT n={n}: build={:.2}ms/tok  eval={:.2}ms/tok  ({:.0}% build)",
                build_ns as f64 / steps as f64 / 1e6,
                eval_ns as f64 / steps as f64 / 1e6,
                100.0 * build_ns as f64 / (build_ns + eval_ns) as f64
            );

            // Pipelined decode (async_eval next before blocking on current).
            let mut cache2 = model.init_cache();
            let logits2 = model.forward(&prompt, &mut cache2).expect("prefill2");
            let mut cur = logits2.index((.., -1, ..));
            let _ = mlx_rs::transforms::async_eval([&cur]);
            let mut pipe_inps: Vec<Array> = Vec::with_capacity(steps);
            let tp = Instant::now();
            for _ in 0..steps {
                let inp = argmax_next(&cur);
                pipe_inps.push(inp.clone());
                let next = model.forward(&inp, &mut cache2).expect("decode2").index((.., -1, ..));
                let _ = mlx_rs::transforms::async_eval([&next]);
                eval([&cur]).unwrap();
                cur = next;
            }
            let pipelined = steps as f64 / tp.elapsed().as_secs_f64();
            let pipe_ids = ids_of(&pipe_inps);

            let match_sp = if serial_ids == pipe_ids { "MATCH" } else { "DIVERGE" };
            eprintln!(
                "MOE BENCH n={n:>4}  prefill={prefill:>6.2}s ({:>6.1} tok/s)  decode serial={serial:>5.1}  pipelined={pipelined:>5.1} t/s  ({:.2}x)  serial==pipe:{match_sp}",
                n as f64 / prefill,
                pipelined / serial
            );
            eprintln!("SERIAL_IDS   n={n}: {:?}", &serial_ids[..serial_ids.len().min(24)]);
            eprintln!("PIPELINE_IDS n={n}: {:?}", &pipe_ids[..pipe_ids.len().min(24)]);
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
        // The pre-allocated KV cache (`ConcatKeyValueCache`, mlx_lm-style) returns a
        // strided `[:offset]` view of a step-padded buffer. When the single-pass length
        // isn't step-aligned its buffer is padded (cap>offset) while the chunked path's
        // step-aligned chunks aren't — so SDPA runs over a strided vs contiguous key and
        // rounds ~1 bf16 ulp differently (here |Δ|≈0.09 on logits ~22). Argmax is the hard
        // gate; the magnitude bound just guards against gross divergence. (The old concat
        // cache was bit-exact here, hence the previous 1e-2.)
        assert!(
            max_abs < 0.2,
            "chunked prefill logits diverged beyond bf16 noise: max|Δ|={max_abs}"
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

    // Stage-0 perf probe (P0 mlx-native-perf-compile): the CORRECT compile API.
    // Unlike `mlx_compile_probe` (which uses `compile_with_state` → re-marshals all
    // ~400 params/call → net-negative), this uses *plain* `compile`: the model lives
    // in a thread-local, the compiled closure captures NOTHING (Copy+'static), so
    // MLX traces once and bakes the weights into the graph — only the token `arg`
    // crosses FFI per call, exactly like Python `mx.compile`. Go/no-go for the
    // fixed-shape-cache + compiled-decode redesign. Small model on purpose (memory).
    // Run: cargo test --features mlx-native -- --ignored --nocapture mlx_compile_probe_plain
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf probe; requires local mlx-community/Qwen3-0.6B-4bit"]
    fn mlx_compile_probe_plain() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3::{load_qwen3_model, Model, ModelInput};
        use mlx_rs::Array;
        use mlx_rs::module::Module;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::compile::compile;
        use mlx_rs::transforms::eval;
        use std::cell::RefCell;
        use std::time::Instant;

        thread_local! {
            static PROBE_MODEL: RefCell<Option<Model>> = const { RefCell::new(None) };
        }

        let dir = resolve_model_dir("mlx-community:Qwen3-0.6B-4bit")
            .expect("Qwen3-0.6B-4bit not in HF cache (auto-download it first)");
        let model = load_qwen3_model(&dir).expect("load");
        PROBE_MODEL.with(|c| *c.borrow_mut() = Some(model));

        // Non-capturing (Copy + 'static): reads the model from the thread-local, runs
        // one forward over `[1, T]` with a fresh cache. Fixed shapes → graph reused.
        let step = |args: &[Array]| -> Vec<Array> {
            PROBE_MODEL.with(|c| {
                let mut m = c.borrow_mut();
                let model = m.as_mut().expect("probe model set");
                let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
                let input = ModelInput { inputs: &args[0], mask: None, cache: &mut cache };
                let logits =
                    <Model as Module<ModelInput<'_, ConcatKeyValueCache>>>::forward(model, input)
                        .expect("forward");
                vec![logits]
            })
        };

        for &t in &[1i32, 16] {
            let ids: Vec<u32> = (0..t).map(|i| (1000 + i) as u32).collect();
            let input = Array::from(&ids[..]).index(NewAxis);
            let args = [input];
            let iters = 64;

            // Uncompiled baseline.
            let _ = eval([&step(&args)[0]]);
            let t0 = Instant::now();
            for _ in 0..iters {
                let o = step(&args);
                eval([&o[0]]).unwrap();
            }
            let uncompiled = t0.elapsed().as_secs_f64() / iters as f64;

            // Compiled (plain — weights captured, only `args` marshaled).
            let mut compiled = compile(step, None);
            let _ = eval([&compiled(&args).unwrap()[0]]); // warm/trace
            let t1 = Instant::now();
            for _ in 0..iters {
                let o = compiled(&args).unwrap();
                eval([&o[0]]).unwrap();
            }
            let comp = t1.elapsed().as_secs_f64() / iters as f64;

            eprintln!(
                "COMPILE-PROBE-PLAIN T={t:>2}  uncompiled={:.3}ms  compiled={:.3}ms  speedup={:.2}x",
                uncompiled * 1e3,
                comp * 1e3,
                uncompiled / comp
            );
        }
        PROBE_MODEL.with(|c| *c.borrow_mut() = None);
    }

    // Feasibility probe for ragged batched decode: can `rope_dynamic` apply a PER-ROW
    // position offset (a [B] offset array), so a batch of sequences at different lengths
    // can each be rope'd at its own position in one call? If yes, ragged batching is
    // tractable without per-row rope loops.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_rope_per_row_probe
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "rope feasibility probe (no model needed)"]
    fn mlx_rope_per_row_probe() {
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::Array;

        // q: [B=2, H=1, T=1, D=4] — two rows, a single token each.
        let dims = 4i32;
        let q = Array::from_iter(
            [0.1f32, 0.2, 0.3, 0.4, 0.1, 0.2, 0.3, 0.4].into_iter(),
            &[2, 1, 1, dims],
        );
        // Per-row offsets: row0 at position 0, row1 at position 10.
        let off = Array::from_iter([0i32, 10].into_iter(), &[2]);

        // Batched per-row rope (the thing we want to work).
        let batched =
            mlx_rs::fast::rope_dynamic(&q, dims, false, Some(1e6f32), 1.0, &off, None);

        // Reference: rope each row separately with a scalar offset.
        let r0 = mlx_rs::fast::rope(&q.index((0, NewAxis)), dims, false, Some(1e6f32), 1.0, 0, None);
        let r10 =
            mlx_rs::fast::rope(&q.index((1, NewAxis)), dims, false, Some(1e6f32), 1.0, 10, None);

        match (batched, r0, r10) {
            (Ok(b), Ok(r0), Ok(r10)) => {
                let row0 = b.index((0, NewAxis));
                let row1 = b.index((1, NewAxis));
                let d0 = row0.subtract(&r0).unwrap().abs().unwrap().max(None).unwrap().item::<f32>();
                let d1 = row1.subtract(&r10).unwrap().abs().unwrap().max(None).unwrap().item::<f32>();
                eprintln!("PER-ROW ROPE: max|batched_row0 - rope(off=0)|={d0:.2e}  max|batched_row1 - rope(off=10)|={d1:.2e}");
                eprintln!(
                    "=> per-row offset {}",
                    if d0 < 1e-5 && d1 < 1e-5 { "WORKS (ragged batching is tractable)" } else { "does NOT match (offset is not per-row)" }
                );
            }
            (b, _, _) => eprintln!("rope_dynamic per-row offset errored: {:?}", b.err()),
        }
    }

    // RAGGED batched decode must be byte-exact: two sequences of DIFFERENT lengths,
    // left-padded into one cache + per-row rope (cache.offset()−pad_i) + per-row pad
    // mask, must each produce the SAME greedy tokens as running that sequence alone.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_batched_ragged_byte_exact
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "ragged batched-decode byte-exact; requires mlx-community/Qwen3-4B-4bit"]
    fn mlx_batched_ragged_byte_exact() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3::{load_qwen3_model, set_batch_pad_offsets, Model, ModelInput};
        use mlx_rs::ops::arange;
        use mlx_rs::ops::indexing::{argmax_axis, IndexOp, NewAxis};
        use mlx_rs::module::Module;
        use mlx_rs::Array;

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let mut model = load_qwen3_model(&dir).expect("load");
        let n_decode = 16usize;

        // Two DIFFERENT-length sequences (ragged).
        let a: Vec<u32> = vec![785, 6722, 315, 9625, 374, 220, 17]; // len 7
        let b: Vec<u32> = vec![3838, 374, 279, 1429]; // len 4

        let fwd = |m: &mut Model,
                   inp: &Array,
                   mask: Option<&Array>,
                   cache: &mut Vec<Option<ConcatKeyValueCache>>|
         -> Array {
            let input = ModelInput { inputs: inp, mask, cache };
            <Model as Module<ModelInput<'_, ConcatKeyValueCache>>>::forward(m, input).expect("fwd")
        };
        let argmax_row = |logits: &Array, row: i32| -> u32 {
            argmax_axis(&logits.index((row, -1, ..)), -1, false).unwrap().item::<u32>()
        };

        // --- Serial: greedy-decode each alone ---
        let mut serial = |ids: &[u32], model: &mut Model| -> Vec<u32> {
            let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
            let mut logits = fwd(model, &Array::from(ids).index(NewAxis), None, &mut cache);
            let mut out = Vec::new();
            for _ in 0..n_decode {
                let tok = argmax_row(&logits, 0);
                out.push(tok);
                let y = Array::from(&[tok][..]).index(NewAxis);
                logits = fwd(model, &y, None, &mut cache);
            }
            out
        };
        let sa = serial(&a, &mut model);
        let sb = serial(&b, &mut model);

        // --- Batched ragged: prefill each SEPARATELY (no padding → exact keys, no
        // negative rope positions), then ASSEMBLE a batched cache (left-pad each row's
        // KV with zeros on the seq axis to maxL) and decode batched with per-row rope. ---
        use mlx_rs::ops::{concatenate_axis, zeros_dtype};
        let max_l = a.len().max(b.len()) as i32;
        let (pad_a, pad_b) = (max_l - a.len() as i32, max_l - b.len() as i32);
        let pad_off = Array::from(&[pad_a, pad_b][..]); // [2] i32

        let prefill = |m: &mut Model, ids: &[u32]| -> (Array, Vec<Option<ConcatKeyValueCache>>) {
            let mut cache = Vec::new();
            let logits = fwd(m, &Array::from(ids).index(NewAxis), None, &mut cache);
            (logits, cache)
        };
        let (la0, ca) = prefill(&mut model, &a);
        let (lb0, cb) = prefill(&mut model, &b);

        // left-pad a [1,H,L,D] tensor with `pad` zeros at the front of the seq axis.
        let lpad = |x: &Array, pad: i32| -> Array {
            if pad == 0 {
                return x.clone();
            }
            let s = x.shape();
            let z = zeros_dtype(&[s[0], s[1], pad, s[3]], x.dtype()).unwrap();
            concatenate_axis(&[&z, x], 2).unwrap()
        };
        let mut bcache: Vec<Option<ConcatKeyValueCache>> = Vec::with_capacity(ca.len());
        for l in 0..ca.len() {
            let (ka, va, _) = ca[l].as_ref().unwrap().kv_used().unwrap();
            let (kb, vb, _) = cb[l].as_ref().unwrap().kv_used().unwrap();
            let bk = concatenate_axis(&[&lpad(&ka, pad_a), &lpad(&kb, pad_b)], 0).unwrap();
            let bv = concatenate_axis(&[&lpad(&va, pad_a), &lpad(&vb, pad_b)], 0).unwrap();
            bcache.push(Some(ConcatKeyValueCache::from_kv(bk, bv, max_l)));
        }

        // First decode tokens = argmax of each prefill's last-position logits.
        let mut ba = vec![argmax_row(&la0, 0)];
        let mut bb = vec![argmax_row(&lb0, 0)];
        let mut b_logits = vec![lb0.index((0, -1, ..))]; // B's per-step logits (for the gap check)
        set_batch_pad_offsets(Some(pad_off.clone()));
        for step in 0..(n_decode - 1) {
            let y = Array::from(&[*ba.last().unwrap(), *bb.last().unwrap()][..])
                .reshape(&[2, 1])
                .unwrap();
            // decode mask [2,1,1,K] (bool): keep iff k >= pad_i. K = maxL+step+1.
            let k_cur = max_l + step as i32 + 1;
            let kidx = arange::<_, i32>(0, k_cur, None).unwrap().index((NewAxis, ..));
            let padd = pad_off.index((.., NewAxis));
            let dec_mask = kidx.ge(&padd).unwrap().index((.., NewAxis, NewAxis, ..));
            let logits = fwd(&mut model, &y, Some(&dec_mask), &mut bcache);
            ba.push(argmax_row(&logits, 0));
            bb.push(argmax_row(&logits, 1));
            b_logits.push(logits.index((1, -1, ..)));
        }
        set_batch_pad_offsets(None);

        eprintln!("serial A: {sa:?}\nbatch  A: {ba:?}");
        eprintln!("serial B: {sb:?}\nbatch  B: {bb:?}");
        // Correctness bar for a bf16 model: byte-exact, OR any first divergence is a
        // bf16 NEAR-TIE (the two candidate tokens' logits within a few ulps) — a valid
        // alternative greedy choice, not a structural error (same class as MoE greedy
        // float-reduction nondeterminism). Row A (no pad) is byte-exact.
        assert_eq!(sa, ba, "ragged batched row0 (A, len 7) must byte-match serial");
        if let Some(step) = sb.iter().zip(&bb).position(|(s, b)| s != b) {
            let lg = &b_logits[step];
            let gs = lg.index(sb[step] as i32).item::<f32>();
            let gb = lg.index(bb[step] as i32).item::<f32>();
            let gap = (gb - gs).abs();
            eprintln!(
                "B: byte-exact for {step} tokens, then a near-tie flip (serial tok {} logit={gs:.4} vs batched tok {} logit={gb:.4}, gap={gap:.4} ≈ 1 bf16 ulp at this magnitude) — a valid greedy choice.",
                sb[step], bb[step]
            );
            assert!(
                gap < 0.5,
                "row B (len 4) divergence at step {step} must be a bf16 near-tie (gap={gap}), not a structural error"
            );
        } else {
            eprintln!("B: byte-exact too.");
        }
    }

    // HYBRID (Qwen3.6) batched-decode THROUGHPUT: B=2 batched decode vs 2× serial on the
    // 27B. Hybrid decode is ~92% CPU graph-build (the gated_delta/full-attn op launches),
    // so doing ONE build for B rows should amortize it just like the dense path (1.98×).
    // Uniform-length sequences (pad 0) to isolate the throughput question from raggedness.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_hybrid_batched_decode_throughput
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf; requires mlx-community/Qwen3.6-27B-4bit + ROZUM_MLX_RETAIN"]
    fn mlx_hybrid_batched_decode_throughput() {
        use mlx_lm::models::qwen3_5::{load_qwen3_5_model, set_batch_pad_offsets, Model};
        use mlx_rs::ops::indexing::{argmax_axis, IndexOp, NewAxis};
        use mlx_rs::transforms::eval;
        use mlx_rs::Array;
        use std::time::Instant;

        unsafe {
            std::env::set_var("ROZUM_MLX_RETAIN", "1");
        }
        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let mut model = load_qwen3_5_model(&dir).expect("load");
        let steps = 32usize;
        let a: Vec<u32> = vec![785, 6722, 315, 9625, 374, 220, 17];
        let b: Vec<u32> = vec![3838, 374, 279, 1429, 5089, 1372, 30]; // same length (7)
        let argmax_row = |l: &Array, r: i32| -> u32 {
            argmax_axis(&l.index((r, -1, ..)), -1, false).unwrap().item::<u32>()
        };

        // Serial: decode each sequence alone, blocking per step.
        let mut serial = |ids: &[u32], model: &mut Model| {
            let mut cache = model.init_cache();
            let mut y = model.forward(&Array::from(ids).index(NewAxis), &mut cache).unwrap();
            for _ in 0..steps {
                let t = argmax_row(&y, 0);
                y = model.forward(&Array::from(&[t][..]).index(NewAxis), &mut cache).unwrap();
                eval([&y]).unwrap();
            }
        };
        let ts = Instant::now();
        serial(&a, &mut model);
        serial(&b, &mut model);
        let serial_s = ts.elapsed().as_secs_f64();

        // Batched B=2: prefill each into its own cache, assemble (uniform length → pad 0),
        // then decode in lockstep.
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3_5::LayerCache;
        use mlx_rs::ops::concatenate_axis;
        let mut ca = model.init_cache();
        let la = model.forward(&Array::from(&a[..]).index(NewAxis), &mut ca).unwrap();
        let mut cb = model.init_cache();
        let lb = model.forward(&Array::from(&b[..]).index(NewAxis), &mut cb).unwrap();
        let max_l = a.len() as i32;
        let mut bcache: Vec<LayerCache> = Vec::with_capacity(ca.len());
        for l in 0..ca.len() {
            match (&ca[l], &cb[l]) {
                (LayerCache::Full(akv), LayerCache::Full(bkv)) => {
                    let (ka, va, _) = akv.kv_used().unwrap();
                    let (kb, vb, _) = bkv.kv_used().unwrap();
                    let bk = concatenate_axis(&[&ka, &kb], 0).unwrap();
                    let bv = concatenate_axis(&[&va, &vb], 0).unwrap();
                    bcache.push(LayerCache::Full(ConcatKeyValueCache::from_kv(bk, bv, max_l)));
                }
                (LayerCache::Linear { conv: ac, state: as_ }, LayerCache::Linear { conv: bc, state: bs }) => {
                    let conv = concatenate_axis(&[ac.as_ref().unwrap(), bc.as_ref().unwrap()], 0).unwrap();
                    let state = concatenate_axis(&[as_.as_ref().unwrap(), bs.as_ref().unwrap()], 0).unwrap();
                    bcache.push(LayerCache::Linear { conv: Some(conv), state: Some(state) });
                }
                _ => unreachable!(),
            }
        }
        drop((ca, cb));
        let mut ta = argmax_row(&la, 0);
        let mut tb = argmax_row(&lb, 0);
        let pad_off = Array::from(&[0i32, 0][..]);
        let tb_start = Instant::now();
        for _ in 0..steps {
            let y = Array::from(&[ta, tb][..]).reshape(&[2, 1]).unwrap();
            set_batch_pad_offsets(Some(pad_off.clone()));
            let logits = model.forward(&y, &mut bcache).unwrap();
            set_batch_pad_offsets(None);
            ta = argmax_row(&logits, 0);
            tb = argmax_row(&logits, 1);
            eval([&logits]).unwrap();
        }
        let batched_s = tb_start.elapsed().as_secs_f64();

        let serial_tps = (2 * steps) as f64 / serial_s;
        let batched_tps = (2 * steps) as f64 / batched_s;
        eprintln!(
            "HYBRID THROUGHPUT (2 seqs, {steps} steps): batched(B=2)={batched_tps:.1} tok/s  serial(2×)={serial_tps:.1} tok/s  speedup={:.2}×",
            batched_tps / serial_tps
        );
    }

    // HYBRID (Qwen3.6) ragged batched decode must be byte-exact, the analog of
    // `mlx_batched_ragged_byte_exact` over the heterogeneous `qwen3_5::LayerCache`:
    // the full-attention layers batch like the dense path (left-pad+stack KV, per-row
    // rope + key-pad mask) while the GatedDeltaNet layers stack their fixed-size conv +
    // recurrent state on the batch axis. Two different-length sequences, prefilled
    // separately then assembled, must each decode the SAME greedy tokens as alone.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_hybrid_batched_ragged_byte_exact
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "hybrid ragged batched-decode byte-exact; requires mlx-community/Qwen3.6-27B-4bit"]
    fn mlx_hybrid_batched_ragged_byte_exact() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3_5::{
            load_qwen3_5_model, set_batch_pad_mask, set_batch_pad_offsets, LayerCache, Model,
        };
        use mlx_rs::ops::indexing::{argmax_axis, IndexOp, NewAxis};
        use mlx_rs::ops::{arange, concatenate_axis, zeros_dtype};
        use mlx_rs::Array;

        // The GatedDeltaNet kernel needs retained command-buffer refs for correctness in
        // the large forward (docs/mlx-gd-bug). The backend sets this for hybrid loads; the
        // test loads the model directly, so set it here. SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("ROZUM_MLX_RETAIN", "1");
        }

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let mut model = load_qwen3_5_model(&dir).expect("load");
        let n_decode = 12usize;

        // Two DIFFERENT-length sequences (ragged).
        let a: Vec<u32> = vec![785, 6722, 315, 9625, 374, 220, 17]; // len 7
        let b: Vec<u32> = vec![3838, 374, 279, 1429]; // len 4

        let argmax_row = |logits: &Array, row: i32| -> u32 {
            argmax_axis(&logits.index((row, -1, ..)), -1, false).unwrap().item::<u32>()
        };

        // --- Serial: greedy-decode each alone (its own heterogeneous cache). ---
        let mut serial = |ids: &[u32], model: &mut Model| -> Vec<u32> {
            let mut cache = model.init_cache();
            let mut logits =
                model.forward(&Array::from(ids).index(NewAxis), &mut cache).expect("fwd");
            let mut out = Vec::new();
            for _ in 0..n_decode {
                let tok = argmax_row(&logits, 0);
                out.push(tok);
                let y = Array::from(&[tok][..]).index(NewAxis);
                logits = model.forward(&y, &mut cache).expect("fwd");
            }
            out
        };
        let sa = serial(&a, &mut model);
        let sb = serial(&b, &mut model);

        // --- Batched ragged: prefill each SEPARATELY, then assemble. ---
        let max_l = a.len().max(b.len()) as i32;
        let (pad_a, pad_b) = (max_l - a.len() as i32, max_l - b.len() as i32);
        let pad_off = Array::from(&[pad_a, pad_b][..]);

        let prefill = |m: &mut Model, ids: &[u32]| -> (Array, Vec<LayerCache>) {
            let mut cache = m.init_cache();
            let logits = m.forward(&Array::from(ids).index(NewAxis), &mut cache).expect("prefill");
            (logits, cache)
        };
        let (la0, ca) = prefill(&mut model, &a);
        let (lb0, cb) = prefill(&mut model, &b);

        let lpad = |x: &Array, pad: i32| -> Array {
            if pad == 0 {
                return x.clone();
            }
            let s = x.shape();
            let z = zeros_dtype(&[s[0], s[1], pad, s[3]], x.dtype()).unwrap();
            concatenate_axis(&[&z, x], 2).unwrap()
        };
        // Full → left-pad+stack KV; Linear → stack conv + recurrent state (no padding).
        let mut bcache: Vec<LayerCache> = Vec::with_capacity(ca.len());
        for l in 0..ca.len() {
            match (&ca[l], &cb[l]) {
                (LayerCache::Full(akv), LayerCache::Full(bkv)) => {
                    let (ka, va, _) = akv.kv_used().unwrap();
                    let (kb, vb, _) = bkv.kv_used().unwrap();
                    let bk = concatenate_axis(&[&lpad(&ka, pad_a), &lpad(&kb, pad_b)], 0).unwrap();
                    let bv = concatenate_axis(&[&lpad(&va, pad_a), &lpad(&vb, pad_b)], 0).unwrap();
                    bcache.push(LayerCache::Full(ConcatKeyValueCache::from_kv(bk, bv, max_l)));
                }
                (
                    LayerCache::Linear { conv: ac, state: as_ },
                    LayerCache::Linear { conv: bc, state: bs },
                ) => {
                    let conv =
                        concatenate_axis(&[ac.as_ref().unwrap(), bc.as_ref().unwrap()], 0).unwrap();
                    let state =
                        concatenate_axis(&[as_.as_ref().unwrap(), bs.as_ref().unwrap()], 0).unwrap();
                    bcache.push(LayerCache::Linear { conv: Some(conv), state: Some(state) });
                }
                _ => panic!("layer {l}: cache kind mismatch between sequences"),
            }
        }

        let mut ba = vec![argmax_row(&la0, 0)];
        let mut bb = vec![argmax_row(&lb0, 0)];
        let mut b_logits = vec![lb0.index((0, -1, ..))];
        for step in 0..(n_decode - 1) {
            let y = Array::from(&[*ba.last().unwrap(), *bb.last().unwrap()][..])
                .reshape(&[2, 1])
                .unwrap();
            let k_cur = max_l + step as i32 + 1;
            let kidx = arange::<_, i32>(0, k_cur, None).unwrap().index((NewAxis, ..));
            let padd = pad_off.index((.., NewAxis));
            let dec_mask = kidx.ge(&padd).unwrap().index((.., NewAxis, NewAxis, ..));
            set_batch_pad_offsets(Some(pad_off.clone()));
            set_batch_pad_mask(Some(dec_mask));
            let logits = model.forward(&y, &mut bcache).expect("batched fwd");
            set_batch_pad_offsets(None);
            set_batch_pad_mask(None);
            ba.push(argmax_row(&logits, 0));
            bb.push(argmax_row(&logits, 1));
            b_logits.push(logits.index((1, -1, ..)));
        }

        eprintln!("serial A: {sa:?}\nbatch  A: {ba:?}");
        eprintln!("serial B: {sb:?}\nbatch  B: {bb:?}");
        // Row A (no pad) byte-exact; row B may diverge only on a bf16 near-tie.
        assert_eq!(sa, ba, "hybrid ragged batched row0 (A, len 7) must byte-match serial");
        if let Some(step) = sb.iter().zip(&bb).position(|(s, b)| s != b) {
            let lg = &b_logits[step];
            let gs = lg.index(sb[step] as i32).item::<f32>();
            let gb = lg.index(bb[step] as i32).item::<f32>();
            let gap = (gb - gs).abs();
            eprintln!(
                "B: byte-exact for {step} tokens, then a near-tie flip (gap={gap:.4} ≈ bf16 ulp) — valid greedy choice."
            );
            assert!(
                gap < 0.5,
                "row B (len 4) divergence at step {step} must be a bf16 near-tie (gap={gap}), not structural"
            );
        } else {
            eprintln!("B: byte-exact too.");
        }
        eprintln!("HYBRID-BATCH: Qwen3.6 ragged batched decode byte-exact ✓");
    }

    // Batched-decode probe (P0 de-risk for `mlx-native-batched-decode`): does a B>1
    // batched `forward` produce per-sequence-IDENTICAL output to running each alone
    // (byte-exact), and does it gain THROUGHPUT vs serial? Dense Qwen3 (no GatedDeltaNet);
    // uniform-length sequences (lockstep decode, single cache offset).
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_batched_decode_probe
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "batched-decode probe; requires mlx-community/Qwen3-4B-4bit"]
    fn mlx_batched_decode_probe() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3::{load_qwen3_model, Model, ModelInput};
        use mlx_rs::Array;
        use mlx_rs::module::Module;
        use mlx_rs::ops::indexing::{argmax_axis, IndexOp, NewAxis};
        use mlx_rs::transforms::eval;
        use std::time::Instant;

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let mut model = load_qwen3_model(&dir).expect("load");

        // Two distinct same-length prompts.
        let a: Vec<u32> = vec![785, 6722, 315, 9625, 374, 220, 17];
        let b: Vec<u32> = vec![3838, 374, 279, 1429, 5089, 1372, 30];
        let t = a.len() as i32;
        assert_eq!(a.len(), b.len());

        let forward = |model: &mut Model,
                       inp: &Array,
                       cache: &mut Vec<Option<ConcatKeyValueCache>>|
         -> Array {
            let input = ModelInput { inputs: inp, mask: None, cache };
            <Model as Module<ModelInput<'_, ConcatKeyValueCache>>>::forward(model, input)
                .expect("forward")
        };
        // argmax of `row`'s last position.
        let last = |logits: &Array, row: i32| -> u32 {
            argmax_axis(&logits.index((row, -1, ..)), -1, false)
                .unwrap()
                .item::<u32>()
        };

        // --- Byte-exactness: batched seq i must equal serial seq i ---
        let mut ca: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let mut cb: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let la = forward(&mut model, &Array::from(&a[..]).index(NewAxis), &mut ca);
        let lb = forward(&mut model, &Array::from(&b[..]).index(NewAxis), &mut cb);
        let (sa, sb) = (last(&la, 0), last(&lb, 0));

        let batch = Array::from_iter(a.iter().chain(b.iter()).copied(), &[2, t]);
        let mut cbatch: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let lbatch = forward(&mut model, &batch, &mut cbatch);
        let (ba, bb) = (last(&lbatch, 0), last(&lbatch, 1));

        eprintln!("serial next:  A={sa} B={sb}\nbatched next: A={ba} B={bb}");
        assert_eq!(sa, ba, "batched row0 must byte-match serial A");
        assert_eq!(sb, bb, "batched row1 must byte-match serial B");

        // --- Throughput: N decode steps, batched B=2 vs 2× serial (one model, one GPU) ---
        let n = 64;
        let next_row = |logits: &Array| -> Array {
            // argmax over each row's last position -> [B,1]
            argmax_axis(&logits.index((.., -1, ..)), -1, false)
                .unwrap()
                .index((.., NewAxis))
        };

        // batched
        let mut yb = next_row(&lbatch);
        eval([&yb]).unwrap();
        let t0 = Instant::now();
        for _ in 0..n {
            let l = forward(&mut model, &yb, &mut cbatch);
            yb = next_row(&l);
            eval([&yb]).unwrap();
        }
        let batched_tps = (2 * n) as f64 / t0.elapsed().as_secs_f64();

        // serial 2× (two sequences, one after the other)
        let mut ya = next_row(&la);
        let mut yb2 = next_row(&lb);
        eval([&ya, &yb2]).unwrap();
        let t1 = Instant::now();
        for _ in 0..n {
            let l = forward(&mut model, &ya, &mut ca);
            ya = next_row(&l);
            eval([&ya]).unwrap();
        }
        for _ in 0..n {
            let l = forward(&mut model, &yb2, &mut cb);
            yb2 = next_row(&l);
            eval([&yb2]).unwrap();
        }
        let serial_tps = (2 * n) as f64 / t1.elapsed().as_secs_f64();

        eprintln!(
            "THROUGHPUT (2 seqs): batched(B=2)={batched_tps:.1} tok/s  serial(2×)={serial_tps:.1} tok/s  speedup={:.2}×",
            batched_tps / serial_tps
        );
    }

    // Stage-0b perf probe (P0): decode PIPELINING. Python `mlx_lm` builds step n+1's
    // graph from the lazy token n and `async_eval`s it BEFORE blocking on token n's
    // `.item()` — so the GPU never idles waiting for the CPU to build the next graph.
    // Our `stream_generation` does `eval`+`item` (blocking) THEN builds the next step
    // → a sync bubble every token. A/B serial vs pipelined decode on Qwen3-4B-4bit.
    // Run: cargo test --features mlx-native -- --ignored --nocapture mlx_decode_pipeline_probe
    #[cfg(feature = "mlx-native")]
    #[test]
    #[ignore = "perf probe; requires local mlx-community/Qwen3-4B-4bit"]
    fn mlx_decode_pipeline_probe() {
        use mlx_lm::cache::ConcatKeyValueCache;
        use mlx_lm::models::qwen3::{load_qwen3_model, Generate};
        use mlx_rs::Array;
        use mlx_rs::ops::indexing::{IndexOp, NewAxis};
        use mlx_rs::transforms::{async_eval, eval};
        use std::time::Instant;

        let dir =
            resolve_model_dir("mlx-community:Qwen3-4B-4bit").expect("Qwen3-4B-4bit not in HF cache");
        let mut model = load_qwen3_model(&dir).expect("load");
        let prompt_ids: Vec<u32> = (0..32).map(|i| (1000 + i) as u32).collect();
        let prompt = Array::from(&prompt_ids[..]).index(NewAxis);
        let steps = 64usize;

        // --- Serial: eval + item, then build next (our current pattern) ---
        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let mut g = Generate::new(&mut model, &mut cache, 0.0, &prompt);
        let first = g.next().unwrap().unwrap(); // prefill (untimed)
        eval([&first]).unwrap();
        let _ = first.item::<u32>();
        let t0 = Instant::now();
        for _ in 0..steps {
            let tok = g.next().unwrap().unwrap();
            eval([&tok]).unwrap();
            let _ = tok.item::<u32>();
        }
        let serial = t0.elapsed().as_secs_f64();
        drop(g);

        // --- Pipelined: async_eval the next step before reading the current ---
        let mut cache2: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let mut g = Generate::new(&mut model, &mut cache2, 0.0, &prompt);
        let first = g.next().unwrap().unwrap(); // prefill (untimed)
        eval([&first]).unwrap();
        let _ = first.item::<u32>();
        let mut cur = g.next().unwrap().unwrap();
        async_eval([&cur]).unwrap();
        let t1 = Instant::now();
        for _ in 0..steps {
            let next = g.next().unwrap().unwrap(); // builds n+1 from lazy n
            async_eval([&next]).unwrap();
            let _ = cur.item::<u32>(); // already computed by its async_eval
            cur = next;
        }
        let pipelined = t1.elapsed().as_secs_f64();

        eprintln!(
            "DECODE-PIPELINE  serial={:.1} t/s  pipelined={:.1} t/s  speedup={:.2}x",
            steps as f64 / serial,
            steps as f64 / pipelined,
            serial / pipelined
        );
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

    // Batched-decode scheduler, end-to-end through the real ChatBackend. With
    // `ROZUM_BATCH=2`, two concurrent greedy requests on ONE backend (ONE worker)
    // must (a) actually batch — `BATCH_RUN_COUNT` increments exactly once, proving
    // the scheduler didn't silently fall back to serial decode — and (b) each row
    // gets ITS OWN correct, uncontaminated answer. The central batched-decode risk
    // is cross-row leakage through padding/masking, so two different capitals make
    // any contamination obvious. Needs the local Qwen3-4B-4bit snapshot; run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_batched_scheduler_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_batched_scheduler_two_concurrent() {
        use super::MlxNativeBackend;
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        // The worker reads `ROZUM_BATCH` once at startup, so set it BEFORE building
        // the backend. A generous window makes the two near-simultaneous sends land
        // in the SAME batch deterministically (otherwise job 1 could decode before
        // job 2 arrives). SAFETY: test process, set before the worker thread starts.
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }

        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");

        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(32), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);

        // Enqueue both jobs back-to-back (each `chat().await` sends synchronously),
        // then drive both streams together — they coincide inside the batch window.
        let s1 = backend
            .chat(mk("What is the capital of France? Answer in one word. /no_think"))
            .await
            .expect("chat France");
        let s2 = backend
            .chat(mk("What is the capital of Japan? Answer in one word. /no_think"))
            .await
            .expect("chat Japan");
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.expect("collect France"), t2.expect("collect Japan"));

        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("BATCHED France={t1:?}  Japan={t2:?}  (run_batch calls={runs})");

        // SAFETY: test cleanup; no other thread reads these now.
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }

        assert_eq!(
            runs, 1,
            "the two concurrent greedy requests must batch in ONE run_batch call"
        );
        assert!(t1.contains("Paris"), "France row wrong/contaminated: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan row wrong/contaminated: {t2:?}");
    }

    // The LLAMA family batches too (Llama 3.x / Mistral / Phi-3 / SmolLM all load into
    // `LoadedModel::Llama`): two concurrent greedy requests on a Llama-3.2-1B backend must land
    // in ONE `run_batch` (BATCH_RUN_COUNT +1) and each get its own correct answer — proving the
    // per-row RoPE ported into `llama.rs` + the shared ragged batched path work for the family.
    // Auto-downloads the tiny Llama-3.2-1B if absent. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_llama_batched_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires/auto-downloads mlx-community/Llama-3.2-1B-Instruct-4bit"]
    async fn mlx_llama_batched_two_concurrent() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let spec = "mlx-community:Llama-3.2-1B-Instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("llama resolve/download");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load");
        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        let s1 = backend.chat(mk("What is the capital of France? Answer in one word.")).await.unwrap();
        let s2 = backend.chat(mk("What is the capital of Japan? Answer in one word.")).await.unwrap();
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("LLAMA BATCHED  France={t1:?} Japan={t2:?}  (run_batch calls={runs})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "two concurrent Llama requests must batch in ONE run_batch call");
        assert!(t1.contains("Paris"), "France: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan: {t2:?}");
    }

    // Qwen2 / Qwen2.5 (incl. Qwen2.5-Coder) batches too — two concurrent requests on a cached
    // Qwen2.5-0.5B backend land in ONE `run_batch` call, each answer correct. Proves the per-row
    // RoPE ported into `qwen2.rs`. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_qwen2_batched_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires/auto-downloads mlx-community/Qwen2.5-0.5B-Instruct-4bit"]
    async fn mlx_qwen2_batched_two_concurrent() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let spec = "mlx-community:Qwen2.5-0.5B-Instruct-4bit";
        let dir = ensure_model_dir(spec).await.expect("qwen2 resolve/download");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load");
        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        let s1 = backend.chat(mk("What is the capital of France? Answer in one word.")).await.unwrap();
        let s2 = backend.chat(mk("What is the capital of Japan? Answer in one word.")).await.unwrap();
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("QWEN2 BATCHED  France={t1:?} Japan={t2:?}  (run_batch calls={runs})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "two concurrent Qwen2 requests must batch in ONE run_batch call");
        assert!(t1.contains("Paris"), "France: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan: {t2:?}");
    }

    // Gemma 3 batches too — the trickiest dense arch (per-layer local/global masks). Two
    // concurrent requests on a cached gemma-3-1b backend land in ONE `run_batch` call with
    // distinct correct answers. Short prompts (< 512 window) so the per-layer local mask == the
    // pad mask; this validates the per-row rope + the batched-mask plumbing (windowing math is
    // covered by `sliding_window_mask_bands_local_attention`). Run:
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_gemma3_batched_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires/auto-downloads mlx-community/gemma-3-1b-it-4bit"]
    async fn mlx_gemma3_batched_two_concurrent() {
        use super::{MlxNativeBackend, ensure_model_dir};
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let spec = "mlx-community:gemma-3-1b-it-4bit";
        let dir = ensure_model_dir(spec).await.expect("gemma3 resolve/download");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("backend load");
        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        let s1 = backend.chat(mk("What is the capital of France? Answer in one short sentence.")).await.unwrap();
        let s2 = backend.chat(mk("What is the capital of Japan? Answer in one short sentence.")).await.unwrap();
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("GEMMA3 BATCHED  France={t1:?} Japan={t2:?}  (run_batch calls={runs})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "two concurrent Gemma 3 requests must batch in ONE run_batch call");
        assert!(t1.contains("Paris"), "France: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan: {t2:?}");
    }

    // Continuous batching end-to-end: with `ROZUM_BATCH=2`, THREE concurrent greedy requests
    // — the first two fill the batch, the third waits in the queue and is ADMITTED into a
    // freed slot mid-decode when one of the first two finishes (one `run_batch` call serves
    // all three: BATCH_RUN_COUNT +1, BATCH_ADMIT_COUNT +≥1). The admitted row must decode its
    // OWN correct answer (Berlin) — byte-exact insertion, no cross-row leakage.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_continuous_admit_three
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_continuous_admit_three() {
        use super::MlxNativeBackend;
        use super::inner::{BATCH_ADMIT_COUNT, BATCH_RUN_COUNT};
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        // SAFETY: test process, set before the worker thread starts. cap=2 so the 3rd of 3
        // concurrent requests can't fit the initial batch and must be admitted mid-decode.
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");
        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let (runs0, adm0) = (
            BATCH_RUN_COUNT.load(Ordering::Relaxed),
            BATCH_ADMIT_COUNT.load(Ordering::Relaxed),
        );
        // Enqueue all three back-to-back; the worker windows the first two, the third waits.
        let s1 = backend.chat(mk("What is the capital of France? Answer in one word. /no_think")).await.unwrap();
        let s2 = backend.chat(mk("What is the capital of Japan? Answer in one word. /no_think")).await.unwrap();
        let s3 = backend.chat(mk("What is the capital of Germany? Answer in one word. /no_think")).await.unwrap();
        let (t1, t2, t3) = tokio::join!(
            collect_to_string(s1),
            collect_to_string(s2),
            collect_to_string(s3)
        );
        let (t1, t2, t3) = (t1.unwrap(), t2.unwrap(), t3.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - runs0;
        let admits = BATCH_ADMIT_COUNT.load(Ordering::Relaxed) - adm0;
        eprintln!("CONTINUOUS  France={t1:?} Japan={t2:?} Germany={t3:?}  (runs={runs} admits={admits})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "all three must be served by ONE continuous run_batch call");
        assert!(admits >= 1, "the 3rd request must be admitted into a freed slot mid-decode");
        assert!(t1.contains("Paris"), "France: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan: {t2:?}");
        assert!(t3.contains("Berlin"), "Germany (admitted mid-decode) wrong/contaminated: {t3:?}");
    }

    // Batched SAMPLING end-to-end: two concurrent requests with temperature > 0 (+ top_p)
    // must BATCH (the relaxed `is_batchable` gate — not just greedy) and each stream a
    // coherent non-empty response via `qwen3::sample_rows` (per-row temp/top_k/top_p). Output
    // is stochastic so the bar is non-empty + batched (the per-row filter correctness is
    // proven deterministically in the fork's `sample_rows_per_row_collapses_to_argmax` and by
    // the greedy e2e tests above, which now route through `sample_rows` with temp 0).
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_batched_sampling_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_batched_sampling_two_concurrent() {
        use super::MlxNativeBackend;
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        // SAFETY: test process, set before the worker thread starts.
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");
        // Temperature + top_p (NOT greedy) — these used to fall back to serial; now they batch.
        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams {
                temperature: Some(0.7),
                top_p: Some(0.9),
                max_tokens: Some(24),
                ..Default::default()
            };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        let s1 = backend.chat(mk("Name a color. Answer in one word. /no_think")).await.unwrap();
        let s2 = backend.chat(mk("Name an animal. Answer in one word. /no_think")).await.unwrap();
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("SAMPLING(temp=0.7)  t1={t1:?}  t2={t2:?}  (run_batch calls={runs})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "two temp>0 requests must batch (relaxed gate), not run serially");
        assert!(!t1.trim().is_empty(), "sampling row 1 produced no output");
        assert!(!t2.trim().is_empty(), "sampling row 2 produced no output");
    }

    // E2E THROUGH THE ADMISSION LAYER — the exact production concurrency path. The gateway
    // serves requests via `concurrency::admit_wrap(backend)` (limit = `concurrency_capacity`
    // = `batch_cap`), so this wraps the REAL MLX backend the same way and fires two
    // concurrent requests: admission must let BOTH reach the worker (limit 2) so they land in
    // ONE `run_batch` (BATCH_RUN_COUNT +1) — proving the batching actually fires end-to-end
    // and isn't serialized by admission. Closes the loop on "does concurrent load batch?".
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_admit_wrap_batches_e2e
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3-4B-4bit"]
    async fn mlx_admit_wrap_batches_e2e() {
        use super::MlxNativeBackend;
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use crate::concurrency::admit_wrap;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        // SAFETY: test process, set before the worker thread starts.
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }
        let dir = resolve_model_dir("mlx-community:Qwen3-4B-4bit")
            .expect("Qwen3-4B-4bit not in HF cache");
        let inner = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into(), None)
            .await
            .expect("backend load");
        // Wrap exactly as `serve` does. capacity == batch_cap() == 2 → admission limit 2.
        let backend: Arc<dyn ChatBackend> = admit_wrap(Arc::new(inner));
        assert_eq!(
            backend.admission_stats().map(|s| s.limit),
            Some(2),
            "admission limit must equal batch_cap (2), or concurrent requests can't batch"
        );

        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        // Fire both through the admission-wrapped backend concurrently.
        let s1 = backend
            .chat(mk("What is the capital of France? Answer in one word. /no_think"))
            .await
            .expect("admit France");
        let s2 = backend
            .chat(mk("What is the capital of Japan? Answer in one word. /no_think"))
            .await
            .expect("admit Japan");
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.unwrap(), t2.unwrap());
        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("ADMIT-E2E  France={t1:?} Japan={t2:?}  (run_batch calls={runs})");
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }
        assert_eq!(runs, 1, "admission must let both concurrent requests batch in ONE run_batch");
        assert!(t1.contains("Paris"), "France: {t1:?}");
        assert!(t2.contains("Tokyo"), "Japan: {t2:?}");

        // Observability: the /stats batch counters must reflect the 2-row batch.
        let bs = super::batch_stats().expect("batch_stats present after a batched run");
        eprintln!(
            "BATCH STATS  runs={} rows={} admits={} max={}",
            bs.runs, bs.rows, bs.admits, bs.max
        );
        assert!(bs.runs >= 1 && bs.rows >= 2 && bs.max >= 2, "batch stats must reflect a 2-row batch: {bs:?}");
    }

    // Hybrid (Qwen3.6) batched scheduler end-to-end: with `ROZUM_BATCH=2`, two concurrent
    // greedy requests on ONE hybrid backend must route to `run_batch_hybrid` (asserted via
    // BATCH_RUN_COUNT) and stream a coherent, DISTINCT response per row (no cross-row
    // leakage). Qwen3.6-27B thinks even with /no_think, so the bar is non-empty + distinct
    // (byte-exactness of the batched forward is proven separately by
    // `mlx_hybrid_batched_ragged_byte_exact`); this exercises the dispatch + per-row
    // BatchSeq streaming/retire over the heterogeneous cache.
    //   cargo test --features mlx-native -- --ignored --nocapture mlx_hybrid_batched_scheduler_two_concurrent
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "requires local mlx-community/Qwen3.6-27B-4bit"]
    async fn mlx_hybrid_batched_scheduler_two_concurrent() {
        use super::MlxNativeBackend;
        use super::inner::BATCH_RUN_COUNT;
        use crate::backend::{ChatBackend, ChatRequest, SamplingParams, collect_to_string};
        use std::sync::atomic::Ordering;

        // Worker reads ROZUM_BATCH once at startup; set before building the backend. A
        // generous window makes the two sends land in the same batch deterministically.
        // SAFETY: test process, set before the worker thread starts.
        unsafe {
            std::env::set_var("ROZUM_BATCH", "2");
            std::env::set_var("ROZUM_BATCH_WINDOW_MS", "500");
        }

        let dir = resolve_model_dir("mlx-community:Qwen3.6-27B-4bit")
            .expect("Qwen3.6-27B-4bit not in HF cache");
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-27B-4bit".into(), None)
            .await
            .expect("backend load");

        let mk = |q: &str| {
            let mut req = ChatRequest::simple(q);
            req.sampling = SamplingParams { max_tokens: Some(24), ..Default::default() };
            req
        };
        let before = BATCH_RUN_COUNT.load(Ordering::Relaxed);
        let s1 = backend
            .chat(mk("What is the capital of France? Answer in one word. /no_think"))
            .await
            .expect("chat France");
        let s2 = backend
            .chat(mk("Name a primary color. Answer in one word. /no_think"))
            .await
            .expect("chat color");
        let (t1, t2) = tokio::join!(collect_to_string(s1), collect_to_string(s2));
        let (t1, t2) = (t1.expect("collect 1"), t2.expect("collect 2"));

        let runs = BATCH_RUN_COUNT.load(Ordering::Relaxed) - before;
        eprintln!("HYBRID BATCHED  t1={t1:?}\n                t2={t2:?}  (run_batch calls={runs})");

        // SAFETY: test cleanup.
        unsafe {
            std::env::remove_var("ROZUM_BATCH");
            std::env::remove_var("ROZUM_BATCH_WINDOW_MS");
        }

        assert_eq!(runs, 1, "the two concurrent hybrid requests must batch in ONE run_batch_hybrid call");
        assert!(!t1.trim().is_empty(), "row 1 produced no output");
        assert!(!t2.trim().is_empty(), "row 2 produced no output");
        assert_ne!(t1, t2, "distinct prompts must give distinct outputs (no cross-row leakage)");
    }
}
