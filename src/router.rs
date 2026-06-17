//! Small model as router / classifier — the up-front *pre-filter* counterpart to
//! the cascade's after-the-fact escalation. A small model (4B / Coder-7B) reads a
//! query and picks one of a **caller-supplied** label set: intent, a route (which
//! model/tool), RAG relevance, a difficulty bucket. Cheap and single-shot — reserve
//! the big model for the actual work. See `docs/specs/small-model-router.md`.
//!
//! Engine-agnostic: it drives any [`ChatBackend`] (mirrors `cascade::ModelJudge` —
//! tight prompt, `temp 0`, tiny `max_tokens`, parse-with-fallback). It **never
//! errors**: an unparseable / off-set reply snaps to the nearest label, and total
//! failure falls back to the first label with `fallback_used = true`. A cheap
//! pre-filter must degrade gracefully, not break the caller.

use std::sync::Arc;

use crate::rag_lite::{Hit, Retriever};
use crate::{ChatBackend, ChatRequest, collect_to_string};

/// One classification target: a short `name` the model should emit and a one-line
/// `hint` describing it (shown in the prompt to steer the model).
#[derive(Debug, Clone)]
pub struct Label {
    pub name: String,
    pub hint: String,
}

impl Label {
    pub fn new(name: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { name: name.into(), hint: hint.into() }
    }
}

/// The standard difficulty-ordered label set (cheapest → hardest) for using a
/// [`ModelRouter`] as the cascade's [`ModelRouter::difficulty`] source — `trivial`
/// → `0.0`, `moderate` → `0.5`, `hard` → `1.0`.
pub fn difficulty_labels() -> Vec<Label> {
    vec![
        Label::new("trivial", "a greeting, a one-word answer, or a simple lookup"),
        Label::new("moderate", "a normal question needing a short focused answer"),
        Label::new("hard", "long multi-step reasoning, math proofs, or substantial code"),
    ]
}

/// The outcome of a classification: the chosen label, a coarse confidence, and
/// whether the router fell back (the model's reply didn't name a label, or the call
/// failed). `confidence` is coarse in v1 (no logprobs): `1.0` exact, `0.6` snapped
/// from noisy text, `0.0` fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub label: String,
    pub confidence: f32,
    pub fallback_used: bool,
}

/// A small-model classifier over a fixed label set.
pub struct ModelRouter {
    backend: Arc<dyn ChatBackend>,
    labels: Vec<Label>,
    max_tokens: u32,
}

impl ModelRouter {
    /// Build a router. Rejects an empty label set (there'd be nothing to choose).
    pub fn new(backend: Arc<dyn ChatBackend>, labels: Vec<Label>) -> Result<Self, String> {
        if labels.is_empty() {
            return Err("ModelRouter: at least one label is required".into());
        }
        Ok(Self { backend, labels, max_tokens: 16 })
    }

    /// The label set this router chooses among.
    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// Classify `query` into one of the labels. Greedy, single-shot, never errors —
    /// an off-set / empty reply falls back to the first label.
    pub async fn classify(&self, query: &str) -> Classification {
        let mut req = ChatRequest::simple(self.prompt(query));
        req.sampling.temperature = Some(0.0);
        req.sampling.max_tokens = Some(self.max_tokens);
        let reply = match self.backend.chat(req).await {
            Ok(stream) => collect_to_string(stream).await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        match snap_to_label(&reply, &self.labels) {
            Some(m) => Classification {
                label: self.labels[m.index].name.clone(),
                confidence: if m.exact { 1.0 } else { 0.6 },
                fallback_used: false,
            },
            None => Classification {
                label: self.labels[0].name.clone(),
                confidence: 0.0,
                fallback_used: true,
            },
        }
    }

    /// A difficulty score in `0.0..=1.0` for `query`, for use as the cascade's
    /// (async, model-backed) `Classifier` signal. The label set is treated as
    /// ordered **cheapest → hardest**, and the chosen label's position maps to
    /// `index / (n-1)` — so construct the router with difficulty-ordered labels
    /// (e.g. `["trivial","moderate","hard"]`) to use this. A single label, or a
    /// fallback (the model didn't name a label / the call failed), → `0.0` (the
    /// conservative cheap-first default; the cascade still escalates from there).
    pub async fn difficulty(&self, query: &str) -> f32 {
        if self.labels.len() <= 1 {
            return 0.0;
        }
        let c = self.classify(query).await;
        let idx = self.labels.iter().position(|l| l.name == c.label).unwrap_or(0);
        idx as f32 / (self.labels.len() - 1) as f32
    }

    /// The classification prompt: enumerate labels + hints, ask for ONLY the name.
    fn prompt(&self, query: &str) -> String {
        let mut p = String::from(
            "You are a fast text classifier. Classify the input below into exactly one \
             of these categories:\n\n",
        );
        for l in &self.labels {
            p.push_str("- ");
            p.push_str(&l.name);
            if !l.hint.is_empty() {
                p.push_str(": ");
                p.push_str(&l.hint);
            }
            p.push('\n');
        }
        p.push_str("\nInput:\n");
        p.push_str(query);
        p.push_str("\n\nReply with ONLY the category name, nothing else.");
        p
    }
}

/// A matched label and whether it was an exact (whole-reply) match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelMatch {
    pub index: usize,
    pub exact: bool,
}

/// Lowercase alphanumeric tokens of `s` (the same split the lexical index uses).
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Map a small model's noisy reply to one of `labels`. Tolerates surrounding text,
/// casing, punctuation, and a leading `"Label:"`:
///
/// 1. exact — the trimmed/lowercased reply equals a label name → that label, `exact`.
/// 2. snap — the label's name (as a contiguous token run) appears in the reply; if
///    **exactly one** label matches → that label (not exact). Word-boundary matching
///    on tokens, so `code` does not match inside `decode`.
/// 3. otherwise (no label, or two+ ambiguously) → `None` (the caller falls back).
pub fn snap_to_label(reply: &str, labels: &[Label]) -> Option<LabelMatch> {
    let reply_tokens = tokens(reply);
    // 1. exact — the whole reply is just the label's tokens (tolerates stray case /
    //    punctuation / whitespace, e.g. "  Code.  " → exact "code").
    for (i, l) in labels.iter().enumerate() {
        let lt = tokens(&l.name);
        if !lt.is_empty() && reply_tokens == lt {
            return Some(LabelMatch { index: i, exact: true });
        }
    }
    // 2. unique token-run match.
    let mut found: Option<usize> = None;
    for (i, l) in labels.iter().enumerate() {
        let lt = tokens(&l.name);
        if lt.is_empty() {
            continue;
        }
        let matches = reply_tokens.windows(lt.len()).any(|w| w == lt.as_slice());
        if matches {
            if found.is_some() {
                return None; // ambiguous: two+ labels named in the reply
            }
            found = Some(i);
        }
    }
    found.map(|index| LabelMatch { index, exact: false })
}

// ─── RAG rerank / summarize worker ───────────────────────────────────────────

/// How relevant a retrieved passage is to the query, judged by the small model.
/// Ordered most → least useful; [`Relevance::grade`] is the rerank key and
/// `Irrelevant` is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    /// Directly answers or strongly pertains to the query.
    Relevant,
    /// On-topic but only partially useful.
    Related,
    /// Off-topic — does not help answer the query; dropped on rerank.
    Irrelevant,
}

impl Relevance {
    /// Higher = keep-and-rank-first; `0` = drop.
    fn grade(self) -> u8 {
        match self {
            Relevance::Relevant => 2,
            Relevance::Related => 1,
            Relevance::Irrelevant => 0,
        }
    }

    /// The relevance label set the worker classifies into (ordered most → least useful,
    /// so the `snap_to_label` index maps straight to a [`Relevance`]).
    fn labels() -> Vec<Label> {
        vec![
            Label::new("relevant", "directly answers or strongly pertains to the query"),
            Label::new("related", "on-topic but only partially useful"),
            Label::new("irrelevant", "off-topic; does not help answer the query"),
        ]
    }

    fn from_index(i: usize) -> Relevance {
        match i {
            0 => Relevance::Relevant,
            1 => Relevance::Related,
            _ => Relevance::Irrelevant,
        }
    }
}

/// The full result of a grounded retrieval step: the reranked (relevant-first,
/// irrelevant-dropped) hits and a small-model summary of them grounded in their text.
#[derive(Debug, Clone)]
pub struct GroundedAnswer {
    pub hits: Vec<Hit>,
    pub summary: String,
}

/// A small-model **RAG worker** — the P2 counterpart to [`ModelRouter`], same shape
/// (tight prompt, `temp 0`, parse-with-fallback, **never errors**). It runs the narrow,
/// latency-tolerant post-retrieval steps a 4B/Coder-7B handles well:
///
/// - [`rerank`](Self::rerank): judge each [`Hit`]'s relevance to the query, drop the
///   irrelevant ones, and reorder relevant-first (a cheap precision filter over BM25
///   recall from [`crate::rag_lite`]).
/// - [`summarize`](Self::summarize): condense the surviving passages into a concise
///   answer grounded **only** in their text.
///
/// Both degrade gracefully (a model fumble keeps a hit / falls back to the top snippet)
/// so a flaky small model never breaks the caller. See `docs/specs/small-model-router.md`.
pub struct RagWorker {
    backend: Arc<dyn ChatBackend>,
    labels: Vec<Label>,
    summary_max_tokens: u32,
}

impl RagWorker {
    /// Build a worker over a (small) chat backend.
    pub fn new(backend: Arc<dyn ChatBackend>) -> Self {
        Self { backend, labels: Relevance::labels(), summary_max_tokens: 256 }
    }

    /// Cap the summary length (default 256 tokens).
    pub fn with_summary_max_tokens(mut self, n: u32) -> Self {
        self.summary_max_tokens = n;
        self
    }

    /// Judge one passage's relevance to `query`. Never errors — a model failure or an
    /// off-set reply is treated as `Related` (a conservative **keep**, so a fumbled
    /// judgment never silently drops a hit).
    async fn judge(&self, query: &str, passage: &str) -> Relevance {
        let mut req = ChatRequest::simple(self.relevance_prompt(query, passage));
        req.sampling.temperature = Some(0.0);
        req.sampling.max_tokens = Some(16);
        let reply = match self.backend.chat(req).await {
            Ok(stream) => collect_to_string(stream).await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        match snap_to_label(&reply, &self.labels) {
            Some(m) => Relevance::from_index(m.index),
            None => Relevance::Related,
        }
    }

    /// Rerank `hits` by model-judged relevance to `query`: drop the `irrelevant` ones,
    /// then order by relevance grade (most relevant first). `hits` are assumed to arrive
    /// best-first (as [`Retriever::search`] returns them); a **stable** sort preserves
    /// that retriever order within an equal grade, so the model's coarse 3-way verdict
    /// refines — never scrambles — the lexical ranking. Empty in → empty out.
    pub async fn rerank(&self, query: &str, hits: Vec<Hit>) -> Vec<Hit> {
        let mut graded: Vec<(u8, Hit)> = Vec::with_capacity(hits.len());
        for h in hits {
            let grade = self.judge(query, &h.text).await.grade();
            if grade > 0 {
                graded.push((grade, h));
            }
        }
        graded.sort_by(|a, b| b.0.cmp(&a.0)); // stable: ties keep retriever order
        graded.into_iter().map(|(_, h)| h).collect()
    }

    /// Summarize `hits` into a concise answer to `query`, grounded **only** in the
    /// passages. Never errors — an empty/failed generation falls back to the top hit's
    /// text (capped). Empty `hits` → empty string.
    pub async fn summarize(&self, query: &str, hits: &[Hit]) -> String {
        if hits.is_empty() {
            return String::new();
        }
        let mut req = ChatRequest::simple(self.summary_prompt(query, hits));
        req.sampling.temperature = Some(0.0);
        req.sampling.max_tokens = Some(self.summary_max_tokens);
        let out = match self.backend.chat(req).await {
            Ok(stream) => collect_to_string(stream).await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        if out.trim().is_empty() { fallback_snippet(hits) } else { out }
    }

    /// Rerank then summarize the survivors — the full small-model post-retrieval step.
    pub async fn rerank_and_summarize(&self, query: &str, hits: Vec<Hit>) -> GroundedAnswer {
        let hits = self.rerank(query, hits).await;
        let summary = self.summarize(query, &hits).await;
        GroundedAnswer { hits, summary }
    }

    /// Retrieve the top-`k` from `retriever`, model-rerank, and summarize — the
    /// end-to-end grounded answer (composes [`crate::rag_lite`] recall with this
    /// worker's precision filter + summary).
    pub async fn grounded_answer(
        &self,
        retriever: &dyn Retriever,
        query: &str,
        k: usize,
    ) -> GroundedAnswer {
        let hits = retriever.search(query, k);
        self.rerank_and_summarize(query, hits).await
    }

    /// The per-passage relevance prompt: enumerate the labels, then ask for ONLY the name.
    fn relevance_prompt(&self, query: &str, passage: &str) -> String {
        let mut p = String::from(
            "You judge whether a passage helps answer a query. Classify the passage into \
             exactly one of these categories:\n\n",
        );
        for l in &self.labels {
            p.push_str("- ");
            p.push_str(&l.name);
            if !l.hint.is_empty() {
                p.push_str(": ");
                p.push_str(&l.hint);
            }
            p.push('\n');
        }
        p.push_str("\nQuery:\n");
        p.push_str(query);
        p.push_str("\n\nPassage:\n");
        p.push_str(passage);
        p.push_str("\n\nReply with ONLY the category name, nothing else.");
        p
    }

    /// The grounded-summary prompt: answer the query using ONLY the numbered passages.
    fn summary_prompt(&self, query: &str, hits: &[Hit]) -> String {
        let mut p = String::from(
            "Using ONLY the passages below, write a concise answer to the query. Do not add \
             facts that are not in the passages; if they do not contain the answer, say so \
             briefly.\n\nQuery:\n",
        );
        p.push_str(query);
        p.push_str("\n\nPassages:\n");
        for (i, h) in hits.iter().enumerate() {
            p.push_str(&format!("[{}] {}\n", i + 1, h.text));
        }
        p.push_str("\nAnswer:");
        p
    }
}

/// A safe, grounded degrade when the model can't summarize: the best hit's text, capped
/// on a char boundary.
fn fallback_snippet(hits: &[Hit]) -> String {
    const CAP: usize = 500;
    let text = &hits[0].text;
    if text.len() <= CAP {
        return text.clone();
    }
    let mut end = CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag_lite::LexicalIndex;
    use crate::{ChatEvent, ChatStream, ContentBlock, ModelResult, StopReason};
    use async_trait::async_trait;

    fn labels() -> Vec<Label> {
        vec![
            Label::new("code", "writing or editing code"),
            Label::new("math", "calculation or proofs"),
            Label::new("chitchat", "casual conversation"),
        ]
    }

    #[test]
    fn snap_exact_match() {
        let m = snap_to_label("code", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 0, exact: true });
    }

    #[test]
    fn snap_cased_and_punctuated() {
        let m = snap_to_label("  Code.  ", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 0, exact: true });
    }

    #[test]
    fn snap_label_prefix_and_surrounding_text() {
        // "Label: math" / a sentence naming the label → snapped (not exact).
        let m = snap_to_label("Label: math", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 1, exact: false });
        let m2 = snap_to_label("I think this is chitchat, honestly", &labels()).unwrap();
        assert_eq!(m2, LabelMatch { index: 2, exact: false });
    }

    #[test]
    fn snap_no_substring_false_positive() {
        // "decode" must NOT match the label "code" (word-boundary tokens).
        assert!(snap_to_label("please decode this", &labels()).is_none());
    }

    #[test]
    fn snap_ambiguous_is_none() {
        // Two labels named → ambiguous → fall back.
        assert!(snap_to_label("is this code or math?", &labels()).is_none());
    }

    #[test]
    fn snap_off_set_is_none() {
        assert!(snap_to_label("banana", &labels()).is_none());
        assert!(snap_to_label("", &labels()).is_none());
    }

    #[tokio::test]
    async fn empty_label_set_rejected() {
        let backend: Arc<dyn ChatBackend> = Arc::new(crate::HelloBackend::new());
        assert!(ModelRouter::new(backend, vec![]).is_err());
    }

    #[tokio::test]
    async fn fallback_when_model_reply_off_set() {
        // HelloBackend always replies "hello!" — never a label → fallback to labels[0].
        let backend: Arc<dyn ChatBackend> = Arc::new(crate::HelloBackend::new());
        let router = ModelRouter::new(backend, labels()).unwrap();
        let c = router.classify("solve x^2 = 4").await;
        assert_eq!(c.label, "code");
        assert!(c.fallback_used);
        assert_eq!(c.confidence, 0.0);
    }

    // ─── RAG worker (hardware-free, scripted backend) ──────────────────────────

    /// A backend whose reply is computed from the prompt text — lets a test script
    /// per-passage relevance verdicts and a canned summary with no model.
    struct ScriptBackend(Box<dyn Fn(&str) -> String + Send + Sync>);

    #[async_trait]
    impl ChatBackend for ScriptBackend {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            let prompt: String = req
                .messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let reply = (self.0)(&prompt);
            let evs: Vec<ModelResult<ChatEvent>> = vec![
                Ok(ChatEvent::TextDelta { text: reply }),
                Ok(ChatEvent::Done {
                    input_tokens: 1,
                    output_tokens: 1,
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
        fn context_window(&self) -> u32 {
            u32::MAX
        }
    }

    fn worker(f: impl Fn(&str) -> String + Send + Sync + 'static) -> RagWorker {
        RagWorker::new(Arc::new(ScriptBackend(Box::new(f))))
    }

    fn hit(id: &str, score: f32, text: &str) -> Hit {
        Hit { id: id.into(), score, text: text.into() }
    }

    #[tokio::test]
    async fn rerank_drops_irrelevant_and_orders_by_grade() {
        // Script relevance off the passage text: "kept"→relevant, "maybe"→related, else irrelevant.
        let w = worker(|prompt| {
            if prompt.contains("DROPME") {
                "irrelevant".into()
            } else if prompt.contains("MAYBE") {
                "related".into()
            } else {
                "relevant".into()
            }
        });
        // Input is best-first by score, as a retriever returns it.
        let hits = vec![
            hit("a", 3.0, "MAYBE partially useful"),
            hit("b", 2.0, "DROPME off topic"),
            hit("c", 1.0, "directly on point"),
        ];
        let out = w.rerank("q", hits).await;
        // "b" dropped (irrelevant); "c" (relevant, grade 2) outranks "a" (related, grade 1).
        let ids: Vec<&str> = out.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a"]);
    }

    #[tokio::test]
    async fn rerank_stable_within_grade_keeps_retriever_order() {
        // All "relevant" → grade is a tie → retriever order (input order) must be preserved.
        let w = worker(|_| "relevant".into());
        let hits = vec![hit("a", 3.0, "x"), hit("b", 2.0, "y"), hit("c", 1.0, "z")];
        let out = w.rerank("q", hits).await;
        assert_eq!(out.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn rerank_off_set_reply_keeps_hit_conservatively() {
        // A fumbled (off-set) verdict must NOT drop the hit — it's kept as Related.
        let w = worker(|_| "i am not a label".into());
        let out = w.rerank("q", vec![hit("a", 1.0, "x")]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
    }

    #[tokio::test]
    async fn summarize_empty_hits_is_empty() {
        let w = worker(|_| "anything".into());
        assert!(w.summarize("q", &[]).await.is_empty());
    }

    #[tokio::test]
    async fn summarize_falls_back_to_top_snippet_when_model_blank() {
        // Model returns whitespace → fall back to the best hit's text.
        let w = worker(|_| "   ".into());
        let s = w.summarize("q", &[hit("a", 1.0, "the grounded snippet"), hit("b", 0.5, "other")]).await;
        assert_eq!(s, "the grounded snippet");
    }

    #[tokio::test]
    async fn summarize_returns_model_text() {
        let w = worker(|_| "a concise grounded answer".into());
        let s = w.summarize("q", &[hit("a", 1.0, "snippet")]).await;
        assert_eq!(s, "a concise grounded answer");
    }

    #[tokio::test]
    async fn grounded_answer_composes_retrieval_rerank_summary() {
        // Both docs share the query terms (afternoon/nap) so BM25 retrieves both; only the
        // cat doc carries the passage-only discriminator "windowsill".
        let mut ix = LexicalIndex::new();
        ix.add("cat", "cat windowsill afternoon nap");
        ix.add("dog", "dog yard afternoon nap");
        let w = worker(|prompt| {
            if prompt.contains("Passages:") {
                "Cats nap on warm windowsills.".into() // the summary call
            } else if prompt.contains("windowsill") {
                "relevant".into() // only the cat passage
            } else {
                "irrelevant".into()
            }
        });
        let ans = w.grounded_answer(&ix, "afternoon nap location", 3).await;
        assert_eq!(ans.hits.len(), 1, "only the cat passage survives rerank");
        assert_eq!(ans.hits[0].id, "cat");
        assert_eq!(ans.summary, "Cats nap on warm windowsills.");
    }

    // Real-model accuracy eval (M4, ignored). A 4B classifies a small labeled set;
    // accuracy must clear the gate-the-big-model bar. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture model_router_eval
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit"]
    async fn model_router_eval() {
        use crate::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
        let backend: Arc<dyn ChatBackend> =
            Arc::new(MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load"));
        let router = ModelRouter::new(
            backend,
            vec![
                Label::new("code", "writing, editing, or explaining source code"),
                Label::new("math", "arithmetic, algebra, calculus, or proofs"),
                Label::new("chitchat", "greetings and casual small talk"),
            ],
        )
        .unwrap();

        let cases: &[(&str, &str)] = &[
            ("Write a Rust function that reverses a string", "code"),
            ("Fix the borrow checker error in this impl block", "code"),
            ("What is the derivative of x^3 + 2x?", "math"),
            ("Solve the equation 3x + 7 = 22", "math"),
            ("hey there, how's your day going?", "chitchat"),
            ("good morning! nice weather today", "chitchat"),
        ];
        let mut correct = 0usize;
        for (q, want) in cases {
            let c = router.classify(q).await;
            let ok = c.label == *want;
            correct += ok as usize;
            eprintln!(
                "ROUTER  want={want:8}  got={:8}  conf={:.1}  fallback={}  {}  | {q}",
                c.label,
                c.confidence,
                c.fallback_used,
                if ok { "OK" } else { "MISS" }
            );
        }
        let acc = correct as f32 / cases.len() as f32;
        eprintln!("ROUTER accuracy: {correct}/{} = {:.0}%", cases.len(), acc * 100.0);
        assert!(acc >= 0.80, "router accuracy {acc:.2} below the gate-the-big-model bar (0.80)");
    }

    // Real-model RAG-worker eval (M4, ignored). A 4B reranks a small mixed corpus + summarizes
    // the survivor, grounded only in its text. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture rag_worker_eval
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit"]
    async fn rag_worker_eval() {
        use crate::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
        use crate::rag_lite::LexicalIndex;
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
        let backend: Arc<dyn ChatBackend> =
            Arc::new(MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load"));
        let worker = RagWorker::new(backend);

        // A corpus where lexical recall pulls a decoy the model should demote/drop: the rust/
        // ownership doc answers the query; the others share stray terms but are off-topic.
        let mut ix = LexicalIndex::new();
        ix.add("ownership", "In Rust, ownership means each value has a single owner and is freed when the owner goes out of scope.");
        ix.add("python", "In Python, memory is managed by reference counting and a cyclic garbage collector.");
        ix.add("cooking", "A good scope of flavours comes from letting the value of fresh herbs shine.");
        let query = "How does Rust manage memory through ownership?";

        let ans = worker.grounded_answer(&ix, query, 3).await;
        eprintln!("RAG hits (post-rerank): {:?}", ans.hits.iter().map(|h| &h.id).collect::<Vec<_>>());
        eprintln!("RAG summary: {}", ans.summary);

        // The ownership doc must survive and rank first; the cooking decoy (lexical-only match on
        // "scope"/"value") must be dropped.
        assert!(!ans.hits.is_empty(), "rerank dropped everything");
        assert_eq!(ans.hits[0].id, "ownership", "the answering doc must rank first");
        assert!(ans.hits.iter().all(|h| h.id != "cooking"), "the off-topic decoy must be dropped");
        // The summary is grounded — it mentions the key concept and isn't empty.
        let s = ans.summary.to_lowercase();
        assert!(!s.trim().is_empty(), "empty summary");
        assert!(s.contains("owner") || s.contains("scope"), "summary not grounded in the passage");
    }
}
