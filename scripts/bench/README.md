# Benchmarks

Everything here measures the stack **as a user runs it** — a real `rozum launch <agent>`
against a real local model, sandbox and all — rather than a model in isolation. That is
the point and also the cost: a cell is slow, and a number is only worth something if you
can say what it was measured against.

Before changing anything on the strength of a number here, read the `performance` skill
(`vendor/agent-plugins/performance/`). Its core rule governs this directory: **one
measurement is a hypothesis, not a result.**

---

## The agentic matrix — `agentic.sh`

Drives `rozum launch claude` / `rozum launch codex` / `nadia` against a local model over a
ladder of tasks, and records wall time, the agent tree's peak RAM, peak CPU, and pass/fail
per cell. The model is loaded **once** per model (a shared `rozum gateway`, under
`/usr/bin/time -l` for the resident footprint); every task then reuses that resident model,
so no cell pays a reload.

```bash
scripts/bench/agentic.sh                                    # defaults
AGENTS=nadia TASKS="greet build" REPS=3 scripts/bench/agentic.sh
AGENTIC_MODELS="mlx-community:Qwen3.5-4B-MLX-4bit" AGENTS=claude scripts/bench/agentic.sh
```

### The task ladder

Increasing difficulty, each with a **deterministic** verifier — the agent does not get to
declare success:

| Task | What it asks | How it is judged |
|---|---|---|
| `greet` | reply with one word, no tools | output contains `pong` |
| `build` | create `reverse-cli`, run it | `cargo run -- hello` == `olleh` |
| `fix` | find and edit a one-line bug | `cargo run -- hello` == `olleh` |
| `test` | implement reverse + a `#[test]` | `cargo test` green **and** run == `olleh` |
| `debug` | failing test, run-read-fix loop | `cargo test` green |

`tasks.json` carries more beyond the ladder — `rpn`, `wordcount`, `multibug`, `leapday`,
`apportion`, `duration`, `board`. `TASKS=` selects any subset; `ROZUM_BENCH_TASKS` points
at a different task file.

### Two timeouts, deliberately independent

- `ROZUM_GEN_TIMEOUT_SECS` (default 180) bounds **one model request** inside the gateway.
  A wedged generation aborts and the agent loop carries on.
- `RUN_TIMEOUT` (default 1200) bounds **the whole task** — many model calls plus cargo
  builds and tool operations, which do not depend on any single request.

Conflating them is how a slow build gets recorded as a hung model.

### Knobs worth knowing

| Variable | Default | What it does |
|---|---|---|
| `AGENTS` | claude, codex | Which agent CLIs to drive |
| `AGENTIC_MODELS` | curated set | Model specs to measure |
| `TASKS` | the ladder | Subset to run |
| `REPS` | 1 | Repetitions per cell — the difference between a number and a pass rate |
| `REPAIR` | 0 | On a verified FAIL, feed the real build/test error back and let the agent fix it, same workdir, up to N times |
| `BENCH_GATEWAY_URL` | unset | Borrow an already-running gateway instead of loading a second copy (see below) |
| `BENCH_DEDICATED` | unset | Give the run its own gateway |
| `BENCH_BIN` | `target/release/rozum-gateway` | Which binary is under test — **not** `PATH` |
| `BENCH_OUT` | `results/agentic-<ts>` | Where the run lands |
| `BENCH_PORT_BASE` | — | Move the port range; collides with a sibling run otherwise |
| `KEEP=1` | off | Keep the per-run workdirs for forensics |
| `GW_READY_SECS` | 240 | Gateway readiness wait; raise together with `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` when queuing behind other RAM users |
| `TEARDOWN_GRACE` | — | Seconds to let a model finish before teardown; too small once panicked the machine |

`ROZUM_SAMPLING_SEED` and `ROZUM_FORCE_GREEDY` exist for determinism work — both default
off, and a policy carried on the request rather than in the environment (a knob that lives
in the environment goes stale silently when the topology changes).

Requires `claude` and/or `codex` on `PATH`, plus `cargo`, `jq`, `perl`, and macOS
`/usr/bin/time -l`.

---

## Sharing a resident model — `BENCH_GATEWAY_URL`

Two agents CAN work against one resident model: `rozum launch` reuses a healthy gateway
whose model matches, and the gateway admits 2 concurrent requests. What cannot coexist is
two GATEWAYS each holding ~12 GB of the same weights, which is why the harness loads its
own by default and why a matrix waits in the admission queue when someone else holds the
model.

```bash
BENCH_GATEWAY_URL=http://127.0.0.1:8089 AGENTS=nadia TASKS=wordcount REPS=3 scripts/bench/agentic.sh
```

**Pass/fail only in this mode.** Timings are contended — the same task measured 67 s,
193 s and 163 s in one shared run — and the footprint column is left EMPTY rather than
filled from a process this run does not own. Never compare seconds from a shared run with
seconds from a private one.

Two rules this mode had to be taught, both by breaking:

- It must **not** `gateway stop --force` at startup. That killed the very gateway it meant
  to borrow, and every cell then failed with `rozum launch: no gateway running`.
- It must **not** tear the gateway down at the end. A gateway we did not start is
  somebody's resident model, and stopping it takes their work with it.

---

## The full matrix — `run_full_matrix.sh`

The prepared operator run: several models × several agents × every task, capturing
red/green, time, and the verifier's reason. Models load **sequentially**, one at a time;
adaptive load plus the admission gate mean a model that does not fit is refused cleanly
before any weights load — a matrix FAIL, never a reboot.

```bash
scripts/bench/run_full_matrix.sh
scripts/bench/summarize_matrix.py scripts/bench/results/full-matrix-<stamp>
```

### Read the summary the way it is written

`summarize_matrix.py` refuses to print one blended "TOTAL X/Y green", because a run mixes
tiers that mean different things:

- **capable** — the curated agentic coders actually shipped and measured;
- **probe** — small or experimental models kept for context, most of which only clear
  `greet` (the known 7B→27B capability cliff).

Averaging those produces a number that moves when the probe list changes and says nothing
about the models anyone uses. Read the tiers separately.

`rerun_reds.py` re-runs only the non-green cells and merges latest-wins into one CSV;
`agentic_triage.py` classifies failures from the local run artifacts; and
`plan_matrix_schedule.py` plans a RAM-aware model order using
`rozum-gateway gateway --dry-run`, so a run does not discover mid-way that the next model
will not fit.

---

## Focused probes

| Script | Question it answers |
|---|---|
| `rag-ab.sh` | Does `rag.search` help an agent LOCATE code in a real repository? Deliberately separate from the matrix, whose tiny sandboxes have nothing to retrieve. |
| `contention.sh` | Does the residency admission queue survive real contention? A batch 32B antagonist vs an interactive 4B matrix that must preempt it. Asserts 0 jetsam, 0 dead cells, antagonist yields. |
| `nondeterminism-probe.sh` | How many distinct completions come back from N byte-identical requests? Talks to an already-running gateway and never starts one, so it cannot cause a second concurrent load. |
| `smmrd-measure.sh` | Does a big model's real full-context-prefill peak (active + cache high-water) match the footprint admission reserved for it? |
| `memory_correctness_frontier.py` | The model×driver Pareto frontier for correctness, memory, and memory-time cost. |
| `solve.sh` | Not a benchmark — the adaptive plan→execute→verify→critic→repair loop over N models. |

---

## Where results go, and what to do with them

```
scripts/bench/results/<name>-<stamp>/
  per-run.csv     one row per cell
  run-info.txt    what was measured: binary, models, knobs, host
  runs/           per-cell logs (with KEEP=1, the workdirs too)
```

**A run that is not in [`HISTORY.md`](HISTORY.md) did not happen.** That file is the
project's benchmark history — what was measured, on what, and what changed since the
previous entry. It is what makes a later "we got 2× faster" checkable rather than
remembered. Add an entry with the host's load, because a contended run and a quiet one are
different measurements.

Two failure modes this directory has actually produced, worth knowing before trusting a
number:

- **A stale binary.** `BENCH_BIN` decides what is under test, not `PATH`. A morning run
  measured a two-day-old install and reported 23/24; the same suite on the current binary
  reported 24/24 the same evening.
- **A shared-run second.** See `BENCH_GATEWAY_URL` above. Timings from a contended run are
  not comparable to anything.

---

## Tests of the harness itself

`test-classify-rc.sh`, `test-summarize-matrix.sh` and `test-agentic-preserve-cell.sh`
exercise the parts that decide what a cell *is* — `setup_task` and `classify_rc` — without
a model, a gateway, or ten minutes. Those rules had been asserted in comments for a year
and never once executed; now a test drives them directly.
