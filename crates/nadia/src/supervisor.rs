//! Agents as supervised actors.
//!
//! One agent is a task with an identity, a mailbox, a workspace it owns, and a lifecycle
//! somebody outside it can drive: start it, ask what it is doing, pause it, resume it,
//! stop it politely, or kill it and get the resources back.
//!
//! **Where the control points are is the whole design.** A running agent is inside
//! `run_agent_conversation`, which does not return until the turn is over — so there is
//! nowhere to interrupt it from the outside except the places it already yields. Tool
//! dispatch is `async` and happens between every model step, which makes it the natural
//! boundary: [`ControlGate`] sits there and can await a resume, or refuse and end the turn.
//! That is why pause and stop are *cooperative* and land at a tool boundary rather than
//! mid-token, and why `kill` — which aborts the task outright — is a different verb with a
//! different promise.
//!
//! Stopping is delivered to the model as a tool error rather than a silent halt, so the
//! agent gets one last chance to say what it had done. A killed agent gets no such chance;
//! that is the trade the two verbs express.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rozum_agent::agent::{AgentStop, Budget, ToolError, ToolSource};
use rozum_core::backend::ToolDef;
use serde_json::Value;

pub type AgentId = u64;

/// Where an agent is in its life. `Stopping` is a real state rather than a flag: a polite
/// stop is a request that completes at the next tool boundary, and the operator asking
/// "did it stop yet" deserves an honest "not yet".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Paused,
    Stopping,
    Done,
    Failed,
    Killed,
}

impl Phase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Killed)
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Running => "running",
            Phase::Paused => "paused",
            Phase::Stopping => "stopping",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Killed => "killed",
        }
    }
}

/// What an agent is doing, as seen from outside. Everything here is observed — the step
/// count and the current tool come from the dispatch path, not from the agent's own
/// report, because an agent that has lost the thread will happily report progress.
#[derive(Clone, Debug)]
pub struct Status {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub task: String,
    pub workspace: PathBuf,
    pub phase: Phase,
    pub tool_calls: usize,
    pub last_tool: Option<String>,
    pub elapsed: Duration,
    /// The final answer, once there is one.
    pub result: Option<String>,
    /// Files this agent wrote or edited, in the order it first touched them.
    pub touched: Vec<String>,
    /// What the verify gate concluded — the ground truth, as opposed to the model's summary.
    pub report: crate::gate::Report,
}

struct Meta {
    task: String,
    workspace: PathBuf,
    parent: Option<AgentId>,
    phase: Phase,
    tool_calls: usize,
    last_tool: Option<String>,
    started: Instant,
    /// When it reached a terminal phase. `elapsed` is measured to here once set, so a run's
    /// duration stops being "how long ago did it start" the moment it is over.
    finished: Option<Instant>,
    result: Option<String>,
    touched: std::collections::BTreeSet<String>,
    /// The acceptance check derived for this task, if any.
    check: Option<String>,
    /// What the gate concluded. Default = nothing checked, which the report says out loud.
    report: crate::gate::Report,
}

/// The flags the gate reads. Separate from [`Meta`] because the gate touches them on every
/// dispatch and must never contend with a status read.
struct Control {
    paused: AtomicBool,
    stopping: AtomicBool,
    resume: tokio::sync::Notify,
}

impl Control {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            resume: tokio::sync::Notify::new(),
        }
    }
}

/// Sits between the loop and the tools: the one place a running agent can be reached.
pub struct ControlGate<T: ToolSource> {
    inner: T,
    ctl: Arc<Control>,
    meta: Arc<Mutex<Meta>>,
}

#[async_trait]
impl<T: ToolSource> ToolSource for ControlGate<T> {
    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        if self.ctl.stopping.load(Ordering::SeqCst) {
            return Err(ToolError::new(
                "The operator stopped this agent. Do not call any more tools. Reply with a \
                 short summary of what you completed and what is left.",
            ));
        }
        // Park here while paused. `Notify` rather than a sleep loop so a resume is
        // immediate and a paused agent costs nothing.
        while self.ctl.paused.load(Ordering::SeqCst) {
            self.ctl.resume.notified().await;
            if self.ctl.stopping.load(Ordering::SeqCst) {
                return Err(ToolError::new(
                    "The operator stopped this agent while it was paused. Reply with a short \
                     summary of what you completed.",
                ));
            }
        }
        {
            let mut m = self.meta.lock().unwrap();
            m.tool_calls += 1;
            m.last_tool = Some(name.to_string());
        }
        // Which files a run actually touched is the question every operator asks next, and
        // the answer is here rather than in the model's summary on purpose: a model that
        // has lost the thread reports files it never wrote. Recorded AFTER the call, so a
        // refused write (the jail, or a declined approval) never appears as a change.
        let touched = matches!(name, "write_file" | "edit_file")
            .then(|| args.get("path").and_then(Value::as_str).map(str::to_owned))
            .flatten();
        let out = self.inner.dispatch(name, args).await;
        if out.is_ok() {
            if let Some(path) = touched {
                self.meta.lock().unwrap().touched.insert(path);
            }
        }
        out
    }
}

/// A handle the supervisor keeps for each live agent.
struct Handle {
    ctl: Arc<Control>,
    meta: Arc<Mutex<Meta>>,
    join: tokio::task::JoinHandle<()>,
    /// Messages for the agent's next turn (`tell`). Delivered between turns, not mid-turn:
    /// the loop owns its message list until it returns, so injecting into a turn in flight
    /// would mean racing it.
    inbox: Arc<Mutex<Vec<String>>>,
}

/// The registry. Owns every agent's handle, hands out ids, and is the only thing that
/// knows the whole tree.
#[derive(Default)]
pub struct Supervisor {
    next_id: AtomicU64,
    agents: Mutex<HashMap<AgentId, Handle>>,
}

/// Everything one agent needs to run, so [`Supervisor::spawn`] can hand it to a task that
/// outlives the caller's borrows.
pub struct Spec {
    pub task: String,
    pub workspace: PathBuf,
    pub gateway: String,
    pub model: String,
    pub budget: Budget,
    pub parent: Option<AgentId>,
    pub allow_net: bool,
    pub confine: bool,
}

impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Start an agent. Returns immediately with its id — the work happens in its own task.
    pub fn spawn(self: &Arc<Self>, spec: Spec) -> Result<AgentId, String> {
        let sandbox = crate::sandbox::Sandbox::new(&spec.workspace)?;
        let mut sandbox = sandbox;
        sandbox.allow_net = spec.allow_net;
        sandbox.confine = spec.confine && cfg!(target_os = "macos");

        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let ctl = Arc::new(Control::new());
        let meta = Arc::new(Mutex::new(Meta {
            task: spec.task.clone(),
            workspace: sandbox.root().to_path_buf(),
            parent: spec.parent,
            phase: Phase::Running,
            tool_calls: 0,
            last_tool: None,
            started: Instant::now(),
            finished: None,
            result: None,
            touched: Default::default(),
            check: None,
            report: Default::default(),
        }));
        let inbox: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let (ctl_t, meta_t, inbox_t) = (ctl.clone(), meta.clone(), inbox.clone());
        let join = tokio::spawn(async move {
            let root = sandbox.root().to_path_buf();
            let tools = ControlGate {
                inner: crate::session::LoopBreaker::new(crate::tools::tool_source(Arc::new(sandbox))),
                ctl: ctl_t.clone(),
                meta: meta_t.clone(),
            };
            let backend =
                rozum_gateway::openai_http::OpenAiHttpBackend::new(spec.gateway, spec.model);
            let mut session = crate::session::Session::new(
                &backend,
                &tools,
                &crate::session::system_prompt(&root),
                spec.budget,
            );

            // What "done" means for this task, decided BEFORE the run so the run cannot
            // influence it (`gate.rs`). One model call; `None` when the task has no
            // machine-checkable criterion, which is an answer rather than a failure.
            let task_text = spec.task.clone();
            let check = crate::gate::derive(&backend, &task_text, &root).await;
            if let Some(c) = &check {
                meta_t.lock().unwrap().check = Some(c.clone());
            }
            let max_repairs = crate::gate::rounds();
            let mut repairs = 0usize;

            let mut next: Option<String> = Some(spec.task);
            loop {
                let Some(message) = next.take() else { break };
                let outcome = session.turn(&message).await;
                // A failed run must carry WHY. `outcome.text` is empty when the loop never got
                // an answer — a refused connection to the gateway, say — and a `failed` agent
                // with an empty result tells whoever asks absolutely nothing, which is how a
                // misconfigured port reads as "the agent is broken". Measured from a phone:
                // three agents failed in one second each and the chat said only "failed".
                let (phase, result) = match &outcome.stop {
                    AgentStop::Done => (Phase::Done, outcome.text.clone()),
                    AgentStop::Error(e) if outcome.text.is_empty() => (Phase::Failed, e.clone()),
                    AgentStop::Error(e) => (Phase::Failed, format!("{}\n\n{e}", outcome.text)),
                    // Budget exhausted: finished, just not by choice.
                    _ => (Phase::Done, outcome.text.clone()),
                };
                {
                    let mut m = meta_t.lock().unwrap();
                    m.result = Some(result);
                    m.phase = phase;
                    if phase.is_terminal() {
                        // Stop the clock. `elapsed` is measured from `started` on every read,
                        // so without this a run that failed in one second reports half an hour
                        // when someone asks about it half an hour later.
                        m.finished = Some(std::time::Instant::now());
                    }
                }
                if ctl_t.stopping.load(Ordering::SeqCst) {
                    break;
                }
                // THE GATE. The agent has said it is finished; now the check says whether it
                // is. A failure comes back as the next turn carrying the command and what it
                // actually printed — that is a repair round, not another guess.
                //
                // The CHECK runs whatever the stop reason was: the artifact is on disk either
                // way, and a run that ran out of steps is the one an operator has most doubt
                // about (measured 2026-08-04 — the derived check was discarded unrun and the
                // report read "not checked" for a program that printed nothing). What a
                // non-finished run does not get is a REPAIR round: there is no budget to repair
                // with, and gate::check itself declines to ask the judge without a Done.
                if repairs < max_repairs {
                    let (report, repair) =
                        crate::gate::check(&backend, &task_text, &root, check.as_deref(), &outcome)
                            .await;
                    {
                        let mut m = meta_t.lock().unwrap();
                        m.report = crate::gate::Report { rounds: repairs, ..report };
                    }
                    if let Some(prompt) = repair.filter(|_| matches!(outcome.stop, AgentStop::Done)) {
                        repairs += 1;
                        meta_t.lock().unwrap().phase = Phase::Running;
                        next = Some(prompt);
                        continue;
                    }
                }
                // Anything the operator sent while that turn ran becomes the next one.
                let queued: Vec<String> = std::mem::take(&mut *inbox_t.lock().unwrap());
                if !queued.is_empty() {
                    meta_t.lock().unwrap().phase = Phase::Running;
                    next = Some(queued.join("\n"));
                }
            }
        });

        self.agents.lock().unwrap().insert(id, Handle { ctl, meta, join, inbox });
        Ok(id)
    }

    /// Send an agent something to work on next. Delivered between turns.
    pub fn tell(&self, id: AgentId, message: impl Into<String>) -> Result<(), String> {
        let agents = self.agents.lock().unwrap();
        let h = agents.get(&id).ok_or_else(|| format!("no agent {id}"))?;
        if h.meta.lock().unwrap().phase == Phase::Killed {
            return Err(format!("agent {id} was killed"));
        }
        h.inbox.lock().unwrap().push(message.into());
        Ok(())
    }

    pub fn pause(&self, id: AgentId) -> Result<(), String> {
        self.with(id, |h| {
            h.ctl.paused.store(true, Ordering::SeqCst);
            let mut m = h.meta.lock().unwrap();
            if !m.phase.is_terminal() {
                m.phase = Phase::Paused;
            }
        })
    }

    pub fn resume(&self, id: AgentId) -> Result<(), String> {
        self.with(id, |h| {
            h.ctl.paused.store(false, Ordering::SeqCst);
            h.ctl.resume.notify_waiters();
            let mut m = h.meta.lock().unwrap();
            if m.phase == Phase::Paused {
                m.phase = Phase::Running;
            }
        })
    }

    /// Ask the agent to stop at the next tool boundary. It gets to write a closing summary.
    pub fn stop(&self, id: AgentId) -> Result<(), String> {
        self.with(id, |h| {
            h.ctl.stopping.store(true, Ordering::SeqCst);
            // A paused agent is parked in the gate; wake it so it can see the stop.
            h.ctl.paused.store(false, Ordering::SeqCst);
            h.ctl.resume.notify_waiters();
            let mut m = h.meta.lock().unwrap();
            if !m.phase.is_terminal() {
                m.phase = Phase::Stopping;
            }
        })
    }

    /// Abort the task now and release its slot. No summary, no last words.
    pub fn kill(&self, id: AgentId) -> Result<(), String> {
        let mut agents = self.agents.lock().unwrap();
        let h = agents.remove(&id).ok_or_else(|| format!("no agent {id}"))?;
        h.join.abort();
        h.meta.lock().unwrap().phase = Phase::Killed;
        Ok(())
    }

    pub fn status(&self, id: AgentId) -> Result<Status, String> {
        let agents = self.agents.lock().unwrap();
        let h = agents.get(&id).ok_or_else(|| format!("no agent {id}"))?;
        Ok(snapshot(id, &h.meta))
    }

    pub fn list(&self) -> Vec<Status> {
        let agents = self.agents.lock().unwrap();
        let mut v: Vec<Status> = agents.iter().map(|(id, h)| snapshot(*id, &h.meta)).collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// Drop the handles of agents that have finished. Their workspaces are the caller's to
    /// keep or delete — reaping here would throw away the output somebody asked for.
    pub fn reap(&self) -> usize {
        let mut agents = self.agents.lock().unwrap();
        let done: Vec<AgentId> = agents
            .iter()
            .filter(|(_, h)| h.meta.lock().unwrap().phase.is_terminal())
            .map(|(id, _)| *id)
            .collect();
        for id in &done {
            agents.remove(id);
        }
        done.len()
    }

    fn with(&self, id: AgentId, f: impl FnOnce(&Handle)) -> Result<(), String> {
        let agents = self.agents.lock().unwrap();
        let h = agents.get(&id).ok_or_else(|| format!("no agent {id}"))?;
        f(h);
        Ok(())
    }
}

fn snapshot(id: AgentId, meta: &Arc<Mutex<Meta>>) -> Status {
    let m = meta.lock().unwrap();
    Status {
        id,
        parent: m.parent,
        task: m.task.clone(),
        workspace: m.workspace.clone(),
        phase: m.phase,
        tool_calls: m.tool_calls,
        last_tool: m.last_tool.clone(),
        elapsed: m.finished.map_or_else(|| m.started.elapsed(), |f| f - m.started),
        result: m.result.clone(),
        touched: m.touched.iter().cloned().collect(),
        report: m.report.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rozum_agent::agent::CallbackToolSource;
    use serde_json::json;

    fn gate() -> (ControlGate<impl ToolSource>, Arc<Control>, Arc<Mutex<Meta>>) {
        let src = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "noop".into(),
                description: "does nothing".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            |_| Ok(json!({"ok": true})),
        );
        let ctl = Arc::new(Control::new());
        let meta = Arc::new(Mutex::new(Meta {
            task: "t".into(),
            workspace: PathBuf::from("/tmp"),
            parent: None,
            phase: Phase::Running,
            tool_calls: 0,
            last_tool: None,
            started: Instant::now(),
            finished: None,
            result: None,
            touched: Default::default(),
            check: None,
            report: Default::default(),
        }));
        (ControlGate { inner: src, ctl: ctl.clone(), meta: meta.clone() }, ctl, meta)
    }

    #[tokio::test]
    async fn a_stop_reaches_the_model_as_something_it_can_answer() {
        let (g, ctl, _m) = gate();
        ctl.stopping.store(true, Ordering::SeqCst);
        let err = g.dispatch("noop", json!({})).await.unwrap_err();
        assert!(err.0.contains("stopped this agent"), "{}", err.0);
        assert!(
            err.0.contains("summary"),
            "a stop must leave room for a closing report, not just halt: {}",
            err.0
        );
    }

    #[tokio::test]
    async fn a_paused_agent_blocks_at_the_tool_boundary_and_resumes() {
        let (g, ctl, _m) = gate();
        ctl.paused.store(true, Ordering::SeqCst);
        let g = Arc::new(g);
        let g2 = g.clone();
        let call = tokio::spawn(async move { g2.dispatch("noop", json!({})).await });

        // Give it a moment to actually park in the gate rather than race past it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!call.is_finished(), "a paused agent must not run the tool");

        ctl.paused.store(false, Ordering::SeqCst);
        ctl.resume.notify_waiters();
        let out = tokio::time::timeout(Duration::from_secs(2), call).await;
        assert!(out.is_ok(), "resume did not wake the parked dispatch");
        assert!(out.unwrap().unwrap().is_ok());
    }

    #[tokio::test]
    async fn stopping_a_paused_agent_wakes_it() {
        // The deadlock this pins: pause parks the dispatch, and a stop that only sets a
        // flag would leave it parked forever with nobody to notify it.
        let (g, ctl, _m) = gate();
        ctl.paused.store(true, Ordering::SeqCst);
        let g = Arc::new(g);
        let g2 = g.clone();
        let call = tokio::spawn(async move { g2.dispatch("noop", json!({})).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        ctl.stopping.store(true, Ordering::SeqCst);
        ctl.paused.store(false, Ordering::SeqCst);
        ctl.resume.notify_waiters();

        let out = tokio::time::timeout(Duration::from_secs(2), call).await.expect("still parked");
        assert!(out.unwrap().is_err(), "a stopped agent must not run the tool");
    }

    #[tokio::test]
    async fn status_counts_what_was_dispatched_not_what_was_claimed() {
        let (g, _ctl, meta) = gate();
        for _ in 0..3 {
            g.dispatch("noop", json!({})).await.unwrap();
        }
        let s = snapshot(7, &meta);
        assert_eq!(s.tool_calls, 3);
        assert_eq!(s.last_tool.as_deref(), Some("noop"));
    }

    #[tokio::test]
    async fn unknown_ids_are_errors_rather_than_panics() {
        let sup = Supervisor::new();
        assert!(sup.status(99).is_err());
        assert!(sup.pause(99).is_err());
        assert!(sup.kill(99).is_err());
        assert!(sup.tell(99, "hi").is_err());
        assert!(sup.list().is_empty());
    }
}
