//! `SpecDecodeBackend` — a [`ChatBackend`] that pairs a big **target** with a
//! small **draft** for speculative decoding (`docs/specs/speculative-decoding.md`).
//!
//! Iteration-1 (this commit): it loads both models and **delegates `chat` to the
//! target** — a safe fallback that is byte-identical to plain target decode (no
//! speedup yet). The token-level speculative path (draft proposes → target
//! verifies via [`crate::specdecode::decode`]) lands in iteration-2 and only
//! changes latency, never the output — so the agentic matrix pass/fail stays
//! identical and the per-run tok/s is what improves.

use std::sync::Arc;

use crate::{ChatBackend, ChatRequest, ChatStream, ModelResult};

pub struct SpecDecodeBackend {
    target: Arc<dyn ChatBackend>,
    draft: Arc<dyn ChatBackend>,
}

impl SpecDecodeBackend {
    pub fn new(target: Arc<dyn ChatBackend>, draft: Arc<dyn ChatBackend>) -> Self {
        tracing::info!(
            target = target.label(),
            draft = draft.label(),
            "spec-decode: draft + target resident (iteration-1: target-only decode; verify pending)"
        );
        Self { target, draft }
    }
}

#[async_trait::async_trait]
impl ChatBackend for SpecDecodeBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        // Iteration-1: plain target decode (the byte-identical baseline). The
        // speculative draft→verify loop lands in iteration-2; it changes only
        // latency, not output. `draft` is held resident for that next step.
        let _ = &self.draft;
        self.target.chat(req).await
    }

    fn context_window(&self) -> u32 {
        self.target.context_window()
    }
    fn label(&self) -> &'static str {
        "spec-decode"
    }
    fn concurrency_capacity(&self) -> Option<usize> {
        self.target.concurrency_capacity()
    }
    fn count_tokens(&self, text: &str) -> Option<usize> {
        self.target.count_tokens(text)
    }
}
