# Speculative Decoding (draft + target)

## Overview

A small **draft** model proposes `k` tokens; the big **target** verifies them in
one forward, accepts the longest prefix that equals the target's own greedy
choice, and appends the target's next token at the first divergence. Net: fewer
expensive target forwards per emitted token → faster decode with **byte-identical
greedy output (in exact arithmetic)**. This is a **latency win, not a quality
tradeoff** — the orchestrator emits *only* the target's own greedy tokens, so each
output is a valid greedy decode of the target.

**Floating-point caveat (measured on Metal):** the verify forward scores `k+1`
positions in one batched pass, so the target's KV cache is built in a different
*shape* than a one-token-per-step sequential decode. On finite-precision hardware
that tiny difference can flip an argmax at a near-tie — the same batched-vs-sequential
class as chunked prefill — so the output is identical to a sequential greedy decode
*except at rare ties* (e.g. 17/20 tokens shared before the first tie-flip on a
Qwen3-4B self-speculation run; both remain valid greedy decodes). The exact-arithmetic
byte-identity is proven by the engine-agnostic mock unit test; the **functional**
equivalence on real workloads is the agentic-matrix gate (pass/fail unchanged).

Architecturally, draft and target are two **residents** (the residency / warm
layer), so on heterogeneous hardware the draft runs on a cheap device (iGPU/CPU)
while the target verifies on the fast one (dGPU / Metal) — the canonical
single-stream co-use of two devices (`multi-device-residency.md`, rozum's North
Star in `SPEC.md`). The orchestration + acceptance logic is **engine-agnostic and
sits above the `ChatBackend` SPI**; an engine opts in by implementing a small
token-level *verify* capability. The byte-identical-greedy contract is
engine-independent and is the acceptance test — so the orchestrator core is
unit-tested without any real model.

This supersedes the framing of the SPRINT `spec-decode-draft` note (which scoped
it to the MLX decode loop): the MLX work becomes *one implementation of the
capability*, not the whole feature.

## Interface

A `SpeculativeVerify` capability (opt-in, beside `ChatBackend`) — the minimal
token-level ops the orchestrator needs:

```
target.prefill(tokens)            -> (KvState, greedy_argmax_of_next)
target.verify(kv, &draft[..k])    -> { accepted: usize, corrected: TokenId, kv: KvState }
                                     // score k drafts in ONE forward; accept the
                                     // longest prefix == target greedy argmax;
                                     // `corrected` = target's argmax at the first
                                     // divergence; `kv` advanced to accepted+1,
                                     // the rest rolled back.
draft.propose(ctx, k)             -> [TokenId; k]   // greedy
```

- Both models **must share a tokenizer** (enforced — refuse on mismatch); same
  family (e.g. Qwen3 draft + Qwen3 target).
- CLI / config: `--draft-model <spec>` (or a `[[resident]] role = "draft"`);
  lookahead `k` configurable (`ROZUM_SPECDECODE_K`, default e.g. 4).
- **Off by default**: absent a draft, decode is unchanged (strict no-op).

## Behavior

- [x] Output is **byte-identical** to pure greedy decode of the target alone **in
      exact arithmetic** — the invariant. Proven with a mock target whose greedy
      sequence is fixed and a mock draft of arbitrary quality (`src/specdecode.rs`
      tests). On Metal it holds modulo rare float argmax ties (see the FP caveat
      above); the MLX dense verify/propose are proven to track the target greedy on
      a real model by `mlx_spec_decode_byte_identical` (Qwen3-4B self-speculation:
      lcp 17/20, 5 target forwards vs 20 — the speedup).
- [ ] Per step: draft proposes `k`; target verifies in one forward; accept the
      longest prefix equal to the target's greedy argmax; emit the accepted tokens
      + the target's corrected token; advance KV to that point.
- [ ] Degenerate cases hold: an all-wrong draft → 1 token/step (== plain greedy,
      no speedup, still correct); an all-correct draft → `k+1` tokens/step.
- [ ] Draft and target are separate residents; placement may put them on
      different devices (draft cheap, target fast) or the same device.
- [ ] Tokenizer mismatch → error (never silently corrupt output).
- [ ] Dense targets first. Hybrid (Qwen3.6 GatedDeltaNet) is deferred — its
      recurrent state is not freely truncatable, so KV rollback on rejected drafts
      is the hard part (`HybridPrefix`); a hybrid target falls back to plain
      greedy until rollback is solved.
- [ ] A measured tok/s speedup on a real dense target + small draft, with
      unchanged output.

## Out of scope

- Sampled / tree speculative decoding (Medusa, sampling-based acceptance) —
  greedy first.
- Hybrid-recurrent target rollback (deferred; falls back to greedy).
- Cross-tokenizer draft (would need retokenization between models).
- Multi-draft / staged drafting.

## Design

- **Engine-agnostic orchestrator** (`src/specdecode.rs`): the
  accept-longest-prefix loop, pure and unit-tested against a mock
  `SpeculativeVerify` (a deterministic target argmax + a mock draft), proving the
  byte-identical invariant for any draft quality. This is the "грамотно
  архитектурно" core — no real model, no engine, just the algorithm + contract.
- **Capability trait** beside `ChatBackend`. An engine implements it if it can
  expose prefill/verify over its KV. MLX implements it for **dense** targets
  (extend the decode loop in `src/mlx_native_backend.rs` with batch-verify +
  KV-truncate-to-accepted); GGUF/CUDA later (same trait → generalizes per the
  North Star).
- **Residents**: draft + target come from the residency / multislot layer; device
  placement from `multi-device-residency` (`role = "draft"` → cheapest device,
  target → fastest). The orchestrator is device-agnostic — it only sees two
  capability handles.
- **Acceptance**: compare draft tokens to the target's greedy argmax
  position-by-position; the first mismatch ends the accepted prefix, and the
  target's argmax there is the corrected token. With greedy target argmax this is
  provably equal to plain greedy decode.

## Decisions

- **Engine-agnostic orchestrator + opt-in verify capability, above the SPI** —
  chosen for "architecturally proper": generalizes across engines and devices
  (North Star), and lets the byte-identical invariant be unit-tested with no real
  model. Rejected: bolting draft+verify directly into the `mlx_native_backend`
  decode loop as a one-off (MLX-only, can't cheaply test the invariant, doesn't
  reach the heterogeneous-device co-use).
- **Byte-identical greedy is the contract and the test** — chosen because the
  whole point is latency, not quality; the property is engine-independent, so the
  core is verifiable up front.
- **Draft + target as two residents, device-aware placement** — chosen to make
  spec-decode the canonical heterogeneous single-stream co-use; reuses residency
  / `multi-device-residency`.
- **Dense targets first; hybrid deferred** — chosen because GatedDeltaNet
  recurrent-state rollback on rejected drafts is unsolved; a hybrid target
  degrades to plain greedy (still correct) rather than blocking the feature.

## Verification gate (the e2e agentic matrix)

The objective acceptance gate is the existing **agentic matrix**
(`scripts/bench/agentic.sh`): it launches `rozum gateway --model <spec>` and runs
real `claude`/`codex`/`opencode` over the task set, recording per-task pass/fail
and a per-run CSV (tok/s, footprint). Spec-decode plugs in at the gateway:
`rozum gateway --model <target> --draft-model <draft>`. Acceptance:

1. **Correctness (no regression):** the matrix **pass/fail matrix with
   `--draft-model` must be identical** to the baseline run without it. Output is
   byte-identical greedy by construction (the P0 orchestrator only emits the
   target's greedy tokens; the unit test proves it), so the matrix is the e2e
   proof on real agentic workloads, not just the mock.
2. **Speedup:** the per-run CSV decode tok/s (or wall-time) improves with
   `--draft-model` on, on a dense target (e.g. Qwen3-30B-A3B + a Qwen3-4B draft).

This runs on the target Apple-Silicon box (the M4) with the real models — it is
not a headless check. The build loop: implement an iteration → run the matrix
off-vs-on → confirm identical pass-matrix + tok/s gain → iterate.

Tractability note: the MLX `verify` capability builds on primitives the dense
decode loop already has (`src/mlx_native_backend.rs`): KV truncate-to-prefix
(`PREFIX_REUSE`), suffix prefill, and `argmax_u32` — so verify = "prefill the k
draft tokens, take argmax at each position, accept the longest greedy-matching
prefix, truncate KV to accepted+1" rather than new cache machinery.

## Results

**Correctness gate — PASSED.** Agentic matrix (`scripts/bench/agentic.sh`,
claude × Qwen3-30B-A3B-4bit target + Qwen3-4B-4bit draft, greet/build/fix) on the
M4: the pass/fail matrix is **identical** off-vs-on (3/3 both; same turns 1/4/6,
same tool calls 0/3/3). Footprint +2.2 GB (the draft resident). The spec-decode
path engaged (`backend: mlx-native spec-decode … (MLX dense)`). So spec-decode
introduces **no behavioral regression** on real agentic workloads — the
byte-identical-in-exact-arithmetic contract holds functionally.

**Speedup gate — FAILED on this (MoE) target; the economics are the finding.**
Isolated long-generation decode (250-word essay, greedy, 400 max tokens):

| config | tok/s | note |
|---|---|---|
| Qwen3-30B-A3B target alone | **98.1** | MoE, 3B active → already fast |
| Qwen3-4B draft alone | 124.3 | only **1.27×** the target's speed |
| spec-decode (target + draft) | **34.8** | **2.8× slower**, though target forwards fell 2.65× (111 vs 294) |

Two structural reasons, both inherent (not bugs):

1. **MoE target.** A 3B-active MoE decodes a single token cheaply, but a `k+1`-token
   *verify* forward routes those tokens to *more distinct experts*, so it must load
   more expert weights — the dominant (memory-bandwidth) cost. The per-forward cost
   therefore **scales with token count**, which cancels the forward-count reduction.
   Spec-decode's premise is a target whose forward cost is ~flat in sequence length
   (true for a **dense**, bandwidth-bound model that loads its weights once per
   forward regardless of token count) — not a MoE.
2. **Draft not cheap enough.** The smallest same-family draft on hand (4B dense,
   124 t/s) is barely faster than the MoE target (98 t/s). For a win the draft must
   be *much* cheaper (≥5–10×) than the target; a 4B dense draft vs a 3B-active MoE
   target is a wash.

**Conclusion.** The spec-decode infrastructure is correct, proven, and is the
durable engine-agnostic / heterogeneous-device layer (North Star). But it is a
**latency win only for a slow, memory-bandwidth-bound _dense_ target paired with a
much smaller draft** (e.g. a 32B+ dense model at ~15–25 t/s + a ≤1B draft) — a
configuration not present among the cached/recommended M4 models (the recommended
local agentic model, Qwen3-30B-A3B, is MoE). It therefore stays **off by default**
(opt-in via `--draft-model`), correct when enabled, and ready for the dense /
heterogeneous-device case it's designed for.
