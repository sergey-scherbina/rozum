//! Internal engine seam — see `docs/specs/native-engine-spi.md`.
//!
//! `LocalEngine` is the smallest surface an **in-process** inference engine
//! exposes; the engine-agnostic decode/serving loop ([`drive`]) lives above it so
//! a new engine (e.g. the future Vulkan/x86 leaf) is "implement this trait + its
//! kernels", not "re-implement the whole leaf". The seam is at the **token/text**
//! level — the engine owns its whole forward + sampling graph, so there is no
//! per-op cross-runtime sync (the `mistralrs-mlx-direct` dead-end).
//!
//! Status (`native-engine-spi`): the seam is defined here and the engine-agnostic
//! consumption loop [`consume_tokens`] is shared + already used by the MLX leaf
//! (A2a/A2b). [`drive`] wraps it over the `LocalEngine` seam (A2). Routing the MLX
//! leaf *through* `drive` and lifting prompt-render are deferred until the real x86
//! leaf shapes the trait — notably a cache-reclaim seam the MLX **hybrid** path
//! needs (it reclaims the generator's internal KV/conv cache after a run for prefix
//! reuse, which a `generate() -> Box<dyn Iterator>` return type erases).

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::backend::{ChatEvent, ModelError, ModelResult, SamplingParams, StopReason};

/// Static facts the shared driver needs from a loaded model.
pub struct EngineMeta {
    /// Maximum context length in tokens.
    pub n_ctx: u32,
    /// All stop-token ids (multi-EOS — e.g. Qwen `<|im_end|>` + `<|endoftext|>`,
    /// gpt-oss `<|return|>` / `<|call|>` / `<|endoftext|>`).
    pub eos: Vec<u32>,
    /// `config.json`'s effective `model_type`.
    pub model_type: String,
    /// gpt-oss **harmony** channel format (`drive` picks `harmony::parse_harmony`)
    /// vs the Qwen `<tool_call>` parser (`serving::parse_tool_calls`).
    pub harmony: bool,
    /// Decode the run KEEPING special tokens (`skip_special_tokens=false`). Needed for GLM-4.5/4.6/4.7,
    /// whose `<tool_call>` / `<arg_key>` / `<arg_value>` tool-call markers are SPECIAL tokens — the
    /// default skip-special decode strips them, collapsing `<tool_call>bash<arg_key>command</arg_key>…`
    /// to `bashcommand…` so `serving::parse_tool_calls` finds nothing. With this set, the markers
    /// survive in `full_text` (and the stream splits prose from the held-back markup on `<tool_call>`),
    /// and `parse_glm_arg_kv` recovers the call. Harmless for arches that emit no special tokens mid-run.
    pub keep_special: bool,
    /// GLM create-from-scratch artifact synthesis is armed for this request (GLM family AND
    /// `ROZUM_GLM_ARTIFACT_SYNTH` enabled — computed engine-side). When set, `consume_tokens`
    /// falls back to [`crate::serving::synth_glm_tool_calls`] over `tools` if the normal parse
    /// finds no call (docs/specs/glm-artifact-write-synth.md). GLM is a DENSE arch → it finalizes
    /// here in the engine seam, not in the mlx batch path.
    pub glm_synth: bool,
    /// The request's offered tools — the schema source the GLM artifact synth matches bare JSON
    /// args against to recover the tool name. Empty unless `glm_synth`.
    pub tools: Vec<crate::backend::ToolDef>,
}

/// Construction knobs common to local engines.
#[derive(Clone, Debug, Default)]
pub struct EngineOptions {
    /// Override the context window (else the model's max, RAM-bounded).
    pub n_ctx: Option<u32>,
    /// Pick a specific compute device (else the best integrated GPU).
    pub device_index: Option<u32>,
}

/// The smallest hardware-facing surface. Everything above it — templating,
/// tokenization, the detok→[`ChatEvent`] loop, tool-call parsing (serving +
/// harmony), EOS/cancel/max-tokens, stream assembly, sampling glue — is shared in
/// [`drive`]. The engine owns its whole forward + sampling graph; it only yields
/// tokens.
///
/// **Not `Send`.** In-process engines are routinely `!Send`: the MLX leaf pins its
/// model + `Array`s + KV cache to one Metal-stream worker thread for life, and
/// llama.cpp's `LlamaContext` is likewise thread-bound. [`drive`] runs the engine
/// **synchronously on the engine's own thread** (it never moves the engine across
/// threads), so a `Send` bound would only lock these engines out of the seam for no
/// benefit. Engines that *are* `Send` still are — this just doesn't require it.
pub trait LocalEngine {
    /// Load weights (zero-copy `mmap` internally) + tokenizer + config from a
    /// model directory.
    fn load(dir: &Path, opts: &EngineOptions) -> Result<Self, String>
    where
        Self: Sized;

    /// Static facts for the shared driver (context, EOS, template/format).
    fn meta(&self) -> &EngineMeta;

    /// A token iterator for a sampled generation over `prompt` (prefill → decode).
    /// The engine samples however suits its hardware — MLX on the GPU inside its
    /// graph; a CPU/Vulkan engine by materializing the last-row logits and calling
    /// [`crate::sampler::sample`]. Honors `params`; polls `cancel` and stops early
    /// (the driver emits `Done{Cancelled}`).
    fn generate<'a>(
        &'a mut self,
        prompt: &'a [u32],
        params: &'a SamplingParams,
        cancel: &'a CancellationToken,
    ) -> Box<dyn Iterator<Item = Result<u32, String>> + 'a>;

    /// Opt-in: append-only prefix-KV reuse across turns (default: unsupported).
    fn supports_prefix_reuse(&self) -> bool {
        false
    }
}

/// The shared, engine-agnostic decode driver: run [`LocalEngine::generate`] over a
/// rendered `prompt` and feed its token ids through [`consume_tokens`] (detok →
/// [`ChatEvent`]s, tool-call parse via serving/harmony, EOS/cancel/max-tokens/
/// runaway guard, finalize with `Done`). Returns the final [`StopReason`].
///
/// Prompt rendering (chat template + tokenizer) and detokenization stay with the
/// caller: the engine's tokenizer is borrowed separately from its forward graph,
/// so `decode` is passed in rather than reached through `&self` (which `generate`
/// already borrows mutably for the iterator's lifetime).
///
/// **native-engine-spi status:** the shared driver for engines whose generation owns
/// no reclaimable post-run state (the x86 Vulkan leaf, dense MLX, GGUF). The `Send`
/// bound that previously kept `!Send` engines off the seam is now relaxed (see the
/// trait), so a dense MLX / llama.cpp engine *can* implement `LocalEngine` and route
/// through `drive`. The MLX **hybrid** path still calls [`consume_tokens`] directly
/// because it reclaims the generator's internal KV/conv cache *after* the run
/// (`into_cache_and_snapshot`, for prefix reuse) — state a `Box<dyn Iterator>` return
/// erases; that cache-reclaim seam stays deferred, to be shaped against the real x86
/// engine (`docs/specs/native-engine-spi.md`).
#[allow(clippy::too_many_arguments)]
pub fn drive<E, D, F>(
    engine: &mut E,
    prompt: &[u32],
    params: &SamplingParams,
    max_tokens: usize,
    repeat_guard: bool,
    cancel: &CancellationToken,
    decode: D,
    emit: F,
) -> StopReason
where
    E: LocalEngine,
    D: FnMut(&[u32], bool) -> Option<String>,
    F: FnMut(ModelResult<ChatEvent>) -> bool,
{
    // Snapshot the static facts before `generate` takes the mutable borrow.
    let meta = {
        let m = engine.meta();
        EngineMeta {
            n_ctx: m.n_ctx,
            eos: m.eos.clone(),
            model_type: m.model_type.clone(),
            harmony: m.harmony,
            keep_special: m.keep_special,
            glm_synth: m.glm_synth,
            tools: m.tools.clone(),
        }
    };
    let prompt_len = prompt.len();
    let tokens = engine.generate(prompt, params, cancel);
    consume_tokens(
        tokens,
        &meta,
        prompt_len,
        max_tokens,
        repeat_guard,
        &params.stop,
        cancel,
        decode,
        emit,
    )
}

// ─────────── reclaim seam (DRAFT — not wired; shape TBD against x86) ───────────
//
// `LocalEngine::generate` returns a `Box<dyn Iterator>` whose state is DROPPED when
// the run ends. That's fine for engines that keep nothing post-run (dense MLX, GGUF,
// CPU). But the MLX **hybrid** arch (Qwen3.6) reclaims its generator's internal
// KV/conv cache *after* a run — `generator.into_cache_and_snapshot()` in
// `mlx_native_backend.rs` — and stashes it (`store.put_hybrid`) for next-turn prefix
// reuse. `Box<dyn Iterator>` ERASES that concrete state, so a hybrid engine can't go
// through `drive` without losing prefix reuse — which is why it still calls
// `consume_tokens` directly.
//
// This draft sketches the seam: a token stream that can hand its post-run state back,
// and a `drive` variant that returns it. It is **deliberately unwired** — the final
// shape (whether it folds into `LocalEngine`, the exact `State` bounds, engine-side
// production of a `ReclaimStream`) is to be decided against the real x86 engine, the
// second prefix-reuse-capable engine, which doesn't exist on M4 yet. Validated here
// only against a fake hybrid stream (no GPU/MLX), to prove the seam is coherent and
// implementable before committing the API. See `docs/specs/native-engine-spi.md`.

/// A token stream that, once driven to exhaustion, can hand back reclaimable post-run
/// state — the seam the MLX hybrid path needs. `into_state` mirrors the MLX generator's
/// `into_cache_and_snapshot()`; `State` is the reclaimable cache (+ prefill snapshot)
/// the caller stashes for next-turn prefix reuse. DRAFT.
pub trait ReclaimStream: Iterator<Item = Result<u32, String>> {
    /// The reclaimable post-run state (e.g. a prefix-reuse KV/conv cache).
    type State;
    /// Consume the (exhausted) stream and reclaim its state.
    fn into_state(self: Box<Self>) -> Self::State;
}

/// Like [`drive`], but for a [`ReclaimStream`]: run the same shared decode loop
/// ([`consume_tokens`], borrowed so the stream survives), then reclaim the stream's
/// post-run state and return it with the stop reason. The hybrid caller would stash
/// the `State` for next-turn prefix reuse. DRAFT — the engine-side production of the
/// stream + the MLX wire-up are deferred (shape against x86).
pub fn drive_reclaiming<S, D, F>(
    mut stream: Box<S>,
    meta: &EngineMeta,
    prompt_len: usize,
    max_tokens: usize,
    repeat_guard: bool,
    cancel: &CancellationToken,
    decode: D,
    emit: F,
) -> (StopReason, S::State)
where
    S: ReclaimStream + ?Sized,
    D: FnMut(&[u32], bool) -> Option<String>,
    F: FnMut(ModelResult<ChatEvent>) -> bool,
{
    // Borrow the stream (`&mut Box<S>` is an `IntoIterator`) so it isn't consumed by
    // the loop — we still own it afterward to reclaim its state.
    let stop = consume_tokens(
        &mut stream,
        meta,
        prompt_len,
        max_tokens,
        repeat_guard,
        // This seam is a DRAFT and takes no request, so it carries no stop strings. When it is
        // wired for real it must thread them, or it will be the one path that ignores them.
        &[],
        cancel,
        decode,
        emit,
    );
    let state = stream.into_state();
    (stop, state)
}

// ─────────── A2: the engine-agnostic token-consumption loop ───────────
// Lifted verbatim from the MLX leaf's `stream_generation` so every engine shares
// one copy. Pure over token ids + a `decode` closure + an `emit` sink — no GPU,
// unit-tested below. A2b rewires the MLX leaf (and A3 the GGUF leaf) to call this,
// deleting their private copies.

use std::sync::atomic::{AtomicU64, Ordering};

/// The Qwen tool-call opener; once it appears, text streaming stops and the run is
/// parsed into tool calls at the end.
const TOOL_OPEN: &str = "<tool_call>";

/// Runaway-loop guard: the last [`REPEAT_WINDOW`] generated tokens are exactly
/// periodic with some period `1..=`[`REPEAT_MAX_PERIOD`] (a short block repeated) —
/// stop cleanly instead of generating to the cap. Pure logic over token ids.
const REPEAT_WINDOW: usize = 64;
const REPEAT_MAX_PERIOD: usize = 16;

pub fn is_runaway_loop(ids: &[u32]) -> bool {
    if ids.len() < REPEAT_WINDOW {
        return false;
    }
    let tail = &ids[ids.len() - REPEAT_WINDOW..];
    (1..=REPEAT_MAX_PERIOD).any(|p| (p..REPEAT_WINDOW).all(|i| tail[i] == tail[i - p]))
}

/// Where the emittable text ends, and whether a stop string COMPLETED there.
///
/// Two jobs in one pass, because they are the same question asked at different confidence:
/// - a stop string is present → cut there, drop everything after it, end the turn;
/// - a stop string is half-present at the tail → cut before it and emit nothing more YET, because
///   the next token decides whether those bytes were text or the start of a boundary.
///
/// The second half is the one that is easy to skip and impossible to fix later: text streamed to a
/// client cannot be recalled, so emitting `"Fin"` because the stop string `"Finish"` had not
/// arrived yet corrupts the answer for everyone downstream. A model emits multi-byte tokens; a stop
/// string lands mid-token routinely.
///
/// An empty `stops` returns `(text.len(), false)` — the exact no-op that keeps every request which
/// asked for nothing byte-identical.
pub fn stop_boundary(text: &str, stops: &[String]) -> (usize, bool) {
    let mut cut = text.len();
    let mut completed = false;
    let mut hold = 0usize;

    for s in stops.iter().filter(|s| !s.is_empty()) {
        if let Some(i) = text.find(s.as_str()) {
            if !completed || i < cut {
                cut = i;
                completed = true;
            }
            continue;
        }
        // Not present in full. How much of the tail could be its beginning? Walk the stop string's
        // char boundaries, longest first — a partial match must not split a UTF-8 sequence.
        let mut k = s.len();
        while k > 0 {
            k -= 1;
            if !s.is_char_boundary(k) {
                continue;
            }
            if k > 0 && text.ends_with(&s[..k]) {
                hold = hold.max(k);
                break;
            }
        }
    }

    if completed {
        return (cut, true);
    }
    (text.len().saturating_sub(hold), false)
}

/// A process-unique tool-call id. The Anthropic/OpenAI contract requires each
/// `tool_use` id to be unique within a conversation so the client can pair the
/// `tool_result` back to it (a per-response `call_{i}` reset collides across turns
/// and Claude Code then drops the turn). A monotonic counter guarantees freshness.
pub fn next_tool_call_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("call_{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Drive a token-id stream into [`ChatEvent`]s — the engine-agnostic half of the
/// decode loop. Per token: detok the running output and stream the new suffix
/// (harmony: only the `final` channel, analysis/commentary hidden; otherwise plain
/// text until a `<tool_call>` opener), honoring EOS / max-tokens / cancel / the
/// runaway guard. Then finalize: parse tool calls ([`crate::harmony::parse_harmony`]
/// for gpt-oss, else [`crate::serving::parse_tool_calls`]) and emit `ToolUse*`, or
/// flush any held-back text, then `Done`.
///
/// `decode(ids, skip_special)` is the engine's detokenizer; `emit` returns `false`
/// when the client dropped the stream (stop early). Returns the final stop reason.
#[allow(clippy::too_many_arguments)]
pub fn consume_tokens<T, D, E>(
    tokens: T,
    meta: &EngineMeta,
    prompt_len: usize,
    max_tokens: usize,
    repeat_guard: bool,
    stops: &[String],
    cancel: &CancellationToken,
    mut decode: D,
    mut emit: E,
) -> StopReason
where
    T: IntoIterator<Item = Result<u32, String>>,
    D: FnMut(&[u32], bool) -> Option<String>,
    E: FnMut(ModelResult<ChatEvent>) -> bool,
{
    let mut out_ids: Vec<u32> = Vec::new();
    let mut emitted = String::new(); // text already streamed to the client
    let mut full_text = String::new(); // full decoded run (markers / tool markup)
    let mut output_tokens: u32 = 0;
    let mut tool_seen = false;
    let mut stop_reason = StopReason::EndTurn;
    let mut client_gone = false;

    for tok in tokens {
        if cancel.is_cancelled() {
            stop_reason = StopReason::Cancelled;
            break;
        }
        let id = match tok {
            Ok(id) => id,
            Err(e) => {
                emit(Err(ModelError::BackendUnavailable(e)));
                return stop_reason;
            }
        };
        if meta.eos.contains(&id) {
            stop_reason = StopReason::EndTurn;
            break;
        }
        out_ids.push(id);
        output_tokens += 1;

        if meta.harmony {
            // Decode KEEPING special tokens so the channel markers are literal;
            // stream only the growing `final` channel text.
            if let Some(marked) = decode(&out_ids, false) {
                let stable = marked.trim_end_matches('\u{FFFD}');
                full_text = stable.to_string();
                let ft = crate::harmony::parse_harmony(stable).final_text;
                // Stop strings apply to what the client SEES — for harmony that is the `final`
                // channel, not the marker-bearing raw run. `full_text` is deliberately left alone
                // here: harmony's tool calls are parsed out of channels, not out of this string.
                let (cut, hit_stop) = stop_boundary(&ft, stops);
                let ft = ft[..cut].to_string();
                if ft.len() > emitted.len() && ft.starts_with(&emitted) {
                    let delta = ft[emitted.len()..].to_string();
                    emitted = ft;
                    if !emit(Ok(ChatEvent::TextDelta { text: delta })) {
                        client_gone = true;
                        break;
                    }
                }
                if hit_stop {
                    stop_reason = StopReason::StopSequence;
                    break;
                }
            }
        } else if let Some(text) = decode(&out_ids, !meta.keep_special) {
            let stable = text.trim_end_matches('\u{FFFD}');
            full_text = stable.to_string();
            // The client's stop strings bound what may be EMITTED, not what has been generated:
            // `full_text` keeps everything until a stop actually completes, so a held-back tail
            // that turns out to be ordinary text is still there for the tool-call parse below.
            let (cut, hit_stop) = stop_boundary(stable, stops);
            let visible = &stable[..cut];
            if !tool_seen {
                if let Some(pos) = visible.find(TOOL_OPEN) {
                    if pos > emitted.len() && visible.starts_with(&emitted) {
                        let delta = visible[emitted.len()..pos].to_string();
                        emitted = visible[..pos].to_string();
                        let _ = emit(Ok(ChatEvent::TextDelta { text: delta }));
                    }
                    tool_seen = true;
                } else if visible.len() > emitted.len() && visible.starts_with(&emitted) {
                    let delta = visible[emitted.len()..].to_string();
                    emitted = visible.to_string();
                    if !emit(Ok(ChatEvent::TextDelta { text: delta })) {
                        client_gone = true;
                        break;
                    }
                }
            }
            if hit_stop {
                // Now it is decided: everything from the stop string on is dropped, including from
                // the text the tool parser will read.
                full_text = visible.to_string();
                stop_reason = StopReason::StopSequence;
                break;
            }
        }

        if output_tokens as usize >= max_tokens {
            stop_reason = StopReason::MaxTokens;
            break;
        }
        if repeat_guard && is_runaway_loop(&out_ids) {
            stop_reason = StopReason::EndTurn;
            break;
        }
    }

    if client_gone {
        return StopReason::Cancelled; // nothing more to send; client is gone
    }
    if cancel.is_cancelled() {
        stop_reason = StopReason::Cancelled;
    }

    // Diagnostic: dump the raw marker-bearing run + the harmony parse, to verify
    // exactly what the model emitted vs what we forward (ROZUM_HARMONY_DUMP=1).
    if meta.harmony && std::env::var_os("ROZUM_HARMONY_DUMP").is_some() {
        let p = crate::harmony::parse_harmony(&full_text);
        eprintln!("─── HARMONY_DUMP (stop={stop_reason:?} out_tokens={output_tokens}) ───");
        eprintln!("RAW: {full_text:?}");
        eprintln!(
            "PARSED final={:?} | tool_calls={:?} | analysis_len={}",
            p.final_text,
            p.tool_calls,
            p.analysis.len()
        );
    }

    // Diagnostic: the EXACT raw text the model generated, before tool-call parsing (any arch, not
    // just harmony) — to see whether a "no tool call" turn is genuinely prose or a lost/malformed call.
    if std::env::var_os("ROZUM_RAW_DUMP").is_some() {
        eprintln!("─── RAW_DUMP (stop={stop_reason:?} out_tokens={output_tokens}) ───\nRAW: {full_text:?}");
    }

    // Finalize: a cancelled run reports as-is; otherwise parse any tool calls.
    let tool_calls = if matches!(stop_reason, StopReason::Cancelled) {
        Vec::new()
    } else if meta.harmony {
        crate::harmony::parse_harmony(&full_text).tool_calls
    } else {
        let mut calls = crate::serving::parse_tool_calls(&full_text);
        // GLM create-from-scratch artifact fallback: GLM (a DENSE arch) finalizes HERE, not in the
        // mlx batch path. When it showed the work (tool-args JSON, or labeled file content) instead of
        // (or in addition to) NAMING tools, synthesize those calls so the files/commands actually land
        // (docs/specs/glm-artifact-write-synth.md). ADDITIVE: parse (named calls) and synth (nameless
        // args) are complementary — `synth_glm_tool_calls` skips objects that already carry a `name`,
        // and returns empty unless a bare JSON object matches an offered tool's schema or a safe prose
        // filename is found — so a MIXED response (1 named call + N artifacts) keeps both. Dedup by
        // exact (name, args) guards the rare overlap.
        if meta.glm_synth {
            let synth = crate::serving::synth_glm_tool_calls(&full_text, &meta.tools);
            let synth_n = synth.len();
            for c in synth {
                if !calls.contains(&c) {
                    calls.push(c);
                }
            }
            crate::obs::log_event(serde_json::json!({
                "event": "glm_artifact_synth",
                "stop": format!("{stop_reason:?}"),
                "parsed_calls": calls.len().saturating_sub(synth_n.min(calls.len())),
                "synth_calls": synth_n,
                "total_calls": calls.len(),
                "n_tools": meta.tools.len(),
                "text_len": full_text.len(),
            }));
        }
        calls
    };
    // Observability (gw-toolcall-parse-observability): tool-call markup present but NOTHING parsed —
    // the driver/model tool-DELIVERY mismatch signature (a malformed apply_patch form, an unknown XML
    // dialect, escaped-JSON code). Log the miss to `~/.rozum/gateway.jsonl` so it's grep-able instead
    // of needing a manual `ROZUM_RAW_DUMP` re-run. Only the MISS is logged (a parse is the norm → no
    // noise). This is the SINGLE-request seam; the batched mlx path logs the same event.
    if tool_calls.is_empty() && !matches!(stop_reason, StopReason::Cancelled) {
        let markers = crate::serving::toolish_markers(&full_text);
        if !markers.is_empty() {
            let n = full_text.chars().count();
            let tail: String = full_text.chars().skip(n.saturating_sub(200)).collect();
            crate::obs::log_event(serde_json::json!({
                "event": "toolcall_parse_miss",
                "model_type": meta.model_type,
                "markers": markers,
                "text_chars": n,
                "tail": tail,
            }));
        }
    }
    if !tool_calls.is_empty() {
        for (name, args) in tool_calls.iter() {
            let id = next_tool_call_id();
            let _ = emit(Ok(ChatEvent::ToolUseStart {
                id: id.clone(),
                name: name.clone(),
            }));
            let _ = emit(Ok(ChatEvent::ToolUseDelta {
                id: id.clone(),
                input_json_delta: args.clone(),
            }));
            let _ = emit(Ok(ChatEvent::ToolUseEnd { id }));
        }
        stop_reason = StopReason::ToolUse;
    } else if meta.harmony {
        // Flush any `final` text not yet streamed.
        let ft = crate::harmony::parse_harmony(&full_text).final_text;
        if ft.len() > emitted.len() && ft.starts_with(&emitted) {
            let _ = emit(Ok(ChatEvent::TextDelta {
                text: ft[emitted.len()..].to_string(),
            }));
        }
    } else if tool_seen && full_text.len() > emitted.len() {
        // A `<tool_call>` opener appeared but nothing parsed — don't swallow it.
        let _ = emit(Ok(ChatEvent::TextDelta {
            text: full_text[emitted.len()..].to_string(),
        }));
    }
    let _ = emit(Ok(ChatEvent::Done {
        input_tokens: prompt_len as u32,
        output_tokens,
        stop_reason,
    }));
    stop_reason
}

#[cfg(test)]
mod tests {
    use super::{consume_tokens, stop_boundary, EngineMeta};
    use crate::backend::{ChatEvent, StopReason};
    use tokio_util::sync::CancellationToken;

    fn meta(harmony: bool, eos: Vec<u32>) -> EngineMeta {
        EngineMeta {
            n_ctx: 4096,
            eos,
            model_type: "test".into(),
            harmony,
            keep_special: false,
            glm_synth: false,
            tools: Vec::new(),
        }
    }

    /// A toy detokenizer: each id maps to (literal, is_special). `decode(ids, skip)`
    /// concatenates literals, omitting special tokens when `skip` is true.
    fn toy_decode<'a>(
        table: &'a std::collections::HashMap<u32, (&'static str, bool)>,
    ) -> impl FnMut(&[u32], bool) -> Option<String> + 'a {
        move |ids, skip| {
            let mut s = String::new();
            for id in ids {
                if let Some((lit, special)) = table.get(id) {
                    if !(skip && *special) {
                        s.push_str(lit);
                    }
                }
            }
            Some(s)
        }
    }

    fn run(
        ids: &[u32],
        m: &EngineMeta,
        max_tokens: usize,
        table: &std::collections::HashMap<u32, (&'static str, bool)>,
    ) -> (Vec<ChatEvent>, StopReason) {
        run_with_stops(ids, m, max_tokens, table, &[])
    }

    fn run_with_stops(
        ids: &[u32],
        m: &EngineMeta,
        max_tokens: usize,
        table: &std::collections::HashMap<u32, (&'static str, bool)>,
        stops: &[String],
    ) -> (Vec<ChatEvent>, StopReason) {
        let mut events = Vec::new();
        let cancel = CancellationToken::new();
        let stop = consume_tokens(
            ids.iter().map(|&i| Ok(i)),
            m,
            3,
            max_tokens,
            true,
            stops,
            &cancel,
            toy_decode(table),
            |e| {
                if let Ok(ev) = e {
                    events.push(ev);
                }
                true
            },
        );
        (events, stop)
    }

    #[test]
    fn non_harmony_streams_text_then_done() {
        let table = std::collections::HashMap::from([
            (1, ("Hel", false)),
            (2, ("lo", false)),
            (9, ("", false)), // eos
        ]);
        let m = meta(false, vec![9]);
        let (events, stop) = run(&[1, 2, 9], &m, 100, &table);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert_eq!(stop, StopReason::EndTurn);
        assert!(matches!(events.last(), Some(ChatEvent::Done { .. })));
    }

    #[test]
    fn a_stop_string_ends_the_turn_and_nothing_past_it_is_streamed() {
        // Through the real loop, not just the boundary helper: the tokens after the stop string are
        // never decoded into output, and the turn ends without waiting for EOS.
        let table = std::collections::HashMap::from([
            (1, ("answer: 42", false)),
            (2, ("\nHuman:", false)),
            (3, (" and more", false)),
            (9, ("", false)),
        ]);
        let m = meta(false, vec![9]);
        let stops = vec!["\nHuman:".to_string()];
        let (events, stop) = run_with_stops(&[1, 2, 3, 9], &m, 100, &table, &stops);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "answer: 42", "everything from the stop string on is dropped");
        assert_eq!(stop, StopReason::StopSequence, "and the caller can tell WHY it stopped");
        assert!(matches!(events.last(), Some(ChatEvent::Done { .. })), "the turn still finishes properly");
    }

    #[test]
    fn a_stop_string_split_across_tokens_is_not_streamed_before_it_completes() {
        // The case the hold-back exists for. "\nHum" arrives first and is a prefix of the stop
        // string; streaming it would put text on the client's screen that the stop was meant to
        // remove, and no later token can take it back.
        let table = std::collections::HashMap::from([
            (1, ("done", false)),
            (2, ("\nHum", false)),
            (3, ("an:", false)),
            (9, ("", false)),
        ]);
        let m = meta(false, vec![9]);
        let stops = vec!["\nHuman:".to_string()];
        let (events, stop) = run_with_stops(&[1, 2, 3, 9], &m, 100, &table, &stops);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "done", "the half-arrived boundary was held, not streamed");
        assert_eq!(stop, StopReason::StopSequence);
    }

    #[test]
    fn a_held_back_tail_that_turns_out_to_be_text_is_still_delivered() {
        // The other half of hold-back, and the one that would silently truncate answers if wrong:
        // "\nHum" is held while "\nHuman:" is possible, then released once the next token proves
        // it was ordinary prose.
        let table = std::collections::HashMap::from([
            (1, ("done", false)),
            (2, ("\nHum", false)),
            (3, ("ble pie", false)),
            (9, ("", false)),
        ]);
        let m = meta(false, vec![9]);
        let stops = vec!["\nHuman:".to_string()];
        let (events, stop) = run_with_stops(&[1, 2, 3, 9], &m, 100, &table, &stops);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "done\nHumble pie");
        assert_eq!(stop, StopReason::EndTurn, "released as prose: this turn ended normally");
    }

    #[test]
    fn non_harmony_tool_call_is_parsed_not_streamed() {
        // "<tool_call>{\"name\":\"Foo\",\"arguments\":{}}</tool_call>"
        let table = std::collections::HashMap::from([
            (1, ("<tool_call>", false)),
            (2, ("{\"name\":\"Foo\",\"arguments\":{}}", false)),
            (3, ("</tool_call>", false)),
            (9, ("", false)),
        ]);
        let m = meta(false, vec![9]);
        let (events, stop) = run(&[1, 2, 3, 9], &m, 100, &table);
        assert_eq!(stop, StopReason::ToolUse);
        assert!(events.iter().any(
            |e| matches!(e, ChatEvent::ToolUseStart { name, .. } if name == "Foo")
        ));
        // The tool markup must NOT be streamed as text.
        assert!(!events
            .iter()
            .any(|e| matches!(e, ChatEvent::TextDelta { text } if text.contains("tool_call"))));
    }

    #[test]
    fn harmony_streams_final_hides_analysis() {
        // <|channel|>analysis<|message|>think<|end|><|start|>assistant
        // <|channel|>final<|message|>Hi
        let table = std::collections::HashMap::from([
            (10, ("<|channel|>", true)),
            (11, ("<|message|>", true)),
            (12, ("<|end|>", true)),
            (13, ("<|start|>", true)),
            (20, ("analysis", false)),
            (21, ("final", false)),
            (22, ("assistant", false)),
            (30, ("think", false)),
            (31, ("Hi", false)),
            (9, ("", false)),
        ]);
        let m = meta(true, vec![9]);
        let ids = [10, 20, 11, 30, 12, 13, 22, 10, 21, 11, 31, 9];
        let (events, _) = run(&ids, &m, 100, &table);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hi"); // only the final channel; "think" (analysis) hidden
    }

    #[test]
    fn tool_call_ids_are_unique() {
        // A per-turn `call_0` reset collides across turns and the client (Claude
        // Code) then can't pair the tool_result; each id must be fresh.
        assert_ne!(super::next_tool_call_id(), super::next_tool_call_id());
    }

    #[test]
    fn eos_and_max_tokens_stop() {
        let table = std::collections::HashMap::from([(1, ("a", false)), (9, ("", false))]);
        let m = meta(false, vec![9]);
        let (_e, stop) = run(&[1, 1, 1], &m, 2, &table);
        assert_eq!(stop, StopReason::MaxTokens);
        let (_e2, stop2) = run(&[1, 9, 1], &m, 100, &table);
        assert_eq!(stop2, StopReason::EndTurn);
    }

    // A minimal in-memory `LocalEngine` (no hardware) — a second implementor used to
    // exercise `drive` end-to-end: yield scripted ids, let `drive` snapshot `meta`,
    // run `generate`, and feed the ids through `consume_tokens`.
    struct FakeEngine {
        meta: EngineMeta,
        ids: Vec<u32>,
    }

    impl super::LocalEngine for FakeEngine {
        fn load(_dir: &std::path::Path, _opts: &super::EngineOptions) -> Result<Self, String> {
            unreachable!("FakeEngine is constructed directly in tests")
        }
        fn meta(&self) -> &EngineMeta {
            &self.meta
        }
        fn generate<'a>(
            &'a mut self,
            _prompt: &'a [u32],
            _params: &'a crate::backend::SamplingParams,
            _cancel: &'a CancellationToken,
        ) -> Box<dyn Iterator<Item = Result<u32, String>> + 'a> {
            Box::new(self.ids.clone().into_iter().map(Ok::<u32, String>))
        }
    }

    // A deliberately `!Send` engine (holds an `Rc`, like MLX/llama.cpp hold thread-
    // bound GPU state) — it would NOT compile under the old `LocalEngine: Send`
    // supertrait. That it now implements the trait AND runs through `drive` is the
    // proof that the Send-relaxation actually unblocks `!Send` in-process engines.
    struct NotSendEngine {
        meta: EngineMeta,
        ids: std::rc::Rc<Vec<u32>>, // Rc is !Send
    }

    impl super::LocalEngine for NotSendEngine {
        fn load(_dir: &std::path::Path, _opts: &super::EngineOptions) -> Result<Self, String> {
            unreachable!()
        }
        fn meta(&self) -> &EngineMeta {
            &self.meta
        }
        fn generate<'a>(
            &'a mut self,
            _prompt: &'a [u32],
            _params: &'a crate::backend::SamplingParams,
            _cancel: &'a CancellationToken,
        ) -> Box<dyn Iterator<Item = Result<u32, String>> + 'a> {
            Box::new((*self.ids).clone().into_iter().map(Ok::<u32, String>))
        }
    }

    #[test]
    fn drive_accepts_a_not_send_engine() {
        // Compile-time proof of the relaxation + a runtime sanity check.
        fn _assert_not_send<T: ?Sized>() {}
        let table = std::collections::HashMap::from([(1, ("hi", false)), (9, ("", false))]);
        let mut eng = NotSendEngine {
            meta: meta(false, vec![9]),
            ids: std::rc::Rc::new(vec![1, 9]),
        };
        let mut got = String::new();
        let cancel = CancellationToken::new();
        let stop = super::drive(
            &mut eng,
            &[1],
            &crate::backend::SamplingParams::default(),
            100,
            true,
            &cancel,
            toy_decode(&table),
            |e| {
                if let Ok(ChatEvent::TextDelta { text }) = e {
                    got.push_str(&text);
                }
                true
            },
        );
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(got, "hi");
    }

    #[test]
    #[test]
    fn no_stop_strings_is_exactly_a_no_op() {
        // The property every request that asked for nothing depends on.
        assert_eq!(stop_boundary("anything at all", &[]), (15, false));
        assert_eq!(stop_boundary("", &[]), (0, false));
        // An empty stop string is not a boundary — it would match everywhere and stop instantly.
        assert_eq!(stop_boundary("abc", &["".to_string()]), (3, false));
    }

    #[test]
    fn a_present_stop_string_cuts_there_and_ends_the_turn() {
        let stops = vec!["END".to_string()];
        assert_eq!(stop_boundary("hello ENDtrailing", &stops), (6, true));
        // Earliest wins when several are present, whichever list order they came in.
        let two = vec!["ZZ".to_string(), "b".to_string()];
        assert_eq!(stop_boundary("aabZZ", &two), (2, true));
    }

    #[test]
    fn a_half_arrived_stop_string_holds_its_bytes_back() {
        // The case that cannot be fixed after the fact: "Fin" must not reach the client while
        // "Finish" is still possible. Emit up to the tail, keep the tail.
        let stops = vec!["Finish".to_string()];
        assert_eq!(stop_boundary("all done Fin", &stops), (9, false));
        assert_eq!(stop_boundary("all done Finis", &stops), (9, false));
        // …and once it completes, the same text cuts at the same place, now for good.
        assert_eq!(stop_boundary("all done Finish", &stops), (9, true));
        // A tail that merely looks similar is not held.
        assert_eq!(stop_boundary("all done Fun", &stops), (12, false));
    }

    #[test]
    fn a_partial_match_never_splits_a_utf8_character() {
        // A stop string whose first byte lands mid-character would panic a naive slice.
        let stops = vec!["конец".to_string()];
        let text = "почти ко";
        let (cut, done) = stop_boundary(text, &stops);
        assert!(!done);
        assert!(text.is_char_boundary(cut), "cut {cut} is inside a character of {text:?}");
        assert_eq!(&text[..cut], "почти ");
        // Full match, multi-byte: cut at the character boundary where it starts.
        let full = "почти конец!";
        let (c2, d2) = stop_boundary(full, &stops);
        assert!(d2);
        assert_eq!(&full[..c2], "почти ");
    }

    fn drive_runs_generate_through_consume_tokens() {
        let table = std::collections::HashMap::from([
            (1, ("Hel", false)),
            (2, ("lo", false)),
            (9, ("", false)), // eos
        ]);
        let mut eng = FakeEngine {
            meta: meta(false, vec![9]),
            ids: vec![1, 2, 9],
        };
        let mut events = Vec::new();
        let cancel = CancellationToken::new();
        let stop = super::drive(
            &mut eng,
            &[100, 101, 102], // rendered prompt (len 3)
            &crate::backend::SamplingParams::default(),
            100,
            true,
            &cancel,
            toy_decode(&table),
            |e| {
                if let Ok(ev) = e {
                    events.push(ev);
                }
                true
            },
        );
        assert_eq!(stop, StopReason::EndTurn);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        // `Done` reports the rendered prompt length `drive` was given.
        assert!(matches!(
            events.last(),
            Some(ChatEvent::Done { input_tokens: 3, .. })
        ));
    }

    // A fake "hybrid" stream: yields scripted ids AND accumulates a pretend KV cache,
    // reclaimed via `into_state` — mirrors the MLX generator's `into_cache_and_snapshot`.
    struct FakeHybridStream {
        ids: std::vec::IntoIter<u32>,
        cache: Vec<u32>, // stand-in for the reclaimable KV/conv cache
    }
    impl Iterator for FakeHybridStream {
        type Item = Result<u32, String>;
        fn next(&mut self) -> Option<Self::Item> {
            let id = self.ids.next()?;
            self.cache.push(id); // the "cache" grows as the run processes tokens
            Some(Ok(id))
        }
    }
    impl super::ReclaimStream for FakeHybridStream {
        type State = Vec<u32>;
        fn into_state(self: Box<Self>) -> Vec<u32> {
            self.cache
        }
    }

    #[test]
    fn drive_reclaiming_returns_post_run_state() {
        // The seam: drive a reclaim-capable stream through the SAME shared loop, then
        // get its post-run state back (what the MLX hybrid path needs for prefix reuse).
        let table = std::collections::HashMap::from([
            (1, ("a", false)),
            (2, ("b", false)),
            (9, ("", false)), // eos
        ]);
        let stream = Box::new(FakeHybridStream {
            ids: vec![1, 2, 9].into_iter(),
            cache: Vec::new(),
        });
        let m = meta(false, vec![9]);
        let cancel = CancellationToken::new();
        let mut text = String::new();
        let (stop, state) = super::drive_reclaiming(
            stream,
            &m,
            4, // prompt_len
            100,
            true,
            &cancel,
            toy_decode(&table),
            |e| {
                if let Ok(ChatEvent::TextDelta { text: t }) = e {
                    text.push_str(&t);
                }
                true
            },
        );
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(text, "ab"); // streamed text via the shared loop (eos not emitted)
        // The reclaimed state reflects every token the run pulled (incl. the eos it saw)
        // — proving the cache survives the shared decode loop and is handed back.
        assert_eq!(state, vec![1, 2, 9]);
    }

    // Also exercise the seam through a trait object, since the real caller would hold a
    // `Box<dyn ReclaimStream<State = …>>`.
    #[test]
    fn drive_reclaiming_works_through_a_trait_object() {
        let table = std::collections::HashMap::from([(1, ("x", false)), (9, ("", false))]);
        let stream: Box<dyn super::ReclaimStream<State = Vec<u32>>> =
            Box::new(FakeHybridStream { ids: vec![1, 9].into_iter(), cache: Vec::new() });
        let m = meta(false, vec![9]);
        let cancel = CancellationToken::new();
        let (stop, state) =
            super::drive_reclaiming(stream, &m, 1, 100, true, &cancel, toy_decode(&table), |_| true);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(state, vec![1, 9]);
    }
}
