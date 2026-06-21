# Tunable patch `--fuzz` (`ROZUM_PATCH_FUZZ`) + A/B result

## Overview

The Method-B apply_patch bridge applies the model's patch with `patch -p0
--fuzz=3 -N --forward`. `--fuzz` is the context slack `patch` tolerates when
locating a hunk: higher lands a slightly-off-context patch but can mis-place a
stale-anchored one; lower is stricter. This makes the value an env knob
(`ROZUM_PATCH_FUZZ`, default 3, clamped 0..=3) so it can be A/B'd and so an
operator on a different `patch` can tune it. **The default is unchanged (3).**

## Interface

`ROZUM_PATCH_FUZZ` — integer 0..=3, read by `patch_fuzz()`; default 3. Affects
only the Method-B `patch` command the gateway builds.

## Behavior

- [x] Unset → `--fuzz=3` (the existing default; assertions unchanged).
- [x] Set to 0..=3 → that value; >3 clamps to 3; non-numeric → 3.
- [x] Affects only the Method-B path (codex-native V4A apply_patch is unaffected).

## Out of scope

- Changing the default. The A/B below says it wouldn't help.

## Results — A/B is NEGATIVE

codex × gpt-oss-20b × fix, sandbox, loop-breaker on, 3 reps per value:

| `ROZUM_PATCH_FUZZ` | pass | file compiles |
|---|---|---|
| 3 (default) | 1/3 | 2/3 |
| 1 | 2/3 | 2/3 |
| 0 | 1/3 | 2/3 |

No real difference — pass varies within gpt-oss noise at n=3, `compiles` is 2/3
across all values. **Why fuzz can't help:** the corruption is *content-level, not
position-level*. At `--fuzz=0` (zero slack) patches still applied (`patched=4`)
and still broke the file — a run ended with a correct reversal body but an extra
unbalanced `}` (`error: unexpected closing delimiter`). gpt-oss emits
structurally-malformed patch *bodies* (duplicated `fn` signature, extra braces)
that apply cleanly at the right location; `--fuzz` only governs *where* a hunk
lands, never whether its content is sane.

Conclusion: keep the default at 3; the knob stays as a diagnostic/escape hatch.
The residual codex × gpt-oss failure is a model-quality limit (gpt-oss-20b on
codex's apply_patch surface emits bad patch bodies); the loop-breaker
([[loopbreak-edit-churn]]) already caps the damage by stopping the churn. claude
and opencode drive gpt-oss to 5/5; codex × gpt-oss stays the weak combo. A
gateway-side "reject a patch that fails to compile" lever is possible but
task-specific (needs a compiler) and out of scope here.
