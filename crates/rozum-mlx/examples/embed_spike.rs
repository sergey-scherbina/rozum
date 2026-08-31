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

    let mut model = mlx_lm::models::qwen3::load_qwen3_model(dir)?;
    let tok = mlx_lm_utils::tokenizer::Tokenizer::from_file(format!("{dir}/tokenizer.json"))?;
    eprintln!("model + tokenizer loaded");

    // Mean-pooled last hidden state, L2-normalised. `Model.model` is the transformer body and
    // `forward` returns hidden states BEFORE the LM head — which is why no fork change is needed.
    // Qwen3-Embedding's OWN recipe, not a generic one: LAST-token pooling with `<|endoftext|>`
    // appended, and queries wrapped in an instruction. Mean pooling was tried first and is off
    // -recipe — it is how the first version of this spike concluded, wrongly, that embeddings lose.
    let eos: i32 = tok.encode("<|endoftext|>", false)?.get_ids()[0] as i32;
    let mut embed = |text: &str, is_query: bool| -> Result<Vec<f32>, Box<dyn std::error::Error + Send + Sync>> {
        let text = if is_query {
            format!("Instruct: Given a question about a codebase, retrieve the code or document that answers it\nQuery: {text}")
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
    };

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

    let t0 = std::time::Instant::now();
    let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
    for (i, (_, text)) in chunks.iter().enumerate() {
        vecs.push(embed(text, false)?);
        if i % 500 == 0 {
            eprint!("\r  embedded {i}/{}", chunks.len());
            let _ = std::io::stderr().flush();
        }
    }
    eprintln!("\rembedded {} chunks in {:?}", chunks.len(), t0.elapsed());

    let eval: serde_json::Value = serde_json::from_slice(&std::fs::read(eval_path)?)?;
    let (mut top1, mut top5, mut n) = (0, 0, 0);
    for q in eval["questions"].as_array().into_iter().flatten() {
        let (text, answer) = (q["q"].as_str().unwrap(), q["answer"].as_str().unwrap());
        let qv = embed(text, true)?;
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
