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

impl Default for LexicalIndex {
    fn default() -> Self {
        Self { docs: Vec::new(), df: HashMap::new(), total_len: 0, k1: 1.2, b: 0.75 }
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
    // Over-fetch: the code answer may sit well below k among prose.
    let raw = index.search(query, k.saturating_mul(4).max(20));
    // Most of k, not half: measured, the answer is often the 4th or 5th code chunk, and giving
    // code only half the slots loses exactly those.
    let code_slots = (k * 4).div_ceil(5).max(1);
    let mut picked: Vec<Hit> = Vec::new();
    for h in raw.iter().filter(|h| is_code_chunk(&h.id)).take(code_slots) {
        picked.push(h.clone());
    }
    for h in raw.iter().filter(|h| !is_code_chunk(&h.id)) {
        if picked.len() >= k {
            break;
        }
        picked.push(h.clone());
    }
    for h in raw.iter() {
        if picked.len() >= k {
            break;
        }
        if !picked.iter().any(|p| p.id == h.id) {
            picked.push(h.clone());
        }
    }
    // NOT re-sorted by score. Sorting here would undo the whole point: the reason prose wins is
    // that it scores higher, so re-ranking the apportioned set by score puts it straight back on
    // top and the slots become decoration. Code keeps its slots in code order, prose follows.
    picked.truncate(k);
    picked
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
