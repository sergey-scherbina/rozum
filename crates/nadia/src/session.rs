//! One conversation with one model: the prompt, the budget, the repetition guard, and
//! the transcript that carries context from turn to turn.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use rozum_agent::agent::{
    run_agent_observed, AgentObserver, AgentOutcome, Budget, ExecFeedbackPolicy, ToolError,
    ToolSource,
};
use rozum_core::backend::{ChatBackend, Message, ToolDef};
use serde_json::Value;

/// What the model is told before anything else.
///
/// Deliberately short. A small local model follows a page of rules worse than it follows
/// five, and every token here is re-sent on every step of every turn. The three things
/// that actually change behaviour on a 4B model, in order: use a tool instead of
/// answering in prose, verify by running the command, stop when done.
pub fn system_prompt(root: &Path) -> String {
    format!(
        "You are nadia, a coding agent working in {root}.\n\
         \n\
         Work by calling tools, not by describing what should be done. When the task \
         needs a file changed, change it; do not print the file and stop.\n\
         \n\
         Before you claim a task is finished, verify it: run the build, the test, or the \
         program with `bash` and read the output. If the command failed, the task is not \
         finished — fix it and run it again. Never report success you have not observed.\n\
         \n\
         Read a file before editing it, and quote `old_string` exactly as it appears. \
         Make the smallest change that satisfies the task; do not restructure code that \
         already works.\n\
         \n\
         When the task is genuinely done, reply with a short plain-text summary of what \
         you changed and what you ran to check it. That final message ends the task, so \
         do not send it while work remains.",
        root = root.display()
    )
}

/// Budgets tuned for a small local model on the benchmark task classes. `max_steps` is
/// the safety net, not the plan: the tasks in `scripts/bench/agentic.sh` land in 4–12
/// steps, and a run that reaches 24 has lost the thread rather than found a hard problem.
pub fn default_budget() -> Budget {
    Budget {
        max_steps: 24,
        max_tokens: Some(4096),
        wall_time: Some(std::time::Duration::from_secs(900)),
        temperature: 0.0,
    }
}

/// Wraps a [`ToolSource`] and refuses a call that is going in circles.
///
/// Budgets bound the damage; they do not stop the specific failure a small model actually
/// exhibits, which is re-issuing an identical call **after an identical result** — most
/// often an `edit_file` whose `old_string` is not in the file, read as "try again" rather
/// than "the premise is wrong".
///
/// Both halves of that sentence matter, and the first version only implemented the first.
/// Matching on the call alone fired on `multibug`, where the model ran `cargo test` four
/// times — which is not a loop, it is the verify step of fix → test → fix, and the same
/// command returns something different each time because the files changed underneath it.
/// A repeat only counts when the result did not change either.
///
/// The intervention is an error the model reads, not a silent halt: it names the
/// repetition and asks for a different approach, which is recoverable. A halt is not.
pub struct LoopBreaker<T: ToolSource> {
    inner: T,
    /// `(call, result)` per dispatch, newest last. The result half is what keeps a
    /// verification loop from looking like churn.
    history: Mutex<Vec<(String, String)>>,
}

const WINDOW: usize = 12;
const REPEATS: usize = 4;

impl<T: ToolSource> LoopBreaker<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, history: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl<T: ToolSource> ToolSource for LoopBreaker<T> {
    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let key = format!("{name}:{args}");
        // Look at what this exact call produced the last few times. Identical call AND
        // identical result, repeatedly, is a stuck model; identical call with a changing
        // result is a verify loop doing its job.
        let stuck = {
            let h = self.history.lock().unwrap();
            let start = h.len().saturating_sub(WINDOW);
            let mut results = h[start..].iter().filter(|(k, _)| *k == key).map(|(_, r)| r);
            match results.next() {
                Some(first) => {
                    let same = 1 + results.filter(|r| *r == first).count();
                    same >= REPEATS - 1 && h[start..].iter().filter(|(k, _)| *k == key).count() == same
                }
                None => false,
            }
        };
        if stuck {
            return Err(ToolError::new(format!(
                "You have called `{name}` with these exact arguments several times and got the \
                 same result every time. Repeating it will not help. Re-read the current state \
                 of the file or the command output, and either take a different approach or \
                 stop and report what is blocking you."
            )));
        }
        let result = self.inner.dispatch(name, args).await;
        let fingerprint = match &result {
            Ok(v) => v.to_string(),
            Err(e) => format!("err:{}", e.0),
        };
        self.history.lock().unwrap().push((key, fingerprint));
        result
    }
}

/// The observer a batch run uses: nothing to show, nobody watching.
struct Silent;
impl AgentObserver for Silent {}

/// A live conversation. Batch mode runs one turn and exits; the REPL keeps the session
/// and calls [`Session::turn`] per user message, so context (and the gateway's KV prefix)
/// survives across turns.
pub struct Session<'a> {
    backend: &'a dyn ChatBackend,
    tools: &'a dyn ToolSource,
    budget: Budget,
    messages: Vec<Message>,
}

impl<'a> Session<'a> {
    pub fn new(
        backend: &'a dyn ChatBackend,
        tools: &'a dyn ToolSource,
        system: &str,
        budget: Budget,
    ) -> Self {
        Self { backend, tools, budget, messages: vec![Message::system(system)] }
    }

    /// Run one user message to a final answer, then keep the resulting transcript as the
    /// context for the next turn.
    pub async fn turn(&mut self, user: &str) -> AgentOutcome {
        self.turn_observed(user, &Silent).await
    }

    /// [`Session::turn`] with a live view of the run. The interactive front-end uses this;
    /// batch does not, because nobody is watching a headless run and the buffered result
    /// is the whole output.
    pub async fn turn_observed(&mut self, user: &str, observer: &dyn AgentObserver) -> AgentOutcome {
        let mut messages = std::mem::take(&mut self.messages);
        messages.push(Message::user(user));
        let outcome = run_agent_observed(
            &[self.backend],
            messages,
            self.tools,
            &self.budget,
            // One backend, so there is no stronger tier to escalate to; disable the
            // exec-feedback policy rather than let it count toward a promotion that
            // cannot happen.
            &ExecFeedbackPolicy { escalate_after_error_steps: usize::MAX },
            observer,
        )
        .await;
        self.messages = outcome.transcript.clone();
        outcome
    }

    /// Drop everything but the system prompt (`/clear`).
    pub fn reset(&mut self) {
        self.messages.truncate(1);
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rozum_agent::agent::CallbackToolSource;
    use std::sync::Arc;
    use serde_json::json;

    fn counting_source() -> impl ToolSource {
        CallbackToolSource::new().with_tool(
            ToolDef {
                name: "noop".into(),
                description: "does nothing".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            |_| Ok(json!({"ok": true})),
        )
    }

    #[tokio::test]
    async fn breaks_an_identical_call_with_an_identical_result() {
        let lb = LoopBreaker::new(counting_source());
        let args = json!({"a": 1});
        for i in 0..3 {
            assert!(lb.dispatch("noop", args.clone()).await.is_ok(), "call {i} should pass");
        }
        let err = lb.dispatch("noop", args).await.unwrap_err();
        assert!(err.0.contains("Repeating it will not help"), "{}", err.0);
    }

    /// The regression this pins: `multibug` failed because re-running `cargo test` four
    /// times was read as a loop. It is the verify half of fix -> test -> fix, and the
    /// result changes as the files do.
    #[tokio::test]
    async fn a_repeated_call_whose_result_keeps_changing_is_not_a_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = Arc::new(AtomicUsize::new(0));
        let counter = n.clone();
        let src = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "verify".into(),
                description: "re-runs a check".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            move |_| Ok(json!({"failures": counter.fetch_add(1, Ordering::SeqCst)})),
        );
        let lb = LoopBreaker::new(src);
        for i in 0..8 {
            assert!(
                lb.dispatch("verify", json!({"cmd": "cargo test"})).await.is_ok(),
                "call {i} must not be mistaken for a loop — the result changed every time"
            );
        }
    }

    #[tokio::test]
    async fn different_arguments_are_not_a_loop() {
        let lb = LoopBreaker::new(counting_source());
        for i in 0..8 {
            assert!(lb.dispatch("noop", json!({"a": i})).await.is_ok(), "call {i}");
        }
    }

    #[test]
    fn system_prompt_names_the_workspace_and_demands_verification() {
        let p = system_prompt(Path::new("/tmp/ws"));
        assert!(p.contains("/tmp/ws"));
        assert!(p.contains("verify"));
    }
}
