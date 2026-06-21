# Whitespace-tolerant apply fallback

## Overview

The codex × gpt-oss `fix` residual that *looked* like the model "reverting its own
fix" is actually a **failed patch**: gpt-oss drops the leading indentation on the
changed lines of its apply_patch body (`-s.to_string()` instead of
`-    s.to_string()`). BSD `patch` requires removed lines to match exactly —
`--fuzz` only relaxes *context*, not changed lines — and `--ignore-whitespace`
does **not** compensate for the missing leading whitespace (verified: it fails
with correct line number and with context). `git apply --ignore-whitespace` is
even stricter. So the hunk fails to a `.rej`, the fix never lands, and the file
stays buggy. ("patching file" prints even on hunk failure, which is what made it
look like a revert.)

Add a fallback: when `patch` leaves a `.rej`, a tiny static python helper re-applies
the rejected hunk by **trimmed** matching (ignoring leading whitespace and the line
number) and re-indents the replacement to the file's own indentation.

## Interface

`apply_patch_block_to_fuzz` appends, after the `patch` heredoc:

```sh
f=<path>; if [ -f "$f.rej" ]; then python3 - "$f" <<'ROZUM_PY_EOF'
<static helper>
ROZUM_PY_EOF
rm -f "$f.rej" "$f.orig"; fi
```

The helper (`PATCH_WS_FALLBACK_PY`) is fully static — only the file path is dynamic
(argv) — so nothing in the patch content needs shell-escaping. It reads `<file>.rej`,
collects `-`/`+` lines, finds the removed block by `str.strip()` comparison, and
replaces it with the added lines prefixed by the matched line's leading whitespace.

## Behavior

- [ ] A patch that applies cleanly is unaffected (no `.rej` → helper never runs).
- [ ] A patch whose removed line lost its indentation (`-s.to_string()` vs file
      `    s.to_string()`) lands via the fallback, preserving the file's indentation.
- [ ] The fallback is location-independent (wrong `@@` line number is fine).
- [ ] `.rej`/`.orig` are cleaned up so the model doesn't see stale reject files
      (which previously confused it into path-flailing).
- [ ] No match found → file left as `patch` left it (no worse than before).

## Out of scope

- Multi-hunk `.rej` files: the helper treats all `-`/`+` lines as one block
  (best-effort; the dominant gpt-oss fix is a single hunk).
- Ambiguous matches (the removed block occurs more than once): takes the first.
- Making gpt-oss emit correct patches — this translates its sloppy output, the same
  "understand & translate the model's intent" principle as the decode + read-repair
  fixes.

## Results

Isolated (no model/agent): the model's actual failing patch (`-s.to_string()` with no
indent, `@@ -1,1`) → `patch` fails to `.rej` → python fallback applies at the real line
with `    ` indent → `cargo run -- hello` → `olleh`. Confirmed through a real
`zsh -lc "…"` (how codex executes the command).

Unit: gateway suite 57/57, incl. `ws_fallback_lands_a_patch_whose_removed_line_lost_its_indent`
(runs the generated command on a seeded file, asserts the fix lands with preserved indent).

E2e (codex × gpt-oss-20b × fix, sandbox, ×6): **pass 5/6** (was ~1–2/5), and the
`[rozum-apply] whitespace-tolerant fallback applied` marker fired in **all 6** runs
(1–8× each) — the fallback is the active mechanism that lands gpt-oss's
indentation-dropped patches. The one miss (rep2) was a non-compiling file unrelated to
whitespace. Checkboxes covered.
