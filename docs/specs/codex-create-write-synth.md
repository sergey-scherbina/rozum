# Spec — codex create-from-scratch: synthesize a real write from `{path, content}`

Status: done (2026-06-22)
Related: `docs/matrix-failure-analysis.md` Finding 5; `apply-patch-ws-fallback.md`,
`apply-patch-fn-decode.md`, `apply-patch-idempotent.md` (the sibling codex bridges).

## Problem

A weak model (gpt-oss-20b) asked to **create a file from scratch** (matrix `build`/`test` tasks)
routes a write-INTENT through the codex shell tool instead of emitting a patch. Captured via
`ROZUM_CODEX_TOOL_CAPTURE` on a real `codex × gpt-oss-20b × build` run — 10 of 11 tool calls were one
shape:

```json
{"cmd":"apply_patch","shell":"zsh","cmd":"apply_patch","path":"Cargo.toml",
 "content":"[package]\nname = \"reverse-cli\"\nversion = \"0.1.0\"\n..."}
```

`content` is a **valid whole file** (the model knows what to write), but it is expressed as
`exec_command` args carrying `{cmd:"apply_patch", path, content}` (note the duplicate `cmd`). codex's
`exec_command` understands only `{cmd:"<shell>"}`, so it runs `apply_patch` as a **bare shell command
with no patch** and silently drops `path`+`content` → the file never lands → `build` loops to the
600 s timeout (rc=143) and `test` fails (pass=0).

This is distinct from the *edit-existing-file* failures (handled by the apply_patch bridges): the
model isn't emitting a patch at all, so the patch path never sees it. It is **not** a model ceiling —
claude drives the SAME gpt-oss to pass `build` via its Write tool.

## Fix

Hook the existing `normalize_codex_tool_args` walker (the codex `/v1/responses` arg normalizer that
already folds `{cmd:apply_patch, patch-sibling}` → `patch --fuzz`). In the object arm, when the bare
`apply_patch` command has **no foldable patch sibling** but carries a `{path, content}` where
`content` is not a patch (no `*** Begin Patch` / `*** Update File`), synthesize the real write:

```
mkdir -p "$(dirname '<path>')" 2>/dev/null; cat > '<path>' <<'ROZUM_WRITE_EOF'
<content>
ROZUM_WRITE_EOF
```

- **Single-quoted heredoc delimiter** (`'ROZUM_WRITE_EOF'`) → the body lands byte-for-byte; no `$`,
  backtick or backslash expansion (a Rust/TOML file is full of these).
- **`mkdir -p` of the parent** → a nested target (`src/main.rs` into a fresh dir) doesn't fail on a
  missing directory.
- `content`'s `\uXXXX` escapes are decoded first (shared `decode_unicode_escapes`), matching the
  patch paths.
- `path` and `content` keys are removed after rewrite so codex sees a clean `exec_command`.

The result is structurally identical to the proven apply_patch-fold path (replace `cmd` with a shell
string codex runs verbatim), so it rides the same exec transport that is already e2e-proven for the
`fix` task.

### Guards (no false positives)

- Triggers only on `cmd == "apply_patch"` (the captured shape) — a normal `exec_command` is untouched.
- A real **patch** in `content` still folds to `patch --fuzz` (the synthesis returns `None` on patch
  content), never a raw write.
- A **path-only** call (no `content`) is left untouched — we never invent empty files.

## Validation

- Unit: `gateway::tests::synthesizes_file_write_from_path_and_content` — covers the create shape,
  nested-dir mkdir, patch-content-still-folds, and path-only-untouched.
- Shell-level e2e of the exact synthesized command: file lands, `mkdir` creates `src/`, body verbatim
  (`$HOME`/backticks stay literal).
- Full model-in-the-loop matrix cell (`codex × gpt-oss × build/test`): **deferred** — RAM was held by
  a concurrent GLM-4-32B matrix run; re-run expected to move codex × gpt-oss from 3/5 → 5/5 (mirrors
  codex × 27B going 5/5 once Method B landed).

## Knobs

None new for the synthesis. Reuses `ROZUM_CODEX_LEAN`, the apply_patch bridges, and
`ROZUM_CODEX_TOOL_CAPTURE` (for re-capturing shapes). The synthesis is unconditional on the codex
Responses path (same as the fold). **`ROZUM_GPTOSS_TOP_P=0.95`** is the recommended companion (see
below) — it does not change this code, it makes the model emit *coherent* create shapes.

## Update (2026-06-22) — patch-based create shapes + the temperature lever

The explicit `{path, content}` shape above is only ONE of the ways gpt-oss creates a file, and (per
real e2e capture) not the most common. gpt-oss runs at a forced temperature ≈1.0 (greedy collapses
its CoT into repetition loops — verified 0/6), so it is highly **stochastic**: across runs it emits
the create intent as several different shapes. Two more are now handled in `apply_patch_block_to_fuzz`
(the single chokepoint feeding the string-command, sibling-patch, and function-call paths):

- **`*** Add File:` / `*** Create File:` directive** — the canonical V4A create (`*** Create File:`
  is gpt-oss's variant). Content lines follow the directive (bare or `+`-prefixed). `parse_create_
  directives` extracts every such file (multi-file patches included) and writes each.
- **`*** Update File:` on an ABSENT file** — the model labels a new file an "Update" with a bogus
  `---` old-side; `patch` can't update a missing file → `.rej`, nothing lands. Detected by "additions
  present, no real removed/context content" and written from the `+` lines.

Both render through a shared `synth_create_command`:
`[ -e PATH ] || { mkdir -p "$(dirname PATH)"; cat > PATH <<'ROZUM_CREATE_EOF' … }` — verbatim,
**only when absent** (idempotent re-send, never clobbers a real edit). A genuine edit (real
removed/context lines) is byte-identical on the `patch --fuzz` path → the `fix` task is unaffected.

### The temperature lever (`ROZUM_GPTOSS_TOP_P`)

At the default `top_p=1.0`, a temp-1.0 draw periodically picks a junk token, so gpt-oss emits
*unparseable* shapes (broken JSON, `rg`-searching for the apply_patch tool, `echo | apply_patch`,
giving up after ~195K tokens) that no gateway rewrite can salvage — a genuine model-capability floor.
`ROZUM_GPTOSS_TOP_P=0.95` clips that low-probability tail while still sampling (no CoT loop), so the
model emits the coherent create shapes the gateway can land. The two together flipped
`codex × gpt-oss × test` 0→1 (first create-from-scratch green). Consider defaulting the gpt-oss
top_p clip on; `build` remains run-to-run flaky (model variance, not a gateway gap).

### Validation (this round)

- 3 unit tests: `create_file_directive_writes_new_file` (Create/Add, `+`-strip, multi-file),
  `create_patch_against_absent_file_writes_instead_of_patching`, and the original `{path,content}`.
- Shell e2e of the emitted create command in `zsh -lc`: lands, nests (`src/`), idempotent, and
  `cargo run -- hello → olleh`.
- Live matrix (`codex × gpt-oss`, `top_p=0.95`): `test` PASS (files landed, compiled, ran); `build`
  flaky across runs (model sometimes never emits a usable shape).
