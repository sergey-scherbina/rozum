# Loop-breaker: edit-churn signature

## Overview

The gateway's `detect_stuck_loop` already breaks two agentic loop shapes, but
both require *identical* repetition: (1) N byte-identical *failing* tool calls,
(2) N identical assistant texts. A weak model (gpt-oss-20b) under codex/opencode
exhibits a third shape these miss: it keeps re-editing the **same file** with
**different, mostly-succeeding** patches — toggling equivalent forms
(`collect()` ↔ `collect::<String>()`), re-anchoring on stale context — making no
net progress. Because the patches differ and succeed, neither existing signature
fires. The churn runs to `RUN_TIMEOUT`, and along the way fuzzy-applied patches
corrupt the file (duplicated lines, unbalanced braces) so it no longer compiles
→ `pass=0` even though a correct fix had already landed early.

Add a third signature: **edit-churn / ping-pong** — detect that the model is
undoing and redoing its own edits to one file and stop early, preserving the
earlier good state and saving the wall-clock to timeout.

## Interface

`detect_stuck_loop(messages) -> Option<String>` gains a third branch (no public
signature change; same call site `chat_or_loopbreak`). It fires the existing
`synthetic_stop_stream` with a churn-specific reason.

A helper extracts, from each `ContentBlock::ToolUse` input, the edit target and
its changed lines:

```
edit_target_and_lines(&Value) -> Option<(file: String, removed: Vec<String>, added: Vec<String>)>
```

It is shape-agnostic: it stringifies the input and scans for a patch envelope
(`*** Update File:` / `---`+`+++`) plus `+`/`-` body lines, so it works whether
the call is an `apply_patch` function, a `{patch: …}` arg, or a rewritten
`patch -p0 …` heredoc.

## Behavior

- [ ] Fires when one file is edited ≥ 3 times AND a ping-pong occurred (an added
      line re-introduces a line a previous edit to that file removed).
- [ ] Backstop: fires when one file is edited ≥ 6 times regardless of ping-pong.
- [ ] Does NOT fire on a healthy linear fix (1–2 edits, each moving forward, no
      re-adding of removed content) — verified by the 35B runs (1–2 edits) and a
      negative unit test.
- [ ] Compares normalized code content (strip leading `+`/`-` and surrounding
      whitespace), so `collect()` vs `collect::<String>()` are distinct lines but
      a re-added identical line is caught.
- [ ] The synthetic stop tells the model it has been editing one file in circles,
      the fix is likely already applied, and to stop and report.

## Out of scope

- Knowing whether the *current* file state is correct (task-agnostic; the gateway
  cannot compile/verify). The signature stops the churn; it does not guarantee
  the frozen state compiles — it only makes that far more likely by firing before
  the corruption accumulates.
- The `--fuzz` leniency that lets misanchored patches corrupt the file. Tracked
  separately (a fuzz A/B is the planned follow-up); this signature is the primary
  lever.

## Design

Walk `messages` in order, collecting `(file, removed, added)` per edit tool-call.
Maintain per-file: a running set of normalized removed lines, an edit count, and
a ping-pong flag (set when an added line is already in that file's removed set).
After the walk, fire if any file hits `(count ≥ 3 && ping_pong)` or `count ≥ 6`.
O(total patch lines); runs once per request, same place as the existing two
signatures.

## Decisions

- **Require ping-pong + a count gate, not raw edit-count** — chosen so healthy
  iterative work (which edits forward, never re-adding removed lines) doesn't trip
  it; raw "N edits to one file" would risk false positives in general coding.
  The ≥6 backstop covers non-ping-pong churn while staying well above any healthy
  single-fix task. Rejected: a low raw edit-count threshold (false-positive risk).
- **Stop early over repair** — chosen because stopping is task-agnostic and safe;
  the gateway can't know which churned state is correct. Rejected: snapshot/restore
  the first good file (needs task knowledge + a verify oracle).

## Results

<!-- fill after validate: unit tests; e2e codex×gpt-oss×fix before/after -->
