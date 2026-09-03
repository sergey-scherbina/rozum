//! Deterministic **record / replay** for the agent loop.
//!
//! An agent run has exactly two sources of nondeterminism: what the MODEL said, and what a
//! TOOL answered. Everything else in [`crate::agent::run_agent_observed`] is a pure fold over
//! those. So journaling both, in call order, is enough to re-run a whole loop later with no
//! gateway, no network, no tools, and no model — which is what turns "it failed once last
//! night" into a test.
//!
//! Two decorators, one journal:
//!
//! - [`RecordingBackend`] / [`RecordingTools`] wrap a live [`ChatBackend`] / [`ToolSource`],
//!   forward every call unchanged, and append what came back to a JSONL file.
//! - [`ReplayBackend`] / [`ReplayTools`] read that file back in order and answer from it.
//!
//! **Divergence is loud, never silent.** Each entry carries a fingerprint of the call that
//! produced it (the model's message/tool shape, or a tool's name and arguments). On replay a
//! call whose fingerprint does not match the next entry fails with both fingerprints in the
//! message, rather than handing back an answer that belonged to a different question. A run
//! that reaches the end of the journal fails the same way: a shorter replay is a divergence
//! too, just one that shows up late.
//!
//! ## What this proves, and what it does not
//!
//! It proves the agent's OWN calls replay: same messages out, same events back, same tool
//! arguments, same tool results, same order. It does NOT prove the world underneath is
//! unchanged. If a tool read a file, the journal holds what the tool RETURNED, not the file;
//! re-running against a changed working tree replays the old answer and hides the change.
//! That boundary is deliberate (journaling every byte a tool touched is a different, much
//! larger feature), and it is why divergence detection sits on the call fingerprints: the one
//! thing this layer can actually see.
//!
//! ## What a journal contains, and therefore how to treat it
//!
//! Everything: the system prompt, the user's words, every model reply, every tool result. It
//! is exactly as sensitive as the session it recorded. Recording is explicit (a caller
//! constructs the decorators; nothing records by default) and the file is the caller's to
//! place, keep, and delete — the same rules `meeting::repro` already states for incident
//! bundles.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{ToolError, ToolSource};
use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, ModelError, ModelResult, Role,
    StopReason, ToolDef,
};

// ─── The journal ──────────────────────────────────────────────────────────────

/// One recorded interaction, in the order the loop made it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// One `ChatBackend::chat` call: the request's fingerprint and every event the stream
    /// yielded, in order, including a terminal error if that is how it ended.
    Model {
        /// Fingerprint of the request that produced it (see [`request_fingerprint`]).
        call: String,
        events: Vec<EventRec>,
        /// Set when the stream itself failed to start.
        error: Option<String>,
    },
    /// One `ToolSource::dispatch` call.
    Tool {
        /// Fingerprint of the call (see [`tool_fingerprint`]).
        call: String,
        name: String,
        args: Value,
        /// `Ok` payload, or the tool's error message.
        ok: Option<Value>,
        err: Option<String>,
    },
}

/// A `ChatEvent`, in a shape that survives a round trip through JSON. The backend's own enum
/// carries no serde derives, and adding them there would put a storage concern into the
/// model SPI — so the mapping lives here, where the storage is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventRec {
    TextDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseDelta {
        id: String,
        input_json_delta: String,
    },
    ToolUseEnd {
        id: String,
    },
    Done {
        input_tokens: u32,
        output_tokens: u32,
        stop: StopRec,
    },
    Progress,
    /// An error the stream yielded mid-flight (as opposed to failing to start).
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "stop", rename_all = "snake_case")]
pub enum StopRec {
    EndTurn,
    MaxTokens,
    ToolUse,
    Cancelled,
    Sequence { text: String },
}

impl From<&StopReason> for StopRec {
    fn from(s: &StopReason) -> Self {
        match s {
            StopReason::EndTurn => Self::EndTurn,
            StopReason::MaxTokens => Self::MaxTokens,
            StopReason::ToolUse => Self::ToolUse,
            StopReason::Cancelled => Self::Cancelled,
            StopReason::StopSequence(t) => Self::Sequence { text: t.clone() },
        }
    }
}

impl From<&StopRec> for StopReason {
    fn from(s: &StopRec) -> Self {
        match s {
            StopRec::EndTurn => Self::EndTurn,
            StopRec::MaxTokens => Self::MaxTokens,
            StopRec::ToolUse => Self::ToolUse,
            StopRec::Cancelled => Self::Cancelled,
            StopRec::Sequence { text } => Self::StopSequence(text.clone()),
        }
    }
}

impl From<&ChatEvent> for EventRec {
    fn from(e: &ChatEvent) -> Self {
        match e {
            ChatEvent::TextDelta { text } => Self::TextDelta { text: text.clone() },
            ChatEvent::ToolUseStart { id, name } => Self::ToolUseStart {
                id: id.clone(),
                name: name.clone(),
            },
            ChatEvent::ToolUseDelta {
                id,
                input_json_delta,
            } => Self::ToolUseDelta {
                id: id.clone(),
                input_json_delta: input_json_delta.clone(),
            },
            ChatEvent::ToolUseEnd { id } => Self::ToolUseEnd { id: id.clone() },
            ChatEvent::Done {
                input_tokens,
                output_tokens,
                stop_reason,
            } => Self::Done {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                stop: stop_reason.into(),
            },
            ChatEvent::Progress => Self::Progress,
        }
    }
}

impl EventRec {
    fn to_event(&self) -> ModelResult<ChatEvent> {
        Ok(match self {
            Self::TextDelta { text } => ChatEvent::TextDelta { text: text.clone() },
            Self::ToolUseStart { id, name } => ChatEvent::ToolUseStart {
                id: id.clone(),
                name: name.clone(),
            },
            Self::ToolUseDelta {
                id,
                input_json_delta,
            } => ChatEvent::ToolUseDelta {
                id: id.clone(),
                input_json_delta: input_json_delta.clone(),
            },
            Self::ToolUseEnd { id } => ChatEvent::ToolUseEnd { id: id.clone() },
            Self::Done {
                input_tokens,
                output_tokens,
                stop,
            } => ChatEvent::Done {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                stop_reason: stop.into(),
            },
            Self::Progress => ChatEvent::Progress,
            Self::Error { message } => {
                return Err(ModelError::BackendUnavailable(message.clone()));
            }
        })
    }
}

/// What a model call is recognised by on replay: the roles and text of the messages plus the
/// tool names offered. Deliberately NOT the whole request — sampling seeds, cancellation
/// tokens and session ids differ run to run without changing what was asked.
pub fn request_fingerprint(req: &ChatRequest) -> String {
    let mut s = String::new();
    for m in &req.messages {
        s.push_str(role_tag(&m.role));
        s.push(':');
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => s.push_str(text),
                other => s.push_str(&format!("{other:?}")),
            }
            s.push('\u{1f}');
        }
        s.push('\u{1e}');
    }
    s.push_str("tools:");
    for t in &req.tools {
        s.push_str(&t.name);
        s.push(',');
    }
    short_hash(&s)
}

fn role_tag(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// What a tool call is recognised by: its name and its arguments, canonicalised so key order
/// cannot make two identical calls look different.
pub fn tool_fingerprint(name: &str, args: &Value) -> String {
    short_hash(&format!("{name}\u{1f}{}", canonical(args)))
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{}", canonical(&m[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => {
            format!(
                "[{}]",
                a.iter().map(canonical).collect::<Vec<_>>().join(",")
            )
        }
        other => other.to_string(),
    }
}

/// FNV-1a, hex. A fingerprint only has to distinguish calls within one run's journal; it is
/// not a security boundary and nothing depends on it being collision-proof across runs.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The journal file: append-only JSONL while recording, a cursor while replaying.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    /// Entries loaded for replay, and how far the cursor has advanced.
    replay: Mutex<(Vec<Entry>, usize)>,
}

impl Journal {
    /// Open for RECORDING: truncates, so a journal is one run and never two runs interleaved.
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, "")?;
        Ok(Self {
            path,
            replay: Mutex::new((Vec::new(), 0)),
        })
    }

    /// Open for REPLAY: reads every entry up front. A line that does not parse is a hard
    /// error, not a skip — a journal with a hole in it cannot prove anything.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text = std::fs::read_to_string(&path)?;
        let mut entries = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: Entry = serde_json::from_str(line).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}:{}: {e}", path.display(), i + 1),
                )
            })?;
            entries.push(entry);
        }
        Ok(Self {
            path,
            replay: Mutex::new((entries, 0)),
        })
    }

    fn append(&self, entry: &Entry) {
        use std::io::Write;
        let line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(_) => return,
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The next entry, or an explanation of why there is not one.
    fn next(&self) -> Result<Entry, String> {
        let mut guard = self.replay.lock().expect("journal cursor poisoned");
        let (entries, at) = &mut *guard;
        match entries.get(*at) {
            Some(e) => {
                *at += 1;
                Ok(e.clone())
            }
            None => Err(format!(
                "replay ran past the end of the journal ({} entries) — the run diverged: it is \
                 making calls the recording never made",
                entries.len()
            )),
        }
    }

    /// How many entries were consumed so far (for a test that wants to assert a whole
    /// journal was used, which is the other half of "the replay matched").
    pub fn consumed(&self) -> usize {
        self.replay.lock().expect("journal cursor poisoned").1
    }

    /// How many entries the journal holds.
    pub fn len(&self) -> usize {
        self.replay.lock().expect("journal cursor poisoned").0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ─── Recording ────────────────────────────────────────────────────────────────

/// A live backend that writes what it answered into a journal, and is otherwise invisible:
/// every event is forwarded as it arrives, so a recorded run streams exactly like an
/// unrecorded one.
pub struct RecordingBackend<'a> {
    inner: &'a dyn ChatBackend,
    journal: &'a Journal,
}

impl<'a> RecordingBackend<'a> {
    pub fn new(inner: &'a dyn ChatBackend, journal: &'a Journal) -> Self {
        Self { inner, journal }
    }
}

#[async_trait]
impl ChatBackend for RecordingBackend<'_> {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let call = request_fingerprint(&req);
        let stream = match self.inner.chat(req).await {
            Ok(s) => s,
            Err(e) => {
                self.journal.append(&Entry::Model {
                    call,
                    events: Vec::new(),
                    error: Some(e.to_string()),
                });
                return Err(e);
            }
        };
        // Collect the WHOLE stream before answering. The agent loop consumes a chat stream to
        // completion before it acts on it anyway, so this changes no timing the loop can
        // observe — and it means the journal entry is written once, whole, rather than
        // needing a partial-entry format for a stream that was dropped mid-flight.
        let events: Vec<ModelResult<ChatEvent>> = stream.collect().await;
        let recs: Vec<EventRec> = events
            .iter()
            .map(|e| match e {
                Ok(ev) => EventRec::from(ev),
                Err(err) => EventRec::Error {
                    message: err.to_string(),
                },
            })
            .collect();
        self.journal.append(&Entry::Model {
            call,
            events: recs,
            error: None,
        });
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }
}

/// A live tool source that writes what each call answered into the same journal.
pub struct RecordingTools<'a> {
    inner: &'a dyn ToolSource,
    journal: &'a Journal,
}

impl<'a> RecordingTools<'a> {
    pub fn new(inner: &'a dyn ToolSource, journal: &'a Journal) -> Self {
        Self { inner, journal }
    }
}

#[async_trait]
impl ToolSource for RecordingTools<'_> {
    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let call = tool_fingerprint(name, &args);
        let result = self.inner.dispatch(name, args.clone()).await;
        let (ok, err) = match &result {
            Ok(v) => (Some(v.clone()), None),
            Err(e) => (None, Some(e.0.clone())),
        };
        self.journal.append(&Entry::Tool {
            call,
            name: name.to_string(),
            args,
            ok,
            err,
        });
        result
    }
}

// ─── Replay ───────────────────────────────────────────────────────────────────

/// A backend that answers from a journal instead of a model. The `tools()` a loop advertises
/// come from [`ReplayTools`], so a replay needs no gateway, no network and no model at all.
pub struct ReplayBackend<'a> {
    journal: &'a Journal,
    context_window: u32,
}

impl<'a> ReplayBackend<'a> {
    pub fn new(journal: &'a Journal) -> Self {
        Self {
            journal,
            context_window: 32_768,
        }
    }

    /// Override the context window a replay reports (only matters to a caller that reads it).
    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = tokens;
        self
    }
}

#[async_trait]
impl ChatBackend for ReplayBackend<'_> {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let want = request_fingerprint(&req);
        let entry = self
            .journal
            .next()
            .map_err(ModelError::BackendUnavailable)?;
        match entry {
            Entry::Model {
                call,
                events,
                error,
            } => {
                if call != want {
                    return Err(ModelError::BackendUnavailable(diverged(
                        "model call",
                        &want,
                        &call,
                    )));
                }
                if let Some(msg) = error {
                    return Err(ModelError::BackendUnavailable(msg));
                }
                let evs: Vec<ModelResult<ChatEvent>> =
                    events.iter().map(EventRec::to_event).collect();
                Ok(Box::pin(futures::stream::iter(evs)))
            }
            Entry::Tool { name, .. } => Err(ModelError::BackendUnavailable(format!(
                "replay diverged: the run asked the MODEL, the journal's next entry is the tool \
                 call '{name}'"
            ))),
        }
    }

    fn context_window(&self) -> u32 {
        self.context_window
    }
}

/// A tool source that answers from a journal. `tools()` is whatever the caller declares —
/// a replay must advertise the same schemas the recording did, and those live in the
/// caller's own tool set, not in the journal.
pub struct ReplayTools<'a> {
    journal: &'a Journal,
    defs: Vec<ToolDef>,
}

impl<'a> ReplayTools<'a> {
    pub fn new(journal: &'a Journal, defs: Vec<ToolDef>) -> Self {
        Self { journal, defs }
    }
}

#[async_trait]
impl ToolSource for ReplayTools<'_> {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let want = tool_fingerprint(name, &args);
        let entry = self.journal.next().map_err(ToolError::new)?;
        match entry {
            Entry::Tool {
                call,
                ok,
                err,
                name: recorded,
                ..
            } => {
                if call != want {
                    return Err(ToolError::new(diverged(
                        &format!("tool call '{name}' (journal has '{recorded}')"),
                        &want,
                        &call,
                    )));
                }
                match (ok, err) {
                    (Some(v), _) => Ok(v),
                    (None, Some(e)) => Err(ToolError::new(e)),
                    (None, None) => Err(ToolError::new(
                        "journal entry has neither a result nor an error",
                    )),
                }
            }
            Entry::Model { .. } => Err(ToolError::new(format!(
                "replay diverged: the run called the tool '{name}', the journal's next entry is \
                 a model call"
            ))),
        }
    }
}

/// Replay the MODEL from a journal while the TOOLS run for real against today's world.
///
/// The operator's framing was "transparent nondeterminism on the actual tool calls, as an
/// optional mode — like transaction isolation levels", and the analogy holds with ONE
/// correction that decides the whole design. A weaker DB isolation level tolerates an anomaly
/// you accept in exchange for concurrency. The anomaly here cannot be accepted: the journal's
/// NEXT model turn was produced while looking at the OLD tool result, so the moment a live tool
/// answers differently, every model reply after it is an answer to a question this run is no
/// longer asking. Continuing would not be a weaker guarantee, it would be a confidently wrong
/// replay — the exact failure [`ReplayBackend`]'s fingerprints exist to prevent.
///
/// So this mode does not tolerate the divergence, it DETECTS it and stops:
///
/// - the tool CALL must still match the journal (the model is being replayed, so it must be
///   asking the same thing — a mismatch here means the plan itself diverged);
/// - the tool then runs for real;
/// - if the real result equals the recorded one, the run continues, still sound;
/// - if it differs, the run stops with both results in the message.
///
/// **Stopping is the feature, not a limitation.** The question this mode answers is "does the
/// plan that failed last night still fail against today's tree?", and the answer is precisely
/// the first step where reality stopped matching the recording — which is what the stop
/// reports. The `bugs` fix loop wants exactly this: record the failing run, fix the code,
/// replay the plan against the fix, and read where (or whether) it now diverges.
///
/// The [`ReplayTools`] above stays the strict mode: nothing executes, the world is not touched,
/// and the same run reproduces forever. Use that one for a regression test of the agent loop;
/// use this one to ask a question about the world.
pub struct ReplayLiveTools<'a> {
    journal: &'a Journal,
    inner: &'a dyn ToolSource,
}

impl<'a> ReplayLiveTools<'a> {
    pub fn new(journal: &'a Journal, inner: &'a dyn ToolSource) -> Self {
        Self { journal, inner }
    }
}

#[async_trait]
impl ToolSource for ReplayLiveTools<'_> {
    /// The LIVE definitions, not the journal's: this mode is about running the real thing, and
    /// a tool that no longer exists should fail as a missing tool rather than be faked.
    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let want = tool_fingerprint(name, &args);
        let entry = self.journal.next().map_err(ToolError::new)?;
        let (call, recorded_ok, recorded_err, recorded_name) = match entry {
            Entry::Tool {
                call,
                ok,
                err,
                name,
                ..
            } => (call, ok, err, name),
            Entry::Model { .. } => {
                return Err(ToolError::new(format!(
                    "replay diverged: the run called the tool '{name}', the journal's next entry                      is a model call"
                )));
            }
        };
        // The plan itself must match. This is the same check the strict mode makes, and it has
        // to come FIRST: running a live tool for a call the recording never made would touch
        // the world on behalf of a run that already diverged.
        if call != want {
            return Err(ToolError::new(diverged(
                &format!("tool call '{name}' (journal has '{recorded_name}')"),
                &want,
                &call,
            )));
        }

        let live = self.inner.dispatch(name, args).await;

        // Compare what the world says NOW against what it said then. Both the ok and the error
        // side matter: a tool that used to fail and now succeeds is exactly the "the fix
        // works" signal this mode exists to produce, and it is still a divergence — the
        // journal's later model turns were written against the failure.
        let (now, then) = match (&live, &recorded_ok, &recorded_err) {
            (Ok(v), Some(r), _) if v == r => return Ok(v.clone()),
            (Err(e), _, Some(r)) if e.to_string() == *r => return Err(ToolError::new(r.clone())),
            (Ok(v), _, Some(r)) => (format!("ok: {v}"), format!("error: {r}")),
            (Ok(v), Some(r), _) => (format!("ok: {v}"), format!("ok: {r}")),
            (Err(e), Some(r), _) => (format!("error: {e}"), format!("ok: {r}")),
            (Err(e), _, Some(r)) => (format!("error: {e}"), format!("error: {r}")),
            (Ok(v), None, None) => (format!("ok: {v}"), "neither a result nor an error".into()),
            (Err(e), None, None) => {
                (format!("error: {e}"), "neither a result nor an error".into())
            }
        };
        Err(ToolError::new(format!(
            "replay stopped at a live tool divergence: '{name}' returned {now}, the \
             recording has {then}. The plan replayed identically up to here ({} of {} \
             journal entries); the world underneath did not. Everything the journal holds \
             after this point was the model answering the OLD result, so replaying further \
             would be a confidently wrong run rather than a weaker one.",
            self.journal.consumed(),
            self.journal.len()
        )))
    }
}

fn diverged(what: &str, want: &str, got: &str) -> String {
    format!(
        "replay diverged at a {what}: this run's fingerprint is {want}, the journal's next \
         entry is {got}. The recording and this run are not the same run."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::CallbackToolSource;
    use serde_json::json;

    fn req(text: &str) -> ChatRequest {
        ChatRequest::simple(text)
    }

    fn done() -> ChatEvent {
        ChatEvent::Done {
            input_tokens: 1,
            output_tokens: 2,
            stop_reason: StopReason::EndTurn,
        }
    }

    /// A backend that answers a fixed script, so a "recording" has something to record.
    struct Scripted {
        replies: Mutex<Vec<Vec<ChatEvent>>>,
    }

    impl Scripted {
        fn new(replies: Vec<Vec<ChatEvent>>) -> Self {
            Self {
                replies: Mutex::new(replies),
            }
        }
    }

    #[async_trait]
    impl ChatBackend for Scripted {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            let mut g = self.replies.lock().unwrap();
            if g.is_empty() {
                return Err(ModelError::BackendUnavailable("script exhausted".into()));
            }
            let evs = g.remove(0);
            Ok(Box::pin(futures::stream::iter(
                evs.into_iter().map(Ok).collect::<Vec<_>>(),
            )))
        }
        fn context_window(&self) -> u32 {
            4096
        }
    }

    /// `ChatStream` is not `Debug`, so `unwrap_err` cannot be used on a chat result.
    fn chat_err(r: ModelResult<ChatStream>) -> ModelError {
        match r {
            Ok(_) => panic!("expected the replay to refuse, it answered"),
            Err(e) => e,
        }
    }

    async fn drain(stream: ChatStream) -> Vec<String> {
        stream
            .map(|e| match e {
                Ok(ChatEvent::TextDelta { text }) => format!("text:{text}"),
                Ok(ChatEvent::Done { stop_reason, .. }) => format!("done:{stop_reason:?}"),
                Ok(other) => format!("other:{other:?}"),
                Err(e) => format!("err:{e}"),
            })
            .collect()
            .await
    }

    #[tokio::test]
    async fn a_recorded_model_call_replays_event_for_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");

        let live = Scripted::new(vec![vec![
            ChatEvent::TextDelta {
                text: "hello".into(),
            },
            done(),
        ]]);
        let rec_journal = Journal::create(&path).unwrap();
        let recording = RecordingBackend::new(&live, &rec_journal);
        let recorded = drain(recording.chat(req("hi")).await.unwrap()).await;

        let replay_journal = Journal::open(&path).unwrap();
        let replay = ReplayBackend::new(&replay_journal);
        let replayed = drain(replay.chat(req("hi")).await.unwrap()).await;

        assert_eq!(recorded, vec!["text:hello", "done:EndTurn"]);
        assert_eq!(
            replayed, recorded,
            "replay must answer exactly what was recorded"
        );
        assert_eq!(replay_journal.consumed(), 1);
    }

    #[tokio::test]
    async fn a_recorded_tool_call_replays_including_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");

        let live = CallbackToolSource::new()
            .with_tool(
                ToolDef {
                    name: "add".into(),
                    description: "add".into(),
                    input_schema: json!({"type":"object"}),
                },
                |args| Ok(json!({"sum": args["a"].as_i64().unwrap_or(0) + 1})),
            )
            .with_tool(
                ToolDef {
                    name: "boom".into(),
                    description: "fails".into(),
                    input_schema: json!({"type":"object"}),
                },
                |_| Err(ToolError::new("no")),
            );

        let journal = Journal::create(&path).unwrap();
        let recording = RecordingTools::new(&live, &journal);
        let ok = recording.dispatch("add", json!({"a": 1})).await;
        let bad = recording.dispatch("boom", json!({})).await;
        assert_eq!(ok.unwrap(), json!({"sum": 2}));
        assert!(bad.is_err());

        let replay_journal = Journal::open(&path).unwrap();
        let replay = ReplayTools::new(&replay_journal, live.tools());
        assert_eq!(
            replay.dispatch("add", json!({"a": 1})).await.unwrap(),
            json!({"sum": 2})
        );
        assert_eq!(
            replay.dispatch("boom", json!({})).await.unwrap_err().0,
            "no",
            "a recorded FAILURE replays as the same failure"
        );
    }

    #[tokio::test]
    async fn key_order_does_not_make_the_same_call_look_different() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let live = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "t".into(),
                description: "t".into(),
                input_schema: json!({"type":"object"}),
            },
            |_| Ok(json!("fine")),
        );
        let journal = Journal::create(&path).unwrap();
        RecordingTools::new(&live, &journal)
            .dispatch("t", json!({"a": 1, "b": 2}))
            .await
            .unwrap();

        let replay_journal = Journal::open(&path).unwrap();
        let replay = ReplayTools::new(&replay_journal, live.tools());
        // the SAME call, serialized with the keys the other way round
        let out = replay.dispatch("t", json!({"b": 2, "a": 1})).await;
        assert_eq!(out.unwrap(), json!("fine"));
    }

    #[tokio::test]
    async fn a_different_tool_argument_is_refused_loudly_not_answered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let live = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "t".into(),
                description: "t".into(),
                input_schema: json!({"type":"object"}),
            },
            |_| Ok(json!("recorded answer")),
        );
        let journal = Journal::create(&path).unwrap();
        RecordingTools::new(&live, &journal)
            .dispatch("t", json!({"a": 1}))
            .await
            .unwrap();

        let replay_journal = Journal::open(&path).unwrap();
        let replay = ReplayTools::new(&replay_journal, live.tools());
        let err = replay.dispatch("t", json!({"a": 999})).await.unwrap_err();
        assert!(err.0.contains("diverged"), "{}", err.0);
    }

    #[tokio::test]
    async fn asking_the_model_where_the_journal_has_a_tool_is_a_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let live = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "t".into(),
                description: "t".into(),
                input_schema: json!({"type":"object"}),
            },
            |_| Ok(json!(1)),
        );
        let journal = Journal::create(&path).unwrap();
        RecordingTools::new(&live, &journal)
            .dispatch("t", json!({}))
            .await
            .unwrap();

        let replay_journal = Journal::open(&path).unwrap();
        let replay = ReplayBackend::new(&replay_journal);
        let err = chat_err(replay.chat(req("anything")).await);
        assert!(format!("{err}").contains("the journal's next entry is the tool call 't'"));
    }

    #[tokio::test]
    async fn running_past_the_end_of_the_journal_is_a_divergence_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        Journal::create(&path).unwrap();

        let replay_journal = Journal::open(&path).unwrap();
        assert!(replay_journal.is_empty());
        let replay = ReplayBackend::new(&replay_journal);
        let err = chat_err(replay.chat(req("hi")).await);
        assert!(format!("{err}").contains("ran past the end"), "{err}");
    }

    #[tokio::test]
    async fn model_and_tool_entries_interleave_in_call_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");

        let live_model = Scripted::new(vec![
            vec![
                ChatEvent::TextDelta {
                    text: "first".into(),
                },
                done(),
            ],
            vec![
                ChatEvent::TextDelta {
                    text: "second".into(),
                },
                done(),
            ],
        ]);
        let live_tools = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "t".into(),
                description: "t".into(),
                input_schema: json!({"type":"object"}),
            },
            |_| Ok(json!("tool said")),
        );

        let journal = Journal::create(&path).unwrap();
        let rb = RecordingBackend::new(&live_model, &journal);
        let rt = RecordingTools::new(&live_tools, &journal);
        drain(rb.chat(req("one")).await.unwrap()).await;
        rt.dispatch("t", json!({})).await.unwrap();
        drain(rb.chat(req("two")).await.unwrap()).await;

        // Replayed through the SAME order, every answer matches; the journal is fully used.
        let jr = Journal::open(&path).unwrap();
        let pb = ReplayBackend::new(&jr);
        let pt = ReplayTools::new(&jr, live_tools.tools());
        assert_eq!(
            drain(pb.chat(req("one")).await.unwrap()).await,
            vec!["text:first", "done:EndTurn"]
        );
        assert_eq!(
            pt.dispatch("t", json!({})).await.unwrap(),
            json!("tool said")
        );
        assert_eq!(
            drain(pb.chat(req("two")).await.unwrap()).await,
            vec!["text:second", "done:EndTurn"]
        );
        assert_eq!(jr.consumed(), jr.len(), "the whole journal was used");
    }

    #[tokio::test]
    async fn the_same_order_swapped_is_caught_rather_than_silently_answered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let live_model = Scripted::new(vec![vec![done()]]);
        let live_tools = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "t".into(),
                description: "t".into(),
                input_schema: json!({"type":"object"}),
            },
            |_| Ok(json!(1)),
        );
        let journal = Journal::create(&path).unwrap();
        drain(
            RecordingBackend::new(&live_model, &journal)
                .chat(req("x"))
                .await
                .unwrap(),
        )
        .await;
        RecordingTools::new(&live_tools, &journal)
            .dispatch("t", json!({}))
            .await
            .unwrap();

        // Replay the two calls in the OTHER order.
        let jr = Journal::open(&path).unwrap();
        let pt = ReplayTools::new(&jr, live_tools.tools());
        let err = pt.dispatch("t", json!({})).await.unwrap_err();
        assert!(err.0.contains("next entry is a model call"), "{}", err.0);
    }

    /// The claim the whole module exists for, end to end: a REAL `run_agent` loop —
    /// model asks for a tool, tool answers, model answers the user — recorded once, then
    /// re-run from the journal with no model and no tools in sight, producing the same
    /// answer, the same steps and the same operations.
    #[tokio::test]
    async fn a_whole_agent_run_replays_with_no_model_and_no_tools() {
        use crate::agent::{Budget, run_agent};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");

        let add_def = ToolDef {
            name: "add".into(),
            description: "add one".into(),
            input_schema: json!({"type":"object","properties":{"a":{"type":"integer"}}}),
        };
        let live_tools = CallbackToolSource::new().with_tool(add_def.clone(), |args| {
            Ok(json!({ "sum": args["a"].as_i64().unwrap_or(0) + 1 }))
        });
        // Step 1: the model asks for the tool. Step 2: it answers in text.
        let live_model = Scripted::new(vec![
            vec![
                ChatEvent::ToolUseStart {
                    id: "c1".into(),
                    name: "add".into(),
                },
                ChatEvent::ToolUseDelta {
                    id: "c1".into(),
                    input_json_delta: "{\"a\":41}".into(),
                },
                ChatEvent::ToolUseEnd { id: "c1".into() },
                ChatEvent::Done {
                    input_tokens: 10,
                    output_tokens: 5,
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                ChatEvent::TextDelta {
                    text: "the sum is 42".into(),
                },
                done(),
            ],
        ]);

        let budget = Budget {
            max_steps: 4,
            ..Budget::default()
        };

        let recorded = {
            let journal = Journal::create(&path).unwrap();
            let backend = RecordingBackend::new(&live_model, &journal);
            let tools = RecordingTools::new(&live_tools, &journal);
            run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await
        };
        assert_eq!(recorded.text, "the sum is 42");
        assert_eq!(
            recorded.operations.len(),
            1,
            "one tool call: {:?}",
            recorded.operations
        );

        // Now the same run, from the file alone: no Scripted model, no CallbackToolSource.
        let journal = Journal::open(&path).unwrap();
        let backend = ReplayBackend::new(&journal);
        let tools = ReplayTools::new(&journal, vec![add_def]);
        let replayed = run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await;

        assert_eq!(replayed.text, recorded.text);
        assert_eq!(replayed.steps, recorded.steps);
        assert_eq!(
            replayed.operations.len(),
            recorded.operations.len(),
            "the same tool calls happened"
        );
        assert_eq!(
            journal.consumed(),
            journal.len(),
            "the whole journal was used"
        );
    }

    /// The live-tools mode's happy path: the world still answers what it answered then, so the
    /// run completes exactly as the strict replay would — same text, whole journal consumed.
    #[tokio::test]
    async fn live_tools_replay_completes_when_the_world_still_agrees() {
        use crate::agent::{Budget, run_agent};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let add_def = ToolDef {
            name: "add".into(),
            description: "add one".into(),
            input_schema: json!({"type":"object","properties":{"a":{"type":"integer"}}}),
        };
        let live_tools = CallbackToolSource::new().with_tool(add_def.clone(), |args| {
            Ok(json!({ "sum": args["a"].as_i64().unwrap_or(0) + 1 }))
        });
        let live_model = Scripted::new(vec![
            vec![
                ChatEvent::ToolUseStart { id: "c1".into(), name: "add".into() },
                ChatEvent::ToolUseDelta { id: "c1".into(), input_json_delta: "{\"a\":41}".into() },
                ChatEvent::ToolUseEnd { id: "c1".into() },
                ChatEvent::Done { input_tokens: 10, output_tokens: 5, stop_reason: StopReason::ToolUse },
            ],
            vec![ChatEvent::TextDelta { text: "the sum is 42".into() }, done()],
        ]);
        let budget = Budget { max_steps: 4, ..Budget::default() };

        let recorded = {
            let journal = Journal::create(&path).unwrap();
            let backend = RecordingBackend::new(&live_model, &journal);
            let tools = RecordingTools::new(&live_tools, &journal);
            run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await
        };
        assert_eq!(recorded.text, "the sum is 42");

        // The same tool, still answering the same way.
        let unchanged = CallbackToolSource::new().with_tool(add_def, |args| {
            Ok(json!({ "sum": args["a"].as_i64().unwrap_or(0) + 1 }))
        });
        let journal = Journal::open(&path).unwrap();
        let backend = ReplayBackend::new(&journal);
        let tools = ReplayLiveTools::new(&journal, &unchanged);
        let replayed = run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await;

        assert_eq!(replayed.text, recorded.text, "the world agreed, so the run reproduced");
        assert_eq!(journal.consumed(), journal.len(), "the whole journal was used");
    }

    /// The point of the mode: the plan is identical, the WORLD changed, and that is reported as
    /// the thing that changed — not replayed over with the recording's stale answer.
    #[tokio::test]
    async fn live_tools_replay_stops_at_the_first_result_that_changed() {
        use crate::agent::{Budget, run_agent};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let add_def = ToolDef {
            name: "add".into(),
            description: "add one".into(),
            input_schema: json!({"type":"object","properties":{"a":{"type":"integer"}}}),
        };
        let before = CallbackToolSource::new()
            .with_tool(add_def.clone(), |_| Err(ToolError::new("disk on fire")));
        let live_model = Scripted::new(vec![
            vec![
                ChatEvent::ToolUseStart { id: "c1".into(), name: "add".into() },
                ChatEvent::ToolUseDelta { id: "c1".into(), input_json_delta: "{\"a\":41}".into() },
                ChatEvent::ToolUseEnd { id: "c1".into() },
                ChatEvent::Done { input_tokens: 10, output_tokens: 5, stop_reason: StopReason::ToolUse },
            ],
            vec![ChatEvent::TextDelta { text: "it failed".into() }, done()],
        ]);
        let budget = Budget { max_steps: 4, ..Budget::default() };

        {
            let journal = Journal::create(&path).unwrap();
            let backend = RecordingBackend::new(&live_model, &journal);
            let tools = RecordingTools::new(&before, &journal);
            let out = run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await;
            assert_eq!(out.text, "it failed");
        }

        // The fix landed: the same call now succeeds. That IS a divergence, and the useful one.
        let after = CallbackToolSource::new().with_tool(add_def, |args| {
            Ok(json!({ "sum": args["a"].as_i64().unwrap_or(0) + 1 }))
        });
        let journal = Journal::open(&path).unwrap();
        let backend = ReplayBackend::new(&journal);
        let tools = ReplayLiveTools::new(&journal, &after);
        let out = run_agent(&backend, "be brief", "add one to 41", &tools, &budget).await;

        let seen = out
            .operations
            .iter()
            .filter_map(|op| op.output.as_ref().err().cloned())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            seen.contains("live tool divergence"),
            "the run must report WHERE reality stopped matching, got: {seen:?}"
        );
        assert!(
            seen.contains("disk on fire"),
            "and what the recording had, got: {seen:?}"
        );
    }

    /// The negative of the test above, and the reason divergence detection exists at all:
    /// replaying a journal against a DIFFERENT question must fail, not answer the old one.
    #[tokio::test]
    async fn replaying_a_journal_against_a_different_question_refuses() {
        use crate::agent::{Budget, run_agent};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let live_model = Scripted::new(vec![vec![
            ChatEvent::TextDelta {
                text: "recorded answer".into(),
            },
            done(),
        ]]);
        let no_tools = CallbackToolSource::new();
        let budget = Budget {
            max_steps: 2,
            ..Budget::default()
        };

        {
            let journal = Journal::create(&path).unwrap();
            let backend = RecordingBackend::new(&live_model, &journal);
            let tools = RecordingTools::new(&no_tools, &journal);
            run_agent(
                &backend,
                "be brief",
                "the original question",
                &tools,
                &budget,
            )
            .await;
        }

        let journal = Journal::open(&path).unwrap();
        let backend = ReplayBackend::new(&journal);
        let tools = ReplayTools::new(&journal, vec![]);
        let out = run_agent(
            &backend,
            "be brief",
            "a DIFFERENT question",
            &tools,
            &budget,
        )
        .await;

        assert_ne!(
            out.text, "recorded answer",
            "a different question must not be answered from the old journal"
        );
    }

    #[test]
    fn a_journal_line_that_does_not_parse_is_an_error_not_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "{\"kind\":\"tool\",\"call\":\"x\"}\nnot json\n").unwrap();
        let err = Journal::open(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn creating_a_journal_truncates_so_one_file_is_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "{\"kind\":\"tool\",\"call\":\"old\",\"name\":\"t\",\"args\":null,\"ok\":null,\"err\":null}\n").unwrap();
        Journal::create(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
