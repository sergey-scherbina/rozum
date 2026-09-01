//! The embedding backend behind `rozum_core::embedding` — the productionised spike from
//! `docs/specs/rag-embeddings-backend.md`. Every parameter here was MEASURED there; changing
//! one without re-running `crates/rozum-agent/tests/rag-eval.json` is how the numbers rot.
//!
//! Runs on its OWN dedicated thread, lazily started on first use, with all of this model's MLX
//! ops confined to it (the chat backend keeps its own worker thread; MLX streams are per-thread
//! and the cross-thread command-encoder bug was fixed core-side, d70a960). Two things it must
//! NEVER do in-process with the gateway:
//!
//! - call `apply_retain_env` — that knob is keyed to the CHAT model's family and process-wide;
//!   flipping it for a dense embed model would corrupt a resident hybrid model's setting;
//! - call `set_cache_limit` — the gateway already owns the process-wide cache policy, and the
//!   spike's own 512 MB limit here would silently throttle the chat model's cache.
//!
//! The spike still proved cache growth kills an UNbounded run, which is why this stays safe:
//! the gateway's existing limit bounds the cache, and the token budget bounds activations.

#![cfg(feature = "mlx-native")]

use std::sync::OnceLock;
use std::sync::mpsc;

/// Query-side instruction. Swept in the spike: worth +2 top-1 / +4 top-5 over the first
/// wording, and the load-bearing word is `implements` (dropping it costs 1 and 2). Qwen's own
/// canonical web-search instruction measured WORST of four — do not "fix" this back to it.
const QUERY_INSTRUCTION: &str = "Retrieve the source code that implements the behaviour described";

/// Per-text token cap. Swept 63..2047: quality is FLAT from 191 up (12/26 & 20/26 on the eval
/// set) while cost falls, so 255 keeps a margin above the measured knee at ~40% of the 511
/// default's cost. Only ~3% of distilled chunks exceed even 511.
const MAX_TOKENS_PER_TEXT: usize = 255;

/// Batch budget in TOKENS (rows × padded width), not rows: cost scales with the product, and a
/// fixed row count walks into the memory ceiling as rows get longer (SIGKILL at chunk 3008 of
/// 10551 in the spike).
const TOKEN_BUDGET: usize = 4096;

enum Req {
    Embed {
        texts: Vec<String>,
        is_query: bool,
        reply: mpsc::Sender<Result<Vec<Vec<f32>>, String>>,
    },
}

static WORKER: OnceLock<mpsc::Sender<Req>> = OnceLock::new();

/// The `rozum_core::embedding::EmbedFn` — register with
/// `rozum_core::embedding::register_embedder(rozum_mlx::embedder::embed)`.
pub fn embed(texts: &[String], is_query: bool) -> Result<Vec<Vec<f32>>, String> {
    let tx = WORKER.get_or_init(spawn_worker);
    let (reply_tx, reply_rx) = mpsc::channel();
    tx.send(Req::Embed { texts: texts.to_vec(), is_query, reply: reply_tx })
        .map_err(|_| "embed worker gone".to_string())?;
    reply_rx.recv().map_err(|_| "embed worker died mid-request".to_string())?
}

fn spawn_worker() -> mpsc::Sender<Req> {
    let (tx, rx) = mpsc::channel::<Req>();
    std::thread::Builder::new()
        .name("rozum-embed".into())
        .spawn(move || worker_loop(rx))
        .expect("spawn embed worker");
    tx
}

/// The model spec, overridable for tests and for trying alternatives. The default is the one
/// the eval numbers were measured on.
fn model_spec() -> String {
    std::env::var("ROZUM_EMBED_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ".into())
}

struct Loaded {
    model: mlx_lm::models::qwen3::Model,
    tok: mlx_lm_utils::tokenizer::Tokenizer,
    eos: i32,
}

fn worker_loop(rx: mpsc::Receiver<Req>) {
    // Lazy: nothing is downloaded or loaded until the first request, so a machine that never
    // uses retrieval never pays for the model.
    let mut loaded: Option<Loaded> = None;
    while let Ok(Req::Embed { texts, is_query, reply }) = rx.recv() {
        if loaded.is_none() {
            match load() {
                Ok(l) => loaded = Some(l),
                Err(e) => {
                    let _ = reply.send(Err(e));
                    continue;
                }
            }
        }
        let l = loaded.as_mut().expect("just loaded");
        let _ = reply.send(embed_batch(l, &texts, is_query));
    }
}

fn load() -> Result<Loaded, String> {
    let spec = model_spec();
    let dir = crate::model_source::resolve_model_dir(&spec)
        .ok_or_else(|| format!("embed model not downloaded: {spec} (fetch it with `rozum models`)"))?;
    let model = mlx_lm::models::qwen3::load_qwen3_model(&dir)
        .map_err(|e| format!("embed model load {spec}: {e}"))?;
    let tok = mlx_lm_utils::tokenizer::Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| format!("embed tokenizer {spec}: {e}"))?;
    let eos = tok
        .encode("<|endoftext|>", false)
        .map_err(|e| format!("eos encode: {e}"))?
        .get_ids()
        .first()
        .copied()
        .ok_or("eos token missing")? as i32;
    Ok(Loaded { model, tok, eos })
}

fn embed_batch(l: &mut Loaded, texts: &[String], is_query: bool) -> Result<Vec<Vec<f32>>, String> {
    use mlx_rs::module::Module as _;
    use mlx_rs::ops::indexing::IndexOp as _;

    let mut toks: Vec<(usize, Vec<i32>)> = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let text = if is_query {
            format!("Instruct: {QUERY_INSTRUCTION}\nQuery: {t}")
        } else {
            t.clone()
        };
        let mut ids: Vec<i32> = l
            .tok
            .encode(text.as_str(), false)
            .map_err(|e| format!("tokenize: {e}"))?
            .get_ids()
            .iter()
            .map(|&x| x as i32)
            .collect();
        ids.truncate(MAX_TOKENS_PER_TEXT);
        // Last-token pooling wants a defined last position: `<|endoftext|>` appended, per the
        // model's recipe. Mean pooling was measured at 0/26 — off-recipe measures nothing.
        ids.push(l.eos);
        toks.push((i, ids));
    }
    // Sorted by length so a batch pads to (nearly) its own longest row, not the corpus's.
    toks.sort_by_key(|(_, v)| v.len());

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    let mut start = 0usize;
    while start < toks.len() {
        let mut end = start + 1;
        while end < toks.len() && (end + 1 - start) * toks[end].1.len() <= TOKEN_BUDGET {
            end += 1;
        }
        let group = &toks[start..end];
        start = end;
        let width = group.iter().map(|(_, v)| v.len()).max().unwrap_or(1);
        let mut flat: Vec<i32> = Vec::with_capacity(group.len() * width);
        for (_, ids) in group {
            flat.extend_from_slice(ids);
            flat.extend(std::iter::repeat_n(l.eos, width - ids.len()));
        }
        let arr = mlx_rs::Array::from_slice(&flat, &[group.len() as i32, width as i32]);
        let mut cache: Vec<Option<mlx_lm::cache::ConcatKeyValueCache>> = Vec::new();
        // `Model.model` is the transformer BODY — hidden states before the LM head, which is
        // the whole reason no fork change was needed.
        let h = l
            .model
            .model
            .forward(mlx_lm::models::qwen3::ModelInput { inputs: &arr, mask: None, cache: &mut cache })
            .map_err(|e| format!("embed forward: {e}"))?;
        let h = h
            .as_dtype(mlx_rs::Dtype::Float32)
            .map_err(|e| format!("embed cast: {e}"))?;
        h.eval().map_err(|e| format!("embed eval: {e}"))?;
        let all: Vec<f32> = h.as_slice::<f32>().to_vec();
        let dim = all.len() / (group.len() * width);
        for (row, (idx, ids)) in group.iter().enumerate() {
            // Right padding is safe HERE because attention is causal and pooling takes the last
            // REAL token: a real token never attends to a pad that follows it. A property of
            // this pooling choice, not a general licence to pad.
            let last = ids.len() - 1;
            let off = (row * width + last) * dim;
            let v = &all[off..off + dim];
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
            out[*idx] = v.iter().map(|x| x / n).collect();
        }
    }
    Ok(out)
}
