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
}

impl AgentOutcome {
    pub fn is_done(&self) -> bool {
        self.stop == AgentStop::Done
    }
}

/// Run the agent loop: `[system, user] → model → (tool calls → dispatch → results)* →
/// final text`, bounded by `budget`. See Contract 2 in `integration.md`.
pub async fn run_agent(
    backend: &dyn ChatBackend,
    system: &str,
    user: &str,
    tools: &dyn ToolSource,
    budget: &Budget,
) -> AgentOutcome {
    let start = Instant::now();
    let tool_defs = tools.tools();
    let mut messages = vec![Message::system(system), Message::user(user)];
    let mut operations: Vec<ToolInvocation> = Vec::new();
    let mut steps = 0usize;

    loop {
        if steps >= budget.max_steps {
            return outcome(String::new(), AgentStop::BudgetSteps, steps, operations, messages);
        }
        if budget.wall_time.is_some_and(|wt| start.elapsed() >= wt) {
            return outcome(String::new(), AgentStop::BudgetTime, steps, operations, messages);
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

        let turn = match collect_turn(backend, req).await {
            Ok(t) => t,
            Err(e) => return outcome(String::new(), AgentStop::Error(e), steps, operations, messages),
        };

        // No tool calls → the model gave its final answer.
        if turn.tool_calls.is_empty() {
            if !turn.text.is_empty() {
                messages.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: turn.text.clone() }],
                });
            }
            return outcome(turn.text, AgentStop::Done, steps, operations, messages);
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

        for (id, name, args) in turn.tool_calls {
            let input = parse_args(&args);
            let result = tools.dispatch(&name, input.clone()).await;
            let (content, is_error) = match &result {
                Ok(v) => (value_to_tool_content(v), false),
                Err(e) => (e.0.clone(), true),
            };
            messages.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error,
                }],
            });
            operations.push(ToolInvocation {
                id,
                name,
                input,
                output: result.map_err(|e| e.0),
            });
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
) -> AgentOutcome {
    AgentOutcome { text, stop, steps, operations, transcript }
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

    // End-to-end against a REAL backend: the reference loop drives native MLX through a
    // genuine tool call and back to a final answer. Constrained decoding on, so the args
    // are guaranteed valid. ~2.5 GB Qwen3-4B. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture agent_loop_real_backend
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "network: uses mlx-community/Qwen3-4B-4bit"]
    async fn agent_loop_real_backend() {
        use crate::mlx_native_backend::{ensure_model_dir, MlxNativeBackend};

        unsafe {
            std::env::set_var("ROZUM_MLX_CONSTRAIN", "1");
        }
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve");
        let backend = MlxNativeBackend::new(dir, spec.replace(':', "/")).await.expect("load");

        let out = run_agent(
            &backend,
            "You are a calculator. Use the add tool for any addition; do not compute it yourself.",
            "What is 3 + 5? Use the tool, then tell me the result.",
            &add_tool(),
            &Budget { max_steps: 4, max_tokens: Some(128), ..Budget::default() },
        )
        .await;
        unsafe {
            std::env::remove_var("ROZUM_MLX_CONSTRAIN");
        }

        eprintln!(
            "AGENT e2e  stop={:?} steps={} ops={} text={:?}",
            out.stop, out.steps, out.operations.len(), out.text
        );
        assert_eq!(out.stop, AgentStop::Done, "loop must reach a final answer");
        let add = out.operations.iter().find(|o| o.name == "add").expect("the model must call add");
        assert_eq!(add.output.as_ref().expect("add succeeded")["sum"], 8);
        assert!(!out.text.is_empty(), "a final answer is produced");
    }
}
