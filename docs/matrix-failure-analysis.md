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

## Finding 1 — codex: file creation blocked by the approval / meta-tool layer (NOT the edit format)

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

**Open (to confirm before concluding a fix):**
- The raw tool call — does the model set `with_escalated_permissions=true`? Which meta-tools does
  codex 0.137 offer? (gateway doesn't log request bodies — capture via `codex --json` or a dump.)

**Fix candidates (NOT yet concluded):**
- Trim codex's tool surface: drop/disable `request_user_input` + `update_plan` (a codex analog of
  `--lean`) so the model uses plain shell / apply_patch directly.
- Gateway-normalize the escalation flag out of the model's tool calls so codex stops rejecting them.
- Validate with an A/B re-run of the codex `build`/`fix`/`debug` reds.

---

## Finding 2 — opencode (`fix` on gpt-oss, 27B) — INVESTIGATING
Not yet reproduced with a transcript. opencode shares only the `fix` reds with codex (and passes
nearly everything else, incl. 5/5 on 30B+35B), so its mechanism is likely distinct.

## Finding 3 — gpt-oss `build` (the one model/task ceiling — all 3 agents fail) — INVESTIGATING
Not yet reproduced. Hypotheses to test: temp-floor not applied / "no subdirectory" prompt
ambiguity / genuine gpt-oss ceiling. claude fails build ONLY on gpt-oss (5/5 build on all Qwen),
so it is gpt-oss-specific — our-bug vs ceiling still to be proven.
