# Matrix non-determinism — isolate + reproducibility instrument

Status: **DONE — two-layer root cause proven from code AND live; E1 instrument shipped +
unit-tested + harness-wired; Layer-A confirmed as the irreducible residual.** Task: SPRINT
`matrix-nondeterminism-flip` (green-matrix #1). Branch: `feature/matrix-nondeterminism-flip`.

**Two layers (confirmed):**
- **E1 — our bug, FIXED.** Gateway never threaded `SamplingParams.seed` → entropy RNG →
  `temp>0` non-deterministic. Seed-pin fixes it (GLM-4-9B: temp1 5/5 distinct → 1/5).
- **Layer-A — the agent's own, IRREDUCIBLE by us.** The agent CLIs inject a fresh per-run
  `session-id`+timestamp into every request (observed in codex's log: distinct `session id`
  per run), so the request is non-identical run-to-run → the trajectory varies *even at a
  fixed seed*. Demonstrated end-to-end: `codex×35B×test` FAIL(baseline)→PASS(repro) at the
  SAME seed=1234, then **4/4 PASS** on fresh re-isolation (an ~80% flake, not a bug).
- **Consequence:** a fixed seed makes the matrix *more* reproducible (kills E1 noise) but NOT
  deterministic. **The agentic matrix must be read as an N-run PASS-RATE, not a single binary
  cell.** A single-run red is not a bug until confirmed over N runs.

## Live probe result (decisive)

Ran under the BUG-003 residency guard on **GLM-4-9B-0414-4bit** (dense, ~5 GB; one model
at a time, graceful SIGINT teardown, 0 reboots). `scripts/bench/nondeterminism-probe.sh`,
N=5, high-entropy prompt ("imaginative six-line poem"):

| config | distinct / 5 | verdict |
|---|---|---|
| temp=1.0, unseeded | **5 / 5** | **NON-DETERMINISTIC — the flip (E1 confirmed)** |
| temp=0, greedy | 1 / 5 | deterministic (dense argmax baseline) |
| temp=1.0, `ROZUM_SAMPLING_SEED=42` | **1 / 5** | **deterministic — the fix works** |

The seeded output (295 B) differs from the greedy output (219 B): the seed preserves the
real temperature-1 distribution, it just replays it identically — the correct knob (not a
behaviour-distorting force-greedy). **Control:** a *canonical-code* prompt ("reverse a
string") gave 1/5 even **unseeded** — a peaked distribution collapses temp=1 to argmax. So
only **high-entropy** trajectories (reasoning, free phrasing, tool-arg values) flip; that
is why some matrix cells flip and others never do, and why a single canonical-output run
can mislead.

## Problem

The agentic matrix (`scripts/bench/agentic.sh`) has cells that flip pass↔fail on a
**byte-identical config** (observed: `claude × test` 0↔1 across two runs of the same
binary; see `docs/matrix-failure-analysis.md` Finding-on-GLM META-finding). Per the
sprint rule, every non-deterministic flip is a concrete bug **in our stack** to isolate
fully before any conclusion — non-determinism undermines every other matrix reading
(you cannot call a cell "model too weak" if the same config sometimes passes).

## Root cause, read from the code (no model run needed)

The gateway is a faithful pass-through of `temperature` / `top_p` / `top_k` but **never
sets `seed`**. All three request handlers build `SamplingParams { temperature, top_p,
top_k, max_tokens, .. }` with `seed` left at `Default` = `None`
(`crates/rozum-gateway/src/gateway.rs`: `oai_chat_handler`, `responses_handler`,
`anthropic_handler`). Consequences:

- **Sampling RNG seeds from entropy.** `sampler::seeded_rng(None)` → `StdRng::from_entropy()`,
  and the MLX path's `if let Some(s) = job.sampling.seed { mlx_rs::random::seed(s) }`
  (`mlx_native_backend.rs:1097/2474/2497`) is **never taken** from the gateway path →
  MLX's RNG is also entropy/time-seeded. So any request with `temperature > 0` produces a
  **different token stream every run**.
- **The agent CLIs sample.** Claude Code's main loop sends `temperature: 1.0`; gpt-oss is a
  reasoning model that must sample (~1.0, greedy loops its CoT). So most cells run hot.
- **Tool turns sample too.** Agents send tools → the constrained masked-decode path
  (`run_constrained_dense`/`hybrid`) still calls `sampler_opts_of(job)`: the structural
  tokens (tool name + JSON skeleton) are forced, but free-form **argument values** (file
  paths, code) sample with the job temperature → non-deterministic arg content.

So whenever the agent sends `temperature > 0`, the matrix is **inherently non-deterministic
at the sampling layer**, and that is 100% our stack: we expose no way to make a run
reproducible (no `seed` threaded, no force-greedy / force-seed knob).

## A layered model of matrix non-determinism

Separate the sources so the probe can attribute a flip to exactly one:

- **E1 — sampling RNG** (this finding). `temp > 0` + unseeded RNG → different tokens. Fixable
  in our stack by pinning the seed. **Dominant hypothesis.**
- **E2 — engine greedy numerics.** Even at `temperature == 0` (argmax), quantized **MoE**
  `gather_qmm` is **not bit-invariant to sequence length** (`L=k+1` verify ≠ `L=1` decode;
  proven in `project-spec-decode-moe-numerics`), so argmax can flip at near-ties on
  Qwen3.6-35B-A3B / gpt-oss MoE. Dense greedy is byte-identical. Bites only at `temp == 0`.
- **A — agent-layer request variance.** The CLI may inject per-run data (session id, a
  timestamp, tool ordering) into the prompt → a *different request* → different logits →
  different output even under a pinned seed. Not an engine bug; measured separately.
- **T — timing / timeout.** A borderline-slow cell sometimes finishes within `RUN_TIMEOUT`,
  sometimes not → a pass/fail flip with no token difference at all.

## The instrument (shipped in this branch, default OFF)

`apply_determinism_env(SamplingParams) -> SamplingParams`, applied at all three handler
construction sites. Pure core `apply_determinism(s, force_greedy, seed)` is unit-tested
(3 tests). Two env knobs, **both default off → byte-for-byte unchanged behaviour**:

- `ROZUM_SAMPLING_SEED=<u64>` — fills `seed` **only when the client didn't send one**.
  Pins the Rust categorical RNG *and* MLX's RNG → a `temp > 0` run replays identically
  **without changing the agent's real temperature** (so it does not distort behaviour —
  the right knob to make the matrix a stable instrument).
- `ROZUM_FORCE_GREEDY=1|true|on` — forces `temperature = 0` and clears `top_p`/`top_k`
  (argmax, no RNG at all). Distorts behaviour (reasoning models loop their CoT under
  greedy), so **not** for the benchmark — it is the *isolation control* that removes E1
  entirely to expose E2.

## Decisive probe protocol (one coordinated single-model slot)

Run only when the host single-flight guard is on master and no sibling holds a model
(see "Safety" below). One model load; a handful of direct `curl` calls to the gateway —
cheap, bypasses the agent so it isolates the engine (E1/E2) from the agent (A):

1. **E1 present:** POST a byte-identical `/v1/chat/completions` body with `temperature: 1.0`
   N×; expect **divergent** completions → confirms unseeded sampling non-determinism.
2. **E1 fixed:** same body with `ROZUM_SAMPLING_SEED=42` set on the gateway; expect
   **byte-identical** completions → proves the seed pins it.
3. **E2 isolated:** `temperature: 0` (greedy) N× on a **dense** model (identical, control)
   and on the **MoE** matrix model (watch for an argmax flip at a near-tie) → attributes
   any residual greedy flip to `gather_qmm` seq-length variance, not sampling.
4. **A (optional):** capture the actual request bodies the agent CLIs send across two runs
   of one task (obs/tool-capture) and diff them → quantify agent-layer request variance.

Then, **iff** the seed makes a representative cell reproducible, wire the harness:
`ROZUM_SAMPLING_SEED=<fixed>` exported when `agentic.sh` starts each gateway, so the
matrix becomes a deterministic measurement instrument and every remaining red is signal.

## Safety (why the probe is gated)

The 2026-06-22 reboot was whole-system RAM overcommit from **multiple concurrent
model-loaded gateways** (`project-reboot-watchdog-oom`): never run >1 at once on the
36 GiB Mac. The probe above ran *after* the host single-flight guard landed (BUG-003,
master `3bcee03`, `share.rs::acquire_residency`), one small dense model at a time, with
graceful teardown — 0 reboots.

## Harness wiring

`scripts/bench/agentic.sh` now exports `ROZUM_SAMPLING_SEED="${ROZUM_SAMPLING_SEED-1234}"`
when it starts each gateway, so the matrix is reproducible by default (every red is a
red you can reproduce and debug, not noise). Override to any `<u64>`; set it **empty** to
restore free entropy sampling (`${VAR-default}` substitutes only when *unset*).

## Follow-up (coordinated, not now)

1. **End-to-end "flip gone" on a live matrix.** A fixed seed pins per-request sampling
   (E1), but the agent CLI may inject per-run data (session id / timestamp) into the
   prompt → a *different request* → different output even under a pinned seed (Layer A).
   So confirm on a real single-model matrix run (guarded, coordinated for RAM) that the
   `claude × test` flip is actually gone; any residual flip then isolates Layer A, E2
   (MoE numerics), or T (timeout) — already named above.
2. **Optional production complement:** parse the OpenAI `seed` request field into
   `SamplingParams.seed` so an API client can ask for reproducibility per-request without
   the global env (the env stays the bench/global knob). Small, additive; deferred until
   the end-to-end run confirms the approach.
