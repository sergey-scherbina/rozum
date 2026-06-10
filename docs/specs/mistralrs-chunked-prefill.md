# Spec: chunked prefill for mistralrs (bound prefill activation memory)

Status: **proposed** (prototype to live on a branch). Owner: rozum / mistralrs vendor.

## Problem

On a 38 GB Apple-Silicon Mac, the in-process mistralrs MLX backend cannot serve
Claude Code against Qwen3.6-27B / 35B-A3B because a **single large prompt
prefill thrashes**: a ~14k-token prefill of the dense 27B drives free RAM to ~6%
and grows swap, and the request never completes (8+ min, 0 tokens). Claude Code
in a context-heavy repo tokenizes to ~24k, so it always hits this.

This is the last wall after the fixes already landed (serialization, PagedAttention,
device-map `max_seq_len`, auto context, and the KV-layer waste fix). Those moved
the limit up but did not remove it.

## Root cause (measured, not theory)

The prefill forward pass processes the **entire prompt in one shot** through all
64 layers. The peak working set scales with prompt length:

- mistralrs's scheduler runs "ALL prompt or ALL completion" per step
  (`paged_attention/scheduler.rs`); there is **no text chunked prefill**
  (chunked-prefill machinery exists but is gated to multimodal / media spans).
- candle's Metal allocator is **not leaking** — it frees buffers between ops
  (`drop_unused_buffers` on each `new_buffer`). The ~6.5 GB for a 14k prefill is
  the genuine peak working set of one forward over all tokens.
- Attention is already chunked (`ATTENTION_CHUNK_SIZE = 1024` via `naive_sdpa`),
  so the seq^2 score matrix is not the dominant term — the per-layer activations
  over the full token count are.

Empirically the peak is ~465 KB/token ≈ one hidden-state-sized tensor per token
per layer held concurrently during the forward.

This is exactly why llama.cpp (Ollama / LM Studio) does **not** thrash: it splits
the prefill into sub-batches of `n_ubatch` (default 512) tokens, so its activation
peak is bounded by the chunk size regardless of prompt length.

## Goal

Process a prompt prefill in **token chunks** (e.g. 2048) so the activation peak is
bounded to the chunk size, not the prompt length. Concretely: a 24k-token prompt
should prefill on the 27B/35B without exceeding the Metal working-set budget,
matching llama.cpp behaviour, while producing byte-identical output to the
whole-prompt prefill.

## Implementation level (decided after exploring the code)

**Decision: do it at the scheduler level, NOT inside the model forward.**

`Qwen3_5MoeTextModel::forward_embeds` builds `cos_sin`, the causal mask, and the
PagedAttention metadata (`ctx`: block tables, slot mapping, recurrent metadata)
for the *whole* sequence at once. Chunking inside the forward (the original plan
below) would require rebuilding all of that paged metadata per chunk by hand —
fragile. So instead: chunk at the **scheduler/engine** level so each chunk is a
normal forward step (like decode but multi-token), the pipeline builds a correct
`ctx` per chunk, and the paged KV accumulates across steps. **The model forward
stays unchanged.**

mistralrs already has the partial-prefill machinery: `Sequence::prefill_prompt_toks`
(`Option<Vec<u32>>`), `set_prefill_toks` / `reset_prefill_toks` / `has_prefill_toks`,
and `is_chunked_prefill_view()`. **Caveat:** today it is gated to multimodal —
`is_chunked_prefill_view() = prefill_prompt_toks.is_some() && !mm_features().is_empty()`,
and the scheduler/sequence chunk around media spans with per-span attention
policies (`sequence.rs:930,1431,1465,1504`, `paged_attention/scheduler.rs`). The
work is to add a **uniform text** chunking path alongside the media one:

1. Trigger: when a prompt seq has no media and `prompt_len > CHUNK`, drive it
   through `prefill_prompt_toks` in `CHUNK`-sized steps.
2. Relax the `!mm_features().is_empty()` gates to also accept the text path
   (without disturbing the media attention-policy logic).
3. Engine: don't sample / emit a token until the final prefill chunk; advance
   the sequence's token offset by `CHUNK` each step so positions + paged slots
   are correct.

This keeps `forward_embeds` untouched and reuses the per-step `ctx` building.

### (Original model-forward sketch — superseded by the above)

The two ingredients for chunk N to be correct are already present:

1. **Full-attention layers** read accumulated KV from the PagedAttention pool, so
   chunk N can attend to chunks 0..N-1 once their KV is written. The paged cache
   already accumulates across forward calls.
2. **Linear-attention layers** (GatedDeltaNet) carry a fixed recurrent state, and
   already support incremental processing (the recurrent cache). Feeding chunks
   sequentially and carrying the state is the native mode.

So the loop can live inside `Qwen3_5MoeTextModel::forward_embeds` (and the dense
`qwen3_5` twin): split the prompt tokens into chunks, run each chunk through all
layers with the correct position offset and causal mask, let full-attention write
to the paged KV and linear layers update their state, and only sample from the
last chunk's final position.

## Design

```
fn forward_embeds(prompt):
    if prefill and seq_len > CHUNK:
        for chunk in prompt.chunks(CHUNK):        # CHUNK ~ 2048, env-tunable
            xs = embed(chunk)
            for (i, layer) in layers:
                xs = layer.forward(xs,
                                   position_offset = chunk_start,
                                   attn_mask = causal over [0 .. chunk_end],
                                   paged_kv = pool,            # full-attn: append
                                   recurrent_state = state[i]) # linear: carry
        logits = lm_head(last hidden of final chunk)   # only need last position
    else: <existing single-pass path>
```

Key correctness points:

- **Position ids / RoPE**: each chunk's positions are `chunk_start .. chunk_end`,
  not `0 .. chunk_len`. MRoPE cos/sin must be sliced per chunk.
- **Causal mask**: chunk N's queries attend to keys `0 .. chunk_end` (its own
  chunk causally + all previous chunks fully). With PagedAttention the previous
  chunks are in the pool; the in-chunk mask is the standard causal block.
- **Paged KV write**: full-attention layers must write chunk N's K/V into the
  pool at the right block offset; `ctx.paged_layer(i)` slot tracking must advance
  by `chunk_len`, not jump. Reuse the existing `is_first_prompt_chunk` /
  `prefill_prompt_toks` plumbing that the multimodal path already uses.
- **GatedDeltaNet state**: pass and update `GdnLayerCache` across chunks exactly
  as the decode path does, but with multi-token chunks.
- **Sampling**: only the final position of the final chunk feeds `lm_head`; the
  intermediate chunks compute no logits (saves the vocab projection).

## Tunable

`CHUNK` (prefill chunk size) via env `ROZUM_MISTRALRS_PREFILL_CHUNK` (default e.g.
2048). Smaller = lower peak, slightly slower (more kernel launches); larger =
faster, higher peak. This is mistralrs's analogue of llama.cpp `n_ubatch`.

## Test plan (cheap before expensive)

1. **Correctness gate (small prompt):** with chunking forced on at CHUNK=8, the
   "Hello"/short-coding prompt must produce **byte-identical** tokens to the
   single-pass path. This catches position/mask/KV-offset bugs immediately.
2. **Parity vs whole-prefill:** a ~3k-token prompt, greedy, top-1 logit and first
   20 tokens identical chunked vs unchunked.
3. **Memory gate:** a ~14k-token prefill on the 27B stays well under the Metal
   budget (free RAM does not collapse, no swap growth) and **completes** (was: 8
   min / 0 tokens). Watch `memory_pressure` + `vm.swapusage` + gateway `/stats`.
4. **End-to-end:** a real ~24k Claude Code prompt completes with coherent output
   and a `tool_use` block, no thrash.

## Risks

- Getting position/mask/KV-offset wrong yields **garbage output or a panic** — the
  small-prompt byte-identical gate (test 1) is the guardrail; bail and revert if
  it fails.
- The hybrid forward is intricate (attention + GatedDeltaNet + MoE interleave);
  chunking must thread state through all three correctly.
- Estimated 1-2 days with the ~2 min build + load cycle per iteration.

## Relation to upstream

If it works, this is a strong upstream contribution (mistralrs lacks text chunked
prefill entirely). Land it on a branch, prove the gates, then propose upstream
separately from the already-merged-ready correctness fixes
(`docs/specs/mistralrs-qwen36-pr.md`).
