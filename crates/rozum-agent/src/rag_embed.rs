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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(try_embed_lock(dir.path()).unwrap().is_some(), "free again after drop");
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
}
