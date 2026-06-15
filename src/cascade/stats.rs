//! Learned stats store (Phase 7) — the data layer under adaptive routing.
//!
//! Every model attempt is recorded per `(task-class, model)`: did it get accepted, did we have to
//! escalate, how long it took, the judge/quality score, any failure reason — plus the concurrency
//! level and a resource snapshot at the time (the curve the Phase-9 controller consumes). The log
//! is JSONL (the `memory_store` pattern), replayed on start so the cascade keeps learning across
//! restarts. The first consumer here is the **`Learned` start-tier**: start at the cheapest model
//! that has *demonstrably* been good enough for this task-class, instead of always at tier 0. See
//! `docs/specs/cascade-router.md`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::FailReason;
use crate::backend::ChatRequest;

/// The coarse shape of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskShape {
    Freeform,
    Structured,
    ToolUse,
}

/// A coarse difficulty bucket over the classifier's `0..1` score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffBucket {
    Easy,
    Medium,
    Hard,
}

impl DiffBucket {
    pub fn from_score(d: f32) -> Self {
        if d < 0.33 {
            DiffBucket::Easy
        } else if d < 0.66 {
            DiffBucket::Medium
        } else {
            DiffBucket::Hard
        }
    }
}

/// The v1 task-class bucket: `{shape} × {difficulty}`. Stats and the `Learned` start-tier are
/// keyed by it (so "hard tool-use" and "easy chat" learn independently).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskClass {
    pub shape: TaskShape,
    pub difficulty: DiffBucket,
}

impl TaskClass {
    /// Bucket a request by its surface (a `response_schema` → structured; else offered tools →
    /// tool-use; else free-form) and a difficulty score.
    pub fn of(req: &ChatRequest, difficulty: f32) -> Self {
        let shape = if req.sampling.response_schema.is_some() {
            TaskShape::Structured
        } else if !req.tools.is_empty() {
            TaskShape::ToolUse
        } else {
            TaskShape::Freeform
        };
        Self { shape, difficulty: DiffBucket::from_score(difficulty) }
    }

    /// A stable map/JSONL key.
    fn key(&self) -> String {
        format!("{:?}/{:?}", self.shape, self.difficulty)
    }
}

/// System/model resource state at attempt time. Phase 7 records what's cheaply available (often
/// nothing); the Phase-9 controller fills these in and reads them back. All optional.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub mem_free_bytes: Option<u64>,
    pub model_mem_bytes: Option<u64>,
    pub cpu_load: Option<f32>,
}

/// One persisted attempt row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub ts: u64,
    pub task: TaskClass,
    pub model: String,
    pub tier: u32,
    pub accepted: bool,
    pub escalated: bool,
    pub latency_ms: u64,
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub judge_score: Option<f32>,
    #[serde(default)]
    pub fail_reason: Option<FailReason>,
    /// In-flight concurrency for this model at dispatch (Phase-9 signal; 0 until then).
    #[serde(default)]
    pub concurrency: u32,
    #[serde(default)]
    pub resource: ResourceSnapshot,
}

/// In-memory rollup for one `(task-class, model)`.
#[derive(Debug, Clone, Default)]
pub struct ModelTaskStat {
    pub attempts: u64,
    pub accepts: u64,
    pub escalations: u64,
    pub failures: u64,
    /// EWMA latency (ms) and quality score over the attempts that carried one.
    pub latency_ewma_ms: f64,
    pub score_ewma: Option<f64>,
}

impl ModelTaskStat {
    pub fn accept_rate(&self) -> f64 {
        if self.attempts == 0 {
            0.0
        } else {
            self.accepts as f64 / self.attempts as f64
        }
    }
}

/// EWMA smoothing factor — recent attempts weigh more (drift with the live system).
const EWMA_ALPHA: f64 = 0.2;

fn ewma(prev: f64, sample: f64, first: bool) -> f64 {
    if first {
        sample
    } else {
        EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * prev
    }
}

struct Inner {
    agg: HashMap<(String, String), ModelTaskStat>,
}

/// Append-only learned stats, persisted as JSONL and replayed on open.
pub struct StatsStore {
    /// Backing JSONL file; empty = in-memory only.
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl StatsStore {
    /// Open (or create) a store, replaying any existing records into the aggregates.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut agg: HashMap<(String, String), ModelTaskStat> = HashMap::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                let t = line?;
                let t = t.trim();
                if t.is_empty() {
                    continue;
                }
                // Skip a corrupt line rather than failing the whole open.
                if let Ok(rec) = serde_json::from_str::<AttemptRecord>(t) {
                    fold(&mut agg, &rec);
                }
            }
        }
        Ok(Self { path, inner: Mutex::new(Inner { agg }) })
    }

    /// An ephemeral store (no file) for tests / scratch.
    pub fn in_memory() -> Self {
        Self { path: PathBuf::new(), inner: Mutex::new(Inner { agg: HashMap::new() }) }
    }

    /// Record one attempt: append to the JSONL log and fold into the aggregates.
    pub fn record(&self, rec: AttemptRecord) {
        let mut inner = self.inner.lock().unwrap();
        if !self.path.as_os_str().is_empty() {
            if let Ok(line) = serde_json::to_string(&rec) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        fold(&mut inner.agg, &rec);
    }

    /// The rollup for `(task, model)`, if any attempts are on record.
    pub fn stat(&self, task: TaskClass, model: &str) -> Option<ModelTaskStat> {
        self.inner.lock().unwrap().agg.get(&(task.key(), model.to_string())).cloned()
    }

    /// **The `Learned` start-tier.** Among the cost-ordered `model_ids` (cheapest first), the index
    /// of the cheapest model whose historical accept-rate for `task` clears `accept_threshold` with
    /// at least `min_attempts` of evidence. `None` → not enough evidence (the caller falls back to
    /// the classifier / cheapest). This is how the cascade learns to *skip* a cheap tier that
    /// reliably escalates on a class, without hard-coding it.
    pub fn learned_start_tier(
        &self,
        task: TaskClass,
        model_ids: &[&str],
        accept_threshold: f64,
        min_attempts: u64,
    ) -> Option<usize> {
        let inner = self.inner.lock().unwrap();
        let key = task.key();
        for (i, id) in model_ids.iter().enumerate() {
            if let Some(s) = inner.agg.get(&(key.clone(), id.to_string())) {
                if s.attempts >= min_attempts && s.accept_rate() >= accept_threshold {
                    return Some(i);
                }
            }
        }
        None
    }
}

fn fold(agg: &mut HashMap<(String, String), ModelTaskStat>, rec: &AttemptRecord) {
    let e = agg.entry((rec.task.key(), rec.model.clone())).or_default();
    let first = e.attempts == 0;
    e.attempts += 1;
    if rec.accepted {
        e.accepts += 1;
    }
    if rec.escalated {
        e.escalations += 1;
    }
    if rec.fail_reason.is_some() {
        e.failures += 1;
    }
    e.latency_ewma_ms = ewma(e.latency_ewma_ms, rec.latency_ms as f64, first);
    if let Some(score) = rec.judge_score {
        let first_score = e.score_ewma.is_none();
        e.score_ewma = Some(ewma(e.score_ewma.unwrap_or(0.0), score as f64, first_score));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{SamplingParams, ToolDef};
    use serde_json::json;

    fn rec(task: TaskClass, model: &str, tier: u32, accepted: bool) -> AttemptRecord {
        AttemptRecord {
            ts: 0,
            task,
            model: model.into(),
            tier,
            accepted,
            escalated: !accepted,
            latency_ms: 100,
            input_tokens: 0,
            output_tokens: 0,
            judge_score: None,
            fail_reason: None,
            concurrency: 0,
            resource: ResourceSnapshot::default(),
        }
    }

    fn hard_freeform() -> TaskClass {
        TaskClass { shape: TaskShape::Freeform, difficulty: DiffBucket::Hard }
    }

    #[test]
    fn task_class_buckets_shape_and_difficulty() {
        let free = TaskClass::of(&ChatRequest::simple("hi"), 0.1);
        assert_eq!(free, TaskClass { shape: TaskShape::Freeform, difficulty: DiffBucket::Easy });

        let mut tool_req = ChatRequest::simple("use a tool");
        tool_req.tools = vec![ToolDef { name: "t".into(), description: "".into(), input_schema: json!({}) }];
        let tooled = TaskClass::of(&tool_req, 0.5);
        assert_eq!(tooled, TaskClass { shape: TaskShape::ToolUse, difficulty: DiffBucket::Medium });

        let mut struct_req = ChatRequest::simple("give json");
        struct_req.sampling = SamplingParams { response_schema: Some(json!({})), ..Default::default() };
        let structured = TaskClass::of(&struct_req, 0.9);
        assert_eq!(
            structured,
            TaskClass { shape: TaskShape::Structured, difficulty: DiffBucket::Hard }
        );
    }

    #[test]
    fn record_updates_accept_rate() {
        let s = StatsStore::in_memory();
        let task = hard_freeform();
        s.record(rec(task, "cheap", 0, false));
        s.record(rec(task, "cheap", 0, false));
        s.record(rec(task, "cheap", 0, true));
        let st = s.stat(task, "cheap").unwrap();
        assert_eq!(st.attempts, 3);
        assert_eq!(st.accepts, 1);
        assert_eq!(st.escalations, 2);
        assert!((st.accept_rate() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn learned_start_tier_skips_a_chronically_escalating_cheap_tier() {
        let s = StatsStore::in_memory();
        let task = hard_freeform();
        // cheap escalates ~9/10 on hard tasks; strong accepts ~9/10.
        for i in 0..10 {
            s.record(rec(task, "cheap", 0, i == 0));
            s.record(rec(task, "strong", 1, i != 0));
        }
        let ids = ["cheap", "strong"];
        let start = s.learned_start_tier(task, &ids, 0.6, 5);
        assert_eq!(start, Some(1), "skip the cheap tier that reliably escalates on hard tasks");
    }

    #[test]
    fn learned_start_tier_needs_evidence() {
        let s = StatsStore::in_memory();
        let task = hard_freeform();
        s.record(rec(task, "cheap", 0, true)); // only 1 sample (< min_attempts)
        let ids = ["cheap", "strong"];
        assert_eq!(s.learned_start_tier(task, &ids, 0.6, 5), None, "too little evidence → no opinion");
    }

    #[test]
    fn jsonl_persists_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.jsonl");
        let task = hard_freeform();
        {
            let s = StatsStore::open(&path).unwrap();
            s.record(rec(task, "cheap", 0, false));
            s.record(rec(task, "cheap", 0, true));
        }
        // Reopen → the aggregates are rebuilt from the log.
        let s2 = StatsStore::open(&path).unwrap();
        let st = s2.stat(task, "cheap").unwrap();
        assert_eq!(st.attempts, 2);
        assert_eq!(st.accepts, 1);
    }
}
