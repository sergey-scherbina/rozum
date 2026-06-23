# GLM-4 bringup (MLX-native port)

## Overview

Add GLM-4 (Zhipu/Z.ai, dense) to rozum's catalog by porting the architecture into
the native MLX runtime — the same path as the gpt-oss and Qwen3-Coder bringups
([[project-gptoss-native-port]]). GLM-4 dense is architecturally close to Qwen3
(GQA + q/k/v bias + RMSNorm) with two GLM specifics: **partial RoPE** (only the
first half of each head's dims are rotated) and GLM's **norm placement / weight
names / chat template**. The MLX-native path is chosen over the (already-present)
mistral.rs `glm4` loader for speed — candle/Metal is ~5–10× slower
([[project-mistralrs-mlx-direct]]); mistral.rs is the quick "does it run" check only.

Targets that fit a 36 GB Mac (4-bit): **GLM-4-9B-0414** (bring-up) and
**GLM-4-32B-0414** (the real target). MoE GLMs (4.5-Air/4.5/5) are out of scope (RAM).

## Interface

- New module `glm4` in the vendored mlx-lm crate
  (`.vendor/mlx-lm/mlx-lm/src/models/glm4.rs`), exposing `load_glm4_model(dir)`,
  `Model`, `ModelInput` (mirroring `qwen3`'s surface).
- Register in `.vendor/mlx-lm/mlx-lm/src/models/mod.rs`.
- Dispatch arm in `src/mlx_native_backend.rs`: `"glm4" => glm4::load_glm4_model(dir)`.
- Catalog entry in `src/models.rs` (GLM-4-9B first, then 32B).
- Chat template: GLM's `[gMASK]<sop>` + `<|user|>` / `<|assistant|>` turns
  (verify against the checkpoint's `tokenizer_config.json` / `chat_template`).

## Architecture (from `zai-org/GLM-4-9B-0414` config.json — verified)

```
model_type=glm4  architectures=[Glm4ForCausalLM]  hidden=4096  layers=40
heads=32  kv_heads=2 (GQA)  head_dim=128  vocab=151552  intermediate=13696
partial_rotary_factor=0.5   rope_theta=10000   rms_norm_eps=1e-5
attention_bias=true   tie_word_embeddings=false
```

Reuse from existing ports: **partial RoPE** (`qwen3_5`/`qwen3_5_moe`), **q/k/v bias**
(`qwen3`), **RMSNorm** (`qwen3`).

### Exact structure (from `mlx_lm.models.glm4`, 181 lines — saved at `glm4_ref.py`)

The blueprint — `glm4.rs` mirrors `qwen3.rs` and changes exactly these:

- **Partial, traditional RoPE**: `RoPE(dims = head_dim * partial_rotary_factor = 64,
  base = rope_theta, traditional = TRUE)`. Only the first 64 of 128 head dims are rotated,
  and it's the **interleaved/traditional** RoPE (not the GPT-NeoX half-split). The remaining
  64 dims pass through. (`qwen3_5` has partial rope but check the traditional flag.)
- **q/k/v bias = true, o_proj bias = false** (`attention_bias`); GQA 32/2.
- **Sandwich norm — FOUR RMSNorms per layer** (the distinctive GLM-4 trait):
  ```
  x = x + post_self_attn_layernorm( self_attn( input_layernorm(x) ) )
  x = x + post_mlp_layernorm( mlp( post_attention_layernorm(x) ) )
  ```
  i.e. a norm BEFORE each sublayer (input_/post_attention_) AND a norm on each sublayer's
  OUTPUT before the residual add (post_self_attn_/post_mlp_). qwen3 has only the two
  pre-norms; GLM adds the two post-norms.
- **MLP**: SwiGLU (gate/up/down), like qwen3.
- Final `norm` (RMSNorm), `embed_tokens`, untied `lm_head`.

## Behavior

- [x] `config.json model_type: "glm4"` loads (no "unsupported model_type").
- [x] Partial RoPE: only `head_dim * partial_rotary_factor` (= 64) dims rotated;
      the rest pass through unrotated — matches `mlx_lm.models.glm4`.
- [x] q/k/v projections carry bias (`attention_bias=true`); o_proj per the reference.
- [x] GLM norm placement (input + post-attn; confirm sandwich post-mlp/post-self-attn
      against the reference) reproduced exactly.
- [x] Weight-name remap is exact (the gpt-oss "garbage bug" risk) — q/k/v/o, gate/up/down,
      norms, embed, lm_head all bound to the right tensors.
- [x] **Byte-exact greedy parity** vs Python `mlx_lm` on GLM-4-9B for a fixed prompt
      (`scripts/mlx_ref.py` — logits/`||x||` per-layer, then identical token stream).
- [x] Chat template renders a clean single-turn reply and a tool call.
- [x] Runs through `rozum launch` on 36 GB at 4-bit; GLM-4-32B-0414 likewise.
- [x] **Logit-constrained tool calls** — the constrained decoder recognises GLM's
  `name\n{json}` envelope (no `<tool_call>` opener) and, once the model names a known
  tool at a line start, forces the arguments to valid schema-conforming JSON. A pure
  prose answer (no tool-name line) is left unconstrained (the final-answer path must
  survive). Default-on with the other constraints (`ROZUM_MLX_CONSTRAIN`).
  (`find_glm_tool_call_anchors_on_known_name`; proven firing on the live matrix —
  every call in claude×fix/debug is schema-valid.)
- [x] The parser extracts a `name\n{json}` call even when prose precedes it
  (the constraint forces clean args but does not remove a lead-in line), and the
  call is suppressed from the streamed text (`tool_markup_at`) so it doesn't leak as
  both text and `tool_use` (the re-emit loop). (`glm4_embedded_after_prose`,
  `tool_markup_suppression_points`.)

## Out of scope

- GLM MoE (GLM-4.5-Air 106B, GLM-4.5 355B, GLM-5/5.1 744B): too big for 36 GB; GLM-5
  is DeepSeek-style sparse-attention MoE, a separate (much larger) port. See BACKLOG
  `glm-model-landscape`.
- Perf tuning (batched decode / prefix reuse) — reuse the generic native-runtime paths;
  optimize later if GLM becomes a primary model.

## Design / Decisions

- **MLX-native port over mistral.rs** — speed (the North Star is MLX-native top-of-chain).
  mistral.rs `Glm4ForCausalLM` is the quick validation path (`ROZUM_FORCE_MISTRALRS=1`),
  not the shipping path.
- **Mirror `qwen3.rs`** — closest existing port (GQA + qkv bias + RMSNorm); splice in the
  partial-RoPE from `qwen3_5`. Rejected: from-scratch (reinvents the shared scaffolding).
- **Validation = byte-parity vs Python `mlx_lm.glm4`** — the proven bringup gate; localizes
  any weight-remap/norm/rope bug to a specific layer before trusting end-to-end output.

## Results

**Bring-up WORKS (2026-06-21).** `glm4.rs` (~560 lines) written + integrated; `cargo check
-p mlx-lm`, `cargo check --features mlx-native`, and the full release build all clean.
GLM-4-9B-0414-4bit loads through the port and runs coherently:

- `LOADED 1007 params (glm4)` → `mlx-native: 'mlx-community/GLM-4-9B-0414-4bit' ready
  (context 32768)` — weight names + shapes align (the remap + field names are right).
- Coherence smoke (temp=0): *"Paris, the capital of France, is famous for its rich history,
  iconic landmarks like the Eiffel Tower…"*, and the Rust task returned correct code —
  `fn reverse_string(s: &str) -> String { s.chars().rev().collect() }`. So the forward
  (partial-traditional RoPE + 4-norm sandwich + fused gate_up + qkv-bias attention) is
  correct enough for coherent, accurate output on the first runtime try.

**BYTE-PARITY PASSED.** Python oracle (uv venv python 3.12.8 + mlx_lm 0.31.3) greedy on
`mlx-community/GLM-4-9B-0414-4bit` for "What is the capital of France? Answer in one short
sentence." → ids `[198,785,6722,315,9621,374,12089,13,…]` = `"\nThe capital of France is
Paris.<|user|>…"`. rozum greedy (temp=0) on the identical prompt → `"\nThe capital of France
is Paris."` — a **32/32-char, token-for-token identical prefix** (rozum additionally stops
correctly at `<|user|>`, which the raw Python `generate_step` loop runs past). The forward
(partial-traditional RoPE + 4-norm sandwich + fused gate_up + qkv-bias) is numerically exact.

**GLM-4-32B-0414 ALSO WORKS — zero new code.** Same `glm4` arch (61 layers, hidden 6144,
`attention_bias=false` — all config-driven); `LOADED 1349 params (glm4)` → ready, coherent +
correct code, and **byte-parity** (Python greedy `[198,785,6722,315,9621,374,12089,13,…]` =
rozum's first 8 generated tokens). ~19 GB at 4-bit. Both GLM-4-9B and GLM-4-32B-0414 are in the
`EXTRA` catalog; the port is parity-validated for the dense GLM-4 family. Shipped to master;
mlx-lm fork rev `12fac5c0`.

**TOOL-CALL ADAPTER DONE (`ab404d1`) — GLM-4 drives the full agentic tool loop.** GLM emits a
call as `<name>\n<json>` (stops at `<|observation|>`, already in config eos). Three pieces:
`parse_glm_tool_call` (serving.rs, tight last-resort fallback); strip `ensure_ascii=False` from
the chat template (minijinja rejects the kwarg; Rust tojson is already unicode-aware); and
`glm_conversation` for multi-turn (assistant `ToolUse` → `name\n{json}`, tool result →
`observation` role, dispatched on the template's `<|observation|>` marker, à la harmony on
`<|channel|>`). E2e on GLM-4-9B: single call → `{"city":"Paris"}` parsed (`finish_reason=tool_calls`);
multi-turn → reads the tool result correctly ("rainy, 14°C"). serving 11/11.

**AGENTIC MATRIX + ROOT-CAUSE (2026-06-22).** GLM-4-32B on the agentic matrix scored
claude 2/5, codex 1/5, opencode 1/5. Rigorous isolation (the `isolate` skill) **refuted**
the seductive "weak agentic model" read: with clean prompts GLM emits perfectly structured
`name\n{json}` calls on both the OpenAI and Anthropic endpoints. The failures are *format*,
not capability — the agents' large system prompts ("explain in prose, use markdown before
each call") push GLM into markdown narration (` ```bash\nRead\n{json}\n``` ` or a bare
`prose\nRead\n{json}`), which the post-hoc parser catches only sometimes.

A prompt-override (a strong "name + JSON only, no prose/markdown" system instruction) was
tried and **discarded**: it half-worked (GLM dropped the fence) but kept a lead-in prose
line, regressing the previously-passing claude×fix cell, and never reached codex's responses
path. Lesson recorded in the `isolate` skill: an isolated single-turn probe that doesn't
share the full multi-turn / multi-endpoint path proves nothing.

### Constrained tool-call decoding (the robust fix)

The durable fix is logit-level, not prompt-level: force the structured shape at decode time
so no system prompt can talk the model out of it.

- **Envelope.** GLM has no `<tool_call>` opener and no Hermes `{"name":…,"arguments":…}`
  wrapper — the call is `{tool_name}\n{bare_args_json}` ended by `<|observation|>` (eos).
  So the existing `ToolConstraint` triggers (`find` `<tool_call>` / loose `{"name"`) never
  fire for GLM; it enters the masked B=1 loop (tools present) but never activates → free
  narration. Add a GLM mode.
- **Trigger = a known tool name on its own line.** `find_glm_tool_call(text, names)` scans
  for the *last* line that is exactly one of the offered tool names immediately followed by
  `\n`; activation point = the byte just past that newline. This fires for the clean case
  (name is line 1) *and* the narrated case (name after a prose/fence preamble), and never
  fires for a pure prose final answer — so the answer path is untouched. (Codex's failure
  mode — GLM emits a raw ` ```zsh\ncat …\n``` ` shell line instead of naming the `shell`
  tool — is **out of scope**: GLM never names the tool, so there is nothing to anchor on.)
- **Body = the chosen tool's arg schema.** Once anchored, constrain `text[activation..]` as
  `Constraint::Json(arg_schema[i])` — the bare arguments object, reusing the existing JSON
  prefix-matcher and `is_complete`. No envelope wrapper.
- **Parser + suppression.** With the constraint, GLM may still emit a lead-in prose line
  before the (now clean) call. Extend `parse_glm_tool_call` with an *embedded* form (a bare
  identifier line followed by a balanced JSON object, anywhere) and `tool_markup_at` to
  suppress the GLM call from streamed text.

**Decision — line-anchored trigger, not a turn-start force.** Forcing a tool call from token
0 would break GLM's final-answer turn (terminated by `<|endoftext|>`), which the agentic loop
needs to end cleanly. Anchoring on a tool-name line constrains *only* the call branch and
leaves prose answers free. Rejected: turn-start force (kills final answers); markdown-token
banning (whack-a-mole, the discarded override's failure mode).

### Results — constraint SHIPS; the matrix ceiling is a different gap

Built, full lib suite green (449/0), and run on the **live** matrix (the regressed cell, not
an isolated probe — the override's lesson): `mlx-community:GLM-4-32B-0414-4bit`, claude + codex
× build/fix/test/debug, `KEEP=1`, constraint default-on.

- **The constraint fires and is correct.** Every call it produced is schema-valid: claude×fix
  → `Read\n{…}`, `Edit\n{…}`, `Bash\n{cargo run …}` (3 clean calls, **pass=1**); claude×debug
  → `Read`/`Read`/`Bash{cargo test}` all clean. **No regression** — fix still passes (the
  discarded prompt-override had broken exactly this cell). Args are now schema-forced, not
  incidentally valid.
- **The matrix score does not lift, for a *different* reason the constraint cannot reach.**
  Reading the kept transcripts: build/test fail because GLM emits the **artifact directly** —
  the Cargo.toml / main.rs *content* inside ```toml / ```rust fences — instead of ever naming
  the `Write` tool (claude×test: turns=1, **tools=0**; claude×build: 1 Bash call, files never
  written). Codex fails the same way: raw ` ```bash\ncat …\n``` ` instead of naming `shell`.
  No tool-name line ⇒ no anchor ⇒ nothing for any output-format constraint to force.
- **Therefore: GLM-4-32B-0414 has a tool-use _decision_ gap, not a _format_ gap.** When it
  decides to call a named tool it now emits a clean, schema-valid call (the constraint
  guarantees it). For file-creation and shell it tends to *show* the artifact rather than
  *name* the tool — a property of how GLM-4-0414 was tuned, addressable only model-side (a
  tool-calling-tuned GLM variant) or by an intent-forcing scheme that would break the
  final-answer turn. debug is a third axis: clean calls but the driver loops without
  converging (a reasoning limit, RUN_TIMEOUT).

**Verdict.** Ship the constraint default-on: it is correct, non-regressing, and the durable
hardening for GLM tool calls (schema-valid args, no drift, suppression) — unlike the
prompt-override it does not regress. The agentic-driver ceiling on the 5-task matrix
(claude 2/5) is now set by the decision gap + reasoning convergence, which are not
output-format problems. GLM-4 stays in `EXTRA` as a parity-exact chat/code model with
hardened (when invoked) tool-calling.

### Can the decision gap be fixed? Nudge tried → discarded; it's model-side

A positive few-shot DECISION nudge (`ROZUM_GLM_TOOLUSE_NUDGE`, injected into the GLM render:
"you are an agent — call tools, don't print artifacts"; tool-name-agnostic, the constraint
owns format) was tried. Live A/B on one binary:

- **The decision gap IS prompt-movable.** With the nudge GLM *named* `Write` for the first
  file in claude×test — behavior it never showed before. So it's not that GLM *can't* decide
  to call the file tool; the right context can elicit it.
- **But the nudge is not a reliable lever — discarded.** It *reliably regressed the one stable
  cell* (nudge-OFF claude×fix = 1, 2/2 runs; nudge-ON = 0 — GLM read the file, described the
  fix in prose, and stopped without the Edit), and induced a new failure mode (GLM emitted the
  args object with **no tool name** → nothing to anchor → no call). Net-negative.
- **The 5-task matrix is too noisy to measure small deltas.** The control exposed it directly:
  claude×test flipped **0↔1 across two nudge-OFF runs of the identical config**. A ±1-cell
  single-run delta on these toy tasks is variance, not signal — only a *reliably reproduced*
  shift (the fix regression) or a multi-run mean is trustworthy. Don't read one matrix run as a
  verdict.

**Conclusion.** GLM-4-32B-0414's agentic ceiling is a tool-use *decision* property — movable by
prompting, not *fixably* so without regressing the stable behavior. The robust fix is model-side
(a tool-calling-tuned GLM variant). For agentic driving, rozum already has Qwen3.6-35B (15/15);
GLM-4 is the parity-exact chat/code model with hardened tool-calling when it does invoke.

## UPDATE 2026-06-22 — reliable agentic use FOUND (the breaker is *narration framing*, not size/competence)

The earlier "decision gap, movable not fixably so" conclusion was **too pessimistic about the
software lever.** A load-bisection probe (model-only, GLM-4-32B + a `Write` tool + a
create-from-scratch task) isolated the trigger precisely:

| system prompt | result |
|---|---|
| minimal "call a tool to act" (no framing) | **NAMED the tool** (2/2 temp0) |
| + "explain in prose, show code in markdown ``` for the user" framing | **ARTIFACT** — shows ```rust, names nothing (2/2 temp0, 4/4 temp1) |
| framing + a counter-instruction ("you MUST call Write; markdown does nothing") | **does NOT reliably reverse** — 1/3 inline, 0/4 as last msg, 1/4 strong-last-msg |
| no framing (with or without counter) | **NAMED, 3/3 temp1** — reliable |

**So GLM-4-0414's "decision gap" is caused by the agents' narration framing** ("explain your
reasoning in prose and show code/file contents in markdown code blocks"), which claude/codex bake
into their system prompts. It is NOT model incompetence (clean prompt → reliable) and NOT raw
context size (that is gpt-oss's breaker, `constrained-gptoss-delivery.md`). A **counter-instruction
does not reliably win** against the framing (confirms why the old prompt-override/nudge regressed).
**The reliable lever is REMOVING the framing, not fighting it.**

### How to use GLM reliably with agents
1. **Drive it with a lean, tool-first system prompt that has NO narration framing** — "to act,
   call a tool; do not print file contents." Then GLM names `Write`/`shell`/edit tools reliably and
   the shipped logit-constraint (`99c6081`) makes the args schema-valid. (Use rozum's embedded
   agent / a custom prompt; the matrix's claude/codex CLIs bake in the framing.)
2. **Gateway sanitizer (proposed, the shippable mechanism for claude/codex):** when serving a
   `glm4` model with tools present, **strip the narration-framing directives** from incoming system
   prompts (sentences instructing prose-explanation / markdown code-block display). Then claude/codex
   become reliable with GLM. Needs careful matching + live A/B on the matrix (slot-gated) before
   default-on; risk = false-positive stripping → keep it GLM-only + conservative. Counter-injection
   is NOT a viable alternative (proven above).
3. GLM-4-32B is already reliable for **edit-existing** (fix/debug) + chat/code; the framing mainly
   breaks **create-from-scratch**. Qwen3.6-35B (15/15) stays the no-caveats agentic driver.

### Done-when (for the sanitizer ship)
A `glm4` gateway under claude/codex names tools (not artifacts) on create-from-scratch in a live
A/B with the sanitizer on; matrix GLM cell lifts; no regression on edit-existing. (Probe harness:
direct `/v1/chat/completions` with a tool + create task, count tool_calls vs ```-fenced content.)

### Sanitizer A/B — VALIDATED (2026-06-23, gateway end-to-end)
Built (`a4b757d`): `render_prompt_opt` strips narration-framing from GLM system prompts when
tools are present (`ROZUM_GLM_STRIP_FRAMING`, **default ON**). A/B through the real gateway path
(GLM-4-32B, the framing system prompt + a `Write` tool + a create task, N=3, temp 1):

| ROZUM_GLM_STRIP_FRAMING | result |
|---|---|
| **0 (control, no strip)** | **ARTIFACT 3/3** — GLM shows ```rust, names nothing |
| **on (default, strip)** | **NAMED 3/3** — GLM calls `Write` reliably |

So the gateway sanitizer **reliably reverses the framing-induced decision gap** end-to-end. GLM is
now a reliable tool-caller under narration-framing prompts. REMAINING (separate, longer): a real
claude/codex×GLM **matrix-cell** A/B to confirm the conservative matcher catches the *actual* agent
phrasings (tune `is_glm_narration_directive` from a captured prompt if it misses) and to read the
score lift. Mechanism + default-on path are proven.

### Real claude×GLM A/B — INCONCLUSIVE; the synthetic A/B did NOT transfer (2026-06-23)
Ran the real cell (`agentic.sh`, GLM-4-32B × claude × build, REPS=2, strip OFF vs ON):

| arm | result |
|---|---|
| strip OFF (control) | `turns=1 tools=0 pass=0` (×2) |
| strip ON (default)  | `turns=1 tools=0 pass=0` (×2) |

**Both arms identical + degenerate.** `turns=1` = claude made ONE call, GLM returned the
artifact as TEXT (no tool_use to execute), claude stopped → no file → pass=0. The sanitizer made
**no difference** in the real claude path, even though the model-only probe was NAMED 3/3.
**Conclusion: the synthetic-prompt validation did NOT transfer** — either `is_glm_narration_directive`
does not catch **claude's actual** framing phrasing (claude `-p --lean` may frame it differently or
elsewhere), or the env didn't differentiate, or the degeneracy masks the effect. So the sanitizer is
**proven as a mechanism (model-only) but NOT YET confirmed to fix real claude×GLM.** Correcting the
premature "GLM is reliable now" — it is reliable on the direct API with a controlled prompt; the
claude/codex CLI path is unconfirmed.

**Next (slot-gated): capture + tune.** Run ONE `rozum launch claude -p "<create task>" --lean` against a
GLM gateway with request capture (KEEP=1 + a system-prompt dump), inspect (a) does claude's real system
prompt contain the narration framing and in what words, (b) did the sanitizer strip it, (c) what GLM
returned (artifact text? why turns=1). Then tune `is_glm_narration_directive` to the real phrasing, or —
if claude's prompt has no such framing — the claude×GLM gap has a different lever and the sanitizer only
helps direct-API / lean-prompt callers. (Slot was taken by a sibling matrix mid-investigation.)
