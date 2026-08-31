//! SPIKE (rag-embeddings): does a local embedding model actually fix the SEMANTIC misses?
//!
//! Deliberately a throwaway before any plumbing. The whole justification for embeddings is that
//! six of the eval set's answers score ZERO under BM25 because the doc comment says the same
//! thing in other words ("transcript"/"message", "record"/"upsert"). If embeddings do not fix
//! those, the backend, the residency integration and the index format are all wasted work —
//! so measure first, on the same 26 questions, and only then decide.
//!
//! Usage: cargo run --release -p rozum-mlx --features mlx-native --example embed_spike -- <model-dir> <index.json> <eval.json>
use mlx_rs::ops::indexing::IndexOp as _;
use std::io::Write as _;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (dir, index_path, eval_path) = (&a[0], &a[1], &a[2]);

    // Bound the MLX CACHE before the first op. Without it the spike was SIGKILLed twice — at
    // chunk 3008 with fixed 16-row batches and at 6009 with a 4096-token budget — while ~16 GB of
    // system memory was free. That is the signature of MLX's cache growing unboundedly across
    // batches, not of the activations themselves being too large: `set_cache_limit` bounds the
    // CACHE, which is what accumulates here, while active memory (weights + activations) is
    // bounded by the token budget above. Both levers are needed; either alone kills the run.
    mlx_rs::memory::set_cache_limit(512 * 1024 * 1024);
    let mut model = mlx_lm::models::qwen3::load_qwen3_model(dir)?;
    let tok = mlx_lm_utils::tokenizer::Tokenizer::from_file(format!("{dir}/tokenizer.json"))?;
    eprintln!("model + tokenizer loaded");

    // Mean-pooled last hidden state, L2-normalised. `Model.model` is the transformer body and
    // `forward` returns hidden states BEFORE the LM head — which is why no fork change is needed.
    // Qwen3-Embedding's OWN recipe, not a generic one: LAST-token pooling with `<|endoftext|>`
    // appended, and queries wrapped in an instruction. Mean pooling was tried first and is off
    // -recipe — it is how the first version of this spike concluded, wrongly, that embeddings lose.
    let eos: i32 = tok.encode("<|endoftext|>", false)?.get_ids()[0] as i32;
    fn embed_one(
        model: &mut mlx_lm::models::qwen3::Model,
        tok: &mlx_lm_utils::tokenizer::Tokenizer,
        eos: i32,
        text: &str,
        is_query: bool,
    ) -> Result<Vec<f32>, Box<dyn std::error::Error + Send + Sync>> {
        let text = if is_query {
            {
                // Part of Qwen3-Embedding's recipe, and the model is sensitive to it — the first
                // wording used here was simply invented, so it is a variable rather than a
                // constant. Empty means "no instruction at all", which is itself a variant worth
                // measuring: the recipe is for retrieval in general, not for code.
                let instr = std::env::var("EMB_INSTR").unwrap_or_else(|_| {
                    "Given a question about a codebase, retrieve the code or document that answers it".into()
                });
                if instr.is_empty() { text.to_string() } else { format!("Instruct: {instr}\nQuery: {text}") }
            }
        } else {
            text.to_string()
        };
        let mut ids: Vec<i32> = tok.encode(text.as_str(), false)?.get_ids().iter().map(|&i| i as i32).collect();
        ids.truncate(511);
        ids.push(eos);
        let arr = mlx_rs::Array::from_slice(&ids, &[1, ids.len() as i32]);
        let mut cache: Vec<Option<mlx_lm::cache::ConcatKeyValueCache>> = Vec::new();
        let h = {
            use mlx_rs::module::Module as _;
            model.model.forward(mlx_lm::models::qwen3::ModelInput {
                inputs: &arr,
                mask: None,
                cache: &mut cache,
            })?
        };
        // The model runs in bf16, so pool THEN cast — `as_slice::<f32>()` on a bf16 array is a
        // dtype mismatch, not a conversion.
        // LAST token, not the mean: the model is trained so the final position carries the
        // sentence embedding.
        let last = h.index((.., -1, ..));
        let pooled = last.as_dtype(mlx_rs::Dtype::Float32)?;
        pooled.eval()?;
        let v: Vec<f32> = pooled.as_slice::<f32>().to_vec();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.iter().map(|x| x / n).collect())
    }

    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(index_path)?)?;
    let mut chunks: Vec<(String, String)> = Vec::new();
    for f in index["files"].as_array().into_iter().flatten() {
        for c in f["chunks"].as_array().into_iter().flatten() {
            chunks.push((
                c["id"].as_str().unwrap_or_default().to_string(),
                c["text"].as_str().unwrap_or_default().to_string(),
            ));
        }
    }
    eprintln!("chunks: {}", chunks.len());

    // BATCHED corpus pass. The one-at-a-time version measured 718 s for this repo — batch size 1
    // on a GPU, which is the wrong shape of work, not an inherent cost of embedding.
    //
    // Right-padding is SAFE here specifically because pooling takes the last REAL token and the
    // attention is causal: a real token never attends to a pad that follows it, so its hidden
    // state is identical to the unpadded run. No mask is needed, and that is a property of this
    // pooling choice rather than a general licence to pad.
    //
    // Sorted by length so a batch pads to nearly its own longest row instead of the corpus's.
    // Batched by TOKEN BUDGET, not by row count. A fixed 16 rows was tried and the process was
    // SIGKILLed, deterministically, at chunk 3008 of 10551: the rows are sorted by length, so by
    // then a batch of 16 was 16 x ~400 tokens and the activations no longer fit. Rows are the
    // wrong unit — cost scales with rows x width, so the budget has to be on the product. On a
    // machine whose hard invariant is no-OOM, a batching scheme that grows without a ceiling is
    // not an optimisation, it is a jetsam waiting for a bigger corpus.
    let budget: usize =
        std::env::var("EMB_TOKENS").ok().and_then(|v| v.parse().ok()).unwrap_or(4096);
    // WHAT gets embedded, not just how. The default is the raw source slice — but the question is
    // natural language and the chunk is syntax, so `distilled` tries the part written FOR a
    // reader: the path, the item's kind and name, and its doc comment.
    let distilled = std::env::var("EMB_TEXT").as_deref() == Ok("distilled");
    let doc_of = |id: &str, text: &str| -> String {
        let (path, frag) = id.split_once('#').unwrap_or((id, ""));
        let mut doc = String::new();
        for line in text.lines() {
            let t = line.trim_start();
            if t.is_empty() || (t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")) {
                continue;
            }
            if let Some(rest) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) {
                doc.push_str(rest);
                doc.push(' ');
                continue;
            }
            break;
        }
        // No doc comment (very common) -> fall back to the source, or the chunk would be a bare
        // name and lose every chunk that simply is not documented.
        if doc.trim().is_empty() {
            format!("{path} {frag}\n{text}")
        } else {
            format!("{path} {frag}\n{doc}")
        }
    };
    let mut toks: Vec<(usize, Vec<i32>)> = Vec::with_capacity(chunks.len());
    for (i, (id, raw)) in chunks.iter().enumerate() {
        let owned;
        let text: &String = if distilled {
            owned = doc_of(id, raw);
            &owned
        } else {
            raw
        };
        let mut ids: Vec<i32> =
            tok.encode(text.as_str(), false)?.get_ids().iter().map(|&i| i as i32).collect();
        ids.truncate(511);
        ids.push(eos);
        toks.push((i, ids));
    }
    toks.sort_by_key(|(_, ids)| ids.len());

    // Corpus vectors do NOT depend on the query instruction, so cache them on disk: sweeping
    // instructions then costs 26 query embeddings instead of a 276 s corpus pass per variant.
    let cache_path = std::env::var("EMB_CACHE").unwrap_or_default();
    let cached: Option<Vec<Vec<f32>>> = if cache_path.is_empty() {
        None
    } else {
        std::fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Vec<f32>>>(&b).ok())
            .filter(|v| v.len() == chunks.len())
    };
    let t0 = std::time::Instant::now();
    let mut vecs: Vec<Vec<f32>> = vec![Vec::new(); chunks.len()];
    let mut done = 0usize;
    let mut start = if cached.is_some() { toks.len() } else { 0 };
    if let Some(c) = &cached {
        vecs = c.clone();
        eprintln!("corpus vectors from cache ({})", cache_path);
    }
    while start < toks.len() {
        // Sorted ascending, so the last row in the window is the widest; grow the window while
        // rows x width stays inside the budget.
        let mut end = start + 1;
        while end < toks.len() {
            let width = toks[end].1.len();
            if (end + 1 - start) * width > budget {
                break;
            }
            end += 1;
        }
        let group = &toks[start..end];
        start = end;
        let width = group.iter().map(|(_, v)| v.len()).max().unwrap_or(1);
        let mut flat: Vec<i32> = Vec::with_capacity(group.len() * width);
        for (_, ids) in group {
            flat.extend_from_slice(ids);
            flat.extend(std::iter::repeat_n(eos, width - ids.len()));
        }
        let arr = mlx_rs::Array::from_slice(&flat, &[group.len() as i32, width as i32]);
        let mut cache: Vec<Option<mlx_lm::cache::ConcatKeyValueCache>> = Vec::new();
        let h = {
            use mlx_rs::module::Module as _;
            model.model.forward(mlx_lm::models::qwen3::ModelInput {
                inputs: &arr,
                mask: None,
                cache: &mut cache,
            })?
        };
        let h = h.as_dtype(mlx_rs::Dtype::Float32)?;
        h.eval()?;
        let all: Vec<f32> = h.as_slice::<f32>().to_vec();
        let dim = all.len() / (group.len() * width);
        for (row, (idx, ids)) in group.iter().enumerate() {
            let last = ids.len() - 1;
            let off = (row * width + last) * dim;
            let v = &all[off..off + dim];
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
            vecs[*idx] = v.iter().map(|x| x / n).collect();
        }
        // No `clear_cache` in this binding version — re-asserting the limit is the available
        // lever, and it is what evicts down to the bound (`set_cache_limit` is documented as
        // affecting eviction, not just future growth).
        mlx_rs::memory::set_cache_limit(512 * 1024 * 1024);
        done += group.len();
        if done % 1000 < group.len().max(1) {
            eprint!("\r  embedded {done}/{}", chunks.len());
            let _ = std::io::stderr().flush();
        }
    }
    eprintln!("\rembedded {} chunks in {:?} (token budget {budget})", chunks.len(), t0.elapsed());

    if !cache_path.is_empty() && cached.is_none() {
        let _ = std::fs::write(&cache_path, serde_json::to_vec(&vecs)?);
    }
    let eval: serde_json::Value = serde_json::from_slice(&std::fs::read(eval_path)?)?;
    let (mut top1, mut top5, mut n) = (0, 0, 0);
    for q in eval["questions"].as_array().into_iter().flatten() {
        let (text, answer) = (q["q"].as_str().unwrap(), q["answer"].as_str().unwrap());
        let qv = embed_one(&mut model, &tok, eos, text, true)?;
        let mut scored: Vec<(f32, &str)> = chunks
            .iter()
            .zip(&vecs)
            .filter(|((id, _), _)| !id.contains("rag-eval.json") && !id.contains("rag-code-retrieval"))
            .map(|((id, _), v)| (v.iter().zip(&qv).map(|(a, b)| a * b).sum::<f32>(), id.as_str()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Dump the full ranking so BM25 and this can be FUSED offline — the two miss different
        // questions, which is the only reason to consider embeddings at all given they lose alone.
        let top: Vec<&str> = scored.iter().take(60).map(|s| s.1).collect();
        println!("RANK\t{text}\t{}", top.join("\t"));
        n += 1;
        if scored.first().is_some_and(|(_, id)| id.contains(answer)) {
            top1 += 1;
        }
        if scored.iter().take(5).any(|(_, id)| id.contains(answer)) {
            top5 += 1;
        } else {
            println!("  ✗ {answer}  ← {}", scored.first().map(|s| s.1).unwrap_or("—"));
        }
    }
    println!("\nEMBEDDINGS: top-1 {top1}/{n}  top-5 {top5}/{n}   (BM25 today: 9/26 and 15/26)");
    Ok(())
}
