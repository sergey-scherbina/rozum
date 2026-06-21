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

<!-- e2e codex × gpt-oss × fix before/after — fill after validation -->
