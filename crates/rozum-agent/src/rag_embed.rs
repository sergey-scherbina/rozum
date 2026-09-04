//! The model-free half of embedding retrieval: what text gets embedded, where vectors live on
//! disk, exact cosine search, and the BM25+embedding fusion. The model itself runs in the
//! GATEWAY (`rozum-mlx::embedder`, reached over `/v1/embeddings`); everything here works — and
//! is unit-tested — without one, because BM25-only is a first-class state, not a degraded one.
//!
//! Parameters come from the measured spike (`docs/specs/rag-embeddings-backend.md`):
//! distilled text beat the raw source 10/26 vs 7/26 on top-1; RRF with k=10 and DOUBLE weight
//! on embeddings beat every other combination tried (11/26 & 21/26 fused).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::rag_lite::Hit;

/// Where the shared gateway answers, for the embedding calls: `ROZUM_GATEWAY_URL` wins, else the
/// live registry (`share::active.json`) names the port. `None` = no gateway to ask, callers stay
/// BM25-only.
pub fn gateway_url() -> Option<String> {
    if let Ok(u) = std::env::var("ROZUM_GATEWAY_URL") {
        let u = u.trim().trim_end_matches('/').to_string();
        if !u.is_empty() {
            return Some(u);
        }
    }
    rozum_core::share::read_active().map(|g| format!("http://127.0.0.1:{}", g.port))
}

/// What gets embedded for a chunk: `path frag` plus the doc comment when there is one, the
/// source otherwise.
///
/// The question is natural language and a code chunk is mostly syntax — `detect_project` is 55
/// words of which ONE line was written for a reader. Embedding this distillate instead of the
/// raw slice measured +3 top-1 alone. The fallback matters just as much: without it an
/// undocumented chunk becomes a bare name and is lost, and undocumented is common.
pub fn distill(id: &str, text: &str) -> String {
    let (path, frag) = id.split_once('#').unwrap_or((id, ""));
    let mut doc = String::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || (t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")) {
            // Plain `//` lines are maintainer notes (`// ── Helpers ──`), not statements of
            // what the thing does; skipped rather than collected.
            continue;
        }
        if let Some(rest) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) {
            doc.push_str(rest);
            doc.push(' ');
            continue;
        }
        break;
    }
    if doc.trim().is_empty() {
        format!("{path} {frag}\n{text}")
    } else {
        format!("{path} {frag}\n{doc}")
    }
}

/// Where a project's chunk vectors live: beside the index, same `.rozum/` state dir.
pub fn vectors_path(root: &Path) -> PathBuf {
    root.join(".rozum").join("rag-vectors.bin")
}

const MAGIC_V1: &[u8; 4] = b"RZV1";
const MAGIC_V2: &[u8; 4] = b"RZV2";

/// The seam an external vector store would implement — Qdrant, LanceDB, a remote service, or
/// the in-process [`VecStore`] below, interchangeably.
///
/// Shaped by what the CALLERS actually need, not by what stores offer, so a backend can be
/// swapped without touching retrieval logic:
///
/// - ids are the chunk ids the rest of RAG already speaks (`path#item`) — the store never sees
///   text, only vectors, so chunk content stays in the lexical index and is never duplicated;
/// - vectors are L2-normalised f32 at this boundary, whatever the store keeps internally
///   (RZV2 keeps i8+scale; a server-side store may keep its own quantisation) — the CONTRACT is
///   "dot product == cosine", and normalisation is the caller's obligation stated once, here;
/// - `sync` is a full-state reconcile (upsert + prune to exactly `these ids`), not a CRUD
///   surface: retrieval state is always derivable from the chunk manifest, so the store is a
///   cache by construction and can be dropped and rebuilt at any time. That property is what
///   makes external stores SAFE to adopt — a dead or corrupt store degrades to BM25, never to
///   wrong answers.
///
/// The trait is sync; an HTTP-backed impl would wrap its own runtime the way the proxy's embed
/// calls already do. `dim` guards cross-model mixing at the boundary, same rule as the file
/// format: a dimension change is a model change, and the store must refuse rather than coerce.
pub trait VectorIndex: Send + Sync {
    fn dim(&self) -> usize;
    fn len(&self) -> usize;
    /// Ranked `(id, score)` by cosine, best first.
    fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)>;
}

/// Chunk-id → L2-normalised vector.
///
/// Written as **i8 + per-vector scale** (`RZV2`): each component is `round(x/scale*127)` with
/// `scale = max|x|`. Measured on the 26-question eval against the f32 store it replaces —
/// f32 11/26 & 20/26, f16 11/26 & 20/26, i8 12/26 & 20/26 (the +1 is one question, i.e. noise;
/// the point is NOTHING is lost) — while the file and every proxy's resident copy shrink 4×:
/// 42 MB → 11 MB for this repo. That multiplier matters because each live agent session holds
/// one copy. The older f32 format (`RZV1`) is still READ, so existing stores keep serving and
/// upgrade to `RZV2` on their next save instead of forcing a re-embed.
///
/// In memory vectors are f32 (dequantised at load): the dot-product loop stays trivial, and the
/// savings that matter are disk and steady-state RAM per store copy.
pub struct VecStore {
    pub dim: usize,
    pub vecs: HashMap<String, Vec<f32>>,
}

impl VecStore {
    pub fn new(dim: usize) -> Self {
        Self { dim, vecs: HashMap::new() }
    }

    /// Load, or `None` for missing/corrupt/wrong-dimension — all three mean the same thing to a
    /// caller: no usable vectors, re-embed. A dimension change is a MODEL change, and vectors
    /// from two models do not live in one metric space, so discarding is correctness, not loss.
    pub fn load(path: &Path, expect_dim: Option<usize>) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        let mut at = 0usize;
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let s = bytes.get(*at..*at + n)?;
            *at += n;
            Some(s)
        };
        let magic: [u8; 4] = take(&mut at, 4)?.try_into().ok()?;
        let v2 = &magic == MAGIC_V2;
        if !v2 && &magic != MAGIC_V1 {
            return None;
        }
        let dim = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
        if dim == 0 || expect_dim.is_some_and(|d| d != dim) {
            return None;
        }
        let count = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
        let mut vecs = HashMap::with_capacity(count);
        for _ in 0..count {
            let id_len = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
            let id = String::from_utf8(take(&mut at, id_len)?.to_vec()).ok()?;
            let v: Vec<f32> = if v2 {
                let scale = f32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
                take(&mut at, dim)?.iter().map(|&b| (b as i8) as f32 * scale / 127.0).collect()
            } else {
                take(&mut at, dim * 4)?
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().expect("chunks_exact(4)")))
                    .collect()
            };
            vecs.insert(id, v);
        }
        Some(Self { dim, vecs })
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut out: Vec<u8> = Vec::with_capacity(16 + self.vecs.len() * (self.dim + 68));
        out.extend_from_slice(MAGIC_V2);
        out.extend_from_slice(&(self.dim as u32).to_le_bytes());
        out.extend_from_slice(&(self.vecs.len() as u32).to_le_bytes());
        for (id, v) in &self.vecs {
            out.extend_from_slice(&(id.len() as u32).to_le_bytes());
            out.extend_from_slice(id.as_bytes());
            let scale = v.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-12);
            out.extend_from_slice(&scale.to_le_bytes());
            for x in v {
                out.push(((x / scale * 127.0).round().clamp(-127.0, 127.0) as i8) as u8);
            }
        }
        // Write-temp + rename, so a reader never sees a half-written store.
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, out)?;
        fs::rename(tmp, path)
    }

    /// Ranked ids by cosine (dot product — vectors are normalised), best first.
    ///
    /// Exact search on purpose: the sweep is ~10–20 ms at 10.6k × 1024 and an ANN structure
    /// earns nothing until corpora are two orders larger (the threshold and the next step —
    /// in-process HNSW, not a server — are recorded in `docs/specs/rag-embeddings-impl.md`).
    /// "Exact" still doesn't mean careless: top-k is selected with `select_nth_unstable`
    /// (O(n) average) and only the k winners are sorted, instead of sorting all n scores for
    /// the five the caller wants.
    pub fn rank(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let mut scored: Vec<(f32, &str)> = self
            .vecs
            .iter()
            .map(|(id, v)| (v.iter().zip(query).map(|(a, b)| a * b).sum::<f32>(), id.as_str()))
            .collect();
        if scored.is_empty() || k == 0 {
            return Vec::new();
        }
        let k = k.min(scored.len());
        scored.select_nth_unstable_by(k - 1, |a, b| b.0.total_cmp(&a.0));
        scored.truncate(k);
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.into_iter().map(|(s, id)| (id.to_string(), s)).collect()
    }
}

impl VectorIndex for VecStore {
    fn dim(&self) -> usize {
        self.dim
    }
    fn len(&self) -> usize {
        self.vecs.len()
    }
    fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        self.rank(query, k)
    }
}

/// Reconcile the vector store with the CURRENT chunk set: drop vectors whose chunks are gone,
/// name the chunks that still need embedding. Pure, so the proxy's HTTP loop stays a dumb
/// executor of this plan and the plan itself is testable without a server.
///
/// Pruning is not hygiene here: a vector for a deleted chunk would keep ranking that chunk into
/// fused results, and its id would then point an agent at code that no longer exists — the one
/// wrong answer retrieval cannot recover from, same as the index side.
pub fn plan_embedding<'a>(
    chunks: &'a [(String, String)],
    store: &mut VecStore,
) -> (usize, Vec<&'a (String, String)>) {
    let live: std::collections::HashSet<&str> = chunks.iter().map(|(id, _)| id.as_str()).collect();
    let before = store.vecs.len();
    store.vecs.retain(|id, _| live.contains(id.as_str()));
    let pruned = before - store.vecs.len();
    let missing = chunks.iter().filter(|(id, _)| !store.vecs.contains_key(id)).collect();
    (pruned, missing)
}

/// The gateway-less warmup: refresh the index (cross-process lock, incremental) and embed the
/// missing vectors through the IN-PROCESS embedding hook. Returns how many vectors were added.
///
/// This is what makes the standalone `rozum rag mcp` self-contained: the engine binary registers
/// the hook at startup, so retrieval needs no meeting daemon and no gateway — one process does
/// chunking, embedding and serving. In a build without the hook (no `mlx-native`) the vector
/// half is skipped and the index still refreshes: BM25-only, honestly.
///
/// Same interruptibility contract as the proxy's HTTP path: vectors are saved per batch, so a
/// killed process resumes where it stopped.
pub fn gateway_less_warmup(root: &Path) -> std::io::Result<usize> {
    let refreshed = crate::rag_chunk::refresh_in_background(root, &mut |_, _, _| {})?;
    if refreshed.is_none() {
        return Ok(0); // a sibling holds the build lock and will do the vectors too
    }
    if !rozum_core::embedding::available() {
        return Ok(0);
    }
    let Some(_lock) = try_embed_lock(root)? else {
        return Ok(0); // a sibling is embedding this project right now
    };
    let chunks = crate::rag_chunk::saved_chunk_texts(root);
    if chunks.is_empty() {
        return Ok(0);
    }
    let vpath = vectors_path(root);
    let mut store = VecStore::load(&vpath, None).unwrap_or_else(|| VecStore::new(0));
    let (pruned, missing) = plan_embedding(&chunks, &mut store);
    let mut added = 0usize;
    for group in missing.chunks(64) {
        let texts: Vec<String> = group.iter().map(|(id, t)| distill(id, t)).collect();
        let Some(Ok(vecs)) = rozum_core::embedding::embed(&texts, false) else { break };
        for (v, (id, _)) in vecs.into_iter().zip(group.iter()) {
            if store.dim == 0 {
                store.dim = v.len();
            }
            if v.len() == store.dim && store.dim > 0 {
                store.vecs.insert(id.clone(), v);
                added += 1;
            }
        }
        if store.dim > 0 {
            let _ = store.save(&vpath);
        }
    }
    if (added > 0 || pruned > 0) && store.dim > 0 {
        let _ = store.save(&vpath);
    }
    Ok(added)
}

/// The embedding model this CLIENT is configured for, if any. Same knob the engine reads, so a
/// single `ROZUM_EMBED_MODEL` setting means one thing on both ends of the HTTP call.
fn configured_embed_model() -> Option<String> {
    std::env::var("ROZUM_EMBED_MODEL").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Embed texts through the GATEWAY's `/v1/embeddings`. `None` on any failure — no gateway, 501,
/// timeout — the caller falls back or degrades, never errors.
///
/// This exists because "in-process embedder available" is not the same as "in-process embedder
/// SURVIVES here": under the Seatbelt jail that `rozum launch` puts agents in, lazy MLX/Metal
/// initialisation aborts the whole process with no stderr — the first `rag.search` killed the
/// standalone MCP server and the client saw only `MCP error -32000: Connection closed`. HTTP to
/// 127.0.0.1 is allowed in that same jail (the agent talks to the gateway all day), so the
/// gateway-first order is what makes the standalone server jail-safe.
pub async fn embed_via_gateway(texts: &[String], is_query: bool) -> Option<Vec<Vec<f32>>> {
    embed_via_gateway_at(&gateway_url()?, texts, is_query).await
}

/// [`embed_via_gateway`] against an explicit gateway URL (the corpus embed passes the one it
/// resolved once, and a test passes a fake).
pub async fn embed_via_gateway_at(
    url: &str,
    texts: &[String],
    is_query: bool,
) -> Option<Vec<Vec<f32>>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .ok()?;
    // Name the embedder the client is configured for, rather than taking whatever the gateway
    // happens to serve. The two are usually the same process-wide setting, but they need not be:
    // an eval comparing two embedders, or a project whose store was built with one while the
    // gateway defaults to another, must be able to ask. `/v1/embeddings` answers 400 if it
    // cannot serve the named one, which is what turns a mismatch into a visible refusal instead
    // of vectors from the wrong space.
    let mut body = serde_json::json!({ "input": texts, "query": is_query });
    if let Some(m) = configured_embed_model() {
        body["model"] = serde_json::Value::String(m);
    }
    let resp = client
        .post(format!("{url}/v1/embeddings"))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let data = v["data"].as_array()?;
    let mut out = Vec::with_capacity(data.len());
    for row in data {
        let emb = row.get("embedding")?.as_array()?;
        out.push(emb.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect());
    }
    Some(out)
}

/// The cross-process embed lock. The BUILD lock covers only the chunking inside
/// `refresh_in_background` and is released before any embedding starts — which was fine with one
/// server per project and stopped being fine the day a project can have TWO (the meeting proxy
/// and `rozum rag mcp`, both warming up at startup and both re-embedding after an edit). Vectors
/// were never at risk — the store's temp+rename means last-writer-wins on identical content —
/// the waste was DOUBLE GPU WORK on the machine's busiest resource. `try_lock` + skip, not
/// blocking: when the holder finishes, the vectors are on disk for everyone, same argument as
/// the build lock. `None` = a sibling holds it.
pub fn try_embed_lock(root: &Path) -> std::io::Result<Option<std::fs::File>> {
    let dir = root.join(".rozum");
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("rag-embed.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The gateway-backed corpus embed: plan (prune+missing), batch 64, per-batch saves, shared
/// embed lock. Extracted from the standalone MCP server (`rag-vector-freshness-cli`) so the
/// CLI can run it after its refresh too — VECTOR freshness was the real Q8 mechanism: the
/// index refreshed everywhere, but a new chunk's embedding appeared only when a server
/// happened to warm up, and until then the fusion's semantic half simply did not know the
/// file. A dead gateway breaks the loop quietly and the search stays BM25-backed for the
/// missing chunks, exactly as before.
pub async fn embed_missing_via_gateway(root: &std::path::Path) {
    embed_missing_via_gateway_budgeted(root, None).await
}

/// The budgeted form: `max_batches` caps how many 64-chunk batches ONE call embeds. The CLI
/// passes a small cap — on a freshly indexed large repository (scalascript: 94k chunks) the
/// uncapped call turned the first `rag search` into a ~25-minute synchronous embed of the
/// whole corpus. Per-batch saves make the catch-up incremental: every capped call banks its
/// batches, later calls (or a server warmup, which stays uncapped) finish the rest, and the
/// fusion's semantic half grows with each search instead of blocking one.
pub async fn embed_missing_via_gateway_budgeted(root: &std::path::Path, max_batches: Option<usize>) {
    let Some(url) = gateway_url() else { return };
    embed_missing_with(root, max_batches, &url, EMBED_RETRY_BACKOFF).await;
}

/// How long the corpus embed waits for the gateway to come back before giving a batch up:
/// four retries over ~110 s. The shape comes from an outage that was measured, not imagined —
/// the shared gateway idle-exits and launchd restarts it, and a 4B model takes ~60 s to load;
/// the FIRST version of this loop broke on the first `None`, so scalascript's 94k-chunk
/// catch-up died at 12,864 vectors during one such restart and stayed there until a human
/// noticed. A dead gateway still ends the loop — after the last delay, not before the first.
const EMBED_RETRY_BACKOFF: &[std::time::Duration] = &[
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(15),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(60),
];

/// The corpus embed against an explicit gateway, with a retry schedule: a batch that fails
/// is retried after each delay in `backoff`; only when every delay is spent does the loop
/// stop (progress so far is saved per batch, so the next warmup resumes from there).
/// Returns the number of vectors added.
pub async fn embed_missing_with(
    root: &std::path::Path,
    max_batches: Option<usize>,
    url: &str,
    backoff: &[std::time::Duration],
) -> usize {
    let Ok(Some(_lock)) = try_embed_lock(root) else { return 0 };
    let chunks = crate::rag_chunk::saved_chunk_texts(root);
    if chunks.is_empty() {
        return 0;
    }
    let vpath = vectors_path(root);
    let mut store = VecStore::load(&vpath, None).unwrap_or_else(|| VecStore::new(0));
    let (_pruned, missing) = plan_embedding(&chunks, &mut store);
    let cap = max_batches.unwrap_or(usize::MAX);
    let mut added = 0usize;
    'batches: for group in missing.chunks(64).take(cap) {
        let texts: Vec<String> =
            group.iter().map(|(id, t)| distill(id, t)).collect();
        let mut vecs = embed_via_gateway_at(url, &texts, false).await;
        let mut waits = backoff.iter();
        while vecs.is_none() {
            let Some(delay) = waits.next() else {
                eprintln!(
                    "rag: gateway embedding failed {} times in a row — stopping the corpus embed \
                     at {} vectors; the next warmup resumes from here",
                    backoff.len() + 1,
                    store.vecs.len()
                );
                break 'batches;
            };
            tokio::time::sleep(*delay).await;
            vecs = embed_via_gateway_at(url, &texts, false).await;
        }
        let Some(vecs) = vecs else { break };
        added += vecs.len();
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
    added
}

/// RRF fusion of the BM25 ranking and the embedding ranking.
///
/// k=10 and embedding weight 2, from the sweep. Embeddings carry the HIGHER weight even though
/// fusion exists partly to keep BM25 in the loop: what pays in a fusion is finding what the
/// other source missed, and by the end of the spike embeddings held the better top-1 (12/26 vs
/// 9/26) — BM25 is the junior partner, kept because it is the zero-model fallback.
pub fn fuse(bm25: &[Hit], emb_ranked: &[(String, f32)], k: usize) -> Vec<Hit> {
    const RRF_K: f32 = 10.0;
    const W_BM25: f32 = 1.0;
    const W_EMB: f32 = 2.0;
    let mut score: HashMap<&str, f32> = HashMap::new();
    for (r, h) in bm25.iter().enumerate() {
        *score.entry(h.id.as_str()).or_insert(0.0) += W_BM25 / (RRF_K + (r + 1) as f32);
    }
    for (r, (id, _)) in emb_ranked.iter().enumerate() {
        *score.entry(id.as_str()).or_insert(0.0) += W_EMB / (RRF_K + (r + 1) as f32);
    }
    let mut ids: Vec<(&str, f32)> = score.into_iter().collect();
    ids.sort_by(|a, b| b.1.total_cmp(&a.1));
    ids.truncate(k);
    // Text comes from whichever source carried the chunk; embedding-only hits get their text
    // filled by the caller (it has the index) — here they carry an empty body.
    let by_id: HashMap<&str, &Hit> = bm25.iter().map(|h| (h.id.as_str(), h)).collect();
    ids.into_iter()
        .map(|(id, s)| match by_id.get(id) {
            Some(h) => Hit { id: h.id.clone(), score: s, text: h.text.clone() },
            None => Hit { id: id.to_string(), score: s, text: String::new() },
        })
        .collect()
}

/// **The shipped retrieval policy, in one place** — deep pool, balance, fuse, fill texts,
/// rebalance, truncate. Every surface that answers a retrieval query must call this.
///
/// It exists because it did not: the sequence was copy-pasted into the meeting proxy, the
/// standalone MCP server and the eval harness, and the fourth caller — the in-process agent
/// tool that `nadia` registers — never got it at all. That one ranked with raw BM25 at k=3,
/// the configuration measured at 4/26 top-1, while the two MCP surfaces served the fused 22/25.
/// The agent with the smallest context window, which needs retrieval most, had the worst of it.
/// Three copies of a policy are a policy that drifts; one function with four callers cannot.
///
/// The query VECTOR is a parameter rather than something this fetches, and that is deliberate:
/// how a surface reaches an embedder is exactly what legitimately differs between them (the
/// meeting proxy must use the gateway because the in-process path aborts the whole server at
/// Metal init under the agent jail; the eval carries no model at all). Passing `None` is the
/// first-class BM25-only state, not a degraded one — `fused` in the return says which happened,
/// so a caller can report it rather than guess.
///
/// Synchronous and free of I/O, so the policy is unit-testable without a gateway, a store or a
/// runtime — which is what makes it cheap to keep the callers honest.
pub fn rank_fused(
    index: &crate::rag_lite::LexicalIndex,
    vecs: Option<&dyn VectorIndex>,
    query_vec: Option<&[f32]>,
    query: &str,
    k: usize,
) -> (Vec<Hit>, bool) {
    // The fusion POOL is deeper than the answer (`rag-ab-failure-forensics`): RRF pays when a
    // candidate sits mid-list in BOTH sources, and with a BM25 pool of only k such a candidate
    // never reaches the fusion at all — the Q5 forensics run found the right file at rank 4 with
    // a deep pool and absent entirely at the same query with pool=k.
    let pool = k.max(5) * 4;
    let bm25 = crate::rag_lite::search_balanced(index, query, pool);
    let (Some(vs), Some(qv)) = (vecs, query_vec) else {
        return (crate::rag_lite::rebalance(&bm25, k), false);
    };
    // A dimension mismatch is a MODEL mismatch: fall back to lexical rather than compare
    // vectors from two different embedders, which would rank confidently and wrongly.
    if qv.len() != vs.dim() {
        return (crate::rag_lite::rebalance(&bm25, k), false);
    }
    let ranked = vs.search(qv, pool);
    let mut hits = fuse(&bm25, &ranked, pool);
    // Texts first, THEN rebalance: the test detector reads the chunk text, and an
    // embedding-only hit arrives from `fuse` with an empty one.
    for h in &mut hits {
        if h.text.is_empty()
            && let Some(t) = index.text_of(&h.id)
        {
            h.text = t.to_string();
        }
    }
    (crate::rag_lite::rebalance(&hits, k), true)
}

/// The query embedding, gateway FIRST and in-process second — the order is load-bearing.
/// Under the agent jail the in-process path aborts the whole process at Metal init (the client
/// sees only "Connection closed"), while 127.0.0.1 HTTP is allowed there. `None` means the
/// caller ranks lexically, which is a supported answer and not an error.
pub async fn embed_query(query: &str) -> Option<Vec<f32>> {
    if let Some(v) = embed_via_gateway(std::slice::from_ref(&query.to_string()), true).await
        && let Some(first) = v.into_iter().next()
    {
        return Some(first);
    }
    let q = query.to_string();
    tokio::task::spawn_blocking(move || {
        rozum_core::embedding::embed(&[q], true).and_then(Result::ok)
    })
    .await
    .ok()
    .flatten()
    .and_then(|v| v.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway that is down for two batches and then answers must NOT end the corpus embed:
    /// the retry schedule covers a restart. A fake gateway refuses twice (503) and then serves.
    #[tokio::test]
    async fn corpus_embed_survives_a_gateway_blip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "# Alpha\n\nalpha text\n\n# Beta\n\nbeta text\n").unwrap();
        assert!(crate::rag_chunk::refresh_in_background(root, &mut |_, _, _| {}).unwrap().is_some());
        let n_chunks = crate::rag_chunk::saved_chunk_texts(root).len();
        assert!(n_chunks > 0);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let n = h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 65536];
                let mut got = 0;
                // Read until the JSON body closes — enough for a one-shot fake.
                loop {
                    let r = sock.read(&mut buf[got..]).await.unwrap_or(0);
                    if r == 0 {
                        break;
                    }
                    got += r;
                    let head = String::from_utf8_lossy(&buf[..got]);
                    if let Some(i) = head.find("\r\n\r\n") {
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if got >= i + 4 + cl {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf[..got]).to_string();
                let body = if n < 2 {
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let v: serde_json::Value = serde_json::from_str(
                        &req[req.find("\r\n\r\n").unwrap() + 4..],
                    )
                    .unwrap();
                    let k = v["input"].as_array().unwrap().len();
                    let data: Vec<serde_json::Value> =
                        (0..k).map(|_| serde_json::json!({"embedding": [1.0, 0.0]})).collect();
                    let payload = serde_json::json!({"data": data}).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let zero = [std::time::Duration::ZERO; 3];
        let added = embed_missing_with(root, None, &url, &zero).await;
        assert_eq!(added, n_chunks, "two 503s then success must embed the whole corpus");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3, "2 refusals + 1 answer");
        let st = VecStore::load(&vectors_path(root), None).expect("vectors saved");
        assert_eq!(st.vecs.len(), n_chunks);

        // And with NO retries a refusing gateway ends the loop — the pre-existing contract.
        // Zero requests means the embed lock was not ours: a sibling test's child process can
        // hold a dup of the fd for the fork→exec window (the tolerance the lock test states
        // too), so re-attempt briefly rather than read that window as a failure.
        std::fs::remove_file(vectors_path(root)).unwrap();
        hits.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut added = 0;
        for _ in 0..40 {
            added = embed_missing_with(root, None, &url, &[]).await;
            if hits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(added, 0);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Latency instrument for the exact-search threshold (BACKLOG `rag-ann-threshold-watch`):
    /// the cost of ONE brute-force sweep over N unit vectors of the store's real width, k=20.
    /// Synthetic vectors — the sweep is N×dim multiply-adds whatever the values are — plus the
    /// real store when `RAG_BENCH_VECTORS` names one. Ignored: it is a measurement, not a test.
    ///   cargo test -p rozum-agent --release sweep_latency -- --ignored --nocapture
    #[test]
    #[ignore = "latency instrument, run by hand with --release"]
    fn sweep_latency_curve() {
        fn unit(seed: &mut u64, dim: usize) -> Vec<f32> {
            let mut v: Vec<f32> = (0..dim)
                .map(|_| {
                    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
                })
                .collect();
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= n);
            v
        }
        fn measure(label: &str, st: &VecStore, seed: &mut u64) {
            let qs: Vec<Vec<f32>> = (0..20).map(|_| unit(seed, st.dim)).collect();
            let _ = st.rank(&qs[0], 20); // warm
            let mut times: Vec<f64> = qs
                .iter()
                .map(|q| {
                    let t = std::time::Instant::now();
                    let r = st.rank(q, 20);
                    assert_eq!(r.len(), 20.min(st.vecs.len()));
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            times.sort_by(|a, b| a.total_cmp(b));
            eprintln!(
                "sweep {label}: n={} dim={} k=20  min {:.2} ms  p50 {:.2} ms  max {:.2} ms",
                st.vecs.len(),
                st.dim,
                times[0],
                times[times.len() / 2],
                times[times.len() - 1]
            );
        }
        let mut seed = 42u64;
        for n in [10_000usize, 95_000, 200_000, 400_000] {
            let mut st = VecStore::new(1024);
            for i in 0..n {
                st.vecs.insert(format!("src/file{}.rs#fn item_{i}", i % 997), unit(&mut seed, 1024));
            }
            measure("synthetic", &st, &mut seed);
        }
        if let Some(p) = std::env::var_os("RAG_BENCH_VECTORS") {
            let st = VecStore::load(Path::new(&p), None).expect("RAG_BENCH_VECTORS loads");
            measure(&format!("real {}", p.to_string_lossy()), &st, &mut seed);
        }
    }

    #[test]
    fn distill_prefers_the_doc_and_falls_back_to_source() {
        let documented = "/// Decide whether a model fits.\npub fn admit(x: u64) -> bool { x < 9 }";
        let d = distill("crates/a/src/lib.rs#fn admit", documented);
        assert!(d.starts_with("crates/a/src/lib.rs fn admit\n"));
        assert!(d.contains("Decide whether a model fits."));
        assert!(!d.contains("x < 9"), "documented: the body must not dilute the doc: {d}");

        let bare = "pub fn admit(x: u64) -> bool { x < 9 }";
        let d = distill("crates/a/src/lib.rs#fn admit", bare);
        assert!(d.contains("x < 9"), "undocumented: the source IS the text: {d}");

        // A maintainer rule (`// ──`) is neither doc nor a reason to stop scanning for one.
        let ruled = "// ── Helpers ──\n/// Real doc.\nfn f() {}";
        assert!(distill("x.rs#fn f", ruled).contains("Real doc."));
    }

    #[test]
    fn vecstore_round_trips_and_rejects_a_dimension_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".rozum").join("rag-vectors.bin");
        let mut st = VecStore::new(4);
        st.vecs.insert("a.rs#fn one".into(), vec![1.0, 0.0, 0.0, 0.0]);
        st.vecs.insert("b.rs#fn two".into(), vec![0.0, 1.0, 0.0, 0.0]);
        st.save(&path).unwrap();

        let back = VecStore::load(&path, Some(4)).expect("loads");
        assert_eq!(back.vecs.len(), 2);
        assert_eq!(back.vecs["a.rs#fn one"], vec![1.0, 0.0, 0.0, 0.0]);

        // A different dimension is a different MODEL; the store must refuse, not coerce.
        assert!(VecStore::load(&path, Some(8)).is_none());
        assert!(VecStore::load(&dir.path().join("missing.bin"), Some(4)).is_none());

        // Quantisation error is bounded by scale/254 per component — assert the bound rather
        // than exact equality, since v2 is lossy BY DESIGN and the eval said the loss is free.
        let mut st = VecStore::new(3);
        st.vecs.insert("q".into(), vec![0.7071, -0.3, 0.05]);
        st.save(&path).unwrap();
        let back = VecStore::load(&path, Some(3)).unwrap();
        for (a, b) in back.vecs["q"].iter().zip([0.7071f32, -0.3, 0.05]) {
            assert!((a - b).abs() <= 0.7071 / 254.0 + 1e-6, "{a} vs {b}");
        }

        // The legacy f32 format (RZV1) must still LOAD — existing stores keep serving and
        // upgrade on their next save, instead of forcing every project to re-embed.
        let mut legacy: Vec<u8> = Vec::new();
        legacy.extend_from_slice(b"RZV1");
        legacy.extend_from_slice(&2u32.to_le_bytes());
        legacy.extend_from_slice(&1u32.to_le_bytes());
        legacy.extend_from_slice(&(4u32).to_le_bytes());
        legacy.extend_from_slice(b"a#fn");
        for x in [0.6f32, 0.8] {
            legacy.extend_from_slice(&x.to_le_bytes());
        }
        let lp = dir.path().join("legacy.bin");
        fs::write(&lp, legacy).unwrap();
        let old = VecStore::load(&lp, Some(2)).expect("v1 loads");
        assert_eq!(old.vecs["a#fn"], vec![0.6, 0.8]);
    }

    /// Two servers per project must not embed the same chunks twice. Asserted at the lock,
    /// which is the coordination point both warmup paths share.
    #[test]
    fn a_second_embedder_skips_while_the_first_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_embed_lock(dir.path()).unwrap().expect("first takes it");
        assert!(
            try_embed_lock(dir.path()).unwrap().is_none(),
            "second must skip while the first holds it"
        );
        drop(first);
        // Not a single-shot assert: a PARALLEL test that spawns a process (fork+exec) can
        // inherit our lock fd for the fork->exec window — O_CLOEXEC closes it only at exec —
        // so for a few milliseconds the flock outlives our close. Production try+skip
        // semantics tolerate that; the test states the same tolerance instead of flaking
        // (~1 in 3 full-suite runs on a loaded machine, never under --test-threads=1).
        let mut regained = try_embed_lock(dir.path()).unwrap();
        let mut tries = 0;
        while regained.is_none() && tries < 100 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            regained = try_embed_lock(dir.path()).unwrap();
            tries += 1;
        }
        assert!(regained.is_some(), "free again after drop (and 1s of fork-window grace)");
    }

    #[test]
    fn plan_prunes_dead_vectors_and_names_only_the_missing() {
        let chunks = vec![
            ("kept.rs#fn a".to_string(), "text a".to_string()),
            ("new.rs#fn b".to_string(), "text b".to_string()),
        ];
        let mut st = VecStore::new(2);
        st.vecs.insert("kept.rs#fn a".into(), vec![1.0, 0.0]);
        st.vecs.insert("deleted.rs#fn gone".into(), vec![0.0, 1.0]);
        let (pruned, missing) = plan_embedding(&chunks, &mut st);
        assert_eq!(pruned, 1, "the deleted chunk's vector is dropped");
        assert!(!st.vecs.contains_key("deleted.rs#fn gone"));
        assert_eq!(missing.len(), 1, "only the NEW chunk needs the model");
        assert_eq!(missing[0].0, "new.rs#fn b");
        assert!(st.vecs.contains_key("kept.rs#fn a"), "unchanged vectors are carried forward");
    }

    #[test]
    fn rank_orders_by_cosine_and_fusion_lets_embeddings_outvote_bm25() {
        let mut st = VecStore::new(2);
        st.vecs.insert("near".into(), vec![1.0, 0.0]);
        st.vecs.insert("far".into(), vec![0.0, 1.0]);
        let ranked = st.rank(&[1.0, 0.0], 2);
        assert_eq!(ranked[0].0, "near");

        // BM25 puts `far` first; embeddings put `near` first. With weight 2 on embeddings the
        // fused top-1 is `near` — the configuration the sweep chose, asserted end to end.
        let bm = vec![
            Hit { id: "far".into(), score: 9.0, text: "far text".into() },
            Hit { id: "near".into(), score: 8.0, text: "near text".into() },
        ];
        let fused = fuse(&bm, &ranked, 2);
        assert_eq!(fused[0].id, "near");
        assert_eq!(fused[1].id, "far");
        assert_eq!(fused[1].text, "far text", "text survives fusion for BM25-carried hits");
    }

    /// Behavior: with no vectors, or with no query vector, `rank_fused` is the lexical policy
    /// and SAYS so. BM25-only is a first-class state — the servers report it rather than
    /// pretending the answer was fused.
    #[test]
    fn rank_fused_without_vectors_is_lexical_and_reports_it() {
        let mut ix = crate::rag_lite::LexicalIndex::new();
        ix.add("a.rs#fn one", "the cat sat on the warm windowsill");
        ix.add("b.rs#fn two", "dogs are loyal companions");
        let (hits, fused) = rank_fused(&ix, None, None, "cat windowsill", 2);
        assert!(!fused);
        assert_eq!(hits[0].id, "a.rs#fn one", "{hits:?}");

        // A store present but no query vector (no gateway, no in-process embedder) is the same
        // state: there is nothing to fuse WITH.
        let st = VecStore::new(2);
        let (_, fused) = rank_fused(&ix, Some(&st as &dyn VectorIndex), None, "cat", 2);
        assert!(!fused);
    }

    /// Behavior: a query vector of the wrong width falls back to lexical instead of fusing.
    /// A different dimension is a different MODEL, and comparing vectors across embedders would
    /// rank confidently and wrongly — the one outcome worse than ranking lexically.
    #[test]
    fn rank_fused_refuses_a_query_vector_of_another_model() {
        let mut ix = crate::rag_lite::LexicalIndex::new();
        ix.add("a.rs#fn one", "the cat sat on the warm windowsill");
        let mut st = VecStore::new(2);
        st.vecs.insert("a.rs#fn one".into(), vec![1.0, 0.0]);
        let (hits, fused) = rank_fused(
            &ix,
            Some(&st as &dyn VectorIndex),
            Some(&[1.0, 0.0, 0.0]),
            "cat",
            1,
        );
        assert!(!fused, "a 3-wide query against a 2-wide store must not fuse");
        assert_eq!(hits[0].id, "a.rs#fn one");
    }
}
