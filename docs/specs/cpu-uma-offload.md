# CPU/UMA offload — run models bigger than the Metal cap (weight streaming)

Status: SPEC (2026-06-22, `sunny-civet`). Design only — the impl is a large MLX-forward
change (engine owner's area) + slot-gated. North-Star item (`SPEC.md` § North Star,
`[[project-rozum-north-star]]`): "run intelligence on ANY hardware with what's available".

## One-line

Let a model whose resident footprint exceeds the Metal working-set cap still run, by
keeping its weights **page-cache-backed (pageable) and streaming each layer into a small
Metal residency just-in-time** — trading decode speed for the ability to run it at all
(or to free Metal headroom for co-residency / a bigger context).

## The Apple-UMA correction — this is NOT x86 CPU-offload

Idea #7 was framed as "spill layers/KV to CPU RAM" (the x86 dGPU model, where VRAM is a
separate, smaller pool). **On Apple Silicon that framing is wrong:** CPU and GPU share **one
physical unified-memory pool**. Moving a tensor "to CPU RAM" does **not** free GPU memory —
it is the same bytes. So there is no VRAM to spill out of.

What actually constrains us on a 36 GiB Mac is two things, neither of which is "GPU vs CPU":
1. **The Metal working-set / wired cap** (`recommendedMaxWorkingSetSize`, ~28 GB) — how much
   Metal will keep *resident* for the GPU.
2. **Total RAM vs the OS jetsam ceiling** (BUG-003) — the whole-system limit.

So the real lever on UMA is **residency, not location**: keep weights **mmap'd and pageable**
(in the OS page cache, reclaimable, NOT Metal-wired) and make only the **active layer's**
weights Metal-resident at a time. Peak Metal footprint becomes `~1 layer of weights + KV +
activations` instead of `all weights`. This is what lets a model larger than the cap run.

This also ties straight into the reboot work: **wired memory is non-pageable** — it cannot
be compressed or evicted, so wired weights are exactly what feeds `vm-compressor-space-
shortage` → jetsam → the BUG-003 panic. `mlx_rs::memory::set_wired_limit` is the knob that
governs this. **P0 must first establish whether MLX currently wires the weights or leaves
them pageable** — that single fact decides how much headroom streaming can even recover.

## Why it matters

- **Run bigger-than-fits models** (e.g. a 70B-4bit ≈ 40 GB on a 36 GiB box) at a speed cost
  — the North-Star "use what's available now" goal.
- **Free Metal headroom** for safe co-residency (smmr) or a longer context, by not keeping
  every layer wired.
- It is the Apple-side analog of the x86 `rozum-x86` iGPU work — same goal (device-aware
  placement), different mechanism (residency streaming, not PCIe offload).

## Mechanism

- **Weights:** the safetensors are already mmap'd; the question is residency. Stream
  per-layer: before computing layer L, ensure L's weights are in a Metal buffer; after,
  release (or keep a small LRU of recent layers). The mmap source stays in page cache
  (reclaimable, not against the Metal cap). `set_wired_limit` low ⇒ MLX returns freed
  buffers to the OS instead of wiring them.
- **KV (optional, harder):** keep KV pageable and bring only the attended blocks resident —
  but KV is read every token, so this is bandwidth-heavy; defer past weights.
- **Granularity knob:** stream per-layer (simplest) → per-N-layers LRU (less re-read,
  more resident). `ROZUM_STREAM_LAYERS` picks the resident window.

## Cost — honest

Decode becomes **memory-bandwidth-bound**: each token re-reads the streamed weights (like a
mini-prefill). Rough ceiling ≈ `effective_bandwidth / streamed_bytes` tok/s — e.g. a 40 GB
model at ~100 GB/s ≈ **~2.5 tok/s**. Slow, but it *runs* a model that otherwise can't load.
The win is **feasibility**, not speed; keep it opt-in (`ROZUM_STREAM=1`) and auto-engage only
when admission would otherwise refuse the model. For models that fit, never stream (full speed).

## Phased plan

- **P0 — residency probe (de-risk first; mostly slot-free, extends `mlx_mem_probe`).** With
  a model loaded, read `get_active_memory` vs the wired set and test `set_wired_limit(small)`:
  does peak Metal footprint drop (weights pageable) or not (weights wired)? Decides whether
  streaming can recover headroom at all, and by how much. **Gate:** a measured headroom number.
- **P1 — per-layer weight streaming** in the MLX forward (vendored mlx-rs or the rozum forward):
  load→compute→release each layer; `ROZUM_STREAM_LAYERS` LRU window. **Gate:** a model larger
  than the cap loads + greedy-parity vs a reference on a fixed prompt; peak Metal ≤ cap.
- **P2 — admission integration.** When the residency gate (`acquire_residency`) would refuse a
  model, offer the streamed variant instead of failing; footprint estimate reflects the
  streamed (small) Metal residency, not the full weights. Ties to smmr.
- **P3 — KV streaming** (only if a real need): pageable KV + attended-block residency.

## Risks & open questions

- **The UMA headroom may be small.** If MLX already leaves weights pageable (P0 says so), the
  Metal cap is already near the active set and streaming recovers little — then #7 is mostly a
  non-starter on Apple, and the real "bigger models" path is GGUF/llama.cpp (which already
  mmaps + streams) or the x86 leaf. **P0 decides whether this is worth building at all** —
  same verify-before-build discipline that closed `set_memory_limit` / #5 / #2.
- **Speed** (bandwidth-bound) — opt-in, feasibility-only.
- **Greedy parity** under streaming (numerics must be identical; only residency changes).

## Non-goals

- Not a speed feature; it trades speed for feasibility.
- Not the x86 path (`docs/specs/x86-native-runtime.md`) — that's a separate, true dGPU offload.
- No quality change: streaming changes *where bytes live*, not the math.
