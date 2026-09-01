//! Retrieval eval: top-1/top-5 file localisation over a fixed question set, through the SAME
//! search policy every reader ships (deep-pool fusion + post-fusion rebalance).
//!
//! Exists because the original 26-question spike harness was LOST — its 21/26 lived on only as
//! a number in a spec, unreproducible (`rag-vector-freshness-cli`). This one is committed, and
//! its question file (`scripts/bench/rag-eval-questions.tsv`, `question<TAB>file[|altfile]`)
//! doubles as the regression fence for the retrieval stack: run it after touching chunking,
//! fusion, balancing or the store, and compare against the baseline recorded in
//! `docs/specs/rag-embeddings-impl.md`.
//!
//! The query embed goes through the GATEWAY (this example carries no model); with no gateway
//! the run is BM25-only and says so — the numbers are then not comparable to a fused baseline.
//!
//! Usage: cargo run --release -p rozum-agent --example rag-eval [questions.tsv] [root]

use std::path::PathBuf;

use rozum_agent::{rag_chunk, rag_embed, rag_lite};

fn file_of(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let qpath = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("scripts/bench/rag-eval-questions.tsv"));
    let root = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    let questions: Vec<(String, Vec<String>)> = std::fs::read_to_string(&qpath)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", qpath.display()))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (q, e) = l.split_once('\t').unwrap_or_else(|| panic!("no TAB in line: {l}"));
            (q.to_string(), e.split('|').map(str::to_string).collect())
        })
        .collect();

    // Same freshness the CLI now has: refresh the index, then embed what is missing.
    if rag_chunk::index_path(&root).exists() {
        let _ = rag_chunk::refresh_in_background(&root, &mut |_, _, _| {});
        rag_embed::embed_missing_via_gateway(&root).await;
    }
    let index = rag_chunk::load_project_index(&root)
        .unwrap_or_else(|| panic!("no index at {} — run `rozum rag index`", root.display()));
    let vecs = rag_embed::VecStore::load(&rag_embed::vectors_path(&root), None);

    let k = 5usize;
    let pool = k.max(5) * 4;
    let (mut top1, mut top5, mut fused_runs) = (0usize, 0usize, 0usize);
    for (q, expects) in &questions {
        let bm25 = rag_lite::search_balanced(&index, q, pool);
        let hits = match &vecs {
            Some(vs) => match rag_embed::embed_via_gateway(std::slice::from_ref(q), true).await {
                Some(qv) if qv.first().is_some_and(|v| v.len() == rag_embed::VectorIndex::dim(vs)) => {
                    let ranked = rag_embed::VectorIndex::search(vs, &qv[0], pool);
                    let mut hits = rag_embed::fuse(&bm25, &ranked, pool);
                    for h in &mut hits {
                        if h.text.is_empty()
                            && let Some(t) = index.text_of(&h.id)
                        {
                            h.text = t.to_string();
                        }
                    }
                    fused_runs += 1;
                    rag_lite::rebalance(&hits, k)
                }
                _ => rag_lite::rebalance(&bm25, k),
            },
            None => rag_lite::rebalance(&bm25, k),
        };
        let files: Vec<&str> = hits.iter().map(|h| file_of(&h.id)).collect();
        let t1 = files.first().is_some_and(|f| expects.iter().any(|e| e == f));
        let t5 = files.iter().any(|f| expects.iter().any(|e| e == f));
        if t1 {
            top1 += 1;
        }
        if t5 {
            top5 += 1;
        }
        let mark = if t1 {
            "T1"
        } else if t5 {
            "t5"
        } else {
            "--"
        };
        println!("{mark}  {}  -> {}", &q[..q.len().min(72)], files.first().unwrap_or(&"∅"));
    }
    let n = questions.len();
    let mode = if fused_runs == n {
        "fused"
    } else if fused_runs == 0 {
        "BM25-ONLY (no gateway — not comparable to a fused baseline)"
    } else {
        "MIXED (gateway flaked mid-run — rerun)"
    };
    println!("\ntop-1 {top1}/{n}   top-5 {top5}/{n}   [{mode}]");
}
