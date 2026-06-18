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
