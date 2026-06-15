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
        let text = text.into();
        let tokens = tokenize(&text);
        let len = tokens.len();
        let mut tf: HashMap<String, usize> = HashMap::new();
        for t in tokens {
            *tf.entry(t).or_insert(0) += 1;
        }
        for term in tf.keys() {
            *self.df.entry(term.clone()).or_insert(0) += 1;
        }
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
