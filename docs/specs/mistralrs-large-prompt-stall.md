# mistralrs Large-Prompt Stall (cancellation not honored during prefill)

## Overview

Through the rozum gateway, the in-process mistralrs backend serving Qwen3.6-35B-A3B-4bit
(MLX/AFQ, Metal) **stalls and returns empty responses for any prompt beyond ~2k tokens** —
exactly the prompts Claude Code sends. Small prompts work and stream at ~43 tok/s; large
prompts produce zero output tokens, `ttft=None`, and `stop_reason` of `end_turn`/`incomplete`.

This was initially mistaken for (a) a Metal buffer bug, (b) memory/swap pressure, and
(c) a tool-use translation bug. All three were ruled out by experiment. The actual cause is
a **request-cancellation gap** that, combined with `max_num_seqs=1` serialization, turns one
slow/abandoned request into a queue-blocking zombie that starves every subsequent request.

## Symptom (observed)

`~/.rozum/gateway.jsonl` for real Claude Code traffic:

| request | est_prompt_tokens | tools | result |
|---|---|---|---|
| id=1 (startup) | 335 | 0 | ok, streamed |
| id=2 (real turn) | 1802-2633 | 33-35 | `ttft=None output_tokens=0 stop_reason=end_turn`, 13-175 s |
| later | 21124 | 33 | rejected: "Sequence too long" until n_ctx raised, then stalls |

The duration varied wildly (13 s / 47 s / 175 s) across launches at the same prompt size,
which is the tell that the wait is queue contention, not compute.

## What it is NOT (ruled out by experiment)

1. **Not the Metal zero-buffer bug.** That was a separate, real bug (hybrid no-KV layers ->
   `new_private_buffer(0)`), fixed in `mistralrs-core/src/paged_attention/cache_engine.rs`
   (`elem_count.max(1)` at the buffer-alloc sites). The MoE loads fine after that fix.
2. **Not memory/swap.** Reproduced with `vm.swapusage` steady at ~1 GB and `memory_pressure`
   reporting >90% free. CPU sat at ~30% during the stall (not compute-bound).
3. **Not the tool-use translation.** Tool calls work in isolation. Reproduced WORKING:
   - 1 tool, 3 tools, 33 tools, and a system prompt: model emits `<think>` reasoning then a
     proper `tool_use` call, `stop_reason=tool_use`.
   - two concurrent requests: both completed with `tool_use`.
   The stray `</tool_call>` text delta seen once on a large prompt was incidental output, not
   the cause.

## Decisive A/B (single standalone gateway, n_ctx 24576, no swap)

| request | prompt size | result |
|---|---|---|
| 33 tools, short user msg | ~1.5k tok | ok, `read_file` tool_use, 13 s |
| 33 tools, ~15k-token padded msg | ~15k tok | 1 delta (`</tool_call>`) then stall, curl timeout 180 s |
| **no tools**, ~15k-token msg | ~15k tok | **0 output, stall 180 s** |
| no tools, ~2.6k-token msg | ~2.6k tok | **0 output, stall 150 s** |

Conclusion: the trigger is **prompt size (~2k+ tokens)**, independent of tools and of swap.

## Root cause (proven mechanism)

mistralrs only notices a dead receiver **at a streaming send**, which first happens *after*
prefill. `mistralrs-core/src/pipeline/sampling.rs` already cancels a sequence when
`maybe_send_streaming_response(...)` fails (`set_state(Done(Canceled))`). So an abandoned
request that has *started generating* is reaped after one token. The gap is entirely in the
**prefill window**:

1. **Disconnect is not checked during prefill.** While a sequence is prefilling it produces no
   streaming sends, so the dead-receiver check never runs. An abandoned long prefill therefore
   runs to completion before mistralrs reaps it.

2. **`max_num_seqs=1` makes that prefill block the queue.** rozum serializes requests (one
   sequence at a time, like Ollama). While the abandoned prefill runs, every following request
   waits and appears to "hang with 0 output". A single large abandoned prompt poisons the
   session until its prefill finishes.

3. **rozum's loop also parks on the chunk.** `src/mistralrs_backend.rs` awaited
   `upstream.next()` and only checked `cancel.is_cancelled()` between chunks, so it could not
   observe a `CancelOnDrop` cancel mid-prefill either.

Single clean requests never hit this: a lone 3k-token prompt prefills and answers in ~11 s.
The failure needs an *abandoned* large prefill plus a following request, which is exactly
Claude Code's burst pattern (it fires a small startup request and the real request together).

### Why the size threshold
Small prompts prefill fast enough that the first token reaches the client before any timeout,
so nothing is abandoned. Beyond ~2k tokens, prefill (plus the slower long-context decode,
~7 tok/s vs ~43 tok/s on short context) pushes first-token latency past the client's patience
-> disconnect during prefill -> the abandoned prefill blocks the next request.

## Evidence pointers
- `WARN ...sampling: Receiver disconnected` spam in the gateway stderr/log during a stall.
- obs events with `ttft_ms=None`, `output_tokens=0`, `stop_reason` `end_turn`/`incomplete`.
- `ps -o %cpu` ~30% (not pegged) during the stall.
- A/B table above (size is the only varying factor that flips behavior).

## Fix (implemented)

### Engine — reap disconnected sequences before the forward pass (the real fix)
`mistralrs-core/src/engine/mod.rs`, `PagedAttention` scheduler arm (the path this model uses):
right after `schedule()`, drop any scheduled sequence whose responder is already closed,
before stepping it:

```rust
output.scheduled.retain(|seq| {
    let seq = seq.lock().unwrap();
    if seq.responder().is_closed() {
        seq.set_state(SequenceState::Done(StopReason::Canceled));
        false
    } else { true }
});
```

`Done(Canceled)` reuses the engine's existing completed-sequence reaping (KV blocks are freed
the normal way). This stops an abandoned prefill from ever consuming a forward pass, so it
never blocks the single sequence slot. (The `DefaultScheduler` arm uses a `Box<[&mut Sequence]>`
and is not used by this model; left as a follow-up.)

### rozum — honor cancellation mid-prefill (`src/mistralrs_backend.rs`)
Race the next chunk against the cancel token so a client disconnect breaks the wait
immediately and drops `upstream` (tearing down the mistralrs request) instead of parking on
`upstream.next()`:

```rust
let item = tokio::select! {
    biased;
    _ = cancel.cancelled() => { /* emit Done{Cancelled}; break */ }
    item = upstream.next() => item,
};
let Some(item) = item else { break };
```

### Verified
Repro: fire a 5k-token prompt, disconnect at 3 s (mid-prefill), then immediately fire a small
prompt.
- Before: small request stalls behind the abandoned prefill; continuous
  `Receiver disconnected` spam.
- After: small request completes (~16 s, dominated by the model's own thinking), **zero**
  `Receiver disconnected` lines.

### Remaining blocker — prefill throughput (separate, not fixed here)
With the stall fixed, requests run cleanly and the model is correct (reasons then emits proper
`tool_use`). What still makes Claude Code feel broken is **prefill speed**. Time-to-first-token
scales linearly at ~2.7 ms/token (~370 tok/s prefill) on M4 Max at n_ctx 24576:

| prompt | TTFT |
|---|---|
| 666 tok | 1.8 s |
| 2666 tok | 7.2 s |
| 6666 tok | 18.2 s |
| ~21k tok (real Claude Code turn) | ~57 s (extrapolated; matches observed 67 s) |

A real Claude Code prompt spends ~a minute in prefill before any token; the client's internal
timeout then abandons it, read as an empty `end_turn`. This is prefill *throughput*, distinct
from the cancellation work and from the memory-bounding `mistralrs-chunked-prefill`. Practical
levers until prefill is optimized: keep prompts small (the `CLAUDE_CODE_DISABLE_*` trims in
`rozum launch` help -- TTFT 7.4 s vs 16.4 s with/without a 3k system prompt), disable unused
MCP servers, or use a smaller/faster model.

## Acceptance
- An abandoned large-prompt prefill no longer blocks following requests; no lingering
  `Receiver disconnected` spam after the client is gone. [met]
- A single large prompt to a fresh gateway returns a non-empty response. [met: 3k -> 11 s]
