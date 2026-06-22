# Matrix failure analysis — agentic bench

Living analysis of the **14 reds** from the 2026-06-18 full-canonical matrix
(`scripts/bench/results/full-canonical-091719`, 60 cells on master: gpt-oss-20b +
Qwen3.6-27B + Qwen3-30B-A3B + Qwen3.6-35B-A3B × claude+codex+opencode × 5 tasks).
**Goal:** each fail is studied and either **fixed**, or **proven structural** and the
reason recorded here (what the problem is and why it is insurmountable).

> Discipline (user directive 2026-06-18): investigate the real mechanism from transcripts
> BEFORE concluding. Conclusions come last. Don't trust stale/mock-derived memory.

## Method
- **Reproduce** the failing cell with `KEEP=1` (the matrix discards agent logs) to capture
  the real transcript — what the agent/model actually did.
- **Classify by control:** another agent passes the same model+task ⇒ **agent mechanism**
  (fixable via that agent's tool path); **all** agents fail ⇒ **model/task ceiling**.
- Separate **our-bug vs model-limit** with a control experiment before any fix.

## The 14 reds (classification)
| cell | claude | codex | opencode | verdict |
|---|:-:|:-:|:-:|---|
| gpt-oss build | ✗ | ✗ | ✗ | **all fail → model/task ceiling** |
| gpt-oss fix | ✓ | ✗ | ✗ | agent mechanism (codex+opencode) |
| gpt-oss test/debug | ✓ | ✗ | ✓ | codex mechanism |
| 27B fix | ✓ | ✗ | ✗ | agent mechanism |
| 27B debug | ✓ | ✗⏱ | ✓ | codex mechanism |
| 30B build/test/debug | ✓ | ✗(⏱×2) | ✓ | codex mechanism |
| 35B build | ✓ | ✗⏱ | ✓ | codex mechanism |

⇒ **1 model ceiling** (gpt-oss build) + **13 agent mechanism** (codex-dominant; opencode on 2× `fix`).
codex `⏱` RUN_TIMEOUTs are a *symptom* (retry loops), not a separate cause.

---

## Finding 1a — codex: file creation stalls in the approval / meta-tool layer (Qwen3-30B repro)

**Repro:** codex `build` × Qwen3-30B-A3B (claude✓ opencode✓ codex✗), `KEEP=1`,
workdir `/tmp/rozum-agentic-VJepXV` (2026-06-18).

**Evidence — codex's own errors (verbatim from the transcript):**
```
ERROR codex_core::tools::router: error=approval policy is Never; reject command —
  you cannot ask for escalated permissions if the approval policy is Never
ERROR codex_core::tools::router: error=request_user_input is unavailable in Default mode
```

**Confirmed failure chain:**
1. Model plans correctly: create Cargo.toml, create src/main.rs, run `cargo run`.
2. Structured file-create tool call → **rejected** ("you cannot ask for escalated permissions").
3. Model calls **`request_user_input`** (a meta-tool) → "unavailable in Default mode".
4. Falls back to shell `cargo new reverse-cli` → **creates a SUBDIR** (violates "no subdirectory")
   with cargo's default `main.rs` (`println!("Hello, world!")`).
5. Tries to write the real reverse logic → **rejected again** (escalation).
6. `src/main.rs` stays the default; `cargo run` from cwd → `could not find Cargo.toml` (it's in
   the subdir) → empty output → verifier FAIL.

**Root cause (corrected).** **Plain shell WORKS** — `cargo new` ran in 154 ms. The failure is
NOT an inability to edit and is NOT the patch format. The local model **gratuitously requests
escalated permissions** for its structured file-writes (codex rejects them under `approval=never`)
and **calls codex meta-tools** (`request_user_input`, likely `update_plan`) instead of just running
plain shell. It then leans on `cargo new <name>` (subdir) and never overwrites the default file.

> This **overturns** the earlier "codex needs a structured-edit MCP tool" hypothesis (which came
> from a *mock-codex* harness, not the real CLI). The problem is codex's **tool surface + approval
> routing**, not the edit mechanism. Connects to memory `project-codex-patch-barrier` ("meta-tools
> distract even 35B; codex-lean ✓ vs full ✗") — but rozum's codex integration does NOT trim codex's
> meta-tools the way `--lean` trims claude's.

### Finding 1b — codex writes CODE via `echo > file`; shell escaping corrupts it (gpt-oss build repro)

**codex's failure mode is MODEL-DEPENDENT** — a second repro (codex `build` × gpt-oss-20b, workdir
`/tmp/rozum-agentic-oygvsq`) showed a *different* mechanism, no approval errors this time:
- The model wrote files with `/bin/zsh -lc 'echo -e "fn main(){…println!(\"{}\",rev)…}" > src/main.rs'`
  — files landed in **cwd** (no subdir here).
- But the produced `src/main.rs` is `println!({},rev);` — **the format string `"{}"` lost its quotes**
  to zsh escaping. It does not compile. codex then ran out of the wall-clock budget (timed out at
  180 s) before detecting/fixing it.

**This is the real answer to "why doesn't codex work through plain shell?"** Shell is simple for
*commands*, but writing **code** through `echo "…" > file` is a trap: any code with quotes/braces/
`!` gets mangled by shell escaping. claude/opencode write files with a **structured tool** (raw
content, no shell layer) → the code lands intact. codex leans on shell-echo (or hits the approval
path of Finding 1a for apply_patch) → the code is corrupted or never lands. Reconciles the old "shell
is simpler" note: simpler for commands, fragile for source code.

**Open:** why does the model pick `echo >` over codex's `apply_patch` (which would avoid escaping)?
Is apply_patch being rejected/penalized (cf. Finding 1a), or just not preferred? Capture the raw
tool calls (`codex --json` / gateway dump).

---

## Finding 2 — opencode: model appends a DUPLICATE `fn main` → compile error (gpt-oss build repro)

**Repro:** opencode `build` × gpt-oss-20b, workdir `/tmp/rozum-agentic-0e8uZt`. opencode uses
chat/completions + its built-in `write`/`edit`/`bash` tools (NOT codex's Responses path), so its
structured write does **not** mangle content. Evidence — the produced `src/main.rs` (323 B):
```rust
fn main() { /* correct reverse logic: chars().rev().collect(); println!("{}", reversed) */ }

fn main() {        // ← a DUPLICATE main appended by the model
    main();
}
```
Two `fn main` → `error[E0428]` duplicate definition. opencode DID iterate (it saw `cargo run`'s "no
targets specified" error and Edited `Cargo.toml` to add `[bin]`+version), so its loop works — but the
model's *edit* appended a second `main` instead of replacing, and it ran out of time (153 s) before
fixing the duplicate. **Root: a model edit-quality error** (append-vs-replace), surfaced by opencode's
structured write; not an infra bug. Distinct from codex.

## Finding 3 — gpt-oss `build` is NOT a model ceiling — it's flaky + agent-recovery

**Correction to the classification.** The "all 3 agents fail ⇒ ceiling" label came from a single
matrix run. The repro shows **claude PASSES gpt-oss build** (62 s → `olleh`). So there is **no true
model/task ceiling among the 14** — a single run can mislabel a *flaky* cell.

What actually happens (all from the same gpt-oss-20b model): the model emits a **buggy first draft**
of `main.rs` (claude's first draft: `chars().rev()..collect()` typo + `print!("{}", "{}")` garbage,
and it even emitted a malformed `Edit` with empty params → `InputValidationError`). Whether the run
passes is decided by **agent recovery within the time budget**:
- **claude** ✓ — read the file back, then **re-`Write` the whole file** with correct code (raw
  content, no append) → compiles → `olleh`. Recovered from both the model's bad draft and its own
  malformed tool call.
- **codex** ✗ — shell-echo mangled the code (Finding 1b) + too slow → timed out before recovery.
- **opencode** ✗ — duplicate `fn main` (Finding 2) + too slow → timed out before recovery.

---

## Finding 4 — codex `fix`/`debug` (edit-existing-file): unified-diff vs apply_patch format mismatch

**Repro:** codex `fix` × Qwen3.6-27B (claude✓ codex✗; opencode **flaky-passed** the repro), workdir
`/tmp/rozum-agentic-YwDNGa`. The harness seeds a `src/main.rs` whose `reverse()` returns the input
unchanged (`// BUG`). Sequence:
1. `cat src/main.rs` → the model **correctly diagnoses**: "reverse() just returns the input unchanged".
2. It calls codex `apply_patch` but emits a **unified-diff** body:
   ```
   *** Begin Patch
   --- src/main.rs
   ...
   *** End Patch
   ```
   → `Invalid patch hunk on line 2: '--- src/main.rs' is not a valid hunk header. Valid hunk headers:
   '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'`.
3. Edit **never lands** → file unchanged → `cargo run -> hello` → FAIL (timed out at 200 s, still failing).

**Root cause.** A **patch-format mismatch**: the local model produces a *standard unified diff*
(`--- / +++ / @@`), but codex's `apply_patch` requires its **bespoke** envelope
(`*** Begin Patch` + `*** Update File: {path}` + `@@`-less context lines). The model **knows the
fix** — it just cannot speak codex's patch dialect. This is the precise, evidence-based version of the
old `project-codex-patch-barrier` memory, and it is specific to **edit-existing-file** (create-from-
scratch failed differently — Findings 1a/1b). claude/opencode don't hit it (claude `Edit` old/new
string; opencode its own simple edit) — both are formats the model produces correctly.

**Fix direction (NOT concluded):** bridge the model's unified-diff → codex apply_patch format (a
gateway/wrapper translation), or steer the model to emit codex's format. A real, addressable
incompatibility — unlike the create case, this one IS partly about the edit mechanism.

> **opencode `fix` reds are flaky, not structural** — opencode passed this repro (slow, hit the
> 200 s ceiling but still produced `olleh`). Its matrix `fix` reds are speed/variance, not a hard
> mechanism failure.

---

## Synthesis (so far — NOT final)

The 14 reds are **not one bug**, and **codex fails differently by task shape**: create-from-scratch →
shell-echo corruption / approval-stall (1a/1b); edit-existing → unified-diff vs apply_patch mismatch
(F4). Three interacting factors, none a single clean infra defect:
1. **Model code-quality** — gpt-oss-20b (and weaker models) emit buggy first drafts. Universal; only
   *iteration* or a stronger model fixes it. claude masks it via fast full-file rewrites.
2. **Agent file-write mechanism** — claude (raw structured `Write`) is robust; **codex** corrupts code
   via shell-echo (1b) or stalls in the approval/escalation path (1a); **opencode** is structurally
   fine but exposed a model append-vs-replace error (F2).
3. **Time budget / speed** — codex is slow on our gateway → too few iterations to recover from 1+2 →
   RUN_TIMEOUTs. The `⏱` reds are this, compounding the above.

**Implication for "fix vs prove-impossible":** the recovery-and-speed factors are partly ours
(codex tool-surface / apply-patch preference, decode speed), while the model code-quality factor is a
ceiling for a given model. Verdicts per fail will be recorded above as each is fixed or proven structural.

**Still to do:** raw codex tool-call capture (1a/1b); repro the `fix`/`debug`/`test` reds (edit-on-
existing-file, a different shape than create-from-scratch); A/B any candidate fix; re-run the matrix.

---

## RESOLUTION (2026-06-18) — codex FIXED via lean + Method B (the model was never incapable)

A **mock-codex probe** (`/tmp/mockcodex.py` — talk to the model directly, deterministic, temp=0) was
the decisive tool. Probing 4 edit formats on the SAME Qwen3.6-27B:
- `edit(old/new)`, `write_file`, `unified_diff` → **perfect tool calls** (correct, applicable fix).
- codex `apply_patch` V4A → the model emits the patch as **text, not a tool call** (delivery break),
  and even when delivered, V4A's strict context-matching rejects it.

**The model is fully capable.** It produces a correct fix and delivers it flawlessly in 3 standard
formats. It only breaks on codex's **proprietary V4A apply_patch** — a format built for OpenAI's own
models, a poor fit for others. (This is why the same model gets 5/5 with claude's `Edit` old/new and
with opencode.) The fix is to MEET THE MODEL WHERE IT IS, not blame it.

**Two fixes (both in `src/gateway.rs`, Responses path = codex-only):**
1. **codex-lean** (`responses_tools_to_internal`, default ON, `ROZUM_CODEX_LEAN=0` to disable) —
   codex hands a local model ~18 tools + a ~21 KB prompt; it drowns (stalls / grabs meta-tools).
   `codex_lean_keep()` filters to the real coding surface. Lifts the model from "stalls half the time"
   to "reliably emits an edit".
2. **Method B** (`rewrite_apply_patch_command`) — the model's `apply_patch "<patch>"` shell command is
   rewritten into a reconstructed MINIMAL unified diff applied with `patch -p0 --fuzz=3` (standard
   tool codex runs verbatim; codex only intercepts `apply_patch`, not `patch`). `patch --fuzz` locates
   by context — tolerant of the line-number/whitespace drift that breaks V4A. The V4A header bridge
   (`rewrite_unified_diff_to_apply_patch`) stays as a fallback.

**Validated:** dedicated `codex fix × 27B` A/B = **0/5 → 5/5** (Method B fired + applied every run).
Codex column across 4 models (300 s timeout): **10/20 → 13/20**, with **Qwen3.6-27B and Qwen3.6-35B
now 5/5 — codex matches claude/opencode on the capable tier.** Residual (30B 2/5, gpt-oss 1/5) is
edit-method variance (the model sometimes writes the patch to a temp file instead of an inline
`apply_patch`, which Method B can't intercept) + gpt-oss specifics — improvable, diminishing returns.

**Lesson:** the codex reds were NEVER model incapability. The model speaks the universal formats
fluently; codex's proprietary V4A + tool/prompt overload were the barrier. Lean + a standard-tool
bridge resolved it on the capable models.

## RESOLUTION 2 (2026-06-18) — per-case dig into the 13 residual reds: one more gateway fix, the rest proven model-level

Re-ran every residual red with `KEEP=1` + `ROZUM_HARMONY_DUMP` (raw model output), same probe-first method
("understand the model, don't blame it"). Two structural conclusions:

**(A) The matrix is a noisy SINGLE sample — most "reds" are flakiness, not deterministic failures.**
`claude × gpt-oss fix` was red in the matrix (`turns=0`). Reproduced **7/7 PASS** (turns 3–12). The matrix
caught a rare stall; it *undersold* gpt-oss (claude × gpt-oss is really ~5/5). Lesson for reading the
matrix: a single red cell ≠ a bug — confirm with repetitions before concluding anything.

**(B) `codex × gpt-oss` had ONE more real gateway barrier — now fixed.** Dump showed:
```
ERROR codex_core::tools::router: error=unsupported call: apply_patch
```
gpt-oss (trained on OpenAI's own tool surface) calls **`apply_patch` as a FUNCTION**
(`{"command":["apply_patch","*** Begin Patch …"]}`), but codex — for the rozum-served local-model config —
offers apply_patch only as a **shell command**, never as a function tool (confirmed: not among the 18 tools
in `RESP_DUMP`). So codex rejects the call and the edit is lost. This is the function-call analogue of the
V4A barrier, and it's just as fixable in the gateway.

**Fix (3rd gateway fix, `src/gateway.rs` Responses path):** `rewrite_apply_patch_function_args` +
re-route in both `responses_sse_stream` and `responses_collect`. When the model emits an `apply_patch`
*function* call **and codex did NOT offer apply_patch as a tool** (`apply_patch_is_tool` computed from the
request), rename the item to `exec_command` and convert the args into `{"cmd": "<patch --fuzz heredoc>",
"login": true}` (reusing Method B's `apply_patch_block_to_fuzz`; raw-`apply_patch` heredoc fallback). The
gate means a real codex-with-apply_patch config is untouched, and Qwen (which calls apply_patch via the
*shell*, name=`exec_command`) is unaffected — Method B path unchanged.

**Validated:** `codex × gpt-oss fix` repro — `unsupported call: apply_patch` **eliminated (0/0/0)**, the
re-route fires whenever the model uses the function form, and a run **passes *because* of it** (run1, reroute
×3). Unit test `apply_patch_function_reroutes_to_exec_command`; full gateway suite 50/50.

**The genuine residual is now proven MODEL-LEVEL (not a gateway mechanism), with evidence:**
- **Malformed shell.** gpt-oss emits broken commands — `rg -n "main.rs" "reverse"` (args swapped → exit 2),
  `sed -n -n ./src/main.rs` (double `-n`, no script → exit 1), `sed -n 's/Hello/'"'`?'` (unmatched `` ` ``).
  The model never even reads the file. Repairing arbitrary broken shell in the gateway is unreliable +
  unsafe → **not gateway-fixable**.
- **Looping (temp-1.0 variance).** A failing run generated the *correct* fix (`s.chars().rev()`) **74×** in
  one turn without finalizing. gpt-oss is a reasoning model that must run at temp ~1.0 (greedy makes it loop
  on structural tokens; a repetition penalty corrupts harmony — both already refuted). Variance is intrinsic.
- **`cargo new <name>` → subdirectory** (build, instruction-following): the model creates a subdir despite
  "do NOT create a subdirectory". Only "fixable" by a `cargo new`→`cargo init` rewrite that is **wrong in
  general** (legitimate elsewhere) — declined as a hack; it's a model instruction-following limit.
- **Edit-method variance (30B).** `fix` passes when the model picks `sed`/inline-`apply_patch` (works
  natively / via Method B / via the new re-route) and fails when it stalls or writes the patch to a temp
  file first — the model chooses, per run.

**Bottom line.** Three clean gateway barriers existed and are all fixed (lean, Method B shell-bridge, and now
the apply_patch-function re-route). Everything still red is genuine model behaviour — flakiness, malformed
shell, looping, instruction-following — **proven by raw-output evidence, not assumed**, and not removable in
the gateway without hacks. The matrix's single-sample noise overstates it.

## RESOLUTION 3 (2026-06-19) — codex × gpt-oss dissected to the token; delivery was dropped at THREE layers

Deep per-turn dissection (HARMONY_DUMP + PROMPT_DUMP) of failing `codex × gpt-oss fix` runs overturned the
earlier "it's just temp-1.0 variance" read. The model **knows the fix** (the correct `s.chars().rev()`
appeared in 27/27 generation blocks of a failing run) — we were **dropping its delivery at three layers**,
each a real gateway bug, not a model failure:

1. **apply_patch shape.** codex's own 21 KB instructions teach `{"command":["apply_patch","<patch>"]}` (it
   calls apply_patch a "tool" but shows shell-array args, and apply_patch isn't a registered function for our
   config). The model reproduces this faithfully but in varied shapes (`command` array, `{cmd:"apply_patch",
   patch}` sibling, `begin_patch`/`update` keys) + double-escapes `&`/`>` as `&`/`>`.
   Fixed: `rewrite_apply_patch_function_args`, `normalize_codex_tool_args` folds the `{cmd:apply_patch, patch
   sibling}` shape, `decode_unicode_escapes` repairs the bodies.
2. **File reads (the decisive factor).** Verified: when the model READS the file → patches land
   (`patch-failed=0`) → pass; when it doesn't → fail. But gpt-oss generates broken `sed` reads
   (`sed -n 'src/main.rs'`, missing `p`) that exit non-zero, so it never sees the file. `repair_broken_read`
   (`ROZUM_CODEX_READ_REPAIR`, env-gated) translates a malformed sed/head/tail read → `cat <file>`. NOTE:
   codex's "do NOT re-read files after apply_patch" is NARROW (post-patch verify only) — it does **not** ban
   the initial read.
3. **Garbled harmony envelope (the "stalls").** The dominant residual "stall" (empty turn) was NOT the model
   giving up — it emitted a tool call whose `to=functions.NAME` recipient was dropped or detached into its
   own channel segment, so `parse_harmony` dropped it. `infer_tool_from_body` (harmony.rs, default-on) now
   recovers the function from the args shape (`cmd`→exec_command, `patch`/`*** Begin Patch`→apply_patch).

**REFUTED experiments (kept as negative results):** (a) **injecting an apply_patch tool with a clean schema**
made it WORSE (3/5 → 0/5) — it converged the arg-key (the model stopped guessing) but **conflicted** with the
instruction's `command` format and pushed blind patching; lesson: *translate the model's output, don't change
its interface*. (b) **top_p tail-clip** (`ROZUM_GPTOSS_TOP_P`) damps malformed-shell spirals but doesn't move
pass (within noise).

**Net (full gpt-oss matrix, all fixes, 0 panics): 12/15 (80%)** — claude 5/5, opencode 4/5, codex 3/5 (was
8/15: claude 5/5, codex 1/5, opencode 2/5). The residual reds are **create-from-scratch** (`build`/`test`:
`cargo new` subdir + degenerate impl) + temp-1.0 variance. **Honest caveat:** `codex × fix` in isolation is so
variance-dominated (observed 0/5…4/5 across runs) that n≤8 reps cannot distinguish a 10-15% gateway gain —
each fix removes a real but *intermittent* mode, so the aggregate is noisy; judge gpt-oss on the whole matrix,
not the single hardest cell. The three delivery fixes are the durable win; recovery / read-repair are correct
insurance for the intermittent modes.

## Finding 5 (2026-06-21) — codex create-from-scratch is a gateway-fixable malformed write-intent, NOT a model limit

Finding 1b parked the create-path root cause on "capture the raw tool calls" — never done; the
`apply_patch create-if-missing` attempt was then discarded as a **model limit** (SPRINT) *without* that
data. I captured it: `ROZUM_CODEX_TOOL_CAPTURE=1` on a real `codex × gpt-oss-20b × build` run (the
gateway records `codex_tool_call`). **10 of 11 tool calls were one consistent malformed shape**, e.g.:

```json
{"cmd":"apply_patch","shell":"zsh","cmd":"apply_patch","path":"Cargo.toml",
 "content":"[package]\nname = \"reverse-cli\"\nversion = \"0.1.0\"\n..."}
```

The model **knows what to write** — `content` is a valid `Cargo.toml`. But it expresses the file-write
as `exec_command` args carrying `{cmd:"apply_patch", path, content}` (note the duplicate `cmd` key).
codex's `exec_command` only understands `{cmd:"<shell>"}`, so it runs `apply_patch` as a **bare shell
command with no patch**, silently ignoring `path`+`content` → the file never lands → `build` loops to
the timeout (rc=143, 600s) and `test` fails (pass=0). This is a **write-intent shoved into
`exec_command`**, not the Finding-1b `echo>file`/zsh-escaping theory (no `echo` was used), and not a
model ceiling (claude drives the SAME gpt-oss to pass `build` via its Write tool).

It also explains **why `apply_patch create-if-missing` "didn't help"**: that fix operates on *patches*,
but the model isn't emitting a patch — it's emitting `{path, content}`, which the patch path never
sees.

**Gateway-fixable (the real lever):** in the codex `exec_command` arg handling, detect args that carry
`content` (+ `path`) — a write-intent — and synthesize a real write, e.g.
`cat > <path> <<'ROZUM_EOF'\n<content>\nROZUM_EOF` (or route to the existing apply_patch path with a
create-file patch). Then the correct content lands and `build`/`test` pass. **This lives in the codex
apply_patch/`exec_command` rewrite block (`gateway.rs ~1570–2090`), which a sibling agent is actively
editing — handed to that track to implement (with this capture as the spec) rather than edited here.**
Evidence run: `scripts/bench/results/agentic-20260621-193933/` (codex×gpt-oss build rc=143, test pass=0).

**RESOLVED (2026-06-22).** Implemented in `normalize_codex_tool_args` (the same `exec_command` arg
walker that already folds `{cmd:apply_patch, patch-sibling}` → `patch --fuzz`): when the bare
`apply_patch` command carries `{path, content}` and `content` is **not** a patch (no `*** Begin
Patch`), synthesize the real write the model intended — `mkdir -p "$(dirname '<path>')"; cat >
'<path>' <<'ROZUM_WRITE_EOF'\n<content>\nROZUM_WRITE_EOF` — a *single-quoted* heredoc so the body
lands byte-for-byte (no `$`/backtick/`\` expansion), preceded by `mkdir -p` for nested targets. The
patch path is unaffected (patch-content still folds to `patch --fuzz`); path-only calls are left
untouched (we never invent empty files). Spec: `docs/specs/codex-create-write-synth.md`. Validated:
unit test `synthesizes_file_write_from_path_and_content` + shell-level e2e of the synthesized command
(file lands, body verbatim, `mkdir` creates `src/`). Full model-in-the-loop matrix cell deferred —
RAM held by a concurrent GLM-4-32B matrix run; to be re-run when memory frees.

**Update (2026-06-22) — ran the live cell; verdict is now nuanced.** gpt-oss runs at a forced
temp≈1.0 (greedy collapses its CoT → 0/6) so create-from-scratch is **highly stochastic**: the clean
`{path, content}` shape that the first capture saw 10/11 is only ONE of many. Captured two more
*coherent* PATCH-based create shapes and handled them in `apply_patch_block_to_fuzz` (commit
`a5e051b`): (a) `*** Add File:` / `*** Create File:` directives (the dominant shape; `*** Create
File:` is gpt-oss's variant of standard V4A), (b) `*** Update File:` against an absent file (bogus
`---` old-side → `patch` can't update a missing file). Both write via a shared `synth_create_command`
(`[ -e path ] || { mkdir -p …; cat > path <<'EOF' … }`, idempotent, edits stay on `patch --fuzz`).
Paired with **`ROZUM_GPTOSS_TOP_P=0.95`** (clips the junk-token tail that otherwise makes the model
emit unparseable garbage), this **flipped `codex × gpt-oss × test` 0→1** — the first create-from-
scratch green; files land, compile, and run. **But `build` stayed 0/3**, and inspection shows the
residual is now genuinely **model-side, not a gateway gap**: (#1) files landed + Cargo built but the
model wrote *invalid Rust* (`let args: std::env::args().collect::<String>();`); (#2) the model
*refused* ("you're asking me to create a project that requires a sub-directory `src/`" — misread "no
subdirectory"); (#3) a fragile placeholder-then-patch dance whose patch `.rej`'d. No gateway rewrite
fixes broken Rust or a refusal. **Conclusion:** delivery WAS broken (the gateway fixes are real and
necessary — files now land) AND create-from-scratch ALSO has a real gpt-oss-20b capability ceiling.
Both prior framings were half-right; the honest split is *delivery = gateway-fixed, correctness =
model-bound*. A reliable 5/5 for gpt-oss create-from-scratch is not achievable at the gateway alone.
