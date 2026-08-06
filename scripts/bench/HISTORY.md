# Benchmark history

## The repair prompt does NOT cause the re-run loop (2026-08-06) — null result, nothing shipped

Both measured `wordcount` failures ended with the loop-breaker: `bash` called four times with
identical arguments, i.e. the model re-running the build instead of editing. The repair prompt we
send at that moment ends with *"do not report success until you have run the check yourself"* —
which for a 4B model is the easiest instruction in it to obey. Hypothesis: our own wording feeds
the loop.

Controlled A/B on a fixture — the real workspace from a failed cell, with its actual
`E0277: Vec<(String, usize)> cannot be built from an iterator over (&String, &usize)`. Same broken
code, same task, alternating arms, five each. Only the wording differs:

| arm | wording | fixed the code | loop-breaker fired | mean |
|---|---|---|---|---|
| A | today's ("…run the check yourself") | **5/5** | 0 | **44 s** |
| B | "EDIT THE FILES … running it again without changing anything cannot change the result" | **5/5** | 0 | **121 s** |

**Refuted, and the alternative is 2.7× slower for no gain, so nothing was changed.**

What it does explain: the loops we saw were the symptom of an IMPOSSIBLE check, not of the wording.
Those runs were fighting the fabricated expectation of BUG-026 — no edit could ever satisfy it, so
the model kept re-running until the loop-breaker stopped it. Give the same model a check it *can*
satisfy and it edits, every time, under either wording.


## nadia × Qwen3.5-4B, 8 tasks × 3 reps (2026-08-06)

**23 / 24.** Seven tasks 3/3; `wordcount` 2/3.

| task | | task | |
|---|---|---|---|
| greet | 3/3 · 6 s | debug | 3/3 · 66 s |
| build | 3/3 · 82 s | rpn | 3/3 · 83 s |
| fix | 3/3 · 44 s | **wordcount** | **2/3** · 94 s |
| test | 3/3 · 84 s | multibug | 3/3 · 103 s |

**This run used a two-day-old `nadia`**, and that is the finding, not a footnote. The BUG-026 fix
(an `expect` the task never states is not a criterion) had been installed by hand as
`cp target/release/nadia … || cargo build …`; the `cp` succeeded on a stale binary, so the `||`
never ran. Every cell above therefore measured the OLD gate, which still derived
`a 3 / c 2 / d 2` for `wordcount` and spent both repair rounds fighting it.

**And the single-rep 8/8 the day before did not prove the fix either** — for a second reason worth
keeping: the harness's verdict is INDEPENDENT of the agent's gate. It runs its own verifier, so a
cell passes when the code ends up correct even if the agent's own check failed. A green matrix cell
says nothing about the gate; only the gate's own lines do.

Measured properly afterwards, same task, same model, on the binary that actually carries the fix —
and reading the derived check, not just the verdict:

| | derived check | passing |
|---|---|---|
| old binary (bench ×5) | `cargo build && … 'a 3\nc 2\nd 2'` (invented) | 3 / 5 |
| fixed binary (direct ×7) | `cargo build -q` | **7 / 7**, all printing `apple 3 / banana 3 / cherry 2` |

**Method note, twice earned:** `KEEP=1` or the failing cell's log is gone, and the whole reason
this took three passes is that the first two threw the evidence away. The rule was already written
in this file yesterday, by me, and not followed today.


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

**Result** (`/tmp/gate-ab-20260804-164051`, 16 cells, alternating, one agent at a time):

| task | A: no gate | B: nadia's gate |
|---|---|---|
| greet | 1.1 s **pass** | 3.4 s **pass** |
| build | 40.0 s **pass** | 43.1 s **pass** |
| fix | 44.2 s **pass** | 36.3 s **pass** |
| test | 57.0 s **pass** | 58.6 s **pass** |
| debug | 38.3 s **pass** | 23.4 s **pass** |
| rpn | 148.6 s **pass** | 57.7 s **pass** |
| wordcount | 49.0 s fail | 55.8 s fail |
| multibug | 24.3 s fail | 108.5 s **pass** |
| **pass rate** | **6 / 8** | **7 / 8** |
| **wall clock** | 514 s | 495 s |

**What the prediction got right, and wrong.** Size right (I said 0–2 cells; one moved). `greet`
right: the gate correctly did nothing, `checkable:false`, and cost 2.3 s for asking. `wordcount`
**wrong** — I named it as the likely gain and it failed in both arms, with a different compile
error each time (`E0308`, `E0425`): this model cannot do that task, and the gate does not make it
able to. The cell that moved was `multibug`, which I had listed as genuinely uncertain.

**Then I checked the mechanism, and it changed the conclusion.** The harness discards the agent
log without `KEEP=1`, so "the gate repaired it" was an inference from 24 s → 108 s, not evidence.
Re-running that cell gated with the log kept: **it passed again (102.3 s) with ZERO repair
rounds** — the gate derived `cargo build -q && cargo test -q`, ran it, and it was green first try.
So the gate's repair machinery is not what made that cell pass; the most likely explanation is
model variance between a 24 s run that stopped early and a 100 s run that did the work.

**Conclusion, stated as the evidence supports it: no demonstrated effect on the pass rate.** One
cell of 8 moved at one repetition, and a follow-up of that cell showed the gate passing without
needing to repair anything. That is the null result I said before the run would be the most useful
outcome, and it means what I said it would mean: **the gate is insurance, not a score** — it tells
you whether the work is right, and on the matrix the harness was already doing that. Its value is
where nothing else checks: the phone, where the alternative to the gate is a model's own word.

Costs measured: +2.3 s on a task with no checkable criterion (the derivation is asked for and
declined), and no total wall-clock penalty across the eight (495 s vs 514 s — inside the noise of
a desktop running four other services).

**What would answer it properly:** ≥3 repetitions per arm, alternating, ~1.5–2 h of machine time,
and `KEEP=1` so every cell's gate lines survive. One repetition is a hypothesis; this row is a
hypothesis with its mechanism checked, which is why the conclusion is "not demonstrated" rather
than "no effect".

