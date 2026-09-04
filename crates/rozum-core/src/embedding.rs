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
pub type EmbedFn = fn(Option<&str>, &[String], bool) -> Result<Embedded, String>;

/// What an embed call produced, and — the part that used to be missing — WHICH model produced
/// it. The endpoint could not report the model because nothing downstream ever returned it, so
/// a caller had no way to learn what its vectors were, and vectors from two different models
/// are not comparable. Naming it is what makes the answer checkable.
#[derive(Debug, Clone)]
pub struct Embedded {
    /// The model that actually answered, as a resolved spec — never the string the caller sent.
    pub model: String,
    /// One L2-normalised vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
}

static EMBEDDER: OnceLock<EmbedFn> = OnceLock::new();

/// Register the embedding backend. First registration wins; later calls are ignored (the
/// engine binary registers exactly once, before serving).
pub fn register_embedder(f: EmbedFn) {
    let _ = EMBEDDER.set(f);
}

/// Embed `texts` with a REQUESTED model, or `None` when no backend is registered in this build.
///
/// `None` for `model` means "whatever this process is configured for" — the behaviour every
/// internal caller wants and the default when a client names nothing. A `Some` that names
/// something this machine cannot serve comes back as `Err`, deliberately: substituting a
/// different model silently is how a caller ends up with vectors it cannot compare to the ones
/// it already has, which is worse than a refusal it can read.
pub fn embed_with(
    model: Option<&str>,
    texts: &[String],
    is_query: bool,
) -> Option<Result<Embedded, String>> {
    EMBEDDER.get().map(|f| f(model, texts, is_query))
}

/// Embed `texts` with the process's configured model — the shape every in-process caller uses,
/// none of which chooses a model.
pub fn embed(texts: &[String], is_query: bool) -> Option<Result<Vec<Vec<f32>>, String>> {
    embed_with(None, texts, is_query).map(|r| r.map(|e| e.vectors))
}

/// Whether an embedding backend is available at all — lets an endpoint distinguish
/// "not in this build" (501) from a runtime failure (502).
pub fn available() -> bool {
    EMBEDDER.get().is_some()
}

/// Set by a caller that never loads a resident CHAT model in this process — `rozum rag mcp`,
/// `rozum rag search`, the CLI's corpus catch-up — as opposed to `rozum gateway`, where the
/// chat model owns process-wide MLX cache policy and the embedder must leave it alone
/// (`rozum-mlx/src/embedder.rs`'s own header comment names exactly this split).
///
/// Exists because of a real incident: a standalone `rag mcp` process fell back to embedding an
/// entire 94,857-chunk corpus in-process (the shared gateway was briefly unreachable, so the
/// one-shot probe in `rag_mcp::spawn_warmup` committed to the slow path) and grew to a 28 GB
/// Metal cache over roughly two hours — nothing in that process ever bounded it, because the
/// embedder is written to never touch cache policy when it MIGHT be sharing a process with a
/// resident chat model. `is_standalone_process()` lets the embedder tell the two cases apart
/// and bound its own cache when it is safe to.
static STANDALONE: OnceLock<()> = OnceLock::new();

/// Declare this process a standalone caller (see [`is_standalone_process`]). Idempotent;
/// call once, early, before any embedding happens.
pub fn mark_standalone_process() {
    let _ = STANDALONE.set(());
}

/// Whether [`mark_standalone_process`] has been called in this process.
pub fn is_standalone_process() -> bool {
    STANDALONE.get().is_some()
}
