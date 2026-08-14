# Agentic Delivery Hardening

## Overview

Agentic benchmark failures must be classified before they are used as model-quality evidence. A red cell
can mean real reasoning weakness, but it can also mean a delivery failure: files written to the wrong
place, tool calls malformed or refused, `Edit` attempted without a required `Read`, shell quoting
corrupting source code, a broken manifest, or a false success after a failed command. This feature makes
that distinction cheap and repeatable across models, then applies only model-agnostic repair guidance to
the delivery-shaped cases.

## Interface

- `scripts/bench/agentic_triage.py [PATH ...]`
  - `PATH` may be an `agentic.sh` result directory, a kept workdir, or an `agent.log`.
  - default output: a compact text table.
  - `--brief` writes a single-line `class: reason` summary for the first classified run.
  - `--json` writes a JSON array with one object per run.
  - `--csv` writes CSV with stable columns.
  - `--root <repo>` optionally sets the repo root for relative result paths.
- `scripts/bench/agentic.sh` — **structured exit codes**, one per cell, written to `agentic.meta`
  and the `rc` column of `per-run.csv`. This table is the contract; every reader below quotes it.

  | rc | meaning | is it model-quality evidence? |
  |---|---|---|
  | `0` | verify PASS | yes |
  | `2` | infra failure (gateway crash / `clients_gone`) | no |
  | `10` | verify FAIL — the agent delivered a complete project and it is wrong | **yes** — capability miss |
  | `11` | no project files written at all | no — delivery failure |
  | `12` | manifest present, no `src/*.rs` — cargo has no build target | no — PARTIAL delivery |
  | `13` | the workdir came back byte-identical to what `setup_task` seeded | no — nothing changed |
  | `124` | `RUN_TIMEOUT` fired | no |
  | other | agent error (tool error, signal, …), passed through | no |

  `11`, `12` and `13` are counted separately, never summed: "wrote nothing", "wrote half" and
  "changed nothing it was given" point at different fixes. `greet` is exempt from the file checks —
  it writes no files by design. The codes are decided by `classify_rc`, which is a function
  precisely so `scripts/bench/test-classify-rc.sh` can execute the rule without a model, a gateway
  or ten minutes.

  **`13` says what was MEASURED, not what it means.** Byte-identical to the seed is not the same as
  "the agent did nothing": a repair that edits a file and reverts it ends byte-identical too, and so
  does one whose writes we lost. The code marks the cell as not-a-capability-signal and sends the
  reader to the log; it does not name a culprit. The before/after is `.rozum-seed`, a sha256 of
  every file `setup_task` wrote, kept in the workdir beside `agentic.meta` — tampering with it makes
  the answer "cannot say", which degrades to `10`; nothing about it can manufacture a `13`.
- `scripts/bench/agentic.sh`
  - keeps its current benchmark contract.
  - writes `agentic.meta` and `verify.out` into each workdir so local failure triage has task/result
    context without parsing terminal output.
  - when a cell fails, it may print the triage class for the kept workdir if the triage script is
    available.
  - `repair_diagnostic` may include bounded source/manifest snapshots and targeted manifest/edit
    guidance, but it must not mutate files itself.
  - repair prompts restate the task-specific acceptance criterion, so a repair does not stop at
    "the project compiles" when the verifier requires `cargo run`/`cargo test` behavior.

## Behavior

- [x] A kept workdir with missing `Cargo.toml` or no `src/*.rs` is classified as delivery/setup, not
      reasoning.
- [x] A workdir with root-level `.rs` files while `src/main.rs` is missing or still a stub is classified
      as wrong-entrypoint delivery.
- [x] A Cargo unsupported-edition manifest error is classified as `manifest_invalid` and includes the
      offending `Cargo.toml` path.
- [x] An agent log containing `File has not been read yet` after an `Edit` call is classified as
      `edit_requires_read`.
- [x] An agent log containing `String to replace not found` after an `Edit` call is classified as
      `edit_old_string_miss`.
- [x] Prompt text that merely mentions `File has not been read yet` or `String to replace not found`
      is not treated as a real edit-tool failure unless it appears in a tool-error result.
- [x] An agent log where the last tool result is an error and the assistant then claims success is
      classified as `false_success_after_error`.
- [x] A workdir that compiles but fails the task's verifier with wrong output is classified separately
      from source/setup delivery, so it remains model-quality evidence unless another delivery signal is
      present.
- [x] The EXIT CODE makes the same distinction the triage does. A cell with `Cargo.toml` and no
      `src/*.rs` reports `rc=12`, distinguishably from `rc=11` (nothing written) and from `rc=10`
      (a complete project that is wrong) — the collapse that let partial delivery be read as a
      capability miss on every board entry quoting rc=10. Root-level `.rs` with no `src/` counts as
      partial too: source cargo cannot see is not delivered.
- [x] A SEEDED task reports non-delivery too. `fix`, `debug` and `multibug` start with a manifest
      and a source file because the harness wrote them, so `11` and `12` cannot fire there and an
      agent that changes nothing scored `10`. `setup_task` records `.rozum-seed`; a workdir that
      comes back byte-identical — seeded files unchanged AND nothing added — reports `13`.
- [x] The rule is EXECUTED, not asserted: `scripts/bench/test-classify-rc.sh` drives `classify_rc`
      over the workdir shapes — including each precedence pair (timeout over partial, infra over
      pass, pass over untouched) and both `greet` cases — in under a second, with no model and no
      network. The seeded cases are driven through `setup_task` ITSELF, not a copy of it in the
      test, so the baseline cannot drift from the harness's.
- [x] Every reader of the code agrees: `summarize_matrix.py` and its byte-identical ScalaScript twin
      `summarize_matrix.ssc` both bucket `partial`, and the matrix console gives `12` its own glyph
      (`◐`) and label in all three languages rather than letting it render as `✗`.
- [x] `agentic_triage.py --json` and `--csv` produce stable machine-readable output.
- [x] `agentic_triage.py --brief` produces a short human-readable summary suitable for failed bench
      cells.
- [x] `agentic.sh` repair diagnostics include source/manifest context without dumping large generated
      targets or build artifacts.

## Out of scope

- Automatically editing model-created workdirs from the triage script.
- Marking a model as "good" or "bad" from one run. The triage script classifies failure shape only.
- Loading large models or running the full matrix as part of the triage implementation.
- Building model-specific patchers that silently fix a benchmark cell behind the agent's back.

## Design

The classifier is intentionally heuristic and evidence-first. It reads only local artifacts that already
exist after a run: `agentic.meta`, `verify.out`, `agent.log`, `Cargo.toml`, `src/*.rs`, `run.err`,
`cargo.err`, and `per-run.csv` when a result directory is supplied. Classification is ordered from
concrete delivery signals to broader model quality signals:

1. missing project/source files;
2. wrong entrypoint/path;
3. manifest parse errors;
4. edit/read tool protocol failures;
5. false-success-after-error;
6. shell/source corruption patterns;
7. verifier mismatch / compile failure without a delivery signal;
8. timeout/no-progress;
9. unknown.

`agentic.sh` remains the runner and verifier. It should call the triage script only after verification
has failed, and only as a diagnostic. Repair prompts can use the same bounded source snapshot principles
as `rozum launch`: enough current state for the next attempt to ground itself, no large logs or target
tree dumps, no hidden mutation.

## Decisions

- **Classify before capability verdicts** — chosen because prior matrix work repeatedly showed that
  apparent model weakness was delivery-shaped. Rejected: treating every red cell as model quality,
  because it causes wrong recommendations.
- **Script first, not in-gateway state** — chosen because existing `KEEP=1` artifacts are enough for
  most diagnoses and do not require loading models. Rejected: database-backed telemetry, too heavy for
  a bench debugging loop.
- **Repair prompts, not hidden file mutation** — chosen because the benchmark should still measure the
  agent/model loop. Rejected: a sanitizer that patches generated Rust before verification, because that
  would make the matrix dishonest.

## Results

Implemented in `scripts/bench/agentic_triage.py` and the failed-cell path in `scripts/bench/agentic.sh`.
Validation stayed local and low-load: shell/Python syntax checks, real old GLM kept workdirs
(`/tmp/rozum-agentic-3rUFul` -> `edit_requires_read`, `/tmp/rozum-agentic-tZPNC7` ->
`manifest_invalid`), a real result CSV (`glm47-flash-rpn-20260629-164229` -> failed row with
`unknown_failed` because legacy CSVs do not record kept workdir paths), and synthetic fixtures covering
missing project files, wrong entrypoint, old-string miss, false success after tool error, source syntax
artifact, verifier mismatch, and pass.

Follow-up green sweep added task-specific repair goal hints after Qwen2.5-Coder-7B repaired a compile
failure into a generic Hello World project. With the hint, the same `build` cell passed. The triage
heuristic was also narrowed so stale or prompt-only edit/manifest wording does not override the final
verifier evidence.

Follow-up sectioned artifact synth fixed the remaining Qwen2.5-Coder-7B `rpn` delivery red:
first-line filename labels inside fenced artifacts (`# Cargo.toml`, `// src/main.rs`) are now split or
stripped before the full-program fallback. Live `claude × Qwen2.5-Coder-7B-Instruct-4bit × rpn` with
`ROZUM_ARTIFACT_SYNTH=1`, `NCTX=8192` passed 1/1 in 21.0s (turns=4, tools=2, repairs=0).

Follow-up Qwen3-4B repair hardening closed two more delivery-shaped reds. The repair diagnostic now
pulls same-run Read-before-Edit failures from `agent.log`, tells weak agents not to stop after merely
saying they need to read, permits Bash heredoc/python exact replacement for tiny files, and emits a
canonical `[package]` Cargo.toml snippet when the manifest is missing or malformed. Targeted validation:
Qwen3-4B `fix` and `test` went 0/1 -> 1/1 with `REPAIR=1`; together with baseline `build/rpn/debug`
passes, Qwen3-4B now has single-run green evidence across all five bench tasks, albeit slowly on `rpn`.
