//! Real-MLX integration evals — manual (`#[ignore]`, need the network + a ~2.5 GB
//! model). Relocated here from the agent/router unit tests during the workspace
//! split (Phase 3): they drive the agent loop / model router / RAG worker
//! (`rozum-agent`) against a real native MLX backend (`rozum-mlx`) — a combination
//! only the binary legitimately makes, so the binary's integration tests are their
//! home. The whole file compiles only with the `mlx-native` feature. Run e.g.:
//!   cargo test --features mlx-native --test mlx_evals -- --ignored --nocapture
#![cfg(feature = "mlx-native")]

use std::sync::Arc;

use rozum::ChatBackend;
use rozum::ToolDef;
use rozum::agent::{AgentStop, Budget, CallbackToolSource, ToolError, run_agent};
use rozum::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
use rozum::rag_lite::LexicalIndex;
use rozum::router::{Label, ModelRouter, RagWorker};
use serde_json::json;

fn add_tool() -> CallbackToolSource {
    let def = ToolDef {
        name: "add".into(),
        description: "Add two integers".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"]
        }),
    };
    CallbackToolSource::new().with_tool(def, |args| {
        let a = args["a"]
            .as_i64()
            .ok_or_else(|| ToolError::new("a must be an integer"))?;
        let b = args["b"]
            .as_i64()
            .ok_or_else(|| ToolError::new("b must be an integer"))?;
        Ok(json!({ "sum": a + b }))
    })
}

// End-to-end against a REAL backend: the reference loop drives native MLX through a
// genuine tool call and back to a final answer. Constrained decoding on, so the args
// are guaranteed valid. ~2.5 GB Qwen3-4B.
#[tokio::test]
#[ignore = "network: uses mlx-community/Qwen3-4B-4bit"]
async fn agent_loop_real_backend() {
    unsafe {
        std::env::set_var("ROZUM_MLX_CONSTRAIN", "1");
    }
    let spec = "mlx-community:Qwen3-4B-4bit";
    let dir = ensure_model_dir(spec).await.expect("resolve");
    let backend = MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
        .await
        .expect("load");

    let out = run_agent(
        &backend,
        "You are a calculator. Use the add tool for any addition; do not compute it yourself.",
        "What is 3 + 5? Use the tool, then tell me the result.",
        &add_tool(),
        &Budget {
            max_steps: 4,
            max_tokens: Some(128),
            ..Budget::default()
        },
    )
    .await;
    unsafe {
        std::env::remove_var("ROZUM_MLX_CONSTRAIN");
    }

    eprintln!(
        "AGENT e2e  stop={:?} steps={} ops={} text={:?}",
        out.stop,
        out.steps,
        out.operations.len(),
        out.text
    );
    assert_eq!(out.stop, AgentStop::Done, "loop must reach a final answer");
    let add = out
        .operations
        .iter()
        .find(|o| o.name == "add")
        .expect("the model must call add");
    assert_eq!(add.output.as_ref().expect("add succeeded")["sum"], 8);
    assert!(!out.text.is_empty(), "a final answer is produced");
}

// Real-model router eval (M4). A 4B classifies queries into labels; gate the big model
// only when the small one is confident.
#[tokio::test]
#[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit"]
async fn model_router_eval() {
    let spec = "mlx-community:Qwen3-4B-4bit";
    let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
    let backend: Arc<dyn ChatBackend> = Arc::new(
        MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("load"),
    );
    let router = ModelRouter::new(
        backend,
        vec![
            Label::new("code", "writing, editing, or explaining source code"),
            Label::new("math", "arithmetic, algebra, calculus, or proofs"),
            Label::new("chitchat", "greetings and casual small talk"),
        ],
    )
    .unwrap();

    let cases: &[(&str, &str)] = &[
        ("Write a Rust function that reverses a string", "code"),
        ("Fix the borrow checker error in this impl block", "code"),
        ("What is the derivative of x^3 + 2x?", "math"),
        ("Solve the equation 3x + 7 = 22", "math"),
        ("hey there, how's your day going?", "chitchat"),
        ("good morning! nice weather today", "chitchat"),
    ];
    let mut correct = 0usize;
    for (q, want) in cases {
        let c = router.classify(q).await;
        let ok = c.label == *want;
        correct += ok as usize;
        eprintln!(
            "ROUTER  want={want:8}  got={:8}  conf={:.1}  fallback={}  {}  | {q}",
            c.label,
            c.confidence,
            c.fallback_used,
            if ok { "OK" } else { "MISS" }
        );
    }
    let acc = correct as f32 / cases.len() as f32;
    eprintln!("ROUTER accuracy: {correct}/{} = {:.0}%", cases.len(), acc * 100.0);
    assert!(
        acc >= 0.80,
        "router accuracy {acc:.2} below the gate-the-big-model bar (0.80)"
    );
}

// Real-model RAG-worker eval (M4). A 4B reranks a small mixed corpus + summarizes the
// survivor, grounded only in its text.
#[tokio::test]
#[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit"]
async fn rag_worker_eval() {
    let spec = "mlx-community:Qwen3-4B-4bit";
    let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
    let backend: Arc<dyn ChatBackend> = Arc::new(
        MlxNativeBackend::new(dir, spec.replace(':', "/"), None)
            .await
            .expect("load"),
    );
    let worker = RagWorker::new(backend);

    let mut ix = LexicalIndex::new();
    ix.add(
        "ownership",
        "In Rust, ownership means each value has a single owner and is freed when the owner goes out of scope.",
    );
    ix.add(
        "python",
        "In Python, memory is managed by reference counting and a cyclic garbage collector.",
    );
    ix.add(
        "cooking",
        "A good scope of flavours comes from letting the value of fresh herbs shine.",
    );
    let query = "How does Rust manage memory through ownership?";

    let ans = worker.grounded_answer(&ix, query, 3).await;
    eprintln!(
        "RAG hits (post-rerank): {:?}",
        ans.hits.iter().map(|h| &h.id).collect::<Vec<_>>()
    );
    eprintln!("RAG summary: {}", ans.summary);

    assert!(!ans.hits.is_empty(), "rerank dropped everything");
    assert_eq!(ans.hits[0].id, "ownership", "the answering doc must rank first");
    assert!(
        ans.hits.iter().all(|h| h.id != "cooking"),
        "the off-topic decoy must be dropped"
    );
    let s = ans.summary.to_lowercase();
    assert!(!s.trim().is_empty(), "empty summary");
    assert!(
        s.contains("owner") || s.contains("scope"),
        "summary not grounded in the passage"
    );
}
