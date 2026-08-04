# Benchmark history

**Append, never rewrite.** A history corrected in place cannot show a regression that was
introduced and then hidden. One run is a hypothesis (`performance` skill §1.1); a row here is
only comparable to another row that states the same conditions.

Each entry: what was measured, the arms, the conditions, **the expectation written before the
result**, then the result and what it did to the expectation.

---

## 2026-08-04 · Does nadia's verify gate change the pass rate, and what does it cost?

**Arms** (alternating per task, A then B, one agent at a time — never two blocks):

| arm | env | meaning |
|---|---|---|
| A | `ROZUM_VERIFY=0 NADIA_VERIFY=0` | no gate at all |
| B | `ROZUM_VERIFY=0 NADIA_VERIFY=1` | nadia's gate alone — the phone path |

`ROZUM_VERIFY=0` in both: `rozum launch`'s own gate is off, so the variable really is nadia's
gate. Without that the arms would compare one gate against two (fixed in `a88fbea`, found while
designing this).

**Conditions.** `mlx-community:Qwen3.5-4B-MLX-4bit`, the machine's only model, resident on :8089;
harness `scripts/bench/agentic.sh`, 8 tasks × 1 rep × 2 arms, `RUN_TIMEOUT=420` in BOTH arms,
`BENCH_PORT_BASE=9300`, `REPAIR=0` (the harness's own repair off — the gate is the thing under
test). Binaries from the `feature/gate-matrix` worktree, not the installed ones. The machine also
runs the live gateway, the UCC and two Telegram bridges — a real desktop, not a quiet lab; that is
why the arms alternate.

**Expectation, written before the result:**

- `greet` — the gate should do NOTHING. "Reply with the word pong" has no machine-checkable
  criterion, `checkable:false` is the correct answer, and if the gate touches this task at all
  something is wrong.
- `wordcount`, `rpn` — the tasks that state an exact expected output are where a derived check can
  bite. If the gate buys anything, it buys it here.
- `build`, `fix`, `test`, `debug` — a cargo floor exists either way; I expect little movement,
  because the harness's own verification already fails a broken build and the model's prompt
  already tells it to run the build.
- `multibug` — genuinely uncertain. Repair rounds could help, or could burn the wall-clock budget.
- **Pass rate:** I expect 0–2 cells of improvement out of 8, most likely on `wordcount`. I do NOT
  expect the gate to fix a task the model cannot do.
- **Cost:** +1 model call per task for the derivation (~3–10 s on this model), plus a full extra
  turn per repair round. On a task that passes first try the gate should cost only the derivation.
- **The result I would find most useful is a null one**: if the gate changes nothing on the
  matrix, that says it is insurance for the phone path rather than a scoring improvement, and it
  should be described that way instead of as a win.

**Result:** _(appended when the run finishes)_
