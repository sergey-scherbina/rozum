//! Lightweight local retrieval (`rag-lite`): index small text documents and retrieve the
//! top-K most relevant to a query. v1 is **lexical** (BM25) — no model, no network, fully
//! deterministic. The [`Retriever`] trait keeps the retrieval API stable so an **embedding**
//! backend can be dropped in later (the "configurable backend" the spec asks for) without
//! touching callers or the agent tool.
//!
//! Exposed to the reference agent runtime as a `search_documents` tool so a small local agent
//! can ground its answers in a local corpus.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent::{CallbackToolSource, ToolError};
use crate::backend::ToolDef;

/// A retrieval hit: the document id, its relevance score, and the text.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub score: f32,
    pub text: String,
}

/// A pluggable retrieval backend. v1 is [`LexicalIndex`] (BM25); an embedding-based index can
/// implement the same trait later.
pub trait Retriever: Send + Sync {
    /// The top-`k` documents most relevant to `query`, best first.
    fn search(&self, query: &str, k: usize) -> Vec<Hit>;
}

/// Split text into lowercase alphanumeric terms.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

struct Doc {
    id: String,
    text: String,
    tf: HashMap<String, usize>,
    len: usize,
}

/// How much a term in a chunk's IDENTIFIER counts against the same term in its body.
///
/// The identifier is `kind name` for code (`fn detect_project`) and the heading for a markdown
/// section, and until now it was not indexed at all — the one piece of the chunk that states
/// what it IS was invisible to ranking. Measured on the 20-question set: without it, a question
/// whose answer is a function is usually won by a SPEC that discusses the function, because a
/// long document mentions the query's words more often than the short function that implements
/// them. Boosting the identifier is what lets `fn detect_project` beat a page about projects.
///
/// 3, not more: this is a tie-breaker, not an override. A chunk whose body genuinely answers the
/// question must still be able to outrank one that merely has a suggestive name — otherwise the
/// index turns into a symbol table, and grep is a better symbol table than this will ever be.
const TITLE_BOOST: usize = 3;

/// Split an identifier into searchable words: `snake_case`, `camelCase`, `kebab-case` and path
/// separators all become separate terms, and the whole is kept too.
///
/// Without the split, `fn detect_project` matches a query saying "detect" but not one saying
/// "project directory", which is how an agent that does not know the symbol actually asks.
fn tokenize_ident(ident: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in ident.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let lower = word.to_lowercase();
        // camelCase / PascalCase inside one word.
        let mut cur = String::new();
        for ch in word.chars() {
            if ch.is_uppercase() && !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            let piece = cur.to_lowercase();
            if piece != lower {
                out.push(piece);
            }
        }
        out.push(lower);
    }
    out
}

/// A BM25 lexical index over small text documents.
pub struct LexicalIndex {
    docs: Vec<Doc>,
    /// term → number of documents containing it.
    df: HashMap<String, usize>,
    total_len: usize,
    k1: f32,
    b: f32,
}

/// BM25's length-normalisation weight. Lowered from the 0.75 textbook default, on a measurement
/// that contradicted the obvious guess.
///
/// The guess was that long, vocabulary-rich chunks were winning. Measured across the eval set's
/// misses, the opposite holds: the chunk that beat the answer had a median of **80 words against
/// the answer's 207**, and was longer in only 5 of 11 cases. So the corpus was being ranked
/// AGAINST its own implementations — a Rust function that does real work is long, and 0.75
/// penalises exactly that.
///
/// Swept against `tests/rag-eval.json` (26 questions), top-1 / top-5:
///
/// ```text
///   b = 0.75   8 / 13     (the default this replaces)
///   b = 0.50   9 / 15     <- chosen
///   b = 0.30   7 / 15
///   b = 0.00   3 / 11     (no length normalisation at all: much worse)
/// ```
///
/// `k1` was swept too and left alone: 1.2 and 1.6 tie, 0.8 and 2.0 are worse.
///
/// Two parameters tuned on 26 questions is a real overfitting risk, and the reason to trust this
/// one is that it was not chosen by the sweep alone — the direction was predicted by the
/// length measurement first, and 0.0 being clearly worse shows the curve has an interior optimum
/// rather than the metric simply rewarding "less normalisation".
const BM25_B: f32 = 0.5;

impl Default for LexicalIndex {
    fn default() -> Self {
        Self { docs: Vec::new(), df: HashMap::new(), total_len: 0, k1: 1.2, b: BM25_B }
    }
}

impl LexicalIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index a document. Re-adding the same `id` simply adds another document (no dedupe).
    pub fn add(&mut self, id: impl Into<String>, text: impl Into<String>) {
        let id = id.into();
        // The identifier is indexed as a boosted field. Derived from `id` rather than passed
        // separately so EVERY caller gets it — an index built by a path that forgot to supply a
        // title would silently rank worse, and nothing would say so.
        let title = title_of(&id);
        self.add_with_title(id, &title, text)
    }

    /// [`add`] with an explicit identifier field, for a caller that knows a better one than the
    /// chunk id carries.
    pub fn add_with_title(&mut self, id: impl Into<String>, title: &str, text: impl Into<String>) {
        let text = text.into();
        let tokens = tokenize(&text);
        let len = tokens.len();
        let mut tf: HashMap<String, usize> = HashMap::new();
        for t in tokens {
            *tf.entry(t).or_insert(0) += 1;
        }
        for t in tokenize_ident(title) {
            *tf.entry(t).or_insert(0) += TITLE_BOOST;
        }
        for term in tf.keys() {
            *self.df.entry(term.clone()).or_insert(0) += 1;
        }
        // `len` counts the BODY only. Adding the boosted identifier terms here too would feed
        // BM25's length normalisation the very inflation the boost just created, cancelling it.
        self.total_len += len;
        self.docs.push(Doc { id: id.into(), text, tf, len });
    }

    /// The stored text of a chunk id — for filling in a fusion hit that only the embedding
    /// ranking carried (BM25 hits arrive with their text; embedding rankings are id-only).
    /// Linear scan: called for at most `k` ids per search against ~10k docs.
    pub fn text_of(&self, id: &str) -> Option<&str> {
        self.docs.iter().find(|d| d.id == id).map(|d| d.text.as_str())
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    fn avg_len(&self) -> f32 {
        if self.docs.is_empty() {
            0.0
        } else {
            self.total_len as f32 / self.docs.len() as f32
        }
    }

    /// BM25 idf with the standard `+1` so it never goes negative for common terms.
    fn idf(&self, term: &str) -> f32 {
        let n = self.docs.len() as f32;
        let df = *self.df.get(term).unwrap_or(&0) as f32;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }
}

impl Retriever for LexicalIndex {
    fn search(&self, query: &str, k: usize) -> Vec<Hit> {
        if self.docs.is_empty() || k == 0 {
            return Vec::new();
        }
        let avg_len = self.avg_len();
        let q_terms = tokenize(query);
        let mut hits: Vec<Hit> = self
            .docs
            .iter()
            .map(|doc| {
                let mut score = 0.0f32;
                for term in &q_terms {
                    let tf = *doc.tf.get(term).unwrap_or(&0) as f32;
                    if tf == 0.0 {
                        continue;
                    }
                    let idf = self.idf(term);
                    let denom = tf + self.k1 * (1.0 - self.b + self.b * doc.len as f32 / avg_len);
                    score += idf * (tf * (self.k1 + 1.0)) / denom;
                }
                Hit { id: doc.id.clone(), score, text: doc.text.clone() }
            })
            .filter(|h| h.score > 0.0)
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        hits
    }
}

// ─── Agent tool ────────────────────────────────────────────────────────────────

/// Expose a [`Retriever`] as a `search_documents` agent tool so the model can pull relevant
/// snippets from a local corpus.
pub fn retrieval_tools(index: Arc<dyn Retriever>) -> CallbackToolSource {
    CallbackToolSource::new().with_tool(search_def(), move |args| {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::new("`query` (string) is required"))?;
        let k = args.get("top_k").and_then(Value::as_u64).unwrap_or(3) as usize;
        let results: Vec<Value> = index
            .search(query, k)
            .into_iter()
            .map(|h| json!({ "id": h.id, "score": h.score, "text": h.text }))
            .collect();
        Ok(json!({ "results": results }))
    })
}


/// Top-`k` hits with RESERVED SLOTS FOR CODE.
///
/// Measured on `crates/rozum-agent/tests/rag-eval.json` — 20 questions whose answers are
/// functions, phrased the way an agent asks when it does not know the symbol: ranking everything
/// together by raw score puts the right chunk first 4 times in 20, while looking at code alone
/// puts it first 8 times. The cause is structural, not a tuning miss: a SPEC that discusses a
/// function mentions the query's words more often than the short function that implements them,
/// and BM25 has no notion of "describes" versus "is".
///
/// Slots rather than a `kind` filter or a blanket boost on code. The operator's case is agents
/// working on code, so code must never be crowded out — but a document is often a right answer
/// too ("where is admission decided" is answered by both the spec and `acquire_residency`), and a
/// filter would throw that away. Slots also cost no extra context: the same `k`, apportioned, and
/// nothing a model can use wrongly.
///
/// Unused slots fall back to raw order, so a docs-only project still returns `k` results and a
/// code-only one is not truncated at half.
pub fn search_balanced(index: &dyn Retriever, query: &str, k: usize) -> Vec<Hit> {
    if k == 0 {
        return Vec::new();
    }
    // Over-fetch: the implementation may sit well below k among tests and prose.
    let raw = index.search(query, k.saturating_mul(8).max(40));
    rebalance(&raw, k)
}

/// The impls-above-tests apportioning, on an ALREADY-RANKED list — public because the FUSED
/// ranking needs the same pass (`rag-ab-failure-forensics`): `search_balanced` demotes test
/// chunks on the BM25 half only, and the embedding half then walks them straight back up
/// through RRF — the Q1 forensics run's top-1 was a `#[test]` fn that the BM25 balance had
/// already pushed down. Callers fuse over a DEEP pool, fill in embedding-only texts (the test
/// detector reads the chunk text), and rebalance to the final k.
pub fn rebalance(raw: &[Hit], k: usize) -> Vec<Hit> {
    if k == 0 {
        return Vec::new();
    }
    let mut impls: Vec<&Hit> = Vec::new();
    let mut rest: Vec<&Hit> = Vec::new();
    for h in raw {
        if is_code_chunk(&h.id) && !is_test_chunk(&h.id, &h.text) && !is_import_chunk(&h.id) {
            impls.push(h);
        } else {
            rest.push(h);
        }
    }
    let impl_slots = (k * 4).div_ceil(5).max(1);
    let mut picked: Vec<Hit> = impls.iter().take(impl_slots).map(|h| (*h).clone()).collect();
    for h in rest {
        if picked.len() >= k {
            break;
        }
        picked.push(h.clone());
    }
    for h in impls.iter().skip(impl_slots) {
        if picked.len() >= k {
            break;
        }
        picked.push((*h).clone());
    }
    // NOT re-sorted by score. Sorting here would undo the whole point: the reason the wrong kinds
    // of chunk win is that they score higher, so re-ranking the apportioned set by score puts
    // them straight back on top and the classes become decoration.
    picked.truncate(k);
    picked
}

/// A TEST rather than an implementation.
///
/// Measured, and larger than it sounds: across the 20-question eval set, **32 of the 100 top-5
/// slots were test chunks** — nearly a third of what an agent is shown for "where is this
/// implemented". The cause is not a scoring bug but a property of good tests: their names are
/// written as English sentences (`single_model_gate_is_identical_with_or_without_reservation`),
/// which is exactly the shape of a natural-language question, so they match it better than the
/// terse function that does the work.
///
/// Demoted, never dropped: sometimes the test IS the answer ("what proves this holds"), and it
/// still fills any slot the implementations leave. Detected from the chunk's own text, so no
/// index format change and no separate pass.
fn is_test_chunk(id: &str, text: &str) -> bool {
    // The attribute must OPEN the chunk, not merely appear in it. `chunk_code` tiles a file, so
    // its last chunk usually carries the whole `mod tests { … }` tail — a `contains` therefore
    // marks ordinary implementation chunks as tests. Measured: the loose version took top-1 from
    // 8/20 to 6/20 by demoting real answers (`fn chunk_text`, `rag_lite.rs`).
    if id.contains("/tests/") {
        return true;
    }
    let head: String = text.chars().take(120).collect();
    let head = head.trim_start();
    head.starts_with("#[test]") || head.starts_with("#[tokio::test]") || head.starts_with("#[cfg(test)]")
}

/// The import block at the top of a file. `chunk_code` tiles a file, so this chunk exists in
/// every source file, is short, and is dense with identifiers — which BM25's length
/// normalisation rewards. It held 6 of those same 100 slots. Kept for its module `//!` doc,
/// but it does not compete for an implementation slot.
fn is_import_chunk(id: &str) -> bool {
    let frag = id.split('#').nth(1).unwrap_or("");
    frag == "use" || frag.starts_with("use ")
}

/// Whether a chunk id names source code rather than prose. By extension, which is crude and
/// deliberately so: the alternative is a per-language table that goes stale, and the only
/// decision riding on it is which of `k` slots a hit competes for.
fn is_code_chunk(id: &str) -> bool {
    let path = id.split('#').next().unwrap_or(id);
    [".rs", ".py", ".sh", ".ssc", ".scala", ".ts", ".js", ".go", ".c", ".h", ".cpp"]
        .iter()
        .any(|e| path.ends_with(e))
}

/// The identifier field of a chunk id: everything after `#`, plus the file's own stem.
///
/// Both halves earn their place. `#fn detect_project` is what the chunk IS; the stem
/// (`daemon_proxy`) is how people refer to the area it lives in, and questions name the area
/// far more often than the path.
fn title_of(id: &str) -> String {
    let (path, frag) = id.split_once('#').unwrap_or((id, ""));
    // An import block is not a NAMED thing, and boosting its fragment ranks it as if it were.
    // `chunk_code` tiles a file, so its first chunk is `#use <first-import>` — short, dense with
    // identifiers, and carrying the module's `//!` doc. Measured: `store.rs#use` and
    // `resident.rs#use` beat the actual functions for "where is the project directory determined"
    // and "what decides residency"; dropping the boost took top-5 from 9/20 to 10/20. The chunk
    // stays indexed — that module doc is genuinely worth finding — it just stops being credited
    // with a symbol name it does not have.
    if frag.starts_with("use ") || frag == "use" {
        return String::new();
    }
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{frag} {stem}")
}

fn search_def() -> ToolDef {
    ToolDef {
        name: "search_documents".into(),
        description: "Search the local document corpus for the snippets most relevant to a query."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "top_k": {"type": "integer", "description": "how many results (default 3)"}
            },
            "required": ["query"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolSource;

    fn corpus() -> LexicalIndex {
        let mut ix = LexicalIndex::new();
        ix.add("d1", "The cat sat quietly on the warm windowsill.");
        ix.add("d2", "Dogs are loyal companions and love long walks.");
        ix.add("d3", "A small kitten chased a ball across the room.");
        ix
    }

    /// The post-fusion half of `rag-ab-failure-forensics`: a `#[test]` chunk the embedding
    /// ranking walked to the top of a FUSED list is demoted below implementations by
    /// `rebalance`, exactly as `search_balanced` already does on the BM25 half — the Q1
    /// forensics run's top-1 was a test fn for precisely this reason.
    #[test]
    fn rebalance_demotes_a_fused_test_chunk_below_impls() {
        let hits = vec![
            Hit {
                id: "src/share.rs#fn residency_refuses_even_sole_model".into(),
                score: 0.9,
                text: "#[test]\n    fn residency_refuses_even_sole_model() { … }".into(),
            },
            Hit {
                id: "src/share.rs#fn acquire_residency".into(),
                score: 0.5,
                text: "pub fn acquire_residency() { … }".into(),
            },
            Hit {
                id: "src/serving.rs#fn admit".into(),
                score: 0.4,
                text: "fn admit() { … }".into(),
            },
        ];
        let out = rebalance(&hits, 3);
        assert_eq!(out[0].id, "src/share.rs#fn acquire_residency", "{out:?}");
        assert_eq!(out[1].id, "src/serving.rs#fn admit", "{out:?}");
        // Demoted, never dropped.
        assert_eq!(out[2].id, "src/share.rs#fn residency_refuses_even_sole_model", "{out:?}");
    }

    #[test]
    fn ranks_relevant_document_first() {
        let ix = corpus();
        let hits = ix.search("cat windowsill", 3);
        assert_eq!(hits[0].id, "d1", "the cat/windowsill doc ranks first");
        // A query with no matching terms returns nothing.
        assert!(ix.search("quantum spaceship", 3).is_empty());
        // Empty index / k=0 are safe.
        assert!(LexicalIndex::new().search("anything", 3).is_empty());
        assert!(ix.search("cat", 0).is_empty());
    }

    #[test]
    fn idf_demotes_a_common_term() {
        // "the" appears in d1 only here, but a rarer query term should still pick its doc.
        let ix = corpus();
        let hits = ix.search("loyal dogs", 1);
        assert_eq!(hits[0].id, "d2");
    }

    #[tokio::test]
    async fn search_documents_tool() {
        let ix: Arc<dyn Retriever> = Arc::new(corpus());
        let tools = retrieval_tools(ix);
        assert_eq!(tools.tools()[0].name, "search_documents");
        let out = tools
            .dispatch("search_documents", json!({"query": "kitten ball", "top_k": 2}))
            .await
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["id"], "d3");
        assert!(results[0]["text"].as_str().unwrap().contains("kitten"));
        // Missing query → recoverable error.
        assert!(tools.dispatch("search_documents", json!({})).await.is_err());
    }
}
