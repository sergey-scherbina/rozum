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
- `scripts/bench/agentic.sh`
  - keeps its current benchmark contract.
  - when a cell fails, it may print the triage class for the kept workdir if the triage script is
    available.
  - `repair_diagnostic` may include bounded source/manifest snapshots and targeted manifest/edit
    guidance, but it must not mutate files itself.

## Behavior

- [ ] A kept workdir with missing `Cargo.toml` or no `src/*.rs` is classified as delivery/setup, not
      reasoning.
- [ ] A workdir with root-level `.rs` files while `src/main.rs` is missing or still a stub is classified
      as wrong-entrypoint delivery.
- [ ] A Cargo unsupported-edition manifest error is classified as `manifest_invalid` and includes the
      offending `Cargo.toml` path.
- [ ] An agent log containing `File has not been read yet` after an `Edit` call is classified as
      `edit_requires_read`.
- [ ] An agent log containing `String to replace not found` after an `Edit` call is classified as
      `edit_old_string_miss`.
- [ ] An agent log where the last tool result is an error and the assistant then claims success is
      classified as `false_success_after_error`.
- [ ] A workdir that compiles but fails the task's verifier with wrong output is classified separately
      from source/setup delivery, so it remains model-quality evidence unless another delivery signal is
      present.
- [ ] `agentic_triage.py --json` and `--csv` produce stable machine-readable output.
- [ ] `agentic_triage.py --brief` produces a short human-readable summary suitable for failed bench
      cells.
- [ ] `agentic.sh` repair diagnostics include source/manifest context without dumping large generated
      targets or build artifacts.

## Out of scope

- Automatically editing model-created workdirs from the triage script.
- Marking a model as "good" or "bad" from one run. The triage script classifies failure shape only.
- Loading large models or running the full matrix as part of the triage implementation.
- Building model-specific patchers that silently fix a benchmark cell behind the agent's back.

## Design

The classifier is intentionally heuristic and evidence-first. It reads only local artifacts that already
exist after a run: `agent.log`, `Cargo.toml`, `src/*.rs`, `run.err`, `cargo.err`, and `per-run.csv` when a
result directory is supplied. Classification is ordered from concrete delivery signals to broader model
quality signals:

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

Pending implementation.
