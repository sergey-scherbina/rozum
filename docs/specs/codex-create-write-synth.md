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

None new. Reuses `ROZUM_CODEX_LEAN`, the apply_patch bridges, and `ROZUM_CODEX_TOOL_CAPTURE` (for
re-capturing shapes). The synthesis is unconditional on the codex Responses path (same as the fold).
