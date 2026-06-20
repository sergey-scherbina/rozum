# apply_patch idempotency (`-N --forward`)

## Overview

When the gateway bridges a codex `apply_patch` into standard `patch` tooling
(Method B), the generated command must be **idempotent**: re-submitting an
already-applied patch must be a no-op, never a revert. A weak local model
(gpt-oss-20b) flails — after a fix has already landed it keeps re-emitting the
*same* patch. Without `-N`, `patch` treats the second application as a reversal
("Reversed (or previously applied) patch detected!  Assume -R? [y]") and, with
no tty, assumes yes → it **undoes the fix and restores the bug**. The file then
oscillates fixed↔buggy across the model's retries; whichever state the run
timeout freezes decides pass/fail. Observed as a coin-flip `pass=0`/`pass=1` on
the `fix` task for codex × gpt-oss.

## Interface

`apply_patch_block_to_fuzz()` (and therefore both the apply_patch shell-command
bridge and the apply_patch-function re-route) emit:

```
patch -p0 --fuzz=3 -N --forward <<'ROZUM_PATCH_EOF'
…unified diff…
ROZUM_PATCH_EOF
```

`-N` / `--forward`: ignore a patch whose change is already present (or reversed)
instead of asking to reverse it.

## Behavior

- [x] First application of a fix patch lands normally (`patching file …`).
- [x] Re-submitting the **same** patch is ignored, not reversed — the fix stays
      in place (`Ignoring previously applied (or reversed) patch.`).
- [x] A genuinely different patch still applies normally (the flag only guards
      already-applied/reversed hunks).
- [x] Final repo state is deterministic regardless of how many times the model
      re-submits, so `verify_task`'s file-state check is stable.

## Out of scope

- **Model non-termination.** This fix makes the *outcome* deterministic
  (`pass=1`), but gpt-oss still over-edits and loops on its own malformed tool
  calls (`*** Update file:` lowercase header, bare `apply_patch` → "Usage"),
  so the codex × gpt-oss `fix` run still burns wall-time to `RUN_TIMEOUT`
  (rc=143). Stopping the loop is a separate lever (loop-breaker / prompt /
  accept the cosmetic timeout). Tracked separately.
- Patch *header* leniency (lowercase/extra-star markers). Different shape, not
  addressed here.

## Decisions

- **`-N --forward` over a no-op guard in the wrapper** — chosen because it is
  the native, well-defined `patch` semantic for "already applied"; it needs no
  state tracking and cannot itself corrupt a file. Rejected: pre-checking
  whether the hunk is already present (fragile, re-implements `patch`'s own
  detection).
- **Do not broaden payload extraction** — an earlier schema-agnostic
  `find_patch_payload` fold (grab the patch from any field) regressed: it landed
  bad/no-op patches and `pass` dropped to 0 across the board. This fix touches
  only the *apply* semantics, not *where the patch comes from*, so it carries no
  such risk.

## Results

Isolated reproduction (no model, no agent — pure `patch` on a seeded file):

| command | pass 1 | pass 2 (same patch re-sent) |
|---|---|---|
| `patch -p0 --fuzz=3` (before) | fix lands | **Assume -R? → reverted → bug back** |
| `patch -p0 --fuzz=3 -N --forward` (after) | fix lands | Ignoring previously applied → **fix sticks** |

Unit: `src/gateway.rs` gateway suite 52/52 green (assertions updated to expect
the `-N --forward` flags).

E2e (codex × gpt-oss-20b × fix, Seatbelt sandbox, ×3, `BENCH_BIN`=this build):

| build | pass | "Assume -R" reverts | "Ignoring" (no-op) | final file |
|---|---|---|---|---|
| before (`--fuzz=3`) | **1 / 0 / 0** (coin-flip) | fired | — | oscillates; one run BUGGY(reverted) |
| after (`-N --forward`) | **1 / 1 / 1** | **0 across all 3** | 0 / 3 / 1 | FIXED+clean, compiles, prints `olleh` |

The "Ignoring" count (3 and 1 in the two flailing runs) confirms the model *did*
re-send already-applied patches — the exact path that used to revert — and they
were now no-op'd. rep2 still hit `RUN_TIMEOUT` (rc=143, model non-termination,
out of scope) but stayed `pass=1`: the outcome is decoupled from the model's
inability to stop.

Note: the initial e2e was *invalid* and caught — `scripts/bench/agentic.sh`
resolves the gateway from `BENCH_BIN` (default = main-repo `target/release`), not
`PATH`, so it silently exercised the old binary. Re-run with `BENCH_BIN` pointed
at this build. (Lesson folded into the `isolate` skill: verify the binary under
test is actually the one running.)
