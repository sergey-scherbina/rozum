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
    use mlx_lm::models::{llama, qwen2, qwen3, qwen3_5, qwen3_5_moe, qwen3_moe};
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
    /// Runaway-loop guard: if the last `REPEAT_WINDOW` generated tokens are exactly
    /// periodic with some period ≤ `REPEAT_MAX_PERIOD` (i.e. a short block repeated
    /// ≥ ~4×), the greedy decode is stuck in a loop — stop instead of generating to
    /// the cap. 64 tokens of perfect periodicity essentially never occurs in real
    /// output, so this does not false-trigger on legitimate repetitive text.
    const REPEAT_WINDOW: usize = 64;
    const REPEAT_MAX_PERIOD: usize = 16;
    /// Qwen3 `<|im_end|>`, used when the checkpoint config omits `eos_token_id`.
    const QWEN3_EOS: u32 = 151645;
    /// Fallback context window when config lacks `max_position_embeddings`.
    const DEFAULT_N_CTX: u32 = 32_768;

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
                // Qwen3.6 dense (the config wrapper is `qwen3_5`, text is `qwen3_5_text`).
                "qwen3_5" | "qwen3_5_text" => qwen3_5::load_qwen3_5_model(dir)
                    .map(LoadedModel::Qwen35)
                    .map_err(|e| format!("mlx: load qwen3_5 {}: {e}", dir.display())),
                // Qwen3.6 MoE (wrapper `qwen3_5_moe`, text `qwen3_5_moe_text`).
                "qwen3_5_moe" | "qwen3_5_moe_text" => qwen3_5_moe::load_qwen3_5_moe_model(dir)
                    .map(LoadedModel::Qwen35Moe)
                    .map_err(|e| format!("mlx: load qwen3_5_moe {}: {e}", dir.display())),
                // Llama family (Llama 3.x, and other `model_type: llama` checkpoints).
                "llama" => llama::load_llama_model(dir)
                    .map(LoadedModel::Llama)
                    .map_err(|e| format!("mlx: load llama {}: {e}", dir.display())),
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
        (n_ctx, eos, model_type, kv_per_pos)
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
            let (n_ctx, eos, model_type, kv_per_pos) = read_config(&model_dir);
            let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
            let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
            let label = model_id.clone();

            // The worker loads the model on its own thread and owns it for life;
            // the `!Send` model never crosses a thread boundary.
            thread::Builder::new()
                .name("mlx-native".into())
                .spawn(move || {
                    worker_main(model_dir, model_type, eos, kv_per_pos, jobs_rx, ready_tx)
                })
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
                tools: req.tools,
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

    /// Worker thread entry point. Loads the model (reporting load result over
    /// `ready`), then serves jobs until the queue closes.
    fn worker_main(
        model_dir: PathBuf,
        model_type: String,
        eos: Vec<u32>,
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
        // `prefix` carries the previous dense request's KV across jobs for prefix
        // reuse (the worker is cap-1 serial, so no locking is needed).
        let mut prefix: Option<PrefixCache> = None;
        while let Some(job) = jobs.blocking_recv() {
            run_job(
                &mut model,
                &mut tokenizer,
                &template,
                &eos,
                kv_per_pos,
                &mut prefix,
                job,
            );
        }
    }

    /// True if the tail of `ids` is a runaway loop: the last `REPEAT_WINDOW` tokens
    /// are exactly periodic with some period `1..=REPEAT_MAX_PERIOD`. Used to stop a
    /// greedy decode that has fallen into a short repeating cycle. O(window·period).
    pub(crate) fn is_runaway_loop(ids: &[u32]) -> bool {
        if ids.len() < REPEAT_WINDOW {
            return false;
        }
        let tail = &ids[ids.len() - REPEAT_WINDOW..];
        // For each candidate period p, the window is periodic iff every position
        // equals the one p back. The smallest such p is the true period; any p in
        // range that satisfies it means ≥ REPEAT_WINDOW/p repeats of a ≤p block.
        (1..=REPEAT_MAX_PERIOD).any(|p| (p..REPEAT_WINDOW).all(|i| tail[i] == tail[i - p]))
    }

    /// Persisted dense KV cache from the previous request on this worker, for
    /// prefix reuse. `ids` is the prompt the cache represents (its KV covers
    /// `[0, ids.len())`); when the next prompt extends `ids`, the cache is
    /// truncated to that length and only the new suffix is prefilled.
    struct PrefixCache {
        ids: Vec<u32>,
        cache: Vec<Option<ConcatKeyValueCache>>,
    }

    /// Dense arches own their KV cache externally (`Vec<Option<ConcatKeyValueCache>>`)
    /// and so support prefix reuse via truncation. Hybrid (Qwen3.6) owns its cache
    /// internally and carries a non-truncatable recurrent state (see BACKLOG
    /// `mlx-native-prefix-kv-cache`), so it isn't reused here.
    fn is_dense(model: &LoadedModel) -> bool {
        matches!(
            model,
            LoadedModel::Qwen3(_)
                | LoadedModel::Qwen3Moe(_)
                | LoadedModel::Llama(_)
                | LoadedModel::Qwen2(_)
        )
    }

    /// Render the prompt and stream token events. Dispatches on the model
    /// architecture; each `Generate` iterator feeds the shared streaming loop.
    fn run_job(
        model: &mut LoadedModel,
        tokenizer: &mut Tokenizer,
        template: &str,
        eos: &[u32],
        kv_per_pos: Option<u64>,
        prefix: &mut Option<PrefixCache>,
        job: Job,
    ) {
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

        let temp = job.sampling.temperature.unwrap_or(0.0);
        // top_k <= 0 / top_p >= 1.0 disable those filters (the sampler's defaults).
        let top_p = job.sampling.top_p.unwrap_or(1.0);
        let top_k = job.sampling.top_k.map(|k| k as i32).unwrap_or(0);
        let repeat_penalty = job.sampling.repeat_penalty.unwrap_or(1.0);
        if let Some(s) = job.sampling.seed {
            let _ = mlx_rs::random::seed(s);
        }

        // Prefix-KV reuse (dense arches only — see `is_dense`/`PrefixCache`): if the
        // previous request's prompt is a strict prefix of this one (the append-only
        // agentic-loop case), keep its KV, truncate the cache to the shared length,
        // and prefill only the new suffix. Byte-exact: the kept `[0, reuse)` KV is
        // exactly what a fresh prefill would compute, and `create_attention_mask`
        // builds the causal mask from the cache offset. `ROZUM_PREFIX_CACHE=0` off.
        let prefix_enabled =
            !matches!(std::env::var("ROZUM_PREFIX_CACHE").as_deref(), Ok("0"));
        let dense = is_dense(model);
        let mut cache: Vec<Option<ConcatKeyValueCache>> = Vec::new();
        let mut reuse_len = 0usize;
        if prefix_enabled && dense {
            if let Some(p) = prefix.as_mut() {
                if p.ids.len() < prompt_ids.len() && prompt_ids.starts_with(&p.ids) {
                    reuse_len = p.ids.len();
                    cache = std::mem::take(&mut p.cache);
                    for c in cache.iter_mut().flatten() {
                        c.truncate(reuse_len as i32);
                    }
                }
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
                );
            }
            LoadedModel::Qwen35(m) => {
                // Owns its heterogeneous (KV + conv/recurrent) cache internally.
                let mut generator = qwen3_5::Generate::new(m, temp, &prompt_tokens);
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
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
                );
            }
            LoadedModel::Qwen35Moe(m) => {
                let mut generator = qwen3_5_moe::Generate::new(m, temp, &prompt_tokens);
                let c = job.cancel.clone();
                generator.set_cancel(Box::new(move || c.is_cancelled()));
                generator.set_sampler(top_p, top_k, repeat_penalty);
                stream_generation(
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
                );
            }
        }

        // Persist the (now advanced) dense cache so the next request that extends
        // this prompt can reuse its KV. `cache` is non-empty only for dense arches
        // (hybrid keeps its cache internally); keyed by this prompt's ids, so next
        // turn truncates back to here before prefilling the new suffix. A non-dense
        // run leaves `prefix` untouched (the worker only ever serves one model).
        if prefix_enabled && dense && !cache.is_empty() {
            *prefix = Some(PrefixCache { ids: prompt_ids, cache });
        }
    }

    const TOOL_OPEN: &str = "<tool_call>";
    const TOOL_CLOSE: &str = "</tool_call>";

    /// Parse Qwen3 `<tool_call>{"name":..,"arguments":..}</tool_call>` blocks from
    /// the raw output into `(name, arguments_json)` pairs.
    pub(crate) fn parse_tool_calls(text: &str) -> Vec<(String, String)> {
        let mut calls = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find(TOOL_OPEN) {
            let after = &rest[open + TOOL_OPEN.len()..];
            // Tolerate a missing `</tool_call>` (the model hit EOS right after a
            // complete JSON body): parse the trailing run instead of dropping it.
            let (body, next) = match after.find(TOOL_CLOSE) {
                Some(close) => (after[..close].trim(), Some(&after[close + TOOL_CLOSE.len()..])),
                None => (after.trim(), None),
            };
            if let Some(call) = parse_tool_call_body(body) {
                calls.push(call);
            }
            match next {
                Some(n) => rest = n,
                None => break,
            }
        }
        calls
    }

    /// Parse one tool-call body (the text inside `<tool_call>…</tool_call>`) into
    /// `(name, arguments_json)`. Qwen3.6 emits EITHER form nondeterministically:
    ///   - JSON:  `{"name":"f","arguments":{…}}`
    ///   - XML:   `<function=f><parameter=k>v</parameter>…</function>`
    /// so we accept both (the XML form was silently lost before).
    fn parse_tool_call_body(body: &str) -> Option<(String, String)> {
        // JSON form.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            if !name.is_empty() {
                let args = v
                    .get("arguments")
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                return Some((name.to_string(), args));
            }
        }
        // XML / Hermes form: <function=NAME> <parameter=KEY>VALUE</parameter> … </function>
        let fstart = body.find("<function=")?;
        let aft = &body[fstart + "<function=".len()..];
        let name_end = aft.find('>')?;
        let name = aft[..name_end].trim();
        if name.is_empty() {
            return None;
        }
        let mut args = serde_json::Map::new();
        let mut p = &aft[name_end + 1..];
        while let Some(ps) = p.find("<parameter=") {
            let a2 = &p[ps + "<parameter=".len()..];
            let Some(ke) = a2.find('>') else { break };
            let key = a2[..ke].trim().to_string();
            let vrest = &a2[ke + 1..];
            let Some(ve) = vrest.find("</parameter>") else { break };
            let val = vrest[..ve].trim();
            // Keep a typed value if it's valid JSON (numbers/bools), else a string.
            let jval = serde_json::from_str::<serde_json::Value>(val)
                .unwrap_or_else(|_| serde_json::Value::String(val.to_string()));
            args.insert(key, jval);
            p = &vrest[ve + "</parameter>".len()..];
        }
        Some((name.to_string(), serde_json::Value::Object(args).to_string()))
    }

    /// Architecture-agnostic streaming loop: pull tokens off a `Generate`
    /// iterator, force per-token compute, stop on EOS / max-tokens / cancel,
    /// and emit UTF-8-safe text deltas. Once a `<tool_call>` opener appears, text
    /// streaming stops and the run is parsed into `ToolUse*` events at the end.
    /// A send error means the client dropped the stream -> stop early.
    fn stream_generation<I>(
        generate: I,
        tokenizer: &mut Tokenizer,
        eos: &[u32],
        prompt_len: usize,
        max_tokens: usize,
        job: &Job,
        pipeline: bool,
    ) where
        I: Iterator<Item = Result<Array, Exception>>,
    {
        let mut out_ids: Vec<u32> = Vec::new();
        let mut emitted = String::new(); // text already streamed to the client
        let mut full_text = String::new(); // full decoded run (incl. tool markup)
        let mut output_tokens: u32 = 0;
        let mut tool_seen = false; // once `<tool_call>` appears, stop streaming text

        let mut stop_reason = StopReason::EndTurn;
        // Pipelined decode (mirrors Python `mlx_lm`): build step n+1's graph from the
        // lazy token n and `async_eval` it BEFORE blocking on token n's readback, so
        // the GPU never idles waiting for the CPU to build the next graph. Token
        // output is identical to a serial loop — only the eval timing changes.
        let mut iter = generate;
        let mut cur = match iter.next() {
            Some(Ok(t)) => {
                let _ = mlx_rs::transforms::async_eval([&t]);
                Some(t)
            }
            Some(Err(e)) => {
                let _ = job
                    .events
                    .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                return;
            }
            None => None, // hybrid Generate returns None when cancelled mid-prefill
        };
        // Helper: pull the next token from the iterator, surfacing a stream error.
        let pull = |iter: &mut I, prefetch: bool| -> Result<Option<Array>, ()> {
            match iter.next() {
                Some(Ok(t)) => {
                    // Pipeline: kick off the next token's GPU work now, so the GPU
                    // stays fed while we block reading the current one.
                    if prefetch {
                        let _ = mlx_rs::transforms::async_eval([&t]);
                    }
                    Ok(Some(t))
                }
                Some(Err(e)) => {
                    let _ = job
                        .events
                        .send(Err(ModelError::BackendUnavailable(format!("mlx: {e}"))));
                    Err(())
                }
                None => Ok(None),
            }
        };
        // Per-token cost profiling (ROZUM_DETOK_PROFILE): split GPU sync (eval+item)
        // from detok+string, to locate the prod-path overhead vs the raw bench.
        let profile = std::env::var_os("ROZUM_DETOK_PROFILE").is_some();
        // Runaway-loop guard (on by default; `ROZUM_REPEAT_GUARD=0` disables it).
        let repeat_guard = !matches!(std::env::var("ROZUM_REPEAT_GUARD").as_deref(), Ok("0"));
        let (mut sync_ns, mut detok_ns, mut prof_tokens) = (0u128, 0u128, 0u32);
        while let Some(token) = cur.take() {
            if job.cancel.is_cancelled() {
                stop_reason = StopReason::Cancelled;
                break;
            }
            // Pipelined arches pre-build + `async_eval` the next token now (before we
            // block on the current). Hybrid (custom-kernel) arches don't benefit — the
            // kernel's per-call `eval` already blocks the forward — so they fetch the
            // next token serially after processing the current (`pipeline == false`).
            let next = if pipeline {
                match pull(&mut iter, true) {
                    Ok(n) => n,
                    Err(()) => return,
                }
            } else {
                None
            };
            let t_sync = profile.then(std::time::Instant::now);
            if eval([&token]).is_err() {
                let _ = job.events.send(Err(ModelError::BackendUnavailable(
                    "mlx: eval failed".into(),
                )));
                return;
            }
            let id = token.item::<u32>();
            if let Some(t) = t_sync {
                sync_ns += t.elapsed().as_nanos();
            }
            if output_tokens == 0 && std::env::var("ROZUM_MLX_DEBUG").is_ok() {
                eprintln!("FIRST_TOK {id} (eos={eos:?})");
            }
            if eos.contains(&id) {
                stop_reason = StopReason::EndTurn;
                break;
            }
            out_ids.push(id);
            output_tokens += 1;

            // Incremental detokenize: re-decode the run and emit the new suffix.
            // Hold back a trailing replacement char (an incomplete multi-byte
            // sequence, e.g. mid-Cyrillic) until the next token completes it.
            let t_detok = profile.then(std::time::Instant::now);
            if let Ok(text) = tokenizer.decode(&out_ids, true) {
                let stable = text.trim_end_matches('\u{FFFD}');
                full_text = stable.to_string();
                if !tool_seen {
                    if let Some(pos) = stable.find(TOOL_OPEN) {
                        // Emit any text before the tool opener, then go quiet.
                        if pos > emitted.len() && stable.starts_with(&emitted) {
                            let delta = stable[emitted.len()..pos].to_string();
                            emitted = stable[..pos].to_string();
                            let _ = job.events.send(Ok(ChatEvent::TextDelta { text: delta }));
                        }
                        tool_seen = true;
                    } else if stable.len() > emitted.len() && stable.starts_with(&emitted) {
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
            }

            if let Some(t) = t_detok {
                detok_ns += t.elapsed().as_nanos();
                prof_tokens += 1;
            }

            if output_tokens as usize >= max_tokens {
                stop_reason = StopReason::MaxTokens;
                break;
            }
            // Runaway loop: the greedy decode is repeating a short block (≥64 tokens
            // of perfect periodicity). Stop cleanly instead of generating to the cap
            // and pinning the worker — this is the common runaway failure mode.
            if repeat_guard && is_runaway_loop(&out_ids) {
                if std::env::var_os("ROZUM_MLX_DEBUG").is_some() {
                    eprintln!("REPEAT_GUARD stop after {output_tokens} tokens (loop detected)");
                }
                stop_reason = StopReason::EndTurn;
                break;
            }
            // Advance. Pipelined: the already-`async_eval`'d next token. Serial:
            // fetch it now (after processing the current), no pre-eval.
            cur = if pipeline {
                next
            } else {
                match pull(&mut iter, false) {
                    Ok(n) => n,
                    Err(()) => return,
                }
            };
        }
        if profile && prof_tokens > 0 {
            let n = prof_tokens as f64;
            eprintln!(
                "DETOK_PROFILE tokens={prof_tokens}  sync(eval+item)={:.2}ms/tok  detok+string={:.2}ms/tok",
                sync_ns as f64 / n / 1e6,
                detok_ns as f64 / n / 1e6,
            );
        }
        // The iterator can also end on its own (None): the hybrid `Generate`
        // returns None when cancelled mid-prefill.
        if job.cancel.is_cancelled() {
            stop_reason = StopReason::Cancelled;
        }

        // Finalize: a cancelled run reports as-is; otherwise parse any tool calls.
        let tool_calls = if matches!(stop_reason, StopReason::Cancelled) {
            Vec::new()
        } else {
            parse_tool_calls(&full_text)
        };
        if !tool_calls.is_empty() {
            for (i, (name, args)) in tool_calls.iter().enumerate() {
                let id = format!("call_{i}");
                let _ = job.events.send(Ok(ChatEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                }));
                let _ = job.events.send(Ok(ChatEvent::ToolUseDelta {
                    id: id.clone(),
                    input_json_delta: args.clone(),
                }));
                let _ = job.events.send(Ok(ChatEvent::ToolUseEnd { id }));
            }
            stop_reason = StopReason::ToolUse;
        } else if tool_seen && full_text.len() > emitted.len() {
            // A `<tool_call>` opener appeared (so text streaming stopped) but nothing
            // parsed into a valid call — a truncated/malformed tool call (e.g. MoE
            // greedy nondeterminism emitting odd markup). Don't swallow it: emit the
            // held-back run as text so the client gets a non-empty response instead
            // of silence. (parse_tool_calls also now tolerates a missing close tag.)
            let _ = job.events.send(Ok(ChatEvent::TextDelta {
                text: full_text[emitted.len()..].to_string(),
            }));
        }
        let _ = job.events.send(Ok(ChatEvent::Done {
            input_tokens: prompt_len as u32,
            output_tokens,
            stop_reason,
        }));
    }

    /// Apply the model's chat template to the messages and tokenize, returning
    /// the prompt token ids (with the generation prompt appended).
    /// OpenAI-style tool schemas (`{type:"function", function:{name, description,
    /// parameters}}`) for the chat template's `tools` variable. `None` if empty.
    fn tools_json(tools: &[crate::backend::ToolDef]) -> Option<serde_json::Value> {
        if tools.is_empty() {
            return None;
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

    fn render_prompt(
        tokenizer: &mut Tokenizer,
        template: &str,
        model_id: &str,
        messages: &[Message],
        tools: &[crate::backend::ToolDef],
    ) -> Result<Vec<u32>, String> {
        let convo: Vec<Conversation<&'static str, String>> = messages
            .iter()
            .map(|m| Conversation {
                role: role_str(&m.role),
                content: message_text(m),
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
            add_generation_prompt: Some(true),
            continue_final_message: None,
            enable_thinking: Some(enable_thinking),
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
            | "llama"
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

#[cfg(test)]
mod tests {
    use super::resolve_model_dir;

    #[test]
    fn resolve_missing_spec_is_none() {
        assert!(resolve_model_dir("definitely/not-a-real-model-xyzzy").is_none());
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
        for t in ["qwen3", "qwen3_moe", "llama", "qwen2", "qwen2_5"] {
            assert!(!super::inner::is_hybrid_model(t), "{t} is dense (unretained path)");
        }
    }

    #[cfg(feature = "mlx-native")]
    #[test]
    fn parse_tool_calls_handles_close_tag_and_nested_braces() {
        let p = super::inner::parse_tool_calls;
        // Normal, well-formed.
        assert_eq!(
            p("<tool_call>{\"name\":\"f\",\"arguments\":{\"x\":1}}</tool_call>"),
            vec![("f".to_string(), "{\"x\":1}".to_string())]
        );
        // Nested braces in a string arg (code) — fine, the close tag delimits.
        let code = p("<tool_call>{\"name\":\"write_file\",\"arguments\":{\"content\":\"fn add()->i32{ a + b }\"}}</tool_call>");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].0, "write_file");
        assert!(code[0].1.contains("a + b"));
        // MISSING close tag (model hit EOS after a complete body) — recovered, not dropped.
        assert_eq!(
            p("<tool_call>{\"name\":\"g\",\"arguments\":{}}"),
            vec![("g".to_string(), "{}".to_string())]
        );
        // XML / Hermes form (Qwen3.6 emits this nondeterministically) — was lost before.
        let xml = p("<tool_call>\n<function=write_file>\n<parameter=path>\nadd.rs\n</parameter>\n<parameter=content>\nfn add()->i32{ a + b }\n</parameter>\n</function>\n</tool_call>");
        assert_eq!(xml.len(), 1, "XML tool call should parse: {xml:?}");
        assert_eq!(xml[0].0, "write_file");
        assert!(xml[0].1.contains("add.rs") && xml[0].1.contains("a + b"), "args: {}", xml[0].1);
        // No tool call at all.
        assert!(p("just some text").is_empty());
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

    // Deterministic parser check (no model): extracts name + arguments from the
    // Qwen3 `<tool_call>` markup, handles surrounding text and multiple calls.
    #[cfg(feature = "mlx-native")]
    #[test]
    fn parse_tool_calls_extracts() {
        use super::inner::parse_tool_calls;
        let text = "sure <tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}\n</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "get_weather");
        assert!(calls[0].1.contains("Paris"), "args: {}", calls[0].1);

        assert!(parse_tool_calls("plain answer, no tools").is_empty());

        let two = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\
                   <tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call>";
        let calls = parse_tool_calls(two);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "b");
    }

    // An assistant ToolUse block must survive `message_text` as `<tool_call>`
    // markup so multi-turn tool loops keep the prior call in history. This is
    // the inverse of `parse_tool_calls` — render then re-parse round-trips.
    #[test]
    fn tool_use_round_trips_into_history() {
        use super::inner::{message_text, parse_tool_calls};
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
    fn runaway_loop_detection() {
        use super::inner::is_runaway_loop;
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
        let a = MlxNativeBackend::new(dir.clone(), "mlx-community/Qwen3-4B-4bit".into())
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
        let b = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into())
            .await
            .expect("load B");
        let fresh = collect_to_string(b.chat(req(convo2)).await.unwrap())
            .await
            .unwrap();

        eprintln!("REUSE: {reuse:?}\nFRESH: {fresh:?}");
        assert_eq!(reuse, fresh, "prefix-reuse output must byte-match a fresh prefill");
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
        let backend = MlxNativeBackend::new(dir, model_id.into())
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
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"))
            .await
            .expect("backend load");
        let req =
            ChatRequest::simple("What is the capital of France? Answer in one short sentence.");
        let stream = backend.chat(req).await.expect("chat");
        let text = collect_to_string(stream).await.expect("collect");
        eprintln!("MLX LLAMA OUTPUT: {text}");
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
        let backend = MlxNativeBackend::new(dir, "Qwen2.5-0.5B-Instruct-4bit".into())
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
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"))
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3-4B-4bit".into())
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
        let backend = MlxNativeBackend::new(dir, "mlx-community/Qwen3.6-35B-A3B-4bit".into())
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
}
