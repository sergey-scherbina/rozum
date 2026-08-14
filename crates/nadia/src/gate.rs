//! nadia's verify-repair gate: what "done" means for one task, checked before it is claimed.
//!
//! The primitives are `rozum_agent::verify` — deriving a check from the task, running it,
//! judging semantically when nothing deterministic exists. What lives here is the **policy**: how
//! many repair rounds a phone-launched agent gets, what it is told when the check fails, and what
//! the operator is shown afterwards.
//!
//! Why nadia needs its own: `rozum launch` has carried this gate since the matrix work, and an
//! agent started from Telegram had none. The difference showed up as a working RPN calculator
//! whose own output read `4 + 4 = 7` — the model had verified that the program *builds and runs*,
//! which is what its prompt asks for, and nobody had ever written down what the right answer was.
//! A check derived from the task is that missing sentence.
//!
//! The gate is **opt-out** (`NADIA_VERIFY=0`) rather than opt-in, because the failure it prevents
//! is silent: an unverified run and a verified one look identical until someone reads the output.

use std::path::Path;

use rozum_agent::agent::AgentOutcome;
use rozum_agent::verify::{self, Verdict};
use rozum_core::backend::ChatBackend;

/// What the gate did, for the operator rather than for the model. `None` where a field does not
/// apply — an unchecked run says so instead of implying a pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The command that decided it, when there was one.
    pub check: Option<String>,
    /// Did it pass? `None` = nothing was checked.
    pub passed: Option<bool>,
    /// The tail of what the check printed, or the judge's reason. Empty when it passed first try.
    pub detail: String,
    /// How many repair rounds the agent needed after its first attempt.
    pub rounds: usize,
}

impl Report {
    /// One line for a chat message. Deliberately says "не проверено" out loud: silence about
    /// verification is what let a wrong answer read as a finished task.
    pub fn summary(&self) -> String {
        match (self.passed, &self.check) {
            (Some(true), Some(c)) => {
                let extra = if self.rounds > 0 {
                    format!(" (после {} раунд(ов) починки)", self.rounds)
                } else {
                    String::new()
                };
                format!("✔ проверка прошла: {c}{extra}")
            }
            (Some(false), Some(c)) => format!("✘ проверка НЕ прошла: {c}\n{}", clip(&self.detail, 600)),
            (Some(true), None) => "✔ судья-модель подтвердила результат".to_string(),
            (Some(false), None) => format!("✘ судья-модель отклонила: {}", clip(&self.detail, 400)),
            // Nothing was checked HERE. Which of the two reasons it is matters: a task with no
            // criterion is one thing, and a run whose criterion belongs to an outer gate is quite
            // another. Measured 2026-08-06 under `rozum launch`, the two lines printed together:
            //     nadia: verification is rozum-launch's for this run
            //     nadia: ⚠ не проверено — у задачи нет машинно-проверяемого критерия
            // The second contradicts the first — there IS a criterion, it is simply not ours —
            // and "unverified must not read as failed" (SPEC §3.1) cuts the same way here: a
            // deferred run must not read as uncheckable.
            (None, _) => match owner() {
                Some(o) => format!("⤳ проверку выполняет {o}, не этот прогон"),
                None => "⚠ не проверено — у задачи нет машинно-проверяемого критерия".to_string(),
            },
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Is the gate on? Off when the operator says so, and off when something above us already owns
/// the gate for this run.
///
/// `rozum launch` sets `ROZUM_GATE_OWNER` whenever its own verify-repair loop is live, which is
/// every matrix cell and every UCC coder launch. Running both means two derive calls and two
/// repair budgets stacked on one task — and it silently changes what an A/B on `NADIA_VERIFY`
/// measures, which is how it was found. One owner per run; the launcher is the outer one, so it
/// wins. `nadia serve` (the Telegram path) has no launcher above it and is unaffected — that is
/// the case this gate exists for.
pub fn enabled() -> bool {
    if std::env::var_os("ROZUM_GATE_OWNER").is_some() {
        return false;
    }
    !matches!(
        std::env::var("NADIA_VERIFY").ok().as_deref(),
        Some("0" | "off" | "false" | "no")
    )
}

/// Who is gating this run, for the one line a batch run prints about it.
pub fn owner() -> Option<String> {
    std::env::var("ROZUM_GATE_OWNER").ok().filter(|s| !s.trim().is_empty())
}

/// Repair rounds after the first attempt. Two by default, the same budget `rozum launch` uses:
/// measured there, a model that has not fixed it in two rounds is not converging, and each round
/// is a full turn a person is waiting for.
pub fn rounds() -> usize {
    std::env::var("NADIA_VERIFY_ROUNDS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2)
}

/// The check for this task, decided BEFORE the agent runs so the run cannot influence it.
///
/// Precedence: what the task itself makes checkable, then the workspace's own floor (a cargo
/// project has to build), then nothing. A derived check that is about cargo for a workspace and
/// a task that have nothing to do with Rust is dropped — that guard exists because the model
/// once invented `cargo run -- pong == gnop` for a chat message.
pub async fn derive(backend: &dyn ChatBackend, task: &str, workspace: &Path) -> Option<String> {
    if !enabled() {
        return None;
    }
    if let Some(check) = verify::derive_check(backend, task).await {
        if verify::is_hallucinated_cargo_check(&check, workspace, task) {
            return None;
        }
        return Some(check);
    }
    verify::cargo_floor(workspace)
}

/// What the gate loop does after a check. The policy in one place, so it can be read and tested
/// instead of inferred from a `for` loop (both halves of BUG-027 lived in that inference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// The verdict stands; stop.
    Stop,
    /// Repair. `fresh` asks for a NEW session rather than another turn in this one.
    Repair { fresh: bool },
}

/// `round` is how many repairs have already happened; `max_rounds` the budget; `repair` whether
/// the check produced something to send; `done` whether the agent stopped of its own accord; and
/// `tripped` whether the repetition breaker fired during the turn just checked.
///
/// Three rules, each paid for:
///
/// 1. **The check at `round == max_rounds` is the verdict.** The old loop checked only BEFORE each
///    repair and stopped, leaving the last attempt unjudged — three runs in six then reported
///    `✘ проверка НЕ прошла` about code that builds.
/// 2. **A run that did not finish gets no repair.** There is no budget to repair with, and the
///    check has already been taken (it runs whatever the stop reason was — BUG-019).
/// 3. **A repair after a break starts fresh.** The break leaves its own refusal as the last thing
///    in the conversation and a small model answers by quoting it: measured, one step, zero tool
///    calls, the refusal as the whole reply.
pub fn next_step(round: usize, max_rounds: usize, repair: bool, done: bool, tripped: bool) -> Next {
    if round >= max_rounds || !repair || !done {
        return Next::Stop;
    }
    Next::Repair { fresh: tripped }
}

/// Check a finished attempt. Returns the report and, when a repair is warranted, the message to
/// send the agent for its next round.
///
/// The semantic judge only runs when there is no deterministic check AND the agent believes it
/// finished: a task that ran out of budget has a better explanation than a judge's opinion.
pub async fn check(
    backend: &dyn ChatBackend,
    task: &str,
    workspace: &Path,
    check: Option<&str>,
    outcome: &AgentOutcome,
) -> (Report, Option<String>) {
    let Some(cmd) = check else {
        // Nothing deterministic. Ask the judge, and treat its Unknown as "not checked" rather
        // than as either verdict — the operator is told which of the three it was.
        if !enabled() || !matches!(outcome.stop, rozum_agent::agent::AgentStop::Done) {
            return (Report::default(), None);
        }
        return match verify::judge(backend, task, workspace, &outcome.text).await {
            Verdict::Pass => (
                Report { passed: Some(true), ..Default::default() },
                None,
            ),
            Verdict::Fail(reason) => (
                Report { passed: Some(false), detail: reason.clone(), ..Default::default() },
                Some(format!(
                    "A reviewer judged the task NOT accomplished: {reason}\n\nFix it and say what you changed."
                )),
            ),
            Verdict::Unknown(_) => (Report::default(), None),
        };
    };
    let (passed, output) = verify::run_check(cmd, workspace).await;
    let report = Report {
        check: Some(cmd.to_string()),
        passed: Some(passed),
        detail: if passed { String::new() } else { output.clone() },
        rounds: 0,
    };
    // `_in` rather than the bare prompt: when the check failed because the project is one level
    // down, that is the one diagnosis the model cannot reach by reading the error, and without it
    // the repair rounds go on rediscovering it (measured — both rounds of one run).
    let repair = (!passed).then(|| verify::repair_prompt_in(cmd, &output, workspace));
    (report, repair)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that reads or writes the gate's process-wide environment
    /// (`NADIA_VERIFY`, `NADIA_VERIFY_ROUNDS`, `ROZUM_GATE_OWNER`).
    ///
    /// cargo runs a crate's tests as THREADS in one process, so without this a test that merely
    /// CONSTRUCTS a `Report` and asks for its summary can be answered by another test's
    /// half-applied environment. Measured before this lock existed: **2 failures in 25 runs** of
    /// `cargo test -p nadia --lib gate:: -- --test-threads=16`, on unmodified master (BUG-035).
    ///
    /// Merging the racing tests into one was the earlier fix, and the comment on
    /// `who_owns_the_gate_for_this_run` explains why. It was incomplete in a way worth naming: it
    /// caught the tests that obviously WRITE these variables and missed the one that only READS
    /// them, indirectly, through `Report::summary()` → `owner()`. A lock covers readers too, and a
    /// new test only has to take it — which is why this replaces "keep merging tests together", a
    /// strategy that ends with one test and no names.
    static GATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gate_env() -> std::sync::MutexGuard<'static, ()> {
        GATE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A run that stopped WITHOUT finishing still gets its deterministic check.
    ///
    /// Measured 2026-08-04: an RPN attempt exhausted its steps, the front-end skipped the check
    /// it had already derived, and the operator was told `⚠ не проверено` about a program that
    /// printed nothing for the argument the task named. The run with the most doubt was getting
    /// the least verification. The judge still stands down for a non-finished run — a model's
    /// opinion about an interrupted attempt is worth less than the shell command we already have.
    #[tokio::test]
    async fn a_budget_exhausted_run_is_still_checked() {
        // The backend PANICS if it is called: a deterministic check must not cost a model call,
        // and an interrupted run must not be handed to the judge.
        struct NoModel;
        #[async_trait::async_trait]
        impl rozum_core::backend::ChatBackend for NoModel {
            async fn chat(
                &self,
                _: rozum_core::backend::ChatRequest,
            ) -> rozum_core::backend::ModelResult<rozum_core::backend::ChatStream> {
                panic!("the gate asked a model about a check it could run itself")
            }
            fn context_window(&self) -> u32 {
                8192
            }
        }

        let d = tempfile::tempdir().unwrap();
        let outcome = AgentOutcome {
            text: String::new(),
            stop: rozum_agent::agent::AgentStop::BudgetSteps,
            steps: 2,
            operations: Vec::new(),
            transcript: Vec::new(),
            final_tier: 0,
        };
        let (report, repair) = check(&NoModel, "task", d.path(), Some("exit 3"), &outcome).await;
        assert_eq!(report.passed, Some(false), "the check did not run: {report:?}");
        assert!(repair.is_some(), "a failed check still produces the repair the caller may skip");
    }

    #[test]
    fn a_report_never_implies_a_pass_it_does_not_have() {
        let _env = gate_env();
        // ESTABLISH the precondition instead of assuming it. "Nothing was checked here" has two
        // readings and `owner()` picks between them, so this assertion is about an unowned run —
        // which has to be made true, not hoped for. It was neither locked nor established before,
        // so it failed both when another test held the variable mid-flight and for any developer
        // who happened to have `ROZUM_GATE_OWNER` exported in their shell.
        unsafe { std::env::remove_var("ROZUM_GATE_OWNER") };

        let unchecked = Report::default();
        assert!(unchecked.summary().contains("не проверено"), "{}", unchecked.summary());

        let passed = Report {
            check: Some("cargo test -q".into()),
            passed: Some(true),
            ..Default::default()
        };
        assert!(passed.summary().contains("cargo test -q"), "the check itself is the evidence");
        assert!(!passed.summary().contains("раунд"), "no repair rounds → no mention of them");

        let repaired = Report { rounds: 2, ..passed.clone() };
        assert!(repaired.summary().contains("2 раунд"), "{}", repaired.summary());

        let failed = Report {
            check: Some("cargo test -q".into()),
            passed: Some(false),
            detail: "assertion failed: 4 + 4 = 7".into(),
            rounds: 2,
        };
        let s = failed.summary();
        assert!(s.contains("НЕ прошла") && s.contains("4 + 4 = 7"), "{s}");

        // The judge's two outcomes are distinguishable from a deterministic pass.
        let judged = Report { passed: Some(true), ..Default::default() };
        assert!(judged.summary().contains("судья"), "{}", judged.summary());
    }

    /// The loop's policy, which used to be inferable only by reading a `for` loop — and was wrong
    /// twice there (BUG-027).
    #[test]
    fn the_last_attempt_is_judged_and_a_broken_turn_restarts_clean() {
        use Next::*;
        // Budget of 2: repair, repair, and the third check is the verdict.
        assert_eq!(next_step(0, 2, true, true, false), Repair { fresh: false });
        assert_eq!(next_step(1, 2, true, true, false), Repair { fresh: false });
        assert_eq!(next_step(2, 2, true, true, false), Stop, "the last repair must still be judged");

        // A turn cut for repetition is repaired in a FRESH session.
        assert_eq!(next_step(0, 2, true, true, true), Repair { fresh: true });

        // Nothing to send, or an agent that did not finish: no repair either way.
        assert_eq!(next_step(0, 2, false, true, false), Stop);
        assert_eq!(next_step(0, 2, true, false, false), Stop);
        assert_eq!(next_step(0, 2, true, false, true), Stop, "a broken budget is not a repair case");

        // A budget of zero is "check once, never repair" — and it must still check.
        assert_eq!(next_step(0, 0, true, true, false), Stop);
    }

    /// One test, not two, and that is the point: both read the same process-wide environment,
    /// and as separate `#[test]`s they ran on different threads and raced — the second one's
    /// `ROZUM_GATE_OWNER` made the first one's `enabled()` false. A test that fails depending on
    /// thread scheduling teaches nothing.
    #[test]
    fn who_owns_the_gate_for_this_run() {
        let _env = gate_env();
        unsafe { std::env::remove_var("NADIA_VERIFY") };
        unsafe { std::env::remove_var("ROZUM_GATE_OWNER") };
        unsafe { std::env::remove_var("NADIA_VERIFY_ROUNDS") };

        // No outer gate: on by default, because the failure it prevents is silent.
        assert!(enabled(), "the default must be ON — an unverified run looks like a verified one");
        unsafe { std::env::set_var("NADIA_VERIFY", "0") };
        assert!(!enabled());
        unsafe { std::env::set_var("NADIA_VERIFY", "1") };
        assert!(enabled());

        // An outer gate wins, and an explicit NADIA_VERIFY=1 does not override it: the question
        // is not whether the operator wants a gate, it is which one already owns the run.
        unsafe { std::env::set_var("ROZUM_GATE_OWNER", "rozum-launch") };
        assert!(!enabled(), "two gates on one run is two derive calls and two repair budgets");
        assert_eq!(owner().as_deref(), Some("rozum-launch"), "and the run should be able to SAY so");
        // …and the REPORT must say it too. "Nothing checked here" has two reasons and they are not
        // interchangeable: measured 2026-08-06 under `rozum launch`, this line printed
        // "⚠ не проверено — у задачи нет машинно-проверяемого критерия" directly under
        // main.rs's "verification is rozum-launch's for this run". There IS a criterion; it is not
        // ours. Asserted HERE rather than in a test of its own because both read the same
        // process-wide variable — the reason the rest of this test is one test and not four.
        let deferred = Report::default().summary();
        assert!(deferred.contains("rozum-launch"), "{deferred}");
        assert!(!deferred.contains("нет машинно-проверяемого критерия"), "{deferred}");
        unsafe { std::env::remove_var("ROZUM_GATE_OWNER") };
        assert!(enabled());
        unsafe { std::env::remove_var("NADIA_VERIFY") };
        assert!(owner().is_none());
        // With no outer gate, the same empty report means the other thing.
        assert!(Report::default().summary().contains("нет машинно-проверяемого критерия"));

        // Rounds: two by default, the same budget rozum launch uses.
        assert_eq!(rounds(), 2);
        unsafe { std::env::set_var("NADIA_VERIFY_ROUNDS", "5") };
        assert_eq!(rounds(), 5);
        unsafe { std::env::remove_var("NADIA_VERIFY_ROUNDS") };
    }
}
