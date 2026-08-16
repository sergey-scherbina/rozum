#!/usr/bin/env bash
# Agentic end-to-end benchmark for rozum.
#
# Drives a REAL `rozum launch claude` / `rozum launch codex` against a local MLX
# model — the whole stack as a user runs it. Per model the harness loads it ONCE
# (a shared `rozum gateway`, under /usr/bin/time -l for the resident footprint),
# then runs every task through `rozum launch` (no --model), which *reuses* that
# resident model — no reload between tasks. `rozum launch` still applies its
# Seatbelt sandbox jail (default-on) + Claude-Code prompt trimming + Codex/opencode
# provider config; every agent flag is passed on the command line. Each task is its own process tree, measured independently:
# wall time, the agent tree's peak RAM, peak CPU% (agent + gateway), pass/fail.
#
# Two independent timeouts (per your design — model vs everything else):
#   - ROZUM_GEN_TIMEOUT_SECS  (engine, default 180): bounds a single model request
#     inside the gateway. A wedged generation aborts; the agent loop continues.
#   - RUN_TIMEOUT  (default 1200): the whole agentic task — many model calls plus
#     cargo builds and tool ops, which don't depend on any one model request.
#
# Tasks (increasing difficulty):
#   greet  - reply with one word (no tools)            -> output contains "pong"
#   build  - create reverse-cli, run it                -> cargo run -- hello == olleh
#   fix    - find+EDIT a one-line bug (returns input)  -> cargo run -- hello == olleh
#   test   - implement reverse + a #[test] + cargo test-> cargo test green & run == olleh
#   debug  - failing test, run-read-fix loop           -> cargo test green
#
# Usage:
#   scripts/bench/agentic.sh
#   AGENTIC_MODELS="mlx-community:Qwen3-30B-A3B-4bit" AGENTS=claude scripts/bench/agentic.sh
#   TASKS="greet build" RUN_TIMEOUT=600 scripts/bench/agentic.sh
#
# Env knobs:
#   AGENTIC_MODELS  space-separated specs (default: the 3 installed models — Qwen3-4B, Qwen3.5-4B-VL, Qwen3.6-35B-A3B)
#   AGENTS          subset of "claude codex opencode" (default all three, if installed)
#   TASKS           subset of: greet build fix test debug (default all)
#   RUN_TIMEOUT     whole-task wall ceiling, seconds (default 1200)
#   GEN_TIMEOUT     ROZUM_GEN_TIMEOUT_SECS for the in-process gateway (default 180)
#   MAX_TURNS       Claude --max-turns (default 15 — caps the re-edit/retry loop
#                   weak models fall into; see SPRINT.md "agentic-loop-root-cause")
#   NCTX            override gateway context (default: omit -> model max, auto)
#   GW_READY_SECS   gateway readiness wait, default 240; raise together with
#                   ROZUM_GATEWAY_RESIDENCY_WAIT_SECS to queue behind other RAM users
#   REPAIR          verify-repair retries: on a verified FAIL, feed the real build/test error
#                   back and let the agent fix it, same workdir, up to N more times (default 0).
#                   The deterministic net for "almost-right code + hallucinated success".
#   BENCH_BIN       rozum binary (default target/release/rozum-gateway, absolute)
#   BENCH_OUT       output dir (default scripts/bench/results/agentic-<ts>)
#   KEEP=1          keep per-run workdirs
#
# Requires: claude and/or codex, cargo, jq, perl, macOS /usr/bin/time -l.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

# ── what a cell IS, defined FIRST so it can be tested without running one ──────────────────────
#
# `setup_task` says what a cell STARTS as and `classify_rc` says what it ENDS as; between them they
# define the measurement, and neither needs a model, a gateway or ten minutes to answer. They live
# above the guard so a test can drive them directly — including `setup_task` itself, so the seeded
# baseline the classification depends on is the harness's own and not a copy in the test that
# drifts.
#
# Everything from `set -uo pipefail` to here is inert. The
# SOURCE GUARD below them stops a `source scripts/bench/agentic.sh` from going any further — the
# configuration that follows exits when there is no rozum binary or no agent CLI on PATH, and the
# run itself starts at the bottom of the file. `scripts/bench/test-classify-rc.sh` relies on this:
# it sources this file and calls `classify_rc` against temp directories, so the rule is EXECUTED by
# a test rather than asserted in a comment. It had been asserted for a year and never once run.

# Structured exit codes (agentic-rc-structured):
#   0   = verify PASS
#   2   = infra failure (gateway crash / clients_gone — rc=2 from rozum launch)
#   10  = verify FAIL — agent ran to completion but task not solved (capability miss)
#   11  = verify SKIP — no project files written (delivery failure: agent never wrote code)
#   12  = verify SKIP — manifest present, no `src/*.rs` (PARTIAL delivery: cargo has no target)
#   13  = verify SKIP — the workdir is byte-identical to what the harness seeded (nothing changed)
#   124 = timeout (RUN_TIMEOUT fired)
#   other = agent error (non-zero, non-infra: tool error, segfault, etc.)
#
# WHY 12 EXISTS. rc=11 asked one question — "is `Cargo.toml` there" — so a cell that wrote the
# MANIFEST and lost the SOURCE fell through to rc=10, and every entry on the boards reads rc=10 as
# "the model wrote wrong code, not our problem". That reading is unsafe for exactly the delivery
# question rc=11 was added to answer: an agent that creates a manifest and stops has delivered
# nothing to judge, and `cargo` says so in its own words ("no targets specified in the manifest").
# Measured 2026-08-09 on rep 2 of the rpn run: verify said `FAIL no src/*.rs`, `Cargo.toml` present,
# rc=10. It really was model-side that time, but the rc could not say so — the gateway log had to be
# cross-checked to find out, which is the cost this removes.
#
# `agentic_triage.py` has always separated these two: `missing_cargo_toml` vs `missing_src_rs`, both
# under `missing_project_files`. The rc is what collapsed them, so this is the exit code catching up
# to a distinction the triage already makes — not a new theory about the runs.
#
# NOT a widening of rc=11 to "no src/*.rs": `greet` writes no files at all by design, and the seeded
# tasks (`fix`, `debug`, `multibug`) START with both, so for them rc=12 means the agent DELETED the
# source it was given — also worth seeing, and also not a capability miss.
#
# It also catches the WRONG-PLACE shape — `main.rs` at the workdir root with no `src/` — which the
# triage calls `wrong_entrypoint` and which is delivery, not reasoning, by the same argument. The
# test is deliberately "what cargo can build", not "did any bytes get written": source in a place
# cargo does not look is not delivered.
#
# WHY 13 EXISTS, and why presence could not answer it. rc=11 and rc=12 both ask what is ON DISK, and
# for the SEEDED tasks the answer is "a manifest and a source file" before the agent has done
# anything — the harness put them there (`setup_task`). So on `fix`, `debug` and `multibug` an agent
# that reads the project, writes nothing and exits scores rc=10: the code that means "delivered a
# complete program and it is wrong". That is the same misattribution rc=12 removed, one layer down,
# and it is the layer the fix/debug numbers on the boards come from.
#
# It needs a BEFORE and an AFTER, so `setup_task` records `.rozum-seed` — a sha256 of every file it
# seeded — and `workdir_untouched` re-checks it. Inside the workdir rather than beside it because
# `agentic.meta` already lives there: an agent that tampers with the manifest only makes the answer
# "cannot say", which degrades to today's rc=10. There is no tampering that INVENTS a 13.
#
# SAY WHAT WAS MEASURED. "Byte-identical to the seed" is not "the agent did nothing" — a repair that
# edits a file and reverts it ends byte-identical too, and so does one whose writes were lost by us.
# The code and its label state the measurement; the interpretation is the reader's, with the log.
#
# A FUNCTION, and not the inline block it used to be, so the classification can be tested without a
# model and without a gateway: `scripts/bench/test-classify-rc.sh` drives all of it from temp dirs.
# The rule this encodes had been asserted on the boards for a year and never once executed.

# What the HARNESS puts in a workdir, plus what `cargo` leaves when the verifier builds. Everything
# else is the agent's.
#
# THE DIRECTION OF THE ERROR IS THE POINT. A missing entry here makes a harness artifact look like
# the agent's work, so `workdir_untouched` says no and the cell falls back to today's answer. There
# is no entry whose absence manufactures an "untouched" verdict — this list can only ever SUPPRESS
# the new code, never fabricate it. That is the opposite of the inclusion-list trap and it is why
# the list is allowed to be a list.
SEED_IGNORE='^\./(\.rozum-seed|agentic\.meta|agent\.log|samples\.txt|verify\.out|run\.err|cargo\.err|triage\.out|Cargo\.lock|target/.*)$'

seed_manifest() { # $1=workdir — call at the END of setup_task, over exactly what was seeded
  ( cd "$1" 2>/dev/null || exit 0
    find . -type f ! -name .rozum-seed -print0 | while IFS= read -r -d '' f; do shasum -a 256 "$f"; done
  ) > "$1/.rozum-seed" 2>/dev/null
}

workdir_untouched() { # $1=workdir — 0 iff every seeded file is byte-identical AND nothing was added
  local w="$1"
  [ -f "$w/.rozum-seed" ] || return 1   # no manifest → cannot say, and silence beats a guess
  ( cd "$w" || exit 1
    # A DELETED seeded file fails this too, which is right: deleting the code you were asked to fix
    # is a change, and a loud one.
    #
    # The `-s` guard is not defensive noise. A from-scratch task seeds nothing, so its manifest is
    # empty, and `shasum -c` on an empty file exits 1 — "no properly formatted checksum lines". This
    # function would then answer "changed" for a workdir where nothing happened, and be RIGHT only by
    # accident, because rc=11 fires first. A predicate that returns the wrong answer and is saved by
    # the caller's ordering is a trap for whoever reorders the caller.
    if [ -s .rozum-seed ]; then shasum -a 256 -c --status .rozum-seed 2>/dev/null || exit 1; fi
    # …and nothing NEW. Without this half, an agent that leaves `src/lib.rs` alone and writes
    # `src/main.rs` beside it reads as untouched, which is the reverse of the truth.
    [ "$(find . -type f | grep -Ev "$SEED_IGNORE" | LC_ALL=C sort)" \
      = "$(sed 's/^[0-9a-f]*  //' .rozum-seed | LC_ALL=C sort)" ] )
}

classify_rc() { # $1=task $2=workdir $3=timed_out(0|1) $4=raw agent rc $5=verify pass(0|1)
  local task="$1" work="$2" tmo="$3" raw_rc="$4" pass="$5"
  local files_written=1 partial_delivery=0
  # Call this AFTER verify_task: it hoists a single subdirectory project up into the workdir, and a
  # correct-but-nested delivery read before that hoist counts as no delivery at all.
  if [ "$task" != greet ]; then
    if   [ ! -f "$work/Cargo.toml" ];           then files_written=0
    elif ! ls "$work"/src/*.rs >/dev/null 2>&1; then partial_delivery=1
    fi
  fi
  if   [ "$tmo"    = 1 ];            then echo 124
  elif [ "$raw_rc" = 2 ];            then echo 2
  elif [ "$pass"   = 1 ];            then echo 0
  elif [ "$raw_rc" != 0 ];           then echo "$raw_rc"   # non-zero agent exit, not gateway crash
  elif [ "$files_written" = 0 ];     then echo 11          # agent ran but wrote no project files
  elif [ "$partial_delivery" = 1 ];  then echo 12          # manifest only — nothing for cargo to build
  elif workdir_untouched "$work";    then echo 13          # complete project, byte-identical to seed
  else                                    echo 10          # verify FAIL on a clean agent exit
  fi
}

setup_task() { # $1=task  $2=workdir — pre-create files for fix/debug
  case "$1" in
    fix)
      printf '[package]\nname = "reverse-cli"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/main.rs" <<'EOF'
use std::env;

/// Reverse a string by characters.
fn reverse(s: &str) -> String {
    // BUG: returns the input unchanged.
    s.to_string()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}
EOF
      ;;
    debug)
      printf '[package]\nname = "mathlib"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }
}
EOF
      ;;
    wordcount)
      # From-scratch task: the agent creates Cargo.toml + src/main.rs; we only
      # pre-seed the data file. Mixed case tests case-folding; two words tie at 3
      # (apple/banana) to test the alphabetical tie-break. Expected top-3:
      # `apple 3` / `banana 3` / `cherry 2`.
      printf 'Apple banana apple Cherry BANANA apple date banana cherry\n' >"$2/input.txt"
      ;;
    board)
      # FOUR RULES THAT INTERACT, AND EVERY ONE OF THEM IS OBSERVABLE IN THE OUTPUT.
      #
      # `leapday` (below) failed as a discriminator: the 4B cleared it 3/3, because the defect it
      # hides is the Gregorian leap rule and every model KNOWS that rule — the task only asked where
      # to apply it. So this one asks for nothing to be recalled. It states four requirements and
      # measures whether they are all held at once while one function is written.
      #
      # The interaction is the ORDER: sort on the FULL name, truncate after, disambiguate after
      # that. An implementation that truncates first still sorts plausibly and still fails, because
      # the disambiguation suffix then lands on the wrong row.
      #
      # WHAT WAS TRIED AND DROPPED, so nobody rebuilds it: a fifth trap on truncation ORDER is
      # unobservable by construction. Two names sharing their first nine characters render
      # identically, so with equal scores both orderings produce the same two lines. Truncation is
      # lossy; a rule cannot be tested through output it erases.
      printf '[package]\nname = "board"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Render a leaderboard. See the tests for the exact expected output.
pub fn render(rows: &[(String, u64)]) -> String {
    todo!("implement me")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(n, s)| ((*n).to_string(), *s)).collect()
    }

    #[test]
    fn ordering_case_and_thousands() {
        let got = render(&rows(&[
            ("alice", 1200),
            ("Bob", 1200),
            ("bob", 1200),
            ("christopherson", 999),
            ("dan", 1000),
            ("eve", 42),
            ("Zoe", 42),
        ]));
        assert_eq!(
            got,
            "alice: 1_200\n\
             Bob: 1_200\n\
             bob: 1_200\n\
             dan: 1_000\n\
             christoph…: 999\n\
             eve: 42\n\
             Zoe: 42"
        );
    }

    #[test]
    fn truncation_collision_and_zero() {
        let got = render(&rows(&[
            ("abcdefghiXX", 77),
            ("abcdefghiAA", 77),
            ("verylongname99", 1234567),
            ("ann", 1234567),
            ("ANN", 1234567),
            ("x", 0),
        ]));
        assert_eq!(
            got,
            "ANN: 1_234_567\n\
             ann: 1_234_567\n\
             verylongn…: 1_234_567\n\
             abcdefghi…: 77\n\
             abcdefghi… (2): 77\n\
             x: 0"
        );
    }
}
EOF
      ;;
    duration)
      # THE FIRST OF THESE WHOSE DIFFICULTY THE FEEDBACK LOOP CANNOT ERASE.
      #
      # `leapday`, `board` and `apportion` all put the difficulty in a FAILING TEST, and the
      # operator was right that the third read as the first one again. The systematic error is
      # bigger than the repeat: THE AGENT HAS `cargo test` IN A LOOP. Anything the tests can see
      # reduces to "keep editing until it goes green" — the feedback hands the model exactly what a
      # trap is built to hide. Difficulty that lives in a red test is difficulty the loop returns
      # for free, so all three measured how long the loop takes, not what the model understands.
      #
      # Here `cargo test` is GREEN on arrival and stays green whatever the agent does. The seeded
      # tests cover hours/minutes/seconds and the omission of a zero component. Days and the
      # all-zero case are in the PROMPT and in NO test. A model that reads "cargo test passes" as
      # "done" ships four of eight verifier cases wrong and never sees a red line anywhere.
      #
      # Not a gotcha bolted on: `build`, `fix` and `wordcount` have always verified by RUNNING the
      # program rather than trusting its tests. What is new is that the gap between the two is
      # where the task lives.
      #
      # Measured with this harness's own `verify_task` before it landed — untouched skeleton:
      # `cargo test` 2/2 green, verify 4/8 red; days added but the all-zero case missed (the
      # half-done state, still 2/2 green): 7/8; reference solution: 8/8. So the verifier grades,
      # and a task nobody can pass is `board` again — the reference was written and run first.
      #
      # The prompt gives two examples and the verifier checks eight values, for the reason `rpn`
      # already states: hard-coding the examples must not pass.
      printf '[package]\nname = "duration"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Render `secs` as a human-readable duration.
pub fn format(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_minutes_seconds() {
        assert_eq!(format(3661), "1h 1m 1s");
    }

    #[test]
    fn a_zero_component_is_left_out() {
        assert_eq!(format(3600), "1h");
        assert_eq!(format(61), "1m 1s");
    }
}
EOF
      cat >"$2/src/main.rs" <<'EOF'
use std::env;

fn main() {
    let secs: u64 = env::args()
        .nth(1)
        .expect("usage: duration <seconds>")
        .parse()
        .expect("seconds must be a non-negative integer");
    println!("{}", duration::format(secs));
}
EOF
      ;;
    apportion)
      # ATTEMPT FOUR, AND THE FIRST ONE DESIGNED FROM EVIDENCE RATHER THAN FROM A GUESS AT
      # DIFFICULTY.
      #
      # Three predecessors missed, each for a reason worth not repeating. `leapday` removed the
      # signpost but the rule it hides is the Gregorian calendar, which every model already knows —
      # 4B cleared it 3/3. `board` asked for four interacting rules from scratch and neither model
      # reached them: the 4B died on `expected &str, found String`, the 9B on `cannot borrow as
      # mutable because it is also borrowed as immutable`, 0/3 each over 314-637 s cells. The
      # ceiling on this stack is Rust's type and borrow system, not reasoning, so any write-it-from-
      # scratch task hits that wall first and hides the difference behind it.
      #
      # So: a COMPILING skeleton, the change is to LOGIC and never to ownership structure (plain
      # `Vec<u64>`, indices, integer arithmetic — the reference fix is nine lines and borrows
      # nothing), and the rule is STATED in the doc comment instead of recalled. What is measured is
      # whether the model holds the whole stated rule while editing, not whether it has memorised an
      # algorithm.
      #
      # THE HALF-FIX IS AGAIN THE POINT, and this time it is measured, not assumed. The skeleton
      # hands leftover units out from the END of the vector. Handing them out from the FRONT instead
      # is the plausible edit, and it satisfies `equal_fractions_go_to_the_lower_index_first`
      # completely — where all fractions tie, front-loading IS the correct answer. It still fails
      # `leftover_follows_the_largest_fraction`. Measured in a scratch crate before this landed:
      # skeleton 2/4, front-load 3/4, largest-remainder 4/4. A task nobody can pass is `board`
      # again, so the reference solution was written and run first.
      printf '[package]\nname = "apportion"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Split `total` into one part per weight, in proportion to the weights.
///
/// The parts must sum to exactly `total`. Whole units left over after the proportional split
/// go to the parts whose exact share had the largest fractional part; where two fractional
/// parts are equal, the lower index takes a unit first.
pub fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum: u64 = weights.iter().sum();
    let mut parts: Vec<u64> = weights.iter().map(|w| total * w / sum).collect();
    let mut leftover = total - parts.iter().sum::<u64>();
    let mut i = parts.len();
    while leftover > 0 {
        i -= 1;
        parts[i] += 1;
        leftover -= 1;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_division_leaves_nothing_over() {
        assert_eq!(apportion(100, &[3, 3, 4]), vec![30, 30, 40]);
    }

    #[test]
    fn every_unit_is_handed_out() {
        for total in [1, 5, 7, 11, 100, 1001] {
            let parts = apportion(total, &[2, 5, 3]);
            assert_eq!(parts.iter().sum::<u64>(), total, "total {total}");
        }
    }

    #[test]
    fn leftover_follows_the_largest_fraction() {
        assert_eq!(apportion(11, &[2, 5, 3]), vec![2, 6, 3]);
        assert_eq!(apportion(7, &[1, 1, 2]), vec![2, 2, 3]);
    }

    #[test]
    fn equal_fractions_go_to_the_lower_index_first() {
        assert_eq!(apportion(5, &[1, 1, 1]), vec![2, 2, 1]);
        assert_eq!(apportion(100, &[1, 1, 1, 1, 1, 1]), vec![17, 17, 17, 17, 16, 16]);
    }
}
EOF
      ;;
    leapday)
      # THE DEFECT IS TWO CALLS BELOW THE FAILING TEST, AND NOTHING POINTS AT IT.
      #
      # Every other bug task here is signposted — `// BUG: subtracts instead of adding` sits on the
      # line to change — and single-hop: the failing assertion names the function that is wrong.
      # A 4B clears all of them, which is why the matrix cannot tell two models apart (measured
      # 2026-08-15: Qwen3.5-4B and Qwen3.5-9B both 24/24 on the eight existing tasks). This one
      # removes the signpost and the adjacency: `day_of_year` fails, calls `days_in_month`, which
      # calls `is_leap`, which is where the rule is wrong.
      #
      # THE HALF-FIX IS THE POINT. `is_leap` is missing the century rule. The obvious repair —
      # `y % 4 == 0 && y % 100 != 0` — makes the 1900 case pass and BREAKS 2000, which the tests
      # also check. Special-casing the failing date in `day_of_year` breaks the others too. Only the
      # full Gregorian rule passes all four, so a plausible edit that satisfies the visible failure
      # is not enough.
      printf '[package]\nname = "calendar"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Whether `y` is a leap year.
pub fn is_leap(y: u32) -> bool {
    y % 4 == 0
}

/// Number of days in month `m` (1-12) of year `y`.
pub fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// The 1-based ordinal day of `y-m-d` within its year.
/// January 1st is 1; March 1st is 60 in a common year and 61 in a leap year.
pub fn day_of_year(y: u32, m: u32, d: u32) -> u32 {
    let mut total = 0;
    let mut mm = 1;
    while mm < m {
        total += days_in_month(y, mm);
        mm += 1;
    }
    total + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_year() {
        assert_eq!(day_of_year(2023, 3, 1), 60);
    }

    #[test]
    fn leap_year_divisible_by_four() {
        assert_eq!(day_of_year(2024, 3, 1), 61);
    }

    #[test]
    fn century_is_not_a_leap_year() {
        assert_eq!(day_of_year(1900, 3, 1), 60);
    }

    #[test]
    fn four_hundredth_year_is_a_leap_year() {
        assert_eq!(day_of_year(2000, 3, 1), 61);
    }

    /// Counts leap years over four centuries, and it is here to close a hole the other four leave
    /// open: it exercises `is_leap` DIRECTLY, so no amount of special-casing inside `day_of_year`
    /// can satisfy it, and 98 out of 401 candidate years is not a number reachable by enumerating
    /// exceptions. 1600..=2000 holds 101 multiples of four, minus 1700, 1800 and 1900, which are
    /// centuries not divisible by 400.
    #[test]
    fn leap_years_across_four_centuries() {
        let n = (1600..=2000).filter(|&y| is_leap(y)).count();
        assert_eq!(n, 98);
    }
}
EOF
      ;;
    multibug)
      printf '[package]\nname = "twobugs"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    // BUG: subtracts instead of adding.
    a - b
}

/// True when `n` is even.
pub fn is_even(n: i32) -> bool {
    // BUG: checks odd, not even.
    n % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }
    #[test]
    fn evenness() {
        assert!(is_even(4));
        assert!(!is_even(3));
    }
}
EOF
      ;;
  esac
  # The BEFORE half of rc=13. Last line of the function on purpose: it must see everything the case
  # above wrote and nothing the harness writes afterwards (`write_agentic_meta` runs next).
  seed_manifest "$2"
}

# Does the endpoint this run ANNOUNCES match the one the agent will actually talk to?
#
# It does not follow from setting `BENCH_GATEWAY_URL`. That variable steers this harness — the
# readiness probe, the header line, the CSV — and nothing else. Every agent gets its base URL from
# `rozum launch`, which resolves its own: `ensure_shared_gateway` reads the ACTIVE-gateway registry
# and reuses whatever it names (src/main.rs), so `--port` is where it would SPAWN one, not where it
# will connect. Measured 2026-08-15 with a recording proxy: the run printed "sharing an existing
# gateway at http://127.0.0.1:8199", the proxy logged ZERO request bodies, and every token came
# from :8089 — a full pass/fail cell attributed to a gateway that never saw it.
#
# That is worth refusing rather than warning about, because the mislabel is invisible afterwards:
# the CSV row, the console header and the results directory all name the endpoint that was asked
# for, and nothing in them records which gateway (which build, which model, which n_ctx) actually
# answered. A red then reads as evidence about the wrong binary.
#
# Pure, so `test-agentic-gateway-url.sh` can cover every branch with no model and no network.
# $1 = announced base URL, $2 = port from `rozum gateway status --json` (empty = none registered).
# Echoes the reason when the run would lie; echoes nothing when the two agree.
agent_gateway_mismatch() {
  local base="$1" active="${2:-}" want
  want="$(printf '%s' "$base" | sed -n 's|^[a-zA-Z][a-zA-Z0-9+.-]*://[^/:]*:\([0-9]\{1,5\}\)\(/.*\)\{0,1\}$|\1|p')"
  if [ -z "$want" ]; then
    echo "BENCH_GATEWAY_URL='$base' has no explicit port, so it cannot be compared with the gateway \`rozum launch\` resolves"
    return 0
  fi
  if [ -z "$active" ]; then
    # No registered gateway: `rozum launch` spawns its own daemon — a SECOND copy of the weights,
    # which on a one-model host is the eviction the share-by-default block exists to prevent.
    echo "no gateway is registered as active, so \`rozum launch\` would start its own instead of using :$want"
    return 0
  fi
  if [ "$want" != "$active" ]; then
    echo "\`rozum launch\` will use :$active (the active gateway), not :$want — the agent never sees BENCH_GATEWAY_URL"
    return 0
  fi
  echo ""
}

# Copy a cell's evidence out of the temp workdir before it is deleted.
#
# A red used to leave NOTHING: `rm -rf "$work"` takes the program the model wrote, the stream-json
# transcript and the sample dump with it, and in shared mode `$OUT/runs/` was never written to at
# all — so the footer's "CSV + per-run logs" pointed at an empty directory. The cost is measured,
# not theoretical: HISTORY.md 2026-08-04 records a conclusion ("the gate repaired it") that had to
# be withdrawn because the log was gone, and on 2026-08-15 an `rpn` cell failed printing 20 for
# `3 4 + 5 *` and the program that printed it could not be read afterwards. A row saying `pass=0`
# with no artifact cannot distinguish a model that wrote bad arithmetic from a harness that
# delivered a broken file — which is the single most important distinction this bench makes.
#
# `target/` is excluded deliberately: it is 10–100× the rest, and it is exactly the part that
# rebuilds from what is kept. The whole of one failed cell is a few hundred KB.
#
# $1 = workdir, $2 = destination root ($OUT/runs), $3 = cell label. Echoes the directory it wrote.
# Never overwrites: repetitions (REPS>1) land beside each other, since the pass/fail SPLIT across
# reps is the thing worth reading.
preserve_cell() {
  local work="$1" dest_root="$2" label="$3" dest i=2
  [ -d "$work" ] || return 0
  mkdir -p "$dest_root" || return 0
  dest="$dest_root/$label"
  while [ -e "$dest" ]; do dest="$dest_root/$label.$i"; i=$((i + 1)); done
  mkdir -p "$dest" || return 0
  # tar rather than cp: one traversal, and `--exclude` is honoured by both BSD and GNU tar, which
  # is what a Mac laptop and a Linux CI runner give us.
  ( cd "$work" && tar -cf - --exclude ./target --exclude target . 2>/dev/null ) | ( cd "$dest" && tar -xf - 2>/dev/null )
  echo "$dest"
}

# Sourced rather than executed: helpers are defined, nothing runs. Must stay ABOVE the config
# block, which calls `exit 1` when its preconditions are missing.
(return 0 2>/dev/null) && return 0

RUN_TIMEOUT="${RUN_TIMEOUT:-600}"
# QW1: the codex/opencode drivers reload/parse more per turn than claude, and big/MoE/pipeline models
# are slow to load+serve — so a slow-but-CORRECT cell reads as a rc124 false negative under a tight
# ceiling (observed: codex×GLM-4.7-Flash 3/5 PASSED but every cell hit 300s). Raise the per-cell ceiling
# to a floor for those cases only; claude is fast so its RUN_TIMEOUT is left as-is, and a user-set
# RUN_TIMEOUT already above the floor is never scaled DOWN. Disable with AGENTIC_TIMEOUT_AUTOSCALE=0.
effective_run_timeout() { # $1=agent  $2=model-spec  → echoes seconds
  local agent="$1" spec="$2" eff="$RUN_TIMEOUT" floor=0
  [ "${AGENTIC_TIMEOUT_AUTOSCALE:-1}" = 0 ] && { echo "$eff"; return; }
  case "$agent" in
    codex|opencode)
      floor=600
      case "$spec" in
        *35B*|*32B*|*30B*|*GLM*|*Coder*|*Devstral*|*A3B*|*MoE*|*,*) floor=900 ;;
      esac ;;
  esac
  [ "$eff" -lt "$floor" ] && eff="$floor"
  echo "$eff"
}
GEN_TIMEOUT="${GEN_TIMEOUT:-120}"
MAX_TURNS="${MAX_TURNS:-15}"
PORT_BASE="${BENCH_PORT_BASE:-8300}"
# Verify-repair: after the agent reports "done", DON'T trust it — `verify_task` runs the real
# build/test; if it FAILS, feed the actual compiler/test error back and let the agent fix it, up
# to REPAIR more attempts (same workdir, files persist). 0 = off (legacy behavior). This is the
# deterministic safety net for the "almost-right code + hallucinated success" failure (a missing
# `;` the model never saw because it never really compiled). Helps every model; weak ones most.
# REPAIR default stays 0 (exactly one attempt). Flipping it to 1 was CONSIDERED (a near-miss like
# "wrote Cargo.toml, dropped src/main.rs, stopped" only recovers if a repair attempt runs) but a live
# Devstral×test slice (2026-07-05) showed the flip has a real downside I don't yet mitigate: when the
# repair attempt hits an Edit-before-Read loop it never converges and instead burns the whole
# RUN_TIMEOUT — turning a fast rc=10 (~57 s) into a slow rc=124 (~360 s) for the SAME red. Every real
# launcher already opts in explicitly (run_full_matrix.sh, control.rs matrix job both set REPAIR=1),
# so the default only governs ad-hoc runs, where fail-fast is the more useful signal. Set REPAIR=1 to
# enable the verify-repair retry (recommended for full matrices; see the repair_diagnostic branches).
REPAIR="${REPAIR:-0}"
OUT="${BENCH_OUT:-$here/results/agentic-$(date +%Y%m%d-%H%M%S)}"
NCTX_OPT=(); [ -n "${NCTX:-}" ] && NCTX_OPT=(--n-ctx "$NCTX")

BIN="${BENCH_BIN:-}"
if [ -z "$BIN" ]; then
  if   [ -x "$repo/target/release/rozum-gateway" ]; then BIN="$repo/target/release/rozum-gateway"
  elif [ -x "$repo/target/debug/rozum-gateway"   ]; then BIN="$repo/target/debug/rozum-gateway"
  else echo "no rozum binary; build with: cargo build --release --bin rozum-gateway" >&2; exit 1; fi
fi
case "$BIN" in /*) ;; *) BIN="$repo/$BIN" ;; esac   # launch runs in a temp cwd → need absolute

# Default: the models that actually do agentic coding (4B → 35B), small → large.
# The agentic matrix found a 7B→27B capability cliff: sub-4B / weak tool models
# (Qwen2.5-0.5B, Qwen3-0.6B, Llama-3.2-1B) only manage `greet` even with the
# JSON-repair, and template-less / incompatible models (gemma, Phi-3, SmolLM2,
# Mistral-v0.3) can't drive tools at all — all dropped. Override with AGENTIC_MODELS.
# Default = the model(s) actually installed locally after `models-cleanup` (2026-07-13 / -14): the
# older curated catalog (Qwen3-Coder-30B, Devstral, GLM-4.7-Flash, GLM-4-32B, gpt-oss-20b, the -DWQ
# 35B) was pruned, then Qwen3-4B-4bit + Qwen3.6-35B-A3B were dropped too (the 35B is RAM-blocked on a
# ~39 GB host: it needs ~23.6 GiB and only ~21.8 GiB is available). Keep this in sync with
# `rozum-gateway models list`. Current single kept model:
#   Qwen3.5-4B-MLX-4bit — dense 4B VISION-LANGUAGE (VL port: 4bit text + bf16 vision tower). The
#   standout small model: agentic matrix 8/8 (claude, 2026-07-13) — every task incl. the hard
#   fix/debug/rpn + wordcount/multibug — and ~2× faster than the old Qwen3-4B (which scored 5/8).
# Each space-separated entry is one `gateway --model <spec>`; a comma inside an entry = pipeline.
# Override with AGENTIC_MODELS="spec1 spec2 ...".
DEFAULT_MODELS="mlx-community:Qwen3.5-4B-MLX-4bit"
read -r -a MODELS <<<"${AGENTIC_MODELS:-$DEFAULT_MODELS}"
# Tasks: greet build fix test debug (the originals) + rpn (a from-scratch-hard RPN calculator —
# create-from-scratch where a planner→executor pipeline should help most; see verify_task/prompt_for).
read -r -a TASK_LIST <<<"${TASKS:-greet build fix test debug rpn wordcount multibug}"
# REPS>1 runs every cell N times (fresh workdir each) so the report is a PASS-RATE, not a
# single sample. The agentic matrix is irreducibly noisy — agent CLIs inject a per-run
# session-id/timestamp, so a cell varies run-to-run even at a fixed ROZUM_SAMPLING_SEED
# (docs/specs/matrix-nondeterminism.md). A single-run red is NOT a bug until confirmed over
# N runs. Implemented by repeating TASK_LIST (one model load still serves all reps).
REPS="${REPS:-1}"
if [ "$REPS" -gt 1 ] 2>/dev/null; then
  _base_tasks=("${TASK_LIST[@]}"); TASK_LIST=()
  for _ in $(seq 1 "$REPS"); do TASK_LIST+=("${_base_tasks[@]}"); done
fi

# DECODE POLICY. Both knobs are documented as GATEWAY settings, and until 2026-08-15 that is
# literally all they were: read from the environment of the process serving the model. They worked
# only while every run started its own gateway. Since borrowing a resident one became the default
# (2026-08-07) there is no such process — `run_full_matrix.sh` exported `ROZUM_FORCE_GREEDY=1`,
# this script passed it down, and the daemon actually decoding never saw it. Eight days of cells
# were sampled at the client's temperature under a launcher that said greedy, which is exactly the
# noise the seed was introduced to remove (docs/specs/matrix-nondeterminism.md).
#
# They now travel on the REQUEST: `rozum launch`'s proxy stamps `x-rozum-decode`/`x-rozum-seed` from
# these variables and the gateway honours them per-request, so a borrowed daemon can be pinned
# without being touched. EXPORTED, because the agent is what has to inherit them.
#   ROZUM_SAMPLING_SEED  pins sampler + MLX RNG (same temperature, replayable stream). Default 1234;
#                        set empty to restore free entropy.
#   ROZUM_FORCE_GREEDY   temperature 0 / argmax — no RNG at all. NOT defaulted here: it changes what
#                        the model does, and `run_full_matrix.sh` turns it on for the matrix.
export ROZUM_SAMPLING_SEED="${ROZUM_SAMPLING_SEED-1234}"
[ -n "${ROZUM_FORCE_GREEDY:-}" ] && export ROZUM_FORCE_GREEDY
if [ "${ROZUM_FORCE_GREEDY:-0}" = 1 ] || [ "${ROZUM_FORCE_GREEDY:-}" = true ] || [ "${ROZUM_FORCE_GREEDY:-}" = on ]; then
  DECODE_NOTE="greedy (temperature 0, argmax — pinned per-request)"
elif [ -n "${ROZUM_SAMPLING_SEED:-}" ]; then
  DECODE_NOTE="sampled at the client's temperature, RNG pinned to seed ${ROZUM_SAMPLING_SEED}"
else
  DECODE_NOTE="FREE ENTROPY — a single red is one sample, not a result"
fi

CELL_N=0   # cells run so far, so each gets its own seed (see the runner invocation)
AGENT_RUN=()
for a in ${AGENTS:-claude codex opencode}; do command -v "$a" >/dev/null && AGENT_RUN+=("$a") || echo "skip agent '$a' (not on PATH)"; done
[ "${#AGENT_RUN[@]}" -gt 0 ] || { echo "no agent CLIs available" >&2; exit 1; }
command -v cargo >/dev/null || { echo "need cargo" >&2; exit 1; }

mkdir -p "$OUT/runs"
CSV="$OUT/per-run.csv"
TRIAGE_PY="$here/agentic_triage.py"
echo "agent,model,task,difficulty,seconds,pass,rc,timeout,turns,tool_uses,agent_peak_mb,peak_cpu_pct,model_footprint_mb,repairs,verifier_kind,verdict,verdict_confidence,gateway_generation,context_window,mlx_active_mb,mlx_peak_mb,mlx_cache_mb" > "$CSV"
declare -A DIFF=( [greet]=1 [build]=2 [fix]=3 [test]=4 [debug]=5 [rpn]=6 [wordcount]=7 [multibug]=8 [leapday]=9 [apportion]=10 [duration]=11 [board]=12 )

# ── helpers ──────────────────────────────────────────────────────────────────

# Kill every descendant of $1 (not $1 itself), depth-first. Used on timeout to
# stop the agent so the rozum-launch parent unblocks, exits, and /usr/bin/time
# flushes its footprint line.
kill_descendants() {
  local pid="$1" c
  for c in $(pgrep -P "$pid" 2>/dev/null); do kill_descendants "$c"; kill -TERM "$c" 2>/dev/null; done
}

# One sample of a process tree (root + all descendants): total RSS (KB) + CPU%.
tree_sample() { # $1=root_pid
  ps -axo pid=,ppid=,rss=,pcpu= | awk -v root="$1" '
    { p=$1; ppid[p]=$2; rss[p]=$3; cpu[p]=$4; ids[++n]=p }
    END{
      inset[root]=1; changed=1
      while(changed){ changed=0
        for(i=1;i<=n;i++){ p=ids[i]; if(!inset[p] && inset[ppid[p]]){ inset[p]=1; changed=1 } } }
      r=0; c=0; for(i=1;i<=n;i++){ p=ids[i]; if(inset[p]){ r+=rss[p]; c+=cpu[p] } }
      printf "%d %.1f", r, c
    }'
}

# No-progress early-abort. Watches the agent's stream-json (`$alog`) while it runs and
# kills the cell EARLY — instead of burning the full RUN_TIMEOUT — once the agent stops
# making forward progress. Two signals:
#   (1) churn  — the last NP_REPEAT tool calls are byte-identical (name+input). The
#       gateway loop-breaker truncates each *generation*, but the agent CLI just re-issues
#       the same call next turn; nothing at the agent level ends that, so a stuck cell
#       otherwise loops to the timeout. This is the primary signal (mirrors loop-breaker Sig4).
#   (2) stall  — assistant turns keep advancing but the tool_use count lags by NP_STALL_TURNS
#       (the agent is talking, not acting). Secondary; only bites near the MAX_TURNS cap.
# Off with NP_ABORT=0. Requires jq (already a hard dep). Writes the reason to $3 for the caller.
NP_ABORT="${NP_ABORT:-1}"
NP_REPEAT="${NP_REPEAT:-5}"
NP_STALL_TURNS="${NP_STALL_TURNS:-8}"
NP_POLL="${NP_POLL:-5}"
NP_GRACE="${NP_GRACE:-25}"
no_progress_monitor() { # $1=alog  $2=lp  $3=reasonfile
  local alog="$1" lp="$2" rf="$3" sigs nt tail_uniq aturns reason
  # Let the agent get going before judging progress (initial reasoning/prefill).
  sleep "$NP_GRACE"
  while kill -0 "$lp" 2>/dev/null; do
    sleep "$NP_POLL"
    kill -0 "$lp" 2>/dev/null || break
    [ -s "$alog" ] || continue
    # One line per tool_use so far: name + serialized input.
    sigs=$(jq -rc 'select(.type=="assistant") | .message.content[]?
                   | select(.type=="tool_use") | [.name, (.input|tostring)] | @tsv' \
                   "$alog" 2>/dev/null)
    [ -n "$sigs" ] || continue
    nt=$(printf '%s\n' "$sigs" | grep -c .)
    reason=""
    # (1) churn: the last NP_REPEAT signatures are all identical.
    tail_uniq=$(printf '%s\n' "$sigs" | tail -n "$NP_REPEAT" | sort -u | grep -c .)
    if [ "$nt" -ge "$NP_REPEAT" ] && [ "$tail_uniq" = 1 ]; then
      reason="churn: identical tool call x${NP_REPEAT}"
    else
      # (2) stall: turns advanced far past tool_uses.
      aturns=$(grep -c '"type":"assistant"' "$alog" 2>/dev/null | tr -dc '0-9'); aturns=${aturns:-0}
      if [ "$((aturns - nt))" -ge "$NP_STALL_TURNS" ]; then
        reason="stall: ${aturns} turns / only ${nt} tool_uses"
      fi
    fi
    if [ -n "$reason" ]; then
      printf '%s\n' "$reason" >"$rf"
      kill_descendants "$lp"; kill -TERM "$lp" 2>/dev/null
      return
    fi
  done
}

# NOTE: the build/test parenthetical reads "put files here directly … src/ is expected and fine".
# The older wording "do NOT create a subdirectory" was AMBIGUOUS: a cautious model (gpt-oss-20b)
# read it literally and REFUSED to create src/ ("a valid Rust binary needs a src/ folder … I can't"),
# which dominated the codex×gpt-oss build reds — a prompt artifact, not a coding-skill failure.
# Clarifying it removed the refusal (Cargo.toml lands 3/3). See docs/matrix-failure-analysis.md Finding 5.
prompt_for() {
  local task="$1" agent="${2:-}"
  # The prompts live in scripts/bench/tasks.json, which the gateway ALSO reads to show them in the
  # UCC console. They used to be here and copied there, and five of six had drifted apart — the
  # console showed an older, shorter prompt than the model was given (BUGS.md
  # matrix-task-info-is-a-stale-copy). One source, two readers.
  #
  # The codex/opencode tool reminder stays HERE and not in the file, because it is about the AGENT,
  # not the task: two failure modes measured on weak models (2026-07-14, matrix diag) — they emit
  # code as prose instead of calling a file tool, and on CREATE-from-scratch they write to an
  # ABSOLUTE path, so nothing lands in the workdir. Which tasks take it IS in the file, because
  # that is a property of the task (`greet` needs no tools at all) and it used to be encoded only
  # in whether the arm was single- or double-quoted.
  local tool_hint=' IMPORTANT: use the Write tool or a Bash heredoc to create/modify files — outputting code as text or markdown is not sufficient. Write every file using a path RELATIVE to the current directory (e.g. `Cargo.toml`, `src/main.rs`); NEVER use an absolute path such as `/Cargo.toml` or `/tmp/...` — the files must land in the current working directory.'
  TASK="$task" AGENT="$agent" HINT="$tool_hint" TASKS_FILE="$(bench_tasks_file)" python3 -c '
import json, os, sys
doc = json.load(open(os.environ["TASKS_FILE"]))
t = doc["tasks"].get(os.environ["TASK"])
if t is None:
    sys.exit(1)
p = t["prompt"]
if t.get("tool_hint") and os.environ["AGENT"] in ("codex", "opencode"):
    p += os.environ["HINT"]
sys.stdout.write(p + "\n")   # echo added this; the callers depend on it
'
}

# Where the task definitions live. Same override shape as the other bench paths.
bench_tasks_file() {
  echo "${ROZUM_BENCH_TASKS:-$(dirname "${BASH_SOURCE[0]}")/tasks.json}"
}


write_agentic_meta() { # $1=workdir $2=agent $3=model $4=task $5=pass $6=timeout $7=rc $8=repairs
  {
    printf 'agent=%s\n' "$2"
    printf 'model=%s\n' "$3"
    printf 'task=%s\n' "$4"
    printf 'pass=%s\n' "$5"
    printf 'timeout=%s\n' "$6"
    printf 'rc=%s\n' "$7"
    printf 'repairs=%s\n' "$8"
  } >"$1/agentic.meta"
}


verify_task() { # $1=task  $2=workdir  $3=agent_log — echoes detail, returns 0=pass
  local t="$1" w="$2" log="$3" fail=0
  # Auto-rescue: opencode (and some models) create a subdirectory (e.g. reverse-cli/) instead of
  # putting files directly in the workdir. If Cargo.toml is missing but exactly one first-level
  # subdir has it, move the contents up silently so the verifier doesn't penalise a layout bug.
  if [ "$t" != greet ] && [ ! -f "$w/Cargo.toml" ]; then
    for sub in "$w"/*/; do
      if [ -f "${sub}Cargo.toml" ]; then
        cp -a "${sub}." "$w/" 2>/dev/null; rm -rf "$sub"; break
      fi
    done
  fi
  ( cd "$w"
    case "$t" in
      greet) grep -qiE '\bpong\b' "$log" && { echo "    PASS  said pong"; exit 0; } || { echo "    FAIL  no 'pong'"; exit 1; } ;;
      rpn)
        # From-scratch RPN evaluator. Verify a GENERAL implementation, not just the prompted
        # example: "3 4 + 5 *" -> 35 (the example) AND "5 1 2 + 4 * + 3 -" -> 14 (deeper nesting:
        # 5 + (1+2)*4 - 3). A hack that only handles the example fails the second.
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        o1="$(cargo run -q -- "3 4 + 5 *" 2>"$w/run.err" | tr -d '[:space:]')"
        [ "$o1" = 35 ] && echo "    PASS  '3 4 + 5 *' -> 35" || { echo "    FAIL  '3 4 + 5 *' -> '$o1'"; fail=1; }
        o2="$(cargo run -q -- "5 1 2 + 4 * + 3 -" 2>>"$w/run.err" | tr -d '[:space:]')"
        [ "$o2" = 14 ] && echo "    PASS  '5 1 2 + 4 * + 3 -' -> 14" || { echo "    FAIL  '5 1 2 + 4 * + 3 -' -> '$o2'"; fail=1; }
        exit $fail ;;
      wordcount)
        # From-scratch top-3 word frequency. The seeded input.txt (mixed case + a 3-count
        # tie apple/banana) case-folds to apple=3 banana=3 cherry=2 date=1 → top-3 desc,
        # tie alpha → apple/banana/cherry. Tests HashMap + sort + tie-break + case-fold + I/O.
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        got="$(cargo run -q -- input.txt 2>"$w/run.err" | tr -s ' \t' ' ' | sed 's/^ *//;s/ *$//' | grep -vE '^$')"
        want=$'apple 3\nbanana 3\ncherry 2'
        [ "$got" = "$want" ] && echo "    PASS  wordcount top-3 correct" \
          || { echo "    FAIL  wordcount -> $(echo "$got" | tr '\n' '|')"; fail=1; }
        exit $fail ;;
      duration)
        # THE VERIFIER IS THE TASK. `cargo test` is green in the seeded tree and stays green
        # whatever the agent does, so it cannot answer "is this right" — only running the program
        # can. Eight values, not the two the prompt gives as examples, so hard-coding the examples
        # fails here exactly as it does on `rpn`. Four of the eight are wrong on arrival: `0` prints
        # nothing at all, and everything from a day upward is rendered in hours.
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        # Say WHICH assertion, and whether the agent CHANGED a seeded one or shipped a bad test of
        # its own — those are different defects and the row used to call both "the seeded tests were
        # broken". Both have now been measured on this task: 2026-08-16 the 4B rewrote the seeded
        # `format(3661)` expectation to "1d 1h 1m 1s" (3661 seconds has no day in it), and after
        # the loop-breaker fix it left every seeded assertion intact and added its own
        # `format(86399) == "1d 23h 59m 59s"` — 86399 is one second SHORT of a day. Reporting the
        # second as the first would have sent the next reader looking for a fault we had fixed.
        if ! cargo test -q >"$w/cargo.out" 2>"$w/cargo.err"; then
          seeded_intact=1
          for a in 'assert_eq!(format(3661), "1h 1m 1s");' \
                   'assert_eq!(format(3600), "1h");' \
                   'assert_eq!(format(61), "1m 1s");'; do
            grep -qF "$a" src/lib.rs 2>/dev/null || seeded_intact=0
          done
          if [ "$seeded_intact" = 1 ]; then
            echo "    FAIL  cargo test is red (the seeded assertions are intact — a test the agent added is failing)"
          else
            echo "    FAIL  a seeded assertion was changed, and cargo test is red"
          fi
          grep -hE "^(thread .* panicked|  left:| right:)" "$w/cargo.out" "$w/cargo.err" 2>/dev/null \
            | head -6 | sed 's/^/          /'
          fail=1
        fi
        bad=0
        for pair in "3661:1h 1m 1s" "3600:1h" "61:1m 1s" "59:59s" "0:0s" \
                    "86400:1d" "90000:1d 1h" "90061:1d 1h 1m 1s"; do
          n="${pair%%:*}"; want="${pair#*:}"
          got="$(cargo run -q -- "$n" 2>>"$w/run.err" | sed 's/^ *//;s/ *$//')"
          [ "$got" = "$want" ] || { echo "    FAIL  $n -> '$got' (want '$want')"; bad=$((bad+1)); }
        done
        [ "$bad" = 0 ] && echo "    PASS  all 8 durations correct" || fail=1
        exit $fail ;;
      *)
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        if [ "$t" = test ] || [ "$t" = debug ] || [ "$t" = multibug ] || [ "$t" = leapday ] || [ "$t" = apportion ] || [ "$t" = board ]; then
          cargo test -q >/dev/null 2>"$w/cargo.err" && echo "    PASS  cargo test green" || { echo "    FAIL  cargo test red"; fail=1; }
        fi
        if [ "$t" = build ] || [ "$t" = test ] || [ "$t" = fix ]; then
          out="$(cargo run -q -- hello 2>"$w/run.err")"
          [ "$out" = olleh ] && echo "    PASS  cargo run -- hello -> olleh" || { echo "    FAIL  cargo run -> '$out'"; fail=1; }
        fi
        exit $fail ;;
    esac )
}

bounded_file_excerpt() { # $1=relative path
  local rel="$1" bytes
  [ -f "$rel" ] || return 0
  bytes=$(wc -c < "$rel" 2>/dev/null | tr -d ' ')
  if [ "${bytes:-0}" -gt 12000 ] 2>/dev/null; then
    echo "--- $rel (skipped: ${bytes} bytes)"
    return 0
  fi
  echo "--- $rel"
  sed -n '1,220p' "$rel"
}

repair_context_snapshot() {
  echo
  echo "Current bounded source/manifest snapshot:"
  bounded_file_excerpt Cargo.toml
  bounded_file_excerpt src/main.rs
  bounded_file_excerpt src/lib.rs
  find src -maxdepth 1 -name '*.rs' ! -name main.rs ! -name lib.rs 2>/dev/null | head -3 | while read -r f; do
    bounded_file_excerpt "$f"
  done
}

bench_package_name() { # $1=task
  case "$1" in
    rpn) echo "rpn-calc" ;;
    debug) echo "mathlib" ;;
    wordcount) echo "wordcount" ;;
    multibug) echo "twobugs" ;;
    apportion) echo "apportion" ;;
    duration) echo "duration" ;;
    # These two fell through to `reverse-cli` when they were added, so a model that broke the
    # manifest was told to name the package after a different task. Harmless to the verifier —
    # `cargo test` does not care what the crate is called — and wrong advice all the same.
    leapday) echo "calendar" ;;
    board) echo "board" ;;
    *) echo "reverse-cli" ;;
  esac
}

repair_tool_protocol_hint() {
  if [ -f agent.log ] && grep -qi 'File has not been read yet' agent.log; then
    cat <<'EOF'
Tool-protocol hint: your previous run tried Edit/Write before a same-run Read, then stopped after
saying you needed to read. Do NOT end with prose like "I will read it"; either make the Read tool call
as your first file action in this run, or use Bash with a single-quoted heredoc / python exact replace
to patch the tiny benchmark file. After patching, run the required cargo command.
EOF
  fi
}

repair_manifest_hint() { # $1=task $2=cargo-output
  local pkg
  pkg="$(bench_package_name "$1")"
  if printf '%s\n' "$2" | grep -qiE 'missing either a `?\[package\]`?|missing field `package.name`|invalid type: string.*expected a map|expected a table'; then
    cat <<EOF
Manifest hint: Cargo.toml needs a TOML table named [package] (NOT package = "..."). For this task the
minimal valid manifest is:

[package]
name = "$pkg"
version = "0.1.0"
edition = "2021"

[dependencies]

If the current manifest is malformed, replace the whole tiny Cargo.toml with that content.
EOF
  elif printf '%s\n' "$2" | grep -qiE 'failed to parse the edition key|unknown edition|this version of Cargo is older'; then
    echo 'Manifest hint: use a Cargo.toml edition supported by this bench toolchain, normally edition = "2021".'
  fi
}

# Ground-truth diagnostic for a failed cell — the REAL compiler/test output (not the model's
# self-report). First check it compiles; if so, check the task's runtime behavior. This is the
# exact text fed back to the agent for repair. Echoes a short, actionable diagnostic.
repair_diagnostic() { # $1=task  $2=workdir
  ( cd "$2" 2>/dev/null || exit 0
    repair_tool_protocol_hint
    # Structural check FIRST — agents commonly write code to the WRONG path. `cargo` only builds
    # src/main.rs (+ src/lib.rs, src/bin/*); if the real implementation sits at the repo root while
    # src/main.rs is missing or still the default "Hello, world!" stub, the build "passes" but the
    # program that runs is NOT the agent's code — and a runtime-only diagnostic ("output is X") never
    # reveals why. Surface the placement so repair can converge instead of thrashing.
    # Check for the common opencode/weak-model mistake: creating a subdirectory (e.g. reverse-cli/)
    # instead of writing directly to the workdir. The auto-rescue in verify_task already moves
    # contents up, but if we reach repair_diagnostic after a non-rescued state, surface it clearly.
    if [ ! -f Cargo.toml ]; then
      for sub in */; do
        if [ -f "${sub}Cargo.toml" ]; then
          echo "WRONG DIRECTORY: your files are in ./${sub} but the benchmark expects them in the current directory ($(pwd)). Fix: cd .. && mv ${sub}* . && mv ${sub}src . 2>/dev/null; rm -rf ${sub}"
          exit 0
        fi
      done
    fi
    src_is_stub=0
    if [ -f src/main.rs ] && grep -q 'Hello, world!' src/main.rs && [ "$(wc -l < src/main.rs)" -le 5 ]; then
      src_is_stub=1
    fi
    stray="$(find . -maxdepth 1 -name '*.rs' 2>/dev/null | sed 's#^\./##' | head -1)"
    if [ -n "$stray" ] && { [ ! -f src/main.rs ] || [ "$src_is_stub" = 1 ]; }; then
      echo "WRONG FILE LOCATION: your code is in ./$stray, but \`cargo\` ONLY builds src/main.rs (currently $([ -f src/main.rs ] && echo 'the default \"Hello, world!\" stub' || echo 'missing'), so the program that runs is NOT your code). Move your implementation into src/main.rs: \`mkdir -p src && mv ./$stray src/main.rs\` (overwrite the stub). Then build and run."
      repair_context_snapshot
      exit 0
    fi
    other_src="$(find src -maxdepth 1 -name '*.rs' ! -name 'main.rs' 2>/dev/null | tr '\n' ' ')"
    if [ "$src_is_stub" = 1 ] && [ -n "$other_src" ]; then
      echo "WRONG ENTRY POINT: src/main.rs is still the default \"Hello, world!\" stub, so cargo runs it and ignores your code in ${other_src}. Put the program's main() + logic in src/main.rs (or declare the other files as modules and call them from main)."
      repair_context_snapshot
      exit 0
    fi
    # Manifest present but NO build target at all: the model wrote Cargo.toml and stopped without
    # ever creating src/main.rs (the dominant `test`-cell near-miss — reasoning was fine, delivery
    # was incomplete). `cargo build` here only emits the opaque "no targets specified in the manifest";
    # give a DIRECTIVE fix instead so the retry actually lands the missing file.
    if [ -f Cargo.toml ] && ! ls src/*.rs >/dev/null 2>&1; then
      echo "INCOMPLETE PROJECT: Cargo.toml exists but there is NO src/main.rs, so \`cargo\` has no build target (\"no targets specified in the manifest\"). You created the manifest and stopped — you must ALSO create src/main.rs with the actual implementation. Create it now, then run the required cargo command(s)."
      repair_context_snapshot
      exit 0
    fi
    if ! berr="$(cargo build 2>&1)"; then
      echo "The project does NOT compile. \`cargo build\` reports:"
      repair_manifest_hint "$1" "$berr"
      printf '%s\n' "$berr" | grep -vE '^\s*Compiling|^\s*Finished|^\s*Updating|Blocking|Downloaded' | head -40
      repair_context_snapshot
      exit 0
    fi
    case "$1" in
      test|debug)
        echo "It compiles, but \`cargo test\` is RED:"
        cargo test 2>&1 | grep -vE '^\s*Compiling|^\s*Finished|running [0-9]' | head -40
        repair_context_snapshot ;;
      rpn)
        o1="$(cargo run -q -- "3 4 + 5 *" 2>&1)"; o2="$(cargo run -q -- "5 1 2 + 4 * + 3 -" 2>&1)"
        echo "It compiles, but the result is wrong: \`cargo run -- \"3 4 + 5 *\"\` printed '$o1' (must be 35); \`cargo run -- \"5 1 2 + 4 * + 3 -\"\` printed '$o2' (must be 14). Evaluate ANY valid RPN expression with a stack."
        repair_context_snapshot ;;
      *)
        o="$(cargo run -q -- hello 2>&1)"
        echo "It compiles, but \`cargo run -- hello\` printed '$o' (must be exactly: olleh)."
        repair_context_snapshot ;;
    esac )
}

repair_goal_hint() { # $1=task
  case "$1" in
    build) echo 'Required final behavior: this must be the reverse-cli task. Cargo.toml package name "reverse-cli", edition "2021", and src/main.rs must reverse the first command-line argument. `cargo run -- hello` must print exactly `olleh`; a generic Hello World project is still wrong.' ;;
    test) echo 'Required final behavior: implement reverse(s) plus the requested unit test. `cargo test` must pass and `cargo run -- hello` must print exactly `olleh`; scaffolding or Hello World is still wrong.' ;;
    fix) echo 'Required final behavior: fix the existing reverse bug with a minimal change. `cargo run -- hello` must print exactly `olleh`; returning `hello` or merely compiling is still wrong.' ;;
    debug) echo 'Required final behavior: fix src/lib.rs without changing the test. `cargo test` must pass; merely compiling is still wrong.' ;;
    rpn) echo 'Required final behavior: implement a real stack-based RPN evaluator. `cargo run -- "3 4 + 5 *"` must print `35` and `cargo run -- "5 1 2 + 4 * + 3 -"` must print `14`; hard-coding one example is still wrong.' ;;
    wordcount) echo 'Required final behavior: read the file at argv[1], count words case-insensitively, print the top 3 by count (ties broken alphabetically) one per line as `word count`. `cargo run -- input.txt` must print exactly `apple 3` / `banana 3` / `cherry 2`; a hard-coded or count-only output is still wrong.' ;;
    multibug) echo 'Required final behavior: fix BOTH bugs in src/lib.rs (add and is_even) without changing the tests. `cargo test` must pass ALL tests; fixing only one is still wrong.' ;;
    *) echo 'Required final behavior: satisfy the original task prompt and the verifier, not just the compiler.' ;;
  esac
}

repair_benchmark_recipe() { # $1=task
  case "$1" in
    build)
      cat <<'EOF'
Benchmark repair script for this tiny reverse-cli build project. Do not use apply_patch, Edit,
cargo init, or line patches. If the manifest/source is malformed, replace the whole tiny project
with this exact script and run the check:

```sh
mkdir -p src
cat > Cargo.toml <<'EOT'
[package]
name = "reverse-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
EOT
cat > src/main.rs <<'EOT'
use std::env;

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    let out: String = arg.chars().rev().collect();
    println!("{out}");
}
EOT
cargo run -- hello
```
EOF
      ;;
    fix)
      cat <<'EOF'
Benchmark repair script for this tiny reverse-cli fix project. Do not use apply_patch, Edit,
cargo init, or line patches. If incremental Edit has corrupted src/main.rs, replace the whole tiny
file with this exact content and run the check:

```sh
cat > src/main.rs <<'EOT'
use std::env;

/// Reverse a string by characters.
fn reverse(s: &str) -> String {
    s.chars().rev().collect::<String>()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}
EOT
cargo run -- hello
```
EOF
      ;;
    test)
      cat <<'EOF'
Benchmark repair script for this tiny reverse-cli test project. Do not use apply_patch, Edit,
cargo init, or line patches. If the manifest/source is malformed, replace both tiny files with this
exact script, then run both required checks:

```sh
mkdir -p src
cat > Cargo.toml <<'EOT'
[package]
name = "reverse-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
EOT
cat > src/main.rs <<'EOT'
use std::env;

fn reverse(s: &str) -> String {
    s.chars().rev().collect()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_hello() {
        assert_eq!(reverse("hello"), "olleh");
    }
}
EOT
cargo test
cargo run -- hello
```
EOF
      ;;
    debug)
      cat <<'EOF'
Benchmark repair script for this tiny mathlib debug project. Do not use apply_patch, Edit,
cargo init, or line patches. If src/lib.rs is syntactically corrupt, replace the whole tiny file
with this exact content and run the required test:

```sh
cat > src/lib.rs <<'EOT'
/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }
}
EOT
cargo test
```
EOF
      ;;
    rpn)
      cat <<'EOF'
Benchmark repair script for this tiny rpn-calc project. Do not use apply_patch, Edit, cargo init,
or line patches. Do not hard-code one example. Prefer the one-line command below for opencode/tool
JSON compatibility; copy it as one bash command and do not reformat it into heredocs:

```sh
mkdir -p src && printf '%s\n' '[package]' 'name = "rpn-calc"' 'version = "0.1.0"' 'edition = "2021"' '' '[dependencies]' > Cargo.toml && printf '%s\n' 'use std::env;' '' 'fn main() {' '    let expr = env::args().nth(1).expect("missing expression");' '    let mut stack: Vec<i64> = Vec::new();' '' '    for token in expr.split_whitespace() {' '        match token {' '            "+" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a + b);' '            }' '            "-" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a - b);' '            }' '            "*" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a * b);' '            }' '            "/" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a / b);' '            }' '            n => stack.push(n.parse::<i64>().unwrap()),' '        }' '    }' '' '    println!("{}", stack.pop().unwrap());' '}' > src/main.rs && cargo run -- "3 4 + 5 *" && cargo run -- "5 1 2 + 4 * + 3 -"
```

Fallback multiline script:

```sh
mkdir -p src
cat > Cargo.toml <<'EOT'
[package]
name = "rpn-calc"
version = "0.1.0"
edition = "2021"

[dependencies]
EOT
cat > src/main.rs <<'EOT'
use std::env;

fn main() {
    let expr = env::args().nth(1).expect("missing expression");
    let mut stack: Vec<i64> = Vec::new();

    for token in expr.split_whitespace() {
        match token {
            "+" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a + b);
            }
            "-" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a - b);
            }
            "*" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a * b);
            }
            "/" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a / b);
            }
            n => stack.push(n.parse::<i64>().unwrap()),
        }
    }

    println!("{}", stack.pop().unwrap());
}
EOT
cargo run -- "3 4 + 5 *"
cargo run -- "5 1 2 + 4 * + 3 -"
```
EOF
      ;;
  esac
}

repair_agent_profile_hint() { # $1=agent
  case "$1" in
    opencode)
      echo 'Agent delivery profile: opencode tool calls are sensitive to JSON escaping. Prefer one-line Bash commands with printf arguments over nested heredocs or large multiline JSON strings.' ;;
    codex)
      echo 'Agent delivery profile: codex on weak local models often line-patches tiny corrupted files poorly. Prefer a single shell command that replaces the whole tiny file/project, then run the verifier.' ;;
    claude)
      echo 'Agent delivery profile: claude usually handles the normal Read/Edit/Bash path well; keep the repair minimal, then run the exact verifier command.' ;;
    *)
      echo 'Agent delivery profile: use the simplest tool call shape that writes the required files and runs the verifier.' ;;
  esac
}

# The repair instruction handed back to the agent. It pins the exact error, restates the task goal,
# includes an agent-specific delivery hint, and demands a REAL run before claiming success.
repair_prompt() { # $1=task $2=diagnostic $3=agent
  local task="$1" diagnostic="$2" agent="${3:-}" recipe profile
  profile="$(repair_agent_profile_hint "$agent")"
  recipe="$(repair_benchmark_recipe "$task")"
  if [ -n "$recipe" ]; then
    printf 'The Rust benchmark project in the CURRENT directory is NOT correct yet.\n\n%s\n\n%s\n\nCurrent verifier/build evidence:\n\n%s\n\nBENCHMARK REPAIR MODE: this is a tiny deterministic Rust benchmark, not a real application repo. The current files may be malformed. Do NOT use apply_patch, Edit, cargo init, sed/perl line patches, or a prose-only answer. Your next action should be ONE shell command in the CURRENT directory that runs the full benchmark repair script below exactly, replacing the tiny benchmark files. Then run the required cargo command(s) and stop only after they really pass.\n\n%s' "$(repair_goal_hint "$task")" "$profile" "$diagnostic" "$recipe"
    return
  fi
  printf 'The Rust project in the CURRENT directory is NOT correct yet — do NOT start over or recreate files, FIX what is there.\n\n%s\n\n%s\n\nCurrent verifier/build evidence:\n\n%s\n\nMake the minimal change to satisfy the REQUIRED FINAL BEHAVIOR, then ACTUALLY RUN the build/test yourself and read the output. Do not stop at `cargo build` if the required command is `cargo run` or `cargo test`. If you use Edit, call Read on that file first and copy old_string exactly from the current file; if Edit says "String to replace not found", re-read the file. Only confirm success if the required command really succeeded; if it still fails, keep fixing.' "$(repair_goal_hint "$task")" "$profile" "$diagnostic"
}

# ── main loop ────────────────────────────────────────────────────────────────

echo "rozum agentic benchmark (real rozum launch)"
echo "  binary       : $BIN"
echo "  agents       : ${AGENT_RUN[*]}    models: ${#MODELS[@]}    tasks: ${TASK_LIST[*]}"
echo "  run timeout  : ${RUN_TIMEOUT}s   gen timeout: ${GEN_TIMEOUT}s   ctx: ${NCTX:-auto(max)}   verify-repair: $([ "$REPAIR" -gt 0 ] && echo "ON (up to $REPAIR retries)" || echo off)"
echo "  out          : $OUT"
echo "  decode       : $DECODE_NOTE"
printf 'decode: %s\nseed: %s\nforce_greedy: %s\n' \
  "$DECODE_NOTE" "${ROZUM_SAMPLING_SEED:-<none>}" "${ROZUM_FORCE_GREEDY:-0}" > "$OUT/run-info.txt"
echo

# Stop whatever gateway is resident — EXCEPT when we were told to share one. Without this guard
# the shared mode kills the very gateway it means to borrow: measured 2026-08-06, the operator's
# :8089 went down here and `rozum launch` then reported "no gateway running" for every cell.
# launchd's KeepAlive brought it back, which is the only reason that was seconds and not an
# outage — do not lean on it.
# SHARE BY DEFAULT (2026-08-07, operator's call). On a host that fits ONE model, a private gateway
# per spec loads a SECOND copy of the same weights — which does not fit, so it waits in the
# admission queue behind the resident one, and the `stop --force` below evicts the operator's
# gateway to make room for a duplicate of what it was already serving. Borrow instead.
#
# `BENCH_GATEWAY_URL` still forces a specific endpoint. `BENCH_DEDICATED=1` forces the old
# per-model process, which is what you want when the numbers are the point: only a gateway this
# harness started can be measured with `/usr/bin/time -l`, and only separate processes bound the
# blast radius of a Metal fault (BUG-001, BUG-003).
if [ -z "${BENCH_GATEWAY_URL:-}" ] && [ "${BENCH_DEDICATED:-0}" != 1 ]; then
  # Flatten first, then match once. Matching line-by-line was fragile enough to extract the model
  # and silently miss the port in the same run — and a half-parsed status reads as "no gateway",
  # which would quietly restore the very behaviour this block removes.
  _gw_json="$("$BIN" gateway status --json 2>/dev/null | tr -d '\n ' || true)"
  _gw_model="$(printf '%s' "$_gw_json" | sed -n 's/.*"model":"\([^"]*\)".*/\1/p')"
  _gw_port="$(printf '%s' "$_gw_json" | sed -n 's/.*"port":\([0-9]*\).*/\1/p')"
  # Borrow ONLY when it already serves the spec under test. Switching a shared gateway's model
  # would evict whatever the operator is using, which is the behaviour this change exists to stop.
  if [ -n "$_gw_port" ] && [ "$_gw_model" = "${MODELS[0]}" ] && [ "${#MODELS[@]}" = 1 ]; then
    BENCH_GATEWAY_URL="http://127.0.0.1:$_gw_port"
    echo "sharing the running gateway on :$_gw_port (already serving $_gw_model)"
    echo "  → pass/fail is valid; SECONDS and FOOTPRINT are not this run's. BENCH_DEDICATED=1 for those."
  elif [ -n "$_gw_port" ]; then
    echo "a gateway is running on :$_gw_port with '$_gw_model'; this run wants ${MODELS[*]} — starting its own"
  fi
fi
[ -n "${BENCH_GATEWAY_URL:-}" ] || "$BIN" gateway stop --force >/dev/null 2>&1 || true
idx=0
for spec in "${MODELS[@]}"; do
  port=$((PORT_BASE + idx)); idx=$((idx + 1)); base="http://127.0.0.1:$port"
  # CSV-safe model label: a pipeline spec ("A,B") has a comma that would split the CSV's
  # comma-delimited columns (corrupting the model column AND every naive `-F,` reader below —
  # the footprint backfill, `column -s,`, the summary). Replace it with `+` so every row keeps
  # a stable field count; the label reads "A+B" (a pipeline). The full spec stays in `$spec`.
  spec_csv="${spec//,/+}"
  glog="$OUT/runs/${spec//[:\/]/_}.gateway.log"
  echo "================ model: $spec  (port $port) ================"

  # Load the model ONCE: a shared gateway under /usr/bin/time -l. The backend's
  # cap_mlx_memory keeps the MLX cache bounded (`ROZUM_MLX_CACHE_GB`), so it no longer
  # accumulates across the model's tasks (which used to grow to ~28 GB and starve the
  # next agent — the rc=2 cascade). Each task runs `rozum launch` (no --model) → reuse.
  # ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 is REQUIRED: with the default the gateway is reloadable,
  # so the lifecycle watchdog spawns and its `clients_gone` branch (gateway.rs ~2906) self-
  # exits the process the instant one agent's leases drop — killing the shared gateway in the
  # gap between the claude and codex phases, so every later codex task sees a dead gateway and
  # returns rc=2 at 0.0s (looks like "codex is broken"; it isn't). =0 disables the watchdog so
  # the load-once gateway survives all agents. See BUGS.md BUG-001 / project-agentic-bench-clients-gone.
  # (ROZUM_SAMPLING_SEED is set once, exported, near the top — it has to reach `rozum launch`
  # too, not just a gateway this harness spawns.)
  # BENCH_GATEWAY_URL: run against a gateway that is ALREADY serving this model instead of
  # loading a second copy.
  #
  # Two agents CAN share one resident model — `rozum launch` reuses a healthy gateway whose model
  # matches, and the gateway admits 2 concurrent requests. What cannot coexist is two GATEWAYS
  # each holding ~12 GB of the same weights, which is why this harness (which deliberately loads
  # its own, to measure RSS and time in isolation) waits in the admission queue while somebody
  # else has the model resident.
  #
  # So this knob buys a matrix that can run beside a colleague, and it costs the thing the private
  # gateway was for: **timings become contended and the per-model memory figures are not this
  # run's**. Use it for pass/fail; do not read seconds or footprint from a shared run, and the
  # summary says so out loud rather than trusting whoever reads the CSV later to remember.
  if [ -n "${BENCH_GATEWAY_URL:-}" ]; then
    base="$BENCH_GATEWAY_URL"
    echo "  sharing an existing gateway at $base — pass/fail only, timings are contended"
    if ! curl -s -m5 "$base/v1/models" >/dev/null 2>&1; then
      echo "  ! $base does not answer — nothing to share" >&2; exit 1
    fi
    # Answering is not the same as being the gateway the AGENT will use (`agent_gateway_mismatch`).
    _active_port="$("$BIN" gateway status --json 2>/dev/null | tr -d '\n ' | sed -n 's/.*"port":\([0-9]*\).*/\1/p')"
    _why="$(agent_gateway_mismatch "$base" "$_active_port")"
    if [ -n "$_why" ]; then
      echo "  ! $_why" >&2
      echo "  ! refusing: this run would report a cell against a gateway that never served it." >&2
      echo "  ! Point BENCH_GATEWAY_URL at the active gateway, unset it (it is borrowed by default)," >&2
      echo "  ! or use BENCH_DEDICATED=1 for a private one." >&2
      exit 1
    fi
    TIME_PID=""; GW_PID=""; SHARED_GW=1
  else
  SHARED_GW=0
  ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 ROZUM_GEN_TIMEOUT_SECS="$GEN_TIMEOUT" \
    /usr/bin/time -l \
    "$BIN" gateway --model "$spec" --port "$port" --offline "${NCTX_OPT[@]}" \
    >"$glog" 2>&1 &
  TIME_PID=$!
  GW_PID=""; for _ in $(seq 1 40); do GW_PID="$(pgrep -P "$TIME_PID" 2>/dev/null | head -1)"; [ -n "$GW_PID" ] && break; sleep 0.25; done
  fi
  ok=0
  # GW_READY_SECS: how long to wait for the gateway to answer. Default 240 covers a plain
  # load; raise it (with ROZUM_GATEWAY_RESIDENCY_WAIT_SECS) when the gateway may sit in the
  # host-wide admission QUEUE behind other RAM users (e.g. a sibling's sbt test) — the queue
  # is the coordination mechanism, the bench just has to be patient enough to use it.
  for _ in $(seq 1 "${GW_READY_SECS:-240}"); do curl -s -m2 "$base/v1/models" >/dev/null 2>&1 && { ok=1; break; }
    [ "$SHARED_GW" = 1 ] || kill -0 "$TIME_PID" 2>/dev/null || break; sleep 1; done
  if [ "$ok" != 1 ]; then echo "  ! gateway not ready (see $glog)"; [ "$SHARED_GW" = 1 ] || { kill -INT "$GW_PID" 2>/dev/null; wait "$TIME_PID" 2>/dev/null; }; continue; fi
  echo "  model loaded once; running ${#TASK_LIST[@]} tasks × ${#AGENT_RUN[@]} agent(s)"

  for agent in "${AGENT_RUN[@]}"; do
    for task in "${TASK_LIST[@]}"; do
      # IS THE GATEWAY STILL THERE? The readiness check above runs ONCE, before the loop, and a
      # gateway can go away mid-run: measured 2026-08-13, the shared one took a shutdown signal
      # after the fourth task and the remaining four each "failed" in ZERO SECONDS against a dead
      # port. Those rows are not measurements — `pass=0 rc=2 0.0s` reads afterwards as "the model
      # could not do it", which is the one thing this harness must never say by accident.
      gw_back=0
      for _ in $(seq 1 "${GW_RECOVER_SECS:-120}"); do
        curl -s -m2 "$base/v1/models" >/dev/null 2>&1 && { gw_back=1; break; }
        sleep 1
      done
      if [ "$gw_back" != 1 ]; then
        echo "  ! gateway at $base stopped answering — abandoning the remaining tasks rather than"
        echo "    recording zero-second failures against a dead port (see $glog)"
        break 2
      fi
      diff=${DIFF[$task]:-0}
      eff_timeout="$(effective_run_timeout "$agent" "$spec")"   # QW1: per-cell ceiling (driver/model-aware)
      work="$(mktemp -d /tmp/rozum-agentic-XXXXXX)"
      setup_task "$task" "$work"
      write_agentic_meta "$work" "$agent" "$spec_csv" "$task" "" "" "" "0"
      alog="$work/agent.log"; sfile="$work/samples.txt"
      prompt="$(prompt_for "$task" "$agent")"
      # Verify-repair loop. Attempt 1 = the task; if `verify_task` (the REAL build/test, not the
      # model's self-report) FAILS, feed the actual compiler/test error back and let the agent fix
      # it, in the SAME workdir, up to REPAIR more attempts. REPAIR=0 → exactly one attempt (legacy).
      pass=0; repairs=0; secs_total=0; rc=0; turns="-"; tools="-"; tmo=0; detail=""
      attempts=$(( REPAIR + 1 )); bonus_used=0; attempt=0
      # `while` (not `for`) so a one-time bonus repair attempt can extend `attempts` mid-loop (R2.5).
      while [ "$attempt" -lt "$attempts" ]; do
        attempt=$((attempt + 1))
        # Build the runner per agent from the CURRENT $prompt (task prompt, or the repair prompt on
        # a retry). ALL THREE route through `rozum launch` (no --model): reuse the resident shared
        # gateway + jail the agent (Seatbelt). claude via Anthropic env, codex via injected provider
        # flags, opencode via a written provider config (+ `-m rozum/local`).
        if [ "$agent" = claude ]; then
          # --lean strips non-coding tools (incl. AskUserQuestion, which in headless `-p` can't be
          # answered → a model that calls it to "verify" loops till timeout): 33 tools/~4.9K → 4/~0.8K.
          aargs=(claude -p "$prompt" --output-format stream-json --verbose
                 --dangerously-skip-permissions --max-turns "$MAX_TURNS")
          runner=("$BIN" launch --no-channel-wakeup --no-piggyback --lean "${aargs[@]}")
        elif [ "$agent" = codex ]; then
          aargs=(codex exec "$prompt" --dangerously-bypass-approvals-and-sandbox)
          runner=("$BIN" launch --no-channel-wakeup --no-piggyback "${aargs[@]}")
        elif [ "$agent" = nadia ]; then
          # nadia headless = `nadia run <prompt>`. No provider flags and no tool_hint: it
          # reads OPENAI_BASE_URL / ROZUM_GATEWAY_URL, which `rozum launch` already exports
          # to every agent, and its own system prompt covers "call tools, don't write prose".
          # Its workspace defaults to cwd, which `rozum launch` has already jailed.
          aargs=(nadia run "$prompt")
          runner=("$BIN" launch --no-channel-wakeup --no-piggyback "${aargs[@]}")
        else  # opencode — `rozum launch` wires the gateway provider + -m rozum/local
          aargs=(opencode run "$prompt")
          runner=("$BIN" launch --no-channel-wakeup --no-piggyback "${aargs[@]}")
        fi

        start=$(perl -MTime::HiRes=time -e 'printf "%.2f", time')
        # ROZUM_VERIFY=0: bench has its own verify_task; the gateway's derive_target
        # misclassifies pure-text tasks (e.g. greet) as cargo projects via the model's
        # interpretation of the prompt, spawning a repair loop that wastes the full RUN_TIMEOUT.
        npfile="$work/noprogress.reason"; rm -f "$npfile"
        # ONE SEED PER CELL, not one per run. A fixed seed across repetitions would make REPS>1
        # report a pass-RATE computed from near-identical draws — the opposite of what it is for,
        # and a hole I would have opened myself by exporting a single default into the shared path
        # (before this change shared runs had no seed at all, so their reps were independent). Base
        # + index keeps each cell reproducible on its own while the reps stay separate samples.
        CELL_N=$((CELL_N + 1))
        cell_seed=""
        [ -n "${ROZUM_SAMPLING_SEED:-}" ] && cell_seed=$((ROZUM_SAMPLING_SEED + CELL_N))
        ( cd "$work"; exec env ROZUM_VERIFY=0 ${cell_seed:+ROZUM_SAMPLING_SEED=$cell_seed} "${runner[@]}" ) </dev/null >"$alog" 2>&1 &
        LP=$!
        # Agent-tree RSS + (agent + gateway) CPU; the model's RAM is the gateway footprint.
        ( while kill -0 "$LP" 2>/dev/null; do
            read ar ac < <(tree_sample "$LP")
            gc=$([ -n "$GW_PID" ] && ps -o pcpu= -p "$GW_PID" 2>/dev/null | tr -d ' '); gc=${gc:-0}
            awk -v ar="${ar:-0}" -v ac="${ac:-0}" -v gc="$gc" 'BEGIN{printf "%d %.1f\n", ar, ac+gc}' >>"$sfile"
            sleep 2
          done ) & SAMP=$!
        ( sleep "$eff_timeout"; kill_descendants "$LP"; kill -TERM "$LP" 2>/dev/null ) & WD=$!
        # No-progress early-abort (claude stream-json only). Kills LP itself on churn/stall,
        # so `wait` returns before the timeout; the watchdog is a no-op in that case.
        NPM=""
        if [ "$agent" = claude ] && [ "$NP_ABORT" = 1 ]; then
          no_progress_monitor "$alog" "$LP" "$npfile" & NPM=$!
        fi
        wait "$LP"; rc=$?
        kill "$WD" "$SAMP" $NPM 2>/dev/null; wait "$SAMP" 2>/dev/null
        if [ -s "$npfile" ]; then
          echo "    ⨯ no-progress early-abort — $(cat "$npfile") (saved $(awk -v s="$(perl -MTime::HiRes=time -e 'printf "%.0f", time-'"$start")" -v t="$eff_timeout" 'BEGIN{d=t-s; print (d>0)?d:0}')s vs timeout)"
        fi
        asecs=$(perl -MTime::HiRes=time -e 'printf "%.1f", time-'"$start")
        secs_total=$(awk -v a="$secs_total" -v b="$asecs" 'BEGIN{printf "%.1f", a+b}')
        tmo=$(awk -v s="$asecs" -v t="$eff_timeout" 'BEGIN{print (s>=t-2)?1:0}')

        detail="$(verify_task "$task" "$work" "$alog")"; verify_rc=$?
        printf '%s\n' "$detail" >"$work/verify.out"
        pass=$([ "$verify_rc" = 0 ] && echo 1 || echo 0)
        [ "$pass" = 1 ] && break
        # R2.5: a repair attempt that fell into an Edit-before-Read churn loop ("File has not been
        # read yet") burns the whole RUN_TIMEOUT without converging, and `repair_tool_protocol_hint`
        # (which keys off exactly that marker) fires one attempt too late — the loop is in the FINAL
        # attempt, so no further attempt applies the hint. Grant ONE bonus attempt when the marker
        # first appears so the hint actually gets a shot.
        if [ "$attempt" -ge "$attempts" ] && [ "$bonus_used" = 0 ] \
           && grep -qi 'File has not been read yet' "$alog" 2>/dev/null; then
          bonus_used=1; attempts=$((attempts + 1))
          echo "    + bonus repair attempt (Edit-before-Read loop detected → applying tool-protocol hint)"
        fi
        [ "$attempt" -lt "$attempts" ] || break   # last attempt — no more repair
        repairs=$((repairs + 1))
        diag="$(repair_diagnostic "$task" "$work")"
        prompt="$(repair_prompt "$task" "$diag" "$agent")"
        echo "    ↻ repair $repairs/$REPAIR — verify FAILED, feeding the real build/test error back to $agent"
      done

      agent_mb=$(awk '{if($1>m)m=$1}END{printf "%.0f", m/1024}' "$sfile" 2>/dev/null | tr -dc '0-9'); agent_mb=${agent_mb:-0}
      peak_cpu=$(awk '{if($2>m)m=$2}END{printf "%.0f", m}' "$sfile" 2>/dev/null | tr -dc '0-9'); peak_cpu=${peak_cpu:-0}
      if [ "$agent" = claude ]; then
        turns=$(grep -c '"type":"assistant"' "$alog" 2>/dev/null | tr -dc '0-9'); turns=${turns:-0}
        tools=$(grep -o '"type":"tool_use"' "$alog" 2>/dev/null | wc -l | tr -dc '0-9'); tools=${tools:-0}
      fi

      # The classification lives in `classify_rc` (defined above, with the reasoning and the
      # exit-code table). Called HERE, after the verify loop, because verify_task hoists a nested
      # project up into the workdir and the codes are about what cargo can build afterwards.
      raw_rc=$rc
      rc="$(classify_rc "$task" "$work" "$tmo" "$raw_rc" "$pass")"

      [ "$tmo" = 1 ] && tflag=" (RUN_TIMEOUT)" || tflag=""
      rflag=""; [ "$repairs" -gt 0 ] && rflag=" repairs=$repairs"
      write_agentic_meta "$work" "$agent" "$spec_csv" "$task" "$pass" "$tmo" "$rc" "$repairs"
      printf "  [%s] %-6s %ss%s  pass=%s%s  agent=%sMB  cpu=%s%%  turns=%s tools=%s\n" \
        "$agent" "$task" "$secs_total" "$tflag" "$pass" "$rflag" "$agent_mb" "$peak_cpu" "$turns" "$tools"
      echo "$detail"
      if [ -f "$TRIAGE_PY" ] && command -v python3 >/dev/null 2>&1; then
        triage="$(python3 "$TRIAGE_PY" --brief "$work" 2>/dev/null || true)"
        [ -n "$triage" ] && echo "    TRIAGE $triage"
        printf '%s\n' "${triage:-}" > "$work/triage.out"
      fi

      # Memory × correctness evidence. `/stats` exposes MLX unified-memory counters that process
      # RSS misses. `peak` is generation-scoped by the native runtime; `active` and `cache` show what
      # this run leaves resident (notably retained prefix KV + allocator cache).
      # Missing stats are empty fields — remote/non-MLX backends remain valid CSV rows.
      gw_generation=""; context_window=""; mlx_active=""; mlx_peak=""; mlx_cache=""
      stats_json="$(curl -fsS -m2 "$base/stats" 2>/dev/null || true)"
      if [ -n "$stats_json" ] && command -v jq >/dev/null 2>&1; then
        gw_generation="$(printf '%s' "$stats_json" | jq -r '.generation // ""')"
        context_window="$(printf '%s' "$stats_json" | jq -r '.context_window // ""')"
        mlx_active="$(printf '%s' "$stats_json" | jq -r '.mlx_memory_mb.active // ""')"
        mlx_peak="$(printf '%s' "$stats_json" | jq -r '.mlx_memory_mb.peak // ""')"
        mlx_cache="$(printf '%s' "$stats_json" | jq -r '.mlx_memory_mb.cache // ""')"
      fi
      verifier_kind="benchmark-deterministic"
      if [ "$pass" = 1 ]; then
        verdict="pass"; verdict_confidence="1"
      elif [ "$rc" = 2 ]; then
        verdict="unknown"; verdict_confidence="0"
      else
        verdict="fail"; verdict_confidence="1"
      fi

      printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$agent" "$spec_csv" "$task" "$diff" "$secs_total" "$pass" "$rc" "$tmo" "$turns" "$tools" "$agent_mb" "$peak_cpu" "" "$repairs" \
        "$verifier_kind" "$verdict" "$verdict_confidence" "$gw_generation" "$context_window" "$mlx_active" "$mlx_peak" "$mlx_cache" >> "$CSV"

      # A red keeps its evidence in the results directory, always: the workdir is a temp path that
      # the next run's `rm -rf` (or a reboot) takes away, and a failure nobody can reopen is the
      # one this harness must never produce. KEEP=1 still keeps every cell, in place.
      if [ "$pass" != 1 ]; then
        _kept="$(preserve_cell "$work" "$OUT/runs" "${agent}-${spec_csv//[:\/]/_}-${task}")"
        [ -n "$_kept" ] && echo "    evidence: $_kept"
      fi
      [ "${KEEP:-0}" = 1 ] && echo "    kept: $work" || rm -rf "$work"
    done
  done

  # Stop the shared gateway GRACEFULLY. A `kill -KILL` landing while the MLX worker is inside
  # a Metal eval corrupts the IOGPU driver's buffer accounting → KERNEL PANIC
  # (`IOGPUGroupMemory::remove_memory_object() not found`) that REBOOTS the Mac — see BUGS.md
  # BUG-001. So: SIGINT → the gateway's graceful shutdown drains in-flight generation, joins the
  # MLX worker, and frees buffers with the GPU idle; we wait generously for that. SIGKILL is a
  # last resort only, loudly flagged. At end-of-model the gateway is idle so graceful exit takes
  # a few seconds; the long window is insurance against a wedged eval. /usr/bin/time only flushes
  # the peak-footprint line on a CLEAN exit — another reason to avoid SIGKILL.
  TEARDOWN_GRACE="${TEARDOWN_GRACE:-180}"; GPU_SETTLE="${GPU_SETTLE:-8}"
  # Never tear down a gateway we did not start: on a shared run it is somebody else's resident
  # model, and killing it would take their work with it.
  if [ "$SHARED_GW" = 1 ]; then
    echo "  shared gateway left running (not ours to stop)"
  else
  kill -INT "$GW_PID" 2>/dev/null
  gone=0
  for _ in $(seq 1 "$TEARDOWN_GRACE"); do kill -0 "$TIME_PID" 2>/dev/null || { gone=1; break; }; sleep 1; done
  if [ "$gone" != 1 ]; then
    echo "  ! gateway did not exit gracefully in ${TEARDOWN_GRACE}s — forcing SIGKILL"
    echo "    (PANIC RISK: a SIGKILL on a live Metal eval can panic the GPU driver and reboot the host)"
    kill -KILL "$TIME_PID" 2>/dev/null
  fi
  wait "$TIME_PID" 2>/dev/null
  fi
  # Let the kernel finish async IOGPU reclamation of the just-exited process's GPU buffers
  # before the next gateway allocates ~15-28 GB on the same Metal device (the cross-process
  # remove_memory_object race).
  sleep "$GPU_SETTLE"
  # A shared gateway is not under our `/usr/bin/time -l`, so there is no footprint to back-fill —
  # and inventing one from somebody else's process would be worse than an empty column.
  foot=$([ "$SHARED_GW" = 1 ] && echo "" || grep -m1 'peak memory footprint' "$glog" | awk '{printf "%.0f", $1/1048576}')
  awk -F, -v m="$spec_csv" -v f="${foot:-}" 'BEGIN{OFS=","} NR==1{print;next} $2==m{$13=f} {print}' "$CSV" > "$CSV.tmp" && mv "$CSV.tmp" "$CSV"
  echo "  model footprint: ${foot:-n/a}MB"
  echo
done

echo "============================================================"
column -s, -t "$CSV"
echo
# Pass-RATE summary per agent×model×task (the honest read: a cell is a rate, not a single
# sample). With REPS=1 this is just pass/1 per cell; with REPS>1 it aggregates the reps.
echo "pass-rate (agent × model × task):"
awk -F, 'NR>1{k=$1"|"$2"|"$3; tot[k]++; if($6==1)p[k]++; if(!(k in s)){s[k]=1; ord[++n]=k}}
  END{for(i=1;i<=n;i++){k=ord[i]; split(k,f,"|"); printf "  %-7s %-34s %-6s %d/%d\n", f[1], f[2], f[3], p[k]+0, tot[k]}}' "$CSV"
echo
# Name what is actually there. "per-run logs" used to point at a directory that stayed empty in
# shared mode, which reads as "the logs were kept and say nothing" rather than "there are none".
if [ -n "$(ls -A "$OUT/runs" 2>/dev/null)" ]; then
  echo "CSV: $OUT/per-run.csv    kept evidence (failed cells + gateway logs): $OUT/runs"
else
  echo "CSV: $OUT/per-run.csv    (no failed cells — $OUT/runs is empty)"
fi
