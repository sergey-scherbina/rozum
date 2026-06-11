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

`CHUNK` (prefill chunk size) via env `MISTRALRS_PREFILL_CHUNK` (default 4096, `0`
disables). Smaller = lower peak, slightly slower (more kernel launches); larger =
faster, higher peak. This is mistralrs's analogue of llama.cpp `n_ubatch`. The var
is read in-process by mistralrs, so setting it on the `rozum gateway` env is enough
(no rozum-side forwarding needed).

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

## Implementation log (v2 — rebuilt on the post-#2200 fixes)

Branch `qwen36-chunked-prefill-v2` + worktree `.vendor/mistral-rs-chunked`, off the
latest `qwen36-fixes` tip (Qwen3.6 + zero-buffer + cancellation fixes). The old
`qwen36-chunked-prefill` (old upstream base) is abandoned.

### Mechanism reverse-engineered against the current tree
The prompt forward processes the token range `[prefix_cache_len, get_toks().len())`
at **absolute** positions (`token_offset`), with already-done KV living in the paged
pool. This is exactly the prefix-cache-resume path and it is **not** multimodal-gated,
so it works for plain text. A prefill chunk `[start, end)` is therefore:

- `prefill_prompt_toks = tokens[0 .. end]` (the view caps `get_toks().len()` to `end`)
- `prefix_cache_len = start` (skips the already-prefilled prefix; its KV is in the pool)
- positions are absolute, so RoPE/mask are correct with no extra work
- full-attention layers append the chunk's KV to the pool; GatedDeltaNet carries its
  recurrent state across chunks (the engine already snapshots recurrent state at block
  boundaries, see `engine/mod.rs` hybrid snapshot block)

`is_chunked_prefill_view()` stays `false` for text (`mm_features` empty), so the
multimodal per-span attention-policy paths (sequence.rs:1431/1465/1504) are untouched.

### Exact edit points
1. `paged_attention/scheduler.rs`
   - [done] `DEFAULT_PREFILL_CHUNK = 2048` + `prefill_chunk_size()` (env `MISTRALRS_PREFILL_CHUNK`, `0` disables).
   - [todo] in `schedule()` prompt path (after `allocate_slots` succeeds, ~line 334):
     when text and `num_tokens - chunk_start > CHUNK`, set the chunk view
     (`prefill_prompt_toks = tokens[0..chunk_start+CHUNK]`, `prefix_cache_len = chunk_start`)
     and keep the seq schedulable (do not let the engine treat it as a finished prompt).
2. `sequence.rs`
   - [todo] track prefill progress (reuse `prefix_cache_len`/`token_offset`; add a
     `prefill_total`/`is_last_chunk` helper) so the engine knows when the last chunk runs.
3. `engine/mod.rs` PagedAttention arm (after `pipeline.step`)
   - [todo] if the seq is on a non-final chunk: **do not sample**, advance the chunk
     start by `CHUNK`, reset the view to the next chunk, re-queue as a running prompt.
   - on the final chunk: behave exactly as today (sample, transition to completion).

### Gate to hit first
Force `MISTRALRS_PREFILL_CHUNK=8` on a short prompt and assert **byte-identical**
output vs unset (single-pass). This catches position/mask/KV-offset bugs immediately;
bail/revert if it fails before touching the memory gate.

## Implementation log (v3 — LANDED, much simpler than v2 planned)

The v2 plan above (hand-roll the chunk loop across scheduler.rs + sequence.rs +
engine/mod.rs) was **not needed**. The paged prompt-prefill chunk loop already
exists in `pipeline/mod.rs::step` — it was added for CUDA and drives exactly the
`set_prefix_cache_len(chunk.start)` + `set_prefill_toks(tokens[..chunk.end])`
mechanism v2 reverse-engineered, via `build_prompt_chunk_plan`. It was just gated
to `self.device().is_cuda()`, so Metal always prefilled in one shot.

**The whole fix is one commit (`698bccf1f`):**
- relax the gate to `is_cuda() || is_metal()` in `pipeline/mod.rs::step` (~line 1277);
- replace the hard-coded `DEFAULT_PAGED_PREFILL_CHUNK_SIZE` with
  `paged_prefill_chunk_size()` reading env `MISTRALRS_PREFILL_CHUNK` (default 4096,
  `0` disables).

No scheduler.rs / sequence.rs / engine/mod.rs edits. The v2 "Exact edit points"
list above is **superseded** — keep it only as the reverse-engineering record.

### Verified behaviour and the chunk-size constraint (measured on Qwen3.6-27B, Metal)
The paged block size is **32**. Chunk size interacts with it:

- **Small chunks are BROKEN, not FP-noisy.** `MISTRALRS_PREFILL_CHUNK=8` on a short
  prompt makes the model **misread the prompt** (computed `100 - 37` as `10 - 37 = -27`;
  recalled "2 to 60" as "2 to 6" - drops the trailing token). On a ~2.5k-token prompt it
  **crashes** in the hybrid GatedDeltaNet conv path: `narrow invalid args start + len >
  dim_len: [992, 4, 256], len:1024` - once the cumulative prefill nears the conv's ~1024
  window, the narrow overruns the buffer the small chunks have accumulated. Block
  alignment alone is NOT enough: CHUNK=32 (block-aligned) still crashes at the 1024 mark.
  The earlier "FP-level, not structural" reading was an artifact of testing at CHUNK=8.
- **Chunks >= 512 are correct.** `MISTRALRS_PREFILL_CHUNK=512` on the same ~2.5k prompt
  recalls a needle faithfully and is deterministic; the default 4096 is the size used for
  the memory-win measurement. 512 is the smallest size verified correct.
- **Guard:** `pipeline/mod.rs::step` promotes the chunk to
  `chunk.next_multiple_of(block_size).max(MIN_PAGED_PREFILL_CHUNK_SIZE)` (512), so a
  too-small/unaligned env value can no longer corrupt or crash - it is silently raised to
  a safe, block-aligned size. Verified: `MISTRALRS_PREFILL_CHUNK=8` now passes the gate
  (clamped to 512, no narrow panic) instead of crashing. The underlying GatedDeltaNet
  small-chunk conv overrun is a real upstream bug; the floor sidesteps it (sub-512 chunks
  buy only marginal activation-peak savings for many more kernel launches).
- **Not bit-exact vs single-pass even when correct:** the GatedDeltaNet linear-attention
  layers reorder their FP reductions across chunk boundaries, so a block-aligned multi-chunk
  run is coherent + deterministic but not byte-identical to the unchunked path. A byte-diff
  vs single-pass is therefore the wrong correctness test.

Memory win (the point): a ~20k-token prefill's peak swap drops from ~5.9 GB to ~1.3 GB
for ~13% slower prefill.

### Correctness gate (rewritten)
`scripts/chunked_prefill_gate.sh` no longer diffs against single-pass. It asserts:
1. **Faithfulness** - a secret embedded in an *early* chunk is recalled at the final
   position (proves cross-chunk attention reads the accumulated KV correctly);
2. **Determinism** - two chunked runs at the same (block-aligned) chunk size are
   byte-identical to each other.
Default `CHUNK=512`. The old "byte-identical at CHUNK=8" gate is retired (invalid: 8 is
sub-block, and single-pass parity is unachievable).

### rozum wiring
`Cargo.toml` `[patch.crates-io] mistralrs` -> `.vendor/mistral-rs-chunked/mistralrs`
(branch `qwen36-chunked-prefill-v2`). Knob: `MISTRALRS_PREFILL_CHUNK` (default 4096,
multiple of block size 32, `0` disables) - read in-process, no rozum forwarding needed.
