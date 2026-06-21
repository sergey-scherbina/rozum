# Decode `\uXXXX` in the apply_patch function-call reroute

## Overview

gpt-oss is trained on the OpenAI function surface, so it emits `apply_patch` as a
**function call** (`{"command":["apply_patch","*** Begin Patch …"]}`), not a shell
command. The gateway re-routes that to an `exec_command` via
`rewrite_apply_patch_function_args`. gpt-oss JSON-double-escapes operators in the
patch body — `&`→`&`, `<`→`<`, `>`→`>` — and a Rust fix is full of
them (`&str`, `&arg`, `collect::<String>()`, `->`). The function-call reroute did
**not** decode these (the shell-command path, `normalize_codex_tool_args`,
already did), so they landed **literally** in the patched file
(`reverse(&arg)`, `collect::<String>()`) and broke compilation.
This is a primary, previously-misattributed source of the codex × gpt-oss
corruption — the agent-bisection tell: same model passes 5/5 under opencode/claude
(which don't take this path) but fails under codex.

## Interface

`rewrite_apply_patch_function_args(args) -> Option<String>` now calls
`decode_unicode_escapes` on the extracted patch before building the command —
identical to the two `normalize_codex_tool_args` paths. No signature change.

## Behavior

- [x] `&`/`<`/`>` in a function-call apply_patch body are decoded to
      `&`/`<`/`>` before the patch is applied (new unit test).
- [x] `collect::<String>()`, `&str`, `&arg`, `->` round-trip intact.
- [x] Clean patches (no escapes) are unchanged (existing test still passes).
- [x] `decode_unicode_escapes` only touches bare 4-hex `\uXXXX`, never Rust's own
      `\u{..}`, so source escapes are safe (existing invariant).

## Out of scope

- The model's *structural* over-editing (extra braces from churn) — capped by the
  loop-breaker, not this fix. This fix addresses only the operator mangling.

## Results

Unit: gateway suite 56/56 (new `apply_patch_function_decodes_unicode_escapes`
fails before the one-line decode, passes after).

E2e (codex × gpt-oss × fix, sandbox, ×5): the `\u00` corruption is **gone from
every file** and **4/5 now compile** (before, the function-path runs produced
garbage like `collect::<String>()`). The decode fix is correct and
removes a real failure class.

### Layered finding — this fix exposed the next one (read-repair)

With corruption gone, the dominant residual failure was the model **never reading
the file**: gpt-oss emits a malformed `sed -n "src/main.rs"` (no line range), it
errors, the model retries the same broken read and gives up without ever seeing
the code → no fix. `repair_broken_read` already translated this to `cat <file>`
but was **gated OFF** by default. This change flips `read_repair_enabled()` to
default-ON (`ROZUM_CODEX_READ_REPAIR=0` to disable). A/B (×5) with it on:
broken-read=0 across all runs — the model now reads via `cat`.

### Honest residual — model-side

Even with decode + read-repair + loop-breaker + `-N --forward` all correct, codex
× gpt-oss × fix stayed ~1/5: the pipeline is no longer the bottleneck. The model
applies the correct fix then **reverts it with a 2nd different patch** (a 2-edit
ping-pong, below the loop-breaker's ≥3 gate), or reads-but-doesn't-edit, or
doesn't engage — genuine gpt-oss-20b agentic-loop weakness (claude/opencode drive
the same model to 5/5). Two real gateway bugs were found and fixed here (decode,
read-repair-off); what remains is model quality, capped (not cured) by the
loop-breaker.
