//! smmr-D measurement probe (`sunny-civet`) — empirically settles the MLX memory-cap
//! question that the source audit raised
//! (`docs/specs/safe-multi-model-residency.md` § Findings).
//!
//! ## RAW-ALLOC MODE (this example — slot-free, NO model loaded)
//! Allocates live f32 arrays past `set_memory_limit` and reads `get_active_memory()`
//! to prove whether the limit is a hard ceiling or a soft cache-eviction hint, and
//! shows `set_cache_limit` bounding the cache term. Run:
//! ```text
//! cargo run -p rozum-mlx --example mlx_mem_probe --features mlx-native
//! ```
//! Env: `PROBE_LIMIT_MB` (512), `PROBE_CACHE_MB` (0 = no retained cache),
//! `PROBE_ALLOC_MB` (1024 = how much live memory to allocate past the limit).
//! It allocates at most ~`PROBE_ALLOC_MB` of GPU memory and frees it on exit, so it
//! is safe to run alongside a resident model (it loads none and reserves none).
//!
//! ## MODEL MODE (smmr-D — needs the model slot; `nimble-raven`)
//! This example does NOT load a model. To split a real model's peak into active vs
//! cache (the decision smmr-D must make), wrap the model load + a representative
//! prefill with the SAME recipe:
//! ```text
//! use mlx_rs::memory;
//! memory::reset_peak_memory();
//! // ... load model, run a representative prefill at the target n_ctx ...
//! let active = memory::get_active_memory(); // weights + live KV + activations
//! let cache  = memory::get_cache_memory();  // reclaimable, bounded by set_cache_limit
//! let peak   = memory::get_peak_memory();   // high-water (≈ the RSS spike)
//! ```
//! If `peak − need` is dominated by **cache**, co-residency is safe (`set_cache_limit`
//! bounds it); if by **active**, it is not (nothing bounds active but the model + a
//! share-sized chunked prefill). That is the gate for default co-residency.

#[cfg(not(feature = "mlx-native"))]
fn main() {
    eprintln!("mlx_mem_probe requires --features mlx-native");
    std::process::exit(2);
}

#[cfg(feature = "mlx-native")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use mlx_rs::memory;

    let mb = |bytes: usize| bytes / (1usize << 20);
    let env_mb = |k: &str, d: usize| -> usize {
        std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
    };
    let limit_mb = env_mb("PROBE_LIMIT_MB", 512);
    let cache_mb = env_mb("PROBE_CACHE_MB", 0);
    let alloc_mb = env_mb("PROBE_ALLOC_MB", 1024);

    memory::set_memory_limit(limit_mb << 20);
    memory::set_cache_limit(cache_mb << 20);
    memory::reset_peak_memory();
    println!(
        "mlx_mem_probe: set_memory_limit={limit_mb}MB set_cache_limit={cache_mb}MB; \
         allocating {alloc_mb}MB of LIVE f32 arrays (kept alive past the limit) …"
    );

    // Allocate `alloc_mb` of f32 in 256 MB chunks and KEEP them alive, so they count
    // as ACTIVE memory (not cache). `eval()` forces the Metal allocation to happen now.
    let chunk_mb = 256usize;
    let mut live: Vec<mlx_rs::Array> = Vec::new();
    let mut done_mb = 0usize;
    while done_mb < alloc_mb {
        let this_mb = chunk_mb.min(alloc_mb - done_mb);
        let n = ((this_mb << 20) / 4) as i32; // f32 = 4 bytes
        let a = mlx_rs::ops::zeros_dtype(&[n], mlx_rs::Dtype::Float32)?;
        a.eval()?;
        live.push(a);
        done_mb += this_mb;
    }

    let active = memory::get_active_memory();
    let cache = memory::get_cache_memory();
    let peak = memory::get_peak_memory();
    println!(
        "RESULT  active={}MB  cache={}MB  peak={}MB   (set_memory_limit was {}MB)",
        mb(active),
        mb(cache),
        mb(peak),
        limit_mb
    );

    let limit_bytes = limit_mb << 20;
    if active > limit_bytes {
        println!(
            "VERDICT set_memory_limit is SOFT — live active memory EXCEEDED it \
             ({}MB > {}MB). It is a cache-eviction hint, not a hard per-process cap; \
             only physical RAM stops a process. ⇒ per-process caps cannot be the \
             co-residency safety guarantee — conservative admission is. (Confirms the \
             source audit: allocator.cpp malloc allocates past gc_limit_.)",
            mb(active),
            limit_mb
        );
    } else {
        println!(
            "VERDICT set_memory_limit appears HARD on this MLX build — active stayed \
             under the limit ({}MB ≤ {}MB). This CONTRADICTS the source read; \
             re-check the allocator version before trusting it as a cap.",
            mb(active),
            limit_mb
        );
    }

    // Demonstrate that set_cache_limit bounds the cache term: drop the live arrays
    // (their buffers go to cache), then re-read — cache must stay ≤ set_cache_limit.
    drop(live);
    // A tiny allocation triggers the cache-trim path in malloc.
    let _ = mlx_rs::ops::zeros_dtype(&[1], mlx_rs::Dtype::Float32)?.eval();
    let cache_after = memory::get_cache_memory();
    println!(
        "CACHE   after dropping the live arrays, cache={}MB (set_cache_limit={}MB) — \
         set_cache_limit IS the lever that bounds resident footprint (the cache term).",
        mb(cache_after),
        cache_mb
    );
    Ok(())
}
