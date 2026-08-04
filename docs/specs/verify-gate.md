# Verify gate — deciding what "done" means, and checking it

**Status:** implemented (`crates/rozum-agent/src/verify.rs`), consumed by `rozum launch`
(`src/main.rs`) and by nadia (`crates/nadia/src/gate.rs`).
**Agent-side contract:** `nadia:SPEC.md` §3.1 — that file says what an *agent* owes its
operator. This one specifies the shared primitives underneath both consumers.

## Why it exists

A small model verifies what its prompt asks it to verify — that the program builds and runs —
and stops there, because nothing told it what the right answer is. Measured, twice:

- an RPN calculator that builds, runs, and prints `4 + 4 = 7`, reported as finished;
- a task "completed" against a file written into a mirror of the workspace path (BUG-016).

Both are the same failure: **success declared by the party doing the work**. The gate moves that
decision to a command that either passes or does not.

## The shape

```
derive_check(task)  →  Option<command>        # before the run; the run cannot influence it
        ↓ none
   cargo_floor(workspace)                     # a cargo project must at least build
        ↓ none
   judge(task, workspace)                     # semantic, and its Unknown is not a pass
```

then, after the agent stops:

```
run_check(command, workspace) → (passed, output)
   ↓ failed
repair_prompt(command, output) → the agent's next turn        # bounded rounds
```

## Rules

1. **The model supplies values, never shell.** `derive_check` asks for structured data
   (`{"checkable","cargo_test","run":[{"arg","expect"}]}`) and *we* build the command,
   shell-quoting every string it returned. A model that could write the check could also write
   `rm -rf ~`, and a verifier that runs model-authored shell is not a verifier.
2. **`checkable: false` is a valid answer.** No invented criterion — "reply with the word pong"
   once became `cargo run -- pong == gnop`. `is_hallucinated_cargo_check` catches the same class
   one level down: a cargo check for a workspace with no manifest and a task that never mentioned
   Rust is dropped.
3. **A failing check must SAY what it saw.** `[ "$(cargo run …)" = X ]` fails silently, which
   leaves the repair round with an empty error on exactly the mismatches that matter — where the
   program runs and prints the wrong thing. The generated fragment prints both values.
4. **Unknown is not a pass.** The judge's three outcomes stay three.
5. **The check describes the TASK, not the workspace's accidents.** Where the agent chose to put
   files is the agent's mistake to fix, not the check's to accommodate — see below.

## The two accuracy rules this revision adds

Both come from one measured run (2026-08-04). The gate correctly failed the task and both repair
rounds were spent fighting the *check* rather than the task.

### A. The argument is the value, not the way the task spelled it

`cargo run -- "3 4 + 2 *"` in the task text yielded `arg = "\"3 4 + 2 *\""` — the quotes the task
used to delimit the argument became part of the argument. The check then demanded a program that
accepts a quoted argument, which the task never asked for and which no correct implementation
provides.

**Rule.** When `arg` or `expect` is wrapped in a symmetric pair of `"` or `'`, the pair is
delimiting, not data: strip exactly one pair. A string that is quoted on one side only, or that
contains quotes internally, is left alone — stripping those would corrupt real data.

This is a normalization, not a guess: the model is *also* told in the prompt that the value
excludes the quotes that delimit it. Prompt and code both, because a prompt rule that only
usually holds needs a floor under it.

### B. A project belongs in the workspace root

`cargo new <name>` creates a SUBDIRECTORY. A check that runs `cargo` at the workspace root then
cannot pass however good the code is, and the repair rounds go on rediscovering that.

**Rule, two halves:**

- The agent's system prompt says to create the project in the workspace root (`cargo init`, not
  `cargo new <name>`), because that is where the check runs.
- When a check fails, the workspace root has no `Cargo.toml`, and exactly one immediate
  subdirectory does, the repair prompt SAYS so and names it.

**Explicitly rejected:** having the check `cd` into that subdirectory. It would turn a real
mistake — work delivered somewhere the operator did not ask for — into a passing run, which is
the class of failure this whole gate exists to remove. The check tells the truth about the
workspace it was given; moving the work is the agent's job.

## Non-goals

- **Judging code quality.** The gate answers "does it do what was asked", not "is it good".
- **Protecting the workspace.** That is the sandbox (`nadia:SPEC.md` §3.1–3.2, BUG-017).
- **Being the only verification.** `rozum launch` wraps these primitives in a model-escalation
  chain; nadia runs them around one model. The primitives take no position on that policy.
