//! Internal engine seam — see `docs/specs/native-engine-spi.md`.
//!
//! `LocalEngine` is the smallest surface an **in-process** inference engine
//! exposes; the engine-agnostic decode/serving loop ([`drive`]) lives above it so
//! a new engine (e.g. the future Vulkan/x86 leaf) is "implement this trait + its
//! kernels", not "re-implement the whole leaf". The seam is at the **token/text**
//! level — the engine owns its whole forward + sampling graph, so there is no
//! per-op cross-runtime sync (the `mistralrs-mlx-direct` dead-end).
//!
//! Status: **A1 of `native-engine-spi`** — the seam is *defined* here. A2 extracts
//! [`drive`]'s body from the MLX leaf's `stream_generation` (behavior-preserving,
//! gated by the existing tests); A3 adopts it in the GGUF leaf.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::backend::{
    ChatEvent, ChatRequest, ModelError, ModelResult, SamplingParams, StopReason,
};

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
pub trait LocalEngine: Send {
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
    ) -> Box<dyn Iterator<Item = Result<u32, String>> + Send + 'a>;

    /// Opt-in: append-only prefix-KV reuse across turns (default: unsupported).
    fn supports_prefix_reuse(&self) -> bool {
        false
    }
}

/// The shared, engine-agnostic decode-control loop. Renders the prompt (chat
/// template + tokenizer via [`LocalEngine::meta`]), drives [`LocalEngine::generate`],
/// and turns the token stream into [`ChatEvent`]s — streaming non-tool /
/// `final`-channel text, detecting & emitting tool calls
/// ([`crate::serving::parse_tool_calls`] or the harmony parser per
/// [`EngineMeta::harmony`]), honoring EOS/cancel/max-tokens, finalizing with
/// `Done`. This is today's per-leaf `stream_generation` (MLX) / token loop (GGUF)
/// generalized to one place.
///
/// **A2 of `native-engine-spi`:** extract the body from `stream_generation`,
/// behavior-preserving, gated by the existing MLX + GGUF tests. Defined here as
/// the seam target so A1 type-checks the boundary.
pub fn drive<E, F>(_engine: &mut E, _req: &ChatRequest, _emit: F)
where
    E: LocalEngine,
    F: FnMut(ChatEvent),
{
    // A3 of `native-engine-spi`: render the prompt (chat template + tokenizer over
    // `meta`), run `engine.generate`, and feed its token ids through
    // [`consume_tokens`]. The consumption half is already extracted below;
    // rendering is the remaining lift.
    unimplemented!("native-engine-spi A3: render + engine.generate + consume_tokens")
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
                if ft.len() > emitted.len() && ft.starts_with(&emitted) {
                    let delta = ft[emitted.len()..].to_string();
                    emitted = ft;
                    if !emit(Ok(ChatEvent::TextDelta { text: delta })) {
                        client_gone = true;
                        break;
                    }
                }
            }
        } else if let Some(text) = decode(&out_ids, true) {
            let stable = text.trim_end_matches('\u{FFFD}');
            full_text = stable.to_string();
            if !tool_seen {
                if let Some(pos) = stable.find(TOOL_OPEN) {
                    if pos > emitted.len() && stable.starts_with(&emitted) {
                        let delta = stable[emitted.len()..pos].to_string();
                        emitted = stable[..pos].to_string();
                        let _ = emit(Ok(ChatEvent::TextDelta { text: delta }));
                    }
                    tool_seen = true;
                } else if stable.len() > emitted.len() && stable.starts_with(&emitted) {
                    let delta = stable[emitted.len()..].to_string();
                    emitted = stable.to_string();
                    if !emit(Ok(ChatEvent::TextDelta { text: delta })) {
                        client_gone = true;
                        break;
                    }
                }
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

    // Finalize: a cancelled run reports as-is; otherwise parse any tool calls.
    let tool_calls = if matches!(stop_reason, StopReason::Cancelled) {
        Vec::new()
    } else if meta.harmony {
        crate::harmony::parse_harmony(&full_text).tool_calls
    } else {
        crate::serving::parse_tool_calls(&full_text)
    };
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
    use super::{consume_tokens, EngineMeta};
    use crate::backend::{ChatEvent, StopReason};
    use tokio_util::sync::CancellationToken;

    fn meta(harmony: bool, eos: Vec<u32>) -> EngineMeta {
        EngineMeta {
            n_ctx: 4096,
            eos,
            model_type: "test".into(),
            harmony,
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
        let mut events = Vec::new();
        let cancel = CancellationToken::new();
        let stop = consume_tokens(
            ids.iter().map(|&i| Ok(i)),
            m,
            3,
            max_tokens,
            true,
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
    fn eos_and_max_tokens_stop() {
        let table = std::collections::HashMap::from([(1, ("a", false)), (9, ("", false))]);
        let m = meta(false, vec![9]);
        let (_e, stop) = run(&[1, 1, 1], &m, 2, &table);
        assert_eq!(stop, StopReason::MaxTokens);
        let (_e2, stop2) = run(&[1, 9, 1], &m, 100, &table);
        assert_eq!(stop2, StopReason::EndTurn);
    }
}
