//! Text-embedding SPI: the register-hook seam between the gateway (which serves
//! `/v1/embeddings`) and `rozum-mlx` (which runs the model).
//!
//! The same `OnceLock` pattern as `obs.rs`'s MLX accessors, for the same reason the workspace
//! split established it: the gateway crate must not grow an edge to `rozum-mlx` — the engine
//! BINARY wires the two at startup. Unregistered (a build without `mlx-native`, or a unit test)
//! is a first-class state: [`embed`] returns `None` and the endpoint answers 501, so every
//! caller downstream falls back to lexical retrieval instead of erroring.

use std::sync::OnceLock;

/// `embed(texts, is_query)` → one L2-normalised vector per input text, or `Err` with a
/// human-readable reason (model failed to load, out of memory). `is_query` applies the model's
/// query-side instruction wrapper — corpus texts and queries are embedded DIFFERENTLY by
/// design, and the asymmetry is the recipe's, not ours.
pub type EmbedFn = fn(&[String], bool) -> Result<Vec<Vec<f32>>, String>;

static EMBEDDER: OnceLock<EmbedFn> = OnceLock::new();

/// Register the embedding backend. First registration wins; later calls are ignored (the
/// engine binary registers exactly once, before serving).
pub fn register_embedder(f: EmbedFn) {
    let _ = EMBEDDER.set(f);
}

/// Embed `texts`, or `None` when no backend is registered in this build.
pub fn embed(texts: &[String], is_query: bool) -> Option<Result<Vec<Vec<f32>>, String>> {
    EMBEDDER.get().map(|f| f(texts, is_query))
}

/// Whether an embedding backend is available at all — lets an endpoint distinguish
/// "not in this build" (501) from a runtime failure (502).
pub fn available() -> bool {
    EMBEDDER.get().is_some()
}
