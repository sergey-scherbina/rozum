//! Reference agent runtime — Contracts 2 (agent loop) and 3 (tool) from
//! `docs/specs/integration.md`. It drives a [`ChatBackend`]: send the conversation,
//! run any tool calls the model emits through a [`ToolSource`], feed the results back,
//! and repeat within a [`Budget`]. Dual purpose: the in-process **embedded mode** (a
//! small local model, no network) and an **executable spec** the scalascript agent SDK
//! mirrors.
//!
//! The loop is stateless w.r.t. rozum's gateway: it only uses the public
//! `ChatBackend` SPI, so it works against any backend (native MLX, GGUF, a remote HTTP
//! client backend, the `HelloBackend` in tests).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ContentBlock, Message, Role, SamplingParams, StopReason,
    ToolDef,
};

// ─── Contract 3: Tool / ToolSource ────────────────────────────────────────────

/// A clear, actionable error the model can recover from — the failure half of a tool
/// handler (Contract 3). Its message is fed back to the model as the tool result so it
/// can correct the call on the next step.
#[derive(Debug, Clone)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the agent loop gets the tool schemas to advertise and dispatches the model's
/// tool calls (Contract 3 plumbing). Implement this for a concrete tool set — the
/// in-process [`CallbackToolSource`] below, or (follow-up) an MCP-client adapter.
#[async_trait]
pub trait ToolSource: Send + Sync {
    /// The tool schemas exposed to the model (become `ChatRequest.tools`).
    fn tools(&self) -> Vec<ToolDef>;
    /// Execute one tool call. `Ok(json)` is a structured result; `Err(ToolError)` is a
    /// recoverable error message handed back to the model.
    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError>;
}

type Handler = Box<dyn Fn(Value) -> Result<Value, ToolError> + Send + Sync>;

/// A [`ToolSource`] backed by in-process handlers — the direct-callback adapter (the
/// embedded mode). Register `(ToolDef, handler)` pairs; the handler validates + executes
/// and returns a structured result or a [`ToolError`].
#[derive(Default)]
pub struct CallbackToolSource {
    defs: Vec<ToolDef>,
    handlers: HashMap<String, Handler>,
}

impl CallbackToolSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool and its handler (builder style).
    pub fn with_tool(
        mut self,
        def: ToolDef,
        handler: impl Fn(Value) -> Result<Value, ToolError> + Send + Sync + 'static,
    ) -> Self {
        self.handlers.insert(def.name.clone(), Box::new(handler));
        self.defs.push(def);
        self
    }
}

#[async_trait]
impl ToolSource for CallbackToolSource {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        match self.handlers.get(name) {
            Some(h) => h(args),
            None => Err(ToolError::new(format!("unknown tool: {name}"))),
        }
    }
}

/// Compose several [`ToolSource`]s into one — the union of their tools, with each call routed to
/// the source that declares it. `run_agent` takes a single source, so this is how an app combines
/// the built-in tools, the memory store, a retrieval index, and its own domain tools. On a name
/// clash the **first-added** source wins.
#[derive(Default)]
pub struct MultiToolSource {
    sources: Vec<Box<dyn ToolSource>>,
}

impl MultiToolSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source (builder style). Earlier-added sources take precedence on a name clash.
    pub fn with(mut self, source: impl ToolSource + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }
}

#[async_trait]
impl ToolSource for MultiToolSource {
    fn tools(&self) -> Vec<ToolDef> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for s in &self.sources {
            for t in s.tools() {
                if seen.insert(t.name.clone()) {
                    out.push(t);
                }
            }
        }
        out
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        for s in &self.sources {
            if s.tools().iter().any(|t| t.name == name) {
                return s.dispatch(name, args).await;
            }
        }
        Err(ToolError::new(format!("unknown tool: {name}")))
    }
}

// ─── MCP-client adapter ────────────────────────────────────────────────────────

use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
    Peer, RoleClient, ServiceExt,
};

/// A [`ToolSource`] backed by an external MCP server (over `rmcp`): connect once, cache the
/// server's tool list, and forward each `dispatch` as a `tools/call`. The standard transport
/// is a stdio child process ([`connect_stdio`](McpToolSource::connect_stdio));
/// [`from_service`](McpToolSource::from_service) wraps any already-connected client service
/// (e.g. an in-memory duplex in tests).
pub struct McpToolSource {
    // Keep the connection alive for the lifetime of the source.
    _service: RunningService<RoleClient, ()>,
    peer: Peer<RoleClient>,
    defs: Vec<ToolDef>,
}

impl McpToolSource {
    /// Spawn `program args…` as an MCP server over stdio and connect to it.
    pub async fn connect_stdio<S: AsRef<std::ffi::OsStr>>(
        program: S,
        args: &[&str],
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        let transport = rmcp::transport::TokioChildProcess::new(cmd)
            .map_err(|e| format!("spawn MCP server: {e}"))?;
        let service = ().serve(transport).await.map_err(|e| format!("MCP connect: {e}"))?;
        Self::from_service(service).await
    }

    /// Wrap an already-connected MCP client `service`: list its tools and cache them.
    pub async fn from_service(service: RunningService<RoleClient, ()>) -> Result<Self, String> {
        let raw = service.list_all_tools().await.map_err(|e| format!("MCP list_tools: {e}"))?;
        let defs = raw.into_iter().map(mcp_tool_to_def).collect();
        let peer = service.peer().clone();
        Ok(Self { _service: service, peer, defs })
    }
}

/// Convert an MCP `Tool` to the SPI `ToolDef` the model sees.
fn mcp_tool_to_def(t: rmcp::model::Tool) -> ToolDef {
    ToolDef {
        name: t.name.to_string(),
        description: t.description.map(|d| d.to_string()).unwrap_or_default(),
        input_schema: Value::Object((*t.input_schema).clone()),
    }
}

/// Flatten an MCP `CallToolResult` to a `Value`: prefer the structured content, else the
/// concatenated text parts (parsed as JSON when possible, otherwise kept as a string).
fn call_result_to_value(res: &CallToolResult) -> Value {
    if let Some(sc) = &res.structured_content {
        return sc.clone();
    }
    let text: String = res
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("");
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[async_trait]
impl ToolSource for McpToolSource {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let arguments = args.as_object().cloned().unwrap_or_default();
        let res = self
            .peer
            .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
            .await
            .map_err(|e| ToolError::new(format!("mcp call_tool({name}): {e}")))?;
        let value = call_result_to_value(&res);
        if res.is_error == Some(true) {
            return Err(ToolError::new(value_to_tool_content(&value)));
        }
        Ok(value)
    }
}

// ─── Contract 2: the agent loop ───────────────────────────────────────────────

/// Bounds on the agent loop (Contract 2). When any limit is hit the loop stops and
/// returns a partial [`AgentOutcome`] rather than running away.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Max model round-trips (one prompt → response is one step).
    pub max_steps: usize,
    /// Per-call `max_tokens` (also bounds a runaway single response).
    pub max_tokens: Option<u32>,
    /// Wall-clock ceiling across the whole loop.
    pub wall_time: Option<Duration>,
    /// Sampling temperature — `0.0` (default) for deterministic, reproducible runs.
    pub temperature: f32,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_steps: 8, max_tokens: None, wall_time: None, temperature: 0.0 }
    }
}

/// When to escalate the backend on **execution feedback** (Phase 8). The most reliable quality
/// signal isn't a judge's opinion — it's whether the model's tool calls *worked*. If a model keeps
/// producing failing calls, a stronger tier is warranted for the next step.
#[derive(Debug, Clone)]
pub struct ExecFeedbackPolicy {
    /// Escalate to the next tier after this many **consecutive** steps whose tool calls *all*
    /// errored (the model is stuck). `usize::MAX` disables exec-feedback escalation. A clean step
    /// (any tool call succeeds, or a final text answer) resets the counter.
    pub escalate_after_error_steps: usize,
}

impl Default for ExecFeedbackPolicy {
    fn default() -> Self {
        Self { escalate_after_error_steps: 2 }
    }
}

/// Why the loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStop {
    /// The model returned a final text answer.
    Done,
    /// `budget.max_steps` reached before a final answer.
    BudgetSteps,
    /// `budget.wall_time` elapsed before a final answer.
    BudgetTime,
    /// A model/transport error (the message); the loop aborts.
    Error(String),
}

/// One executed tool call + its result (the audit trail of side effects).
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// `Ok(result)` or `Err(message)` (a [`ToolError`]).
    pub output: Result<Value, String>,
    /// The cost-tier (index into the backend list) whose turn requested this call — `0` for a
    /// single-backend [`run_agent`]. Lets a caller attribute tool-error rate to the model that
    /// produced the call, the grounded execution-quality signal (Phase 8).
    pub tier: usize,
}

/// The result of an agent run.
pub struct AgentOutcome {
    /// The final assistant text (empty if the loop stopped before a final answer).
    pub text: String,
    pub stop: AgentStop,
    /// Model round-trips used.
    pub steps: usize,
    /// Tool calls executed, in order.
    pub operations: Vec<ToolInvocation>,
    /// The full conversation, for audit / continuation.
    pub transcript: Vec<Message>,
    /// The cost-tier that produced the final answer — `> 0` iff execution feedback escalated to a
    /// stronger backend mid-loop (Phase 8).
    pub final_tier: usize,
}

impl AgentOutcome {
    pub fn is_done(&self) -> bool {
        self.stop == AgentStop::Done
    }

    /// Per cost-tier, `(tool errors, total tool calls)` — the grounded "did it actually work"
    /// signal the learned stats consume (Phase 8). A caller maps tier → model id to record the
    /// per-model tool-error rate per task-class. Lower is better; a tier with all-errored calls is
    /// the evidence that a stronger model was (or would be) warranted.
    pub fn tool_error_rate_by_tier(&self) -> HashMap<usize, (usize, usize)> {
        let mut m: HashMap<usize, (usize, usize)> = HashMap::new();
        for op in &self.operations {
            let e = m.entry(op.tier).or_insert((0, 0));
            e.1 += 1;
            if op.output.is_err() {
                e.0 += 1;
            }
        }
        m
    }
}

/// Run the agent loop: `[system, user] → model → (tool calls → dispatch → results)* →
/// final text`, bounded by `budget`. See Contract 2 in `integration.md`. Single backend, no
/// escalation — a thin wrapper over [`run_agent_escalating`].
pub async fn run_agent(
    backend: &dyn ChatBackend,
    system: &str,
    user: &str,
    tools: &dyn ToolSource,
    budget: &Budget,
) -> AgentOutcome {
    run_agent_escalating(&[backend], system, user, tools, budget, &ExecFeedbackPolicy::default())
        .await
}

/// The agent loop over a **cost-ordered list of backends**, escalating to a stronger tier on
/// **execution feedback** (Phase 8): when the current model keeps producing *failing* tool calls
/// (per `policy`), the next tier takes over — carrying the full transcript (including the errors),
/// so the stronger model sees what went wrong and corrects it. `tiers[0]` is the cheapest; one
/// backend = plain [`run_agent`]. The grounded "did it actually work" signal beats any judge's
/// guess — a tool error is ground truth that the answer was wrong.
pub async fn run_agent_escalating(
    tiers: &[&dyn ChatBackend],
    system: &str,
    user: &str,
    tools: &dyn ToolSource,
    budget: &Budget,
    policy: &ExecFeedbackPolicy,
) -> AgentOutcome {
    let messages = vec![Message::system(system), Message::user(user)];
    run_agent_conversation(tiers, messages, tools, budget, policy).await
}

/// The same loop, resumed over an **existing** conversation instead of a fresh
/// `[system, user]` pair — what a multi-turn chat front-end needs. Feed back the
/// [`AgentOutcome::transcript`] of the previous turn with the new user message
/// appended, and the model keeps its context (and the gateway keeps its KV prefix,
/// which is what makes turn N+1 cheap).
///
/// [`run_agent`] / [`run_agent_escalating`] are the one-shot entry points and delegate
/// here. `messages` must be non-empty and should start with a system message.
pub async fn run_agent_conversation(
    tiers: &[&dyn ChatBackend],
    messages: Vec<Message>,
    tools: &dyn ToolSource,
    budget: &Budget,
    policy: &ExecFeedbackPolicy,
) -> AgentOutcome {
    assert!(!tiers.is_empty(), "run_agent_conversation needs at least one backend");
    let start = Instant::now();
    let tool_defs = tools.tools();
    let mut messages = messages;
    let mut operations: Vec<ToolInvocation> = Vec::new();
    let mut steps = 0usize;
    // The active cost-tier and how many consecutive all-errored steps it has produced.
    let mut tier = 0usize;
    let mut consecutive_error_steps = 0usize;

    loop {
        if steps >= budget.max_steps {
            return outcome(String::new(), AgentStop::BudgetSteps, steps, operations, messages, tier);
        }
        if budget.wall_time.is_some_and(|wt| start.elapsed() >= wt) {
            return outcome(String::new(), AgentStop::BudgetTime, steps, operations, messages, tier);
        }
        steps += 1;

        let req = ChatRequest {
            messages: messages.clone(),
            tools: tool_defs.clone(),
            sampling: SamplingParams {
                temperature: Some(budget.temperature),
                max_tokens: budget.max_tokens,
                ..Default::default()
            },
            cancel: CancellationToken::new(),
            session_id: None,
        };

        let turn = match collect_turn(tiers[tier], req).await {
            Ok(t) => t,
            Err(e) => {
                return outcome(String::new(), AgentStop::Error(e), steps, operations, messages, tier)
            }
        };

        // No tool calls → the model gave its final answer.
        if turn.tool_calls.is_empty() {
            if !turn.text.is_empty() {
                messages.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: turn.text.clone() }],
                });
            }
            return outcome(turn.text, AgentStop::Done, steps, operations, messages, tier);
        }

        // Record the assistant turn (text + the tool calls it requested), then execute
        // each call and append its result so the next step sees it.
        let mut assistant: Vec<ContentBlock> = Vec::new();
        if !turn.text.is_empty() {
            assistant.push(ContentBlock::Text { text: turn.text });
        }
        for (id, name, args) in &turn.tool_calls {
            assistant.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: parse_args(args),
            });
        }
        messages.push(Message { role: Role::Assistant, content: assistant });

        let mut step_calls = 0usize;
        let mut step_errors = 0usize;
        for (id, name, args) in turn.tool_calls {
            let input = parse_args(&args);
            let result = tools.dispatch(&name, input.clone()).await;
            let (content, is_error) = match &result {
                Ok(v) => (value_to_tool_content(v), false),
                Err(e) => (e.0.clone(), true),
            };
            step_calls += 1;
            if is_error {
                step_errors += 1;
            }
            messages.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error,
                }],
            });
            operations.push(ToolInvocation { id, name, input, output: result.map_err(|e| e.0), tier });
        }

        // Execution-feedback escalation: a step whose tool calls *all* errored means the model is
        // stuck producing failing calls. Enough consecutive such steps at this tier → hand off to
        // a stronger one. Any progress (a call that worked) resets the counter.
        let all_failed = step_calls > 0 && step_errors == step_calls;
        if all_failed {
            consecutive_error_steps += 1;
            if consecutive_error_steps >= policy.escalate_after_error_steps && tier + 1 < tiers.len() {
                tier += 1;
                consecutive_error_steps = 0;
                tracing::debug!(
                    new_tier = tier,
                    "agent: escalating backend on persistent tool-call failures"
                );
            }
        } else {
            consecutive_error_steps = 0;
        }
    }
}

/// The collected result of one model turn.
struct Turn {
    text: String,
    /// `(id, name, arguments-json)` per tool call.
    tool_calls: Vec<(String, String, String)>,
    #[allow(dead_code)]
    stop: StopReason,
}

/// Drive one `ChatBackend::chat` to completion, collecting text + tool calls. The
/// SPI-level analogue of the gateway's `oai_collect`.
async fn collect_turn(backend: &dyn ChatBackend, req: ChatRequest) -> Result<Turn, String> {
    let mut stream = backend.chat(req).await.map_err(|e| e.to_string())?;
    let mut text = String::new();
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let mut current: Option<(String, String, String)> = None;
    let mut stop = StopReason::EndTurn;

    while let Some(ev) = stream.next().await {
        match ev.map_err(|e| e.to_string())? {
            ChatEvent::TextDelta { text: t } => text.push_str(&t),
            ChatEvent::ToolUseStart { id, name } => current = Some((id, name, String::new())),
            ChatEvent::ToolUseDelta { input_json_delta, .. } => {
                if let Some((_, _, args)) = &mut current {
                    args.push_str(&input_json_delta);
                }
            }
            ChatEvent::ToolUseEnd { .. } => {
                if let Some(c) = current.take() {
                    calls.push(c);
                }
            }
            ChatEvent::Done { stop_reason, .. } => {
                stop = stop_reason;
                break;
            }
        }
    }
    Ok(Turn { text, tool_calls: calls, stop })
}

fn parse_args(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or(Value::Null)
}

/// Render a tool result as the string content a `ToolResult` block carries. A JSON
/// string passes through as-is; anything else is serialized.
fn value_to_tool_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn outcome(
    text: String,
    stop: AgentStop,
    steps: usize,
    operations: Vec<ToolInvocation>,
    transcript: Vec<Message>,
    final_tier: usize,
) -> AgentOutcome {
    AgentOutcome { text, stop, steps, operations, transcript, final_tier }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ModelResult, ToolDef};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One scripted model response.
    enum Scripted {
        Text(&'static str),
        Call { id: &'static str, name: &'static str, args: &'static str },
    }

    /// A backend that replays a fixed script of turns, one per `chat` call. Optionally
    /// asserts (via `on_request`) that the conversation grew as the loop should.
    struct MockBackend {
        script: Vec<Scripted>,
        step: AtomicUsize,
        on_request: Option<Box<dyn Fn(usize, &ChatRequest) + Send + Sync>>,
    }

    impl MockBackend {
        fn new(script: Vec<Scripted>) -> Self {
            Self { script, step: AtomicUsize::new(0), on_request: None }
        }
        fn inspecting(mut self, f: impl Fn(usize, &ChatRequest) + Send + Sync + 'static) -> Self {
            self.on_request = Some(Box::new(f));
            self
        }
    }

    #[async_trait]
    impl ChatBackend for MockBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let i = self.step.fetch_add(1, Ordering::SeqCst);
            if let Some(f) = &self.on_request {
                f(i, &req);
            }
            let evs: Vec<ModelResult<ChatEvent>> = match &self.script[i] {
                Scripted::Text(t) => vec![
                    Ok(ChatEvent::TextDelta { text: (*t).into() }),
                    Ok(ChatEvent::Done {
                        input_tokens: 1,
                        output_tokens: 1,
                        stop_reason: StopReason::EndTurn,
                    }),
                ],
                Scripted::Call { id, name, args } => vec![
                    Ok(ChatEvent::ToolUseStart { id: (*id).into(), name: (*name).into() }),
                    Ok(ChatEvent::ToolUseDelta {
                        id: (*id).into(),
                        input_json_delta: (*args).into(),
                    }),
                    Ok(ChatEvent::ToolUseEnd { id: (*id).into() }),
                    Ok(ChatEvent::Done {
                        input_tokens: 1,
                        output_tokens: 1,
                        stop_reason: StopReason::ToolUse,
                    }),
                ],
            };
            Ok(Box::pin(futures::stream::iter(evs)))
        }
        fn context_window(&self) -> u32 {
            u32::MAX
        }
    }

    use crate::backend::ChatStream;

    fn add_tool() -> CallbackToolSource {
        let def = ToolDef {
            name: "add".into(),
            description: "Add two integers".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                "required": ["a", "b"]
            }),
        };
        CallbackToolSource::new().with_tool(def, |args| {
            let a = args["a"].as_i64().ok_or_else(|| ToolError::new("a must be an integer"))?;
            let b = args["b"].as_i64().ok_or_else(|| ToolError::new("b must be an integer"))?;
            Ok(json!({ "sum": a + b }))
        })
    }

    #[tokio::test]
    async fn full_tool_loop_executes_and_feeds_back() {
        // Step 1: call add(3,5). Step 2 (now seeing the tool result): final answer.
        let backend = MockBackend::new(vec![
            Scripted::Call { id: "call_0", name: "add", args: "{\"a\":3,\"b\":5}" },
            Scripted::Text("The sum is 8."),
        ])
        .inspecting(|i, req| {
            if i == 1 {
                // The 2nd request must carry the assistant tool-call turn + the tool result.
                let has_result = req.messages.iter().any(|m| {
                    matches!(m.role, Role::Tool)
                        && m.content.iter().any(|c| matches!(c, ContentBlock::ToolResult { .. }))
                });
                assert!(has_result, "tool result must be fed back on the next step");
            }
        });

        let out = run_agent(&backend, "sys", "add 3 and 5", &add_tool(), &Budget::default()).await;
        assert_eq!(out.stop, AgentStop::Done);
        assert_eq!(out.steps, 2);
        assert_eq!(out.text, "The sum is 8.");
        assert_eq!(out.operations.len(), 1);
        let op = &out.operations[0];
        assert_eq!(op.name, "add");
        assert_eq!(op.output.as_ref().unwrap()["sum"], 8);
        // system, user, assistant(tool_use), tool(result), assistant(text)
        assert_eq!(out.transcript.len(), 5);
    }

    #[tokio::test]
    async fn no_tools_returns_text_in_one_step() {
        let backend = MockBackend::new(vec![Scripted::Text("hello")]);
        let tools = CallbackToolSource::new();
        let out = run_agent(&backend, "sys", "hi", &tools, &Budget::default()).await;
        assert_eq!(out.stop, AgentStop::Done);
        assert_eq!(out.steps, 1);
        assert_eq!(out.text, "hello");
        assert!(out.operations.is_empty());
    }

    #[tokio::test]
    async fn budget_caps_a_tool_calling_loop() {
        // The model keeps calling the tool forever; max_steps must stop it.
        let backend = MockBackend::new(vec![
            Scripted::Call { id: "c0", name: "add", args: "{\"a\":1,\"b\":1}" },
            Scripted::Call { id: "c1", name: "add", args: "{\"a\":1,\"b\":1}" },
            Scripted::Call { id: "c2", name: "add", args: "{\"a\":1,\"b\":1}" },
        ]);
        let budget = Budget { max_steps: 2, ..Budget::default() };
        let out = run_agent(&backend, "sys", "loop", &add_tool(), &budget).await;
        assert_eq!(out.stop, AgentStop::BudgetSteps);
        assert_eq!(out.steps, 2);
        assert_eq!(out.operations.len(), 2, "two tool calls ran before the cap");
    }

    #[tokio::test]
    async fn unknown_tool_error_is_recoverable() {
        // The model calls a tool that doesn't exist; the error is fed back, then it
        // recovers with the right tool.
        let backend = MockBackend::new(vec![
            Scripted::Call { id: "c0", name: "subtract", args: "{}" },
            Scripted::Call { id: "c1", name: "add", args: "{\"a\":2,\"b\":2}" },
            Scripted::Text("Done: 4."),
        ]);
        let out = run_agent(&backend, "sys", "go", &add_tool(), &Budget::default()).await;
        assert_eq!(out.stop, AgentStop::Done);
        assert_eq!(out.operations.len(), 2);
        assert!(out.operations[0].output.is_err(), "unknown tool → ToolError recorded");
        assert!(out.operations[0].output.as_ref().unwrap_err().contains("unknown tool"));
        assert!(out.operations[1].output.is_ok());
    }

    #[tokio::test]
    async fn handler_validation_error_feeds_back() {
        // add called with a non-integer → the handler's ToolError is recorded + fed back.
        let backend = MockBackend::new(vec![
            Scripted::Call { id: "c0", name: "add", args: "{\"a\":\"x\",\"b\":1}" },
            Scripted::Text("ok"),
        ])
        .inspecting(|i, req| {
            if i == 1 {
                let err_fed = req.messages.iter().any(|m| {
                    m.content.iter().any(|c| {
                        matches!(c, ContentBlock::ToolResult { is_error, .. } if *is_error)
                    })
                });
                assert!(err_fed, "the validation error must reach the model");
            }
        });
        let out = run_agent(&backend, "sys", "bad", &add_tool(), &Budget::default()).await;
        assert_eq!(out.stop, AgentStop::Done);
        assert!(out.operations[0].output.is_err());
    }

    #[tokio::test]
    async fn multi_tool_source_unions_and_routes() {
        let def = |n: &str| ToolDef {
            name: n.into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        };
        let a = CallbackToolSource::new()
            .with_tool(def("alpha"), |_| Ok(json!({"from": "a"})))
            .with_tool(def("shared"), |_| Ok(json!({"who": "a"})));
        let b = CallbackToolSource::new()
            .with_tool(def("beta"), |_| Ok(json!({"from": "b"})))
            .with_tool(def("shared"), |_| Ok(json!({"who": "b"})));
        let multi = MultiToolSource::new().with(a).with(b);

        // Union, first-wins on the `shared` clash → 3 tools, not 4.
        let names: Vec<String> = multi.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["alpha", "shared", "beta"]);

        // Calls route to the owning source.
        assert_eq!(multi.dispatch("alpha", json!({})).await.unwrap()["from"], "a");
        assert_eq!(multi.dispatch("beta", json!({})).await.unwrap()["from"], "b");
        // The clash resolves to the first-added source (a).
        assert_eq!(multi.dispatch("shared", json!({})).await.unwrap()["who"], "a");
        // Unknown tool → recoverable error.
        assert!(multi.dispatch("nope", json!({})).await.is_err());
    }

    // ── Phase 8: execution-feedback escalation ──────────────────────────────

    #[tokio::test]
    async fn exec_feedback_escalates_on_persistent_tool_errors() {
        // The weak tier keeps calling `add` with a non-integer (the handler rejects it); after two
        // consecutive all-errored steps the loop hands off to the strong tier, which calls it
        // correctly and answers. The strong model sees the full transcript (the errors included).
        let weak = MockBackend::new(vec![
            Scripted::Call { id: "w0", name: "add", args: "{\"a\":\"x\",\"b\":1}" },
            Scripted::Call { id: "w1", name: "add", args: "{\"a\":\"y\",\"b\":1}" },
        ]);
        let strong = MockBackend::new(vec![
            Scripted::Call { id: "s0", name: "add", args: "{\"a\":3,\"b\":5}" },
            Scripted::Text("The sum is 8."),
        ]);
        let policy = ExecFeedbackPolicy { escalate_after_error_steps: 2 };
        let out = run_agent_escalating(
            &[&weak, &strong],
            "sys",
            "add the numbers",
            &add_tool(),
            &Budget::default(),
            &policy,
        )
        .await;

        assert_eq!(out.stop, AgentStop::Done);
        assert_eq!(out.final_tier, 1, "exec-feedback escalated to the strong tier");
        assert_eq!(out.text, "The sum is 8.");
        assert_eq!(out.operations.len(), 3);
        assert_eq!(out.operations[0].tier, 0);
        assert!(out.operations[0].output.is_err());
        assert_eq!(out.operations[1].tier, 0);
        assert!(out.operations[1].output.is_err());
        assert_eq!(out.operations[2].tier, 1);
        assert!(out.operations[2].output.is_ok());
        // The grounded per-tier signal: tier 0 errored 2/2, tier 1 succeeded 0 errors of 1.
        let rates = out.tool_error_rate_by_tier();
        assert_eq!(rates[&0], (2, 2));
        assert_eq!(rates[&1], (0, 1));
    }

    #[tokio::test]
    async fn exec_feedback_no_escalation_when_model_recovers() {
        // The weak tier errors once, then recovers on its own — a clean step resets the counter, so
        // we never escalate. The strong tier has an empty script: calling it would panic.
        let weak = MockBackend::new(vec![
            Scripted::Call { id: "w0", name: "add", args: "{\"a\":\"x\",\"b\":1}" }, // error
            Scripted::Call { id: "w1", name: "add", args: "{\"a\":2,\"b\":2}" },     // recovers
            Scripted::Text("Done: 4."),
        ]);
        let strong = MockBackend::new(vec![]); // must never be called
        let out = run_agent_escalating(
            &[&weak, &strong],
            "sys",
            "go",
            &add_tool(),
            &Budget::default(),
            &ExecFeedbackPolicy::default(),
        )
        .await;

        assert_eq!(out.stop, AgentStop::Done);
        assert_eq!(out.final_tier, 0, "the model recovered → no escalation");
        assert_eq!(out.text, "Done: 4.");
        assert!(out.operations.iter().all(|o| o.tier == 0));
    }

    #[tokio::test]
    async fn single_backend_run_agent_stays_at_tier_zero() {
        let backend = MockBackend::new(vec![
            Scripted::Call { id: "c0", name: "add", args: "{\"a\":3,\"b\":5}" },
            Scripted::Text("8"),
        ]);
        let out = run_agent(&backend, "sys", "add", &add_tool(), &Budget::default()).await;
        assert_eq!(out.final_tier, 0);
        assert!(out.operations.iter().all(|o| o.tier == 0));
        assert_eq!(out.tool_error_rate_by_tier()[&0], (0, 1));
    }

    // ── MCP adapter: conversions ────────────────────────────────────────────

    #[test]
    fn mcp_tool_converts_to_tool_def() {
        use std::sync::Arc;
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), json!("object"));
        let t = rmcp::model::Tool::new("add", "Add two integers", Arc::new(schema));
        let def = mcp_tool_to_def(t);
        assert_eq!(def.name, "add");
        assert_eq!(def.description, "Add two integers");
        assert_eq!(def.input_schema["type"], "object");
    }

    #[test]
    fn mcp_call_result_to_value_prefers_structured_then_text() {
        use rmcp::model::{CallToolResult, Content};
        // Structured content wins.
        let mut r = CallToolResult::success(vec![Content::text("ignored")]);
        r.structured_content = Some(json!({ "sum": 8 }));
        assert_eq!(call_result_to_value(&r)["sum"], 8);
        // Else text parsed as JSON.
        let r2 = CallToolResult::success(vec![Content::text("{\"sum\":9}")]);
        assert_eq!(call_result_to_value(&r2)["sum"], 9);
        // Else a plain string.
        let r3 = CallToolResult::success(vec![Content::text("hello")]);
        assert_eq!(call_result_to_value(&r3), json!("hello"));
    }

    // ── MCP adapter: end-to-end over an in-memory MCP connection ─────────────

    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    #[derive(Clone)]
    struct AddServer {
        tool_router: rmcp::handler::server::router::tool::ToolRouter<AddServer>,
    }

    #[rmcp::tool_router]
    impl AddServer {
        fn new() -> Self {
            Self { tool_router: Self::tool_router() }
        }
        #[rmcp::tool(name = "add", description = "Add two integers")]
        async fn add(
            &self,
            p: rmcp::handler::server::wrapper::Parameters<AddArgs>,
        ) -> rmcp::model::CallToolResult {
            rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(
                json!({ "sum": p.0.a + p.0.b }).to_string(),
            )])
        }
    }

    #[rmcp::tool_handler(router = self.tool_router)]
    impl rmcp::ServerHandler for AddServer {
        async fn initialize(
            &self,
            _params: rmcp::model::InitializeRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::InitializeResult, rmcp::ErrorData> {
            Ok(rmcp::model::InitializeResult::new(
                rmcp::model::ServerCapabilities::builder().enable_tools().build(),
            ))
        }
    }

    #[tokio::test]
    async fn mcp_tool_source_lists_and_dispatches() {
        use rmcp::ServiceExt;
        let (server_t, client_t) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            if let Ok(svc) = AddServer::new().serve(server_t).await {
                svc.waiting().await.ok();
            }
        });
        let client = ().serve(client_t).await.expect("client connect");
        let mcp = McpToolSource::from_service(client).await.expect("list tools");

        // list_tools surfaced the server's `add`.
        let defs = mcp.tools();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "add");
        // dispatch forwards to the server and returns its structured result.
        let out = mcp.dispatch("add", json!({ "a": 3, "b": 5 })).await.expect("dispatch");
        assert_eq!(out["sum"], 8);
    }

    // NOTE: the real-MLX agent-loop eval (`agent_loop_real_backend`) moved to the
    // binary's integration tests (`tests/mlx_evals.rs`) in the workspace split —
    // rozum-agent has no dependency on the mlx engine.
}
