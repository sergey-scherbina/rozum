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
            (None, _) => "⚠ не проверено — у задачи нет машинно-проверяемого критерия".to_string(),
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

/// Is the gate on? Off only when the operator says so — see the module note on opt-out.
pub fn enabled() -> bool {
    !matches!(
        std::env::var("NADIA_VERIFY").ok().as_deref(),
        Some("0" | "off" | "false" | "no")
    )
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
        return match verify::judge(backend, task, workspace).await {
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

    #[test]
    fn a_report_never_implies_a_pass_it_does_not_have() {
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

    #[test]
    fn the_gate_is_on_unless_switched_off() {
        // Serial with the env var, like the other env-reading tests in this crate.
        unsafe { std::env::remove_var("NADIA_VERIFY") };
        assert!(enabled(), "the failure it prevents is silent, so the default must be ON");
        unsafe { std::env::set_var("NADIA_VERIFY", "0") };
        assert!(!enabled());
        unsafe { std::env::set_var("NADIA_VERIFY", "1") };
        assert!(enabled());
        unsafe { std::env::remove_var("NADIA_VERIFY") };

        unsafe { std::env::remove_var("NADIA_VERIFY_ROUNDS") };
        assert_eq!(rounds(), 2);
        unsafe { std::env::set_var("NADIA_VERIFY_ROUNDS", "5") };
        assert_eq!(rounds(), 5);
        unsafe { std::env::remove_var("NADIA_VERIFY_ROUNDS") };
    }
}
