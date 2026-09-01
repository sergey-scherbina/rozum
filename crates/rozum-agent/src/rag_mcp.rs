//! A standalone MCP server that serves ONLY `rag.search` — retrieval without the meeting room.
//!
//! The meeting proxy already serves this tool alongside `meeting.*`/`rooms.*`, and that stays;
//! this exists for the user who wants retrieval and nothing else (operator direction,
//! 2026-09-01). Registered like any stdio MCP server:
//!
//! ```json
//! { "rozum-rag": { "command": "rozum", "args": ["rag", "mcp"] } }
//! ```
//!
//! Run from the ENGINE binary it is fully self-contained: the in-process embedding hook
//! (`rozum_core::embedding`, registered at startup when the build has `mlx-native`) does both
//! the corpus warmup and the query embedding, so no meeting daemon and no gateway need to run.
//! In a build without the hook it is BM25-only and says so via `"fused": false` — the same
//! honest degradation the proxy's tool has.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, InitializeRequestParams, InitializeResult, ServerCapabilities},
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::rag_embed::{self, VectorIndex as _};
use crate::rag_lite;

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct RagSearchParams {
    /// What you are looking for, in your own words — a concept, a symptom, a behaviour.
    pub query: String,
    /// How many chunks to return. Default 5, capped at 20.
    pub top_k: Option<u32>,
}

/// Same shape as the proxy's cache, same reasons: the index is read once per process and
/// reloaded on mtime, vectors likewise beside it.
#[derive(Default)]
struct Cache {
    index: Option<Arc<rag_lite::LexicalIndex>>,
    indexed_at: Option<SystemTime>,
    vecs: Option<Arc<rag_embed::VecStore>>,
    vecs_at: Option<SystemTime>,
    chunks: usize,
    tried: bool,
}

#[derive(Clone)]
pub struct RagServer {
    root: PathBuf,
    cache: Arc<Mutex<Cache>>,
    tool_router: ToolRouter<Self>,
}

/// How old an index may be before results carry `stale: true`. Mirrors the proxy's constant —
/// a report, not a refusal.
const STALE_AFTER_SECS: u64 = 3600;

#[tool_router(router = tool_router)]
impl RagServer {
    pub fn new(root: PathBuf) -> Self {
        Self { root, cache: Arc::new(Mutex::new(Cache::default())), tool_router: Self::tool_router() }
    }

    /// Build-or-refresh the index and (when the embedder is in the build) the vectors, in the
    /// background, before the client's first question. The same warmup contract as the proxy:
    /// never on the request path, cross-process lock so N clients starting together do the work
    /// once, per-batch vector saves so it is interruptible.
    pub fn spawn_warmup(&self) {
        if std::env::var("ROZUM_RAG_WARMUP").is_ok_and(|v| v == "0") {
            return;
        }
        let root = self.root.clone();
        tokio::spawn(async move {
            // Index first (blocking, cross-process lock)…
            let r = root.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::rag_chunk::refresh_in_background(&r, &mut |_, _, _| {})
            })
            .await;
            // …then vectors: through the gateway when one answers (jail-safe), in-process
            // otherwise (the truly standalone case, outside any jail).
            if rag_embed::embed_via_gateway(&["probe".into()], false).await.is_some() {
                embed_missing_via_gateway(&root).await;
            } else {
                let r = root.clone();
                let _ = tokio::task::spawn_blocking(move || rag_embed::gateway_less_warmup(&r)).await;
            }
        });
    }

    #[tool(
        name = "rag.search",
        description = "Search THIS project's code and docs by meaning, over syntactic chunks \
         (a markdown section, a Rust `fn`/`impl`/`struct`). Use it when you do NOT know the exact \
         token to grep for: a concept (\"where is admission decided\"), a symptom, or an \
         unfamiliar area whose shape you need before the detail. Do NOT use it when you already \
         know the string, the symbol, or the path — grep and Read are exact, instant and always \
         current, and this index can be stale (every result reports its age). Results name \
         `path#item`, so treat a hit as a pointer to open, not as the answer."
    )]
    pub async fn rag_search(&self, params: Parameters<RagSearchParams>) -> CallToolResult {
        let RagSearchParams { query, top_k } = params.0;
        if query.trim().is_empty() {
            return text(&json!({"error": "`query` must not be empty"}));
        }
        let k = top_k.unwrap_or(5).clamp(1, 20) as usize;
        let root = self.root.clone();

        // Refresh before searching, exactly as the proxy does — and only when an index already
        // exists; the FIRST build belongs to the warmup, not inside a tool call.
        if crate::rag_chunk::index_path(&root).exists() {
            let r = root.clone();
            let refreshed = tokio::task::spawn_blocking(move || {
                crate::rag_chunk::reindex_incremental(&r, &mut |_, _, _| {})
            })
            .await;
            if let Ok(Ok((stats, _))) = refreshed
                && (stats.rechunked > 0 || stats.removed > 0)
            {
                let r = root.clone();
                // Mid-session vectors, same reasoning as the proxy; fire-and-forget.
                tokio::task::spawn_blocking(move || {
                    let _ = rag_embed::gateway_less_warmup(&r);
                });
            }
        }

        let (index, vecs, chunks, age) = {
            let mut c = self.cache.lock().await;
            let ipath = crate::rag_chunk::index_path(&root);
            let i_mtime = std::fs::metadata(&ipath).and_then(|m| m.modified()).ok();
            if !c.tried || (i_mtime.is_some() && i_mtime != c.indexed_at) {
                c.tried = true;
                c.indexed_at = i_mtime;
                match crate::rag_chunk::load_project_index(&root) {
                    Some(ix) => {
                        c.chunks = ix.len();
                        c.index = Some(Arc::new(ix));
                    }
                    None => {
                        c.chunks = 0;
                        c.index = None;
                    }
                }
            }
            let vpath = rag_embed::vectors_path(&root);
            let v_mtime = std::fs::metadata(&vpath).and_then(|m| m.modified()).ok();
            if v_mtime.is_some() && v_mtime != c.vecs_at {
                c.vecs_at = v_mtime;
                c.vecs = rag_embed::VecStore::load(&vpath, None).map(Arc::new);
            }
            let age = c
                .indexed_at
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs());
            (c.index.clone(), c.vecs.clone(), c.chunks, age)
        };

        let Some(index) = index else {
            return text(&json!({
                "results": [],
                "no_index": true,
                "hint": format!(
                    "no RAG index for {} — one is being built in the background if this server \
                     just started; use grep/Read for now, or run `rozum rag index`.",
                    root.display()
                ),
            }));
        };

        let bm25 = rag_lite::search_balanced(index.as_ref(), &query, k);
        let mut fused = false;
        let picked = match &vecs {
            Some(vs) => {
                // The embedder is in-process here (the engine binary registers it); a build
                // without it answers None and we stay lexical, visibly.
                // GATEWAY FIRST, in-process second — order is load-bearing: under the agent
                // jail the in-process path aborts the whole server at Metal init (the client
                // sees only "Connection closed"), while 127.0.0.1 HTTP is allowed there.
                let qv = match rag_embed::embed_via_gateway(&[query.clone()], true).await {
                    Some(v) => Some(v),
                    None => {
                        let q = query.clone();
                        tokio::task::spawn_blocking(move || {
                            rozum_core::embedding::embed(&[q], true).and_then(Result::ok)
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                };
                match qv {
                    Some(qv) if qv.first().is_some_and(|v| v.len() == vs.dim()) => {
                        let ranked = vs.search(&qv[0], k.max(5) * 4);
                        let mut hits = rag_embed::fuse(&bm25, &ranked, k);
                        for h in &mut hits {
                            if h.text.is_empty()
                                && let Some(t) = index.text_of(&h.id)
                            {
                                h.text = t.to_string();
                            }
                        }
                        fused = true;
                        hits
                    }
                    _ => bm25,
                }
            }
            None => bm25,
        };

        let results: Vec<Value> = picked
            .into_iter()
            .map(|h| json!({"id": h.id, "score": h.score, "text": h.text}))
            .collect();
        text(&json!({
            "results": results,
            "fused": fused,
            "chunks": chunks,
            "index_age_secs": age,
            "stale": age.map(|a| a > STALE_AFTER_SECS).unwrap_or(true),
        }))
    }
}

/// The gateway-backed corpus embed, mirroring the proxy's: plan (prune+missing), batch 64,
/// per-batch saves, shared embed lock.
async fn embed_missing_via_gateway(root: &std::path::Path) {
    let Ok(Some(_lock)) = rag_embed::try_embed_lock(root) else { return };
    let chunks = crate::rag_chunk::saved_chunk_texts(root);
    if chunks.is_empty() {
        return;
    }
    let vpath = rag_embed::vectors_path(root);
    let mut store = rag_embed::VecStore::load(&vpath, None).unwrap_or_else(|| rag_embed::VecStore::new(0));
    let (_pruned, missing) = rag_embed::plan_embedding(&chunks, &mut store);
    for group in missing.chunks(64) {
        let texts: Vec<String> =
            group.iter().map(|(id, t)| rag_embed::distill(id, t)).collect();
        let Some(vecs) = rag_embed::embed_via_gateway(&texts, false).await else { break };
        for (v, (id, _)) in vecs.into_iter().zip(group.iter()) {
            if store.dim == 0 {
                store.dim = v.len();
            }
            if v.len() == store.dim && store.dim > 0 {
                store.vecs.insert(id.clone(), v);
            }
        }
        if store.dim > 0 {
            let _ = store.save(&vpath);
        }
    }
}

fn text(v: &Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(v.to_string())])
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RagServer {
    async fn initialize(
        &self,
        _params: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        Ok(InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rozum-rag", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "rag.search — semantic + lexical search over this project's code and docs, by \
                 meaning rather than exact tokens. Reach for it when you don't know the symbol \
                 or path yet; use your own grep/read tools when you do. Results name path#item — \
                 treat a hit as a pointer to open.",
            ))
    }
}

/// Serve on stdio until the client closes it. `root` = the project (nearest `.git` ancestor of
/// the cwd, same rule as everywhere else in rozum).
pub async fn serve_stdio(root: PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use rmcp::ServiceExt as _;
    let server = RagServer::new(root);
    server.spawn_warmup();
    let running = server.serve((tokio::io::stdin(), tokio::io::stdout())).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standalone server exists to serve exactly one tool; both halves of that are the
    /// contract — rag.search present, and nothing meeting-shaped smuggled in.
    #[test]
    fn serves_rag_search_and_only_rag_search() {
        let names: Vec<String> =
            RagServer::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["rag.search"], "got {names:?}");
    }

    #[tokio::test]
    async fn no_index_answers_with_a_hint_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let srv = RagServer::new(dir.path().to_path_buf());
        let r = srv
            .rag_search(Parameters(RagSearchParams { query: "anything".into(), top_k: None }))
            .await;
        assert_ne!(r.is_error, Some(true));
        let v: Value = serde_json::from_str(
            r.content[0].as_text().map(|t| t.text.as_str()).unwrap_or(""),
        )
        .unwrap();
        assert_eq!(v["no_index"], true, "{v}");
    }

    /// End to end on a real (tiny) project: index built, BM25 answers; `fused` reports honestly
    /// (false in a test build with no embedder registered).
    #[tokio::test]
    async fn searches_a_freshly_indexed_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "# Storage\n\nzzq-standalone-sentinel here\n")
            .unwrap();
        crate::rag_chunk::index_and_save(dir.path()).unwrap();
        let srv = RagServer::new(dir.path().to_path_buf());
        let r = srv
            .rag_search(Parameters(RagSearchParams {
                query: "zzq-standalone-sentinel".into(),
                top_k: Some(3),
            }))
            .await;
        let v: Value = serde_json::from_str(
            r.content[0].as_text().map(|t| t.text.as_str()).unwrap_or(""),
        )
        .unwrap();
        let ids: Vec<&str> =
            v["results"].as_array().unwrap().iter().filter_map(|x| x["id"].as_str()).collect();
        assert!(ids.iter().any(|i| i.contains("notes.md")), "{v}");
        assert_eq!(v["fused"], false, "no embedder in a unit-test build: {v}");
    }
}
