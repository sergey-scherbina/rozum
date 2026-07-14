//! Engine- and hardware-agnostic **model source**: resolve a model spec to a
//! local directory, downloading the snapshot from the matching hub (HuggingFace
//! or ModelScope) when it is absent.
//!
//! Lifted out of the MLX leaf (`native-engine-spi` / `portability-shared-model-
//! source`): fetching, the hub cache layout, and spec resolution are useful to
//! **any** safetensors backend (the native MLX runtime, `mistralrs`, a future
//! engine), so they live here instead of being re-implemented per leaf. The one
//! per-engine decision — "can I load this `model_type`?" — is passed in as a
//! `gate` callback, so this module stays independent of any one runtime's
//! catalog.

use std::path::PathBuf;

/// Map a model spec to its HuggingFace `org/name` repo id, or `None` if the spec
/// isn't an HF reference (a filesystem path, `lmstudio:`/`ollama:` spec, …).
pub fn spec_to_hf_repo(spec: &str) -> Option<String> {
    if std::path::Path::new(spec).exists() {
        return None;
    }
    if let Some(r) = spec.strip_prefix("mlx-community:") {
        Some(format!("mlx-community/{r}"))
    } else if let Some(r) = spec.strip_prefix("hf:") {
        Some(r.to_owned())
    } else if spec.contains('/') && !spec.starts_with('/') && !spec.contains(':') {
        // Bare `owner/repo`.
        Some(spec.to_owned())
    } else {
        None
    }
}

/// True when two specs name the SAME model regardless of surface form —
/// `mlx-community:Name`, the bare `mlx-community/Name`, and `hf:mlx-community/Name`
/// all normalize to the same HuggingFace repo. Falls back to exact string equality
/// for specs that don't map to an HF repo (raw dir paths, `lmstudio:`, `modelscope:`).
///
/// Callers that match a `--model` spec against the installed catalog (whose specs are
/// the canonical colon form) MUST use this, not `==`: a user passing the slash form
/// otherwise fails the exact match, the footprint can't be sized, and admission refuses
/// with a sentinel estimate. Unit-tested.
pub fn same_model(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (spec_to_hf_repo(a), spec_to_hf_repo(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y),
        _ => false,
    }
}

/// The effective `model_type` of a parsed `config.json` — top-level, or the
/// `text_config.model_type` of a multimodal wrapper (Qwen3.6 ships the latter).
pub fn config_model_type(cfg: &serde_json::Value) -> Option<&str> {
    cfg.get("model_type")
        .and_then(|v| v.as_str())
        .or_else(|| {
            cfg.get("text_config")
                .and_then(|t| t.get("model_type"))
                .and_then(|v| v.as_str())
        })
}

/// Resolve a model spec to a local directory of safetensors + tokenizer files
/// **already present** on disk (no download — see [`ensure_model_dir`]).
///
/// - an existing directory path -> as-is
/// - `mlx-community:<repo>` / `hf:<user>/<repo>` / `<user>/<repo>` -> the
///   downloaded HuggingFace cache snapshot, if present
///   (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<rev>/`).
/// - `modelscope:<owner>/<repo>` -> ModelScope's own (flat) cache dir.
///
/// Returns `None` when nothing local matches.
pub fn resolve_model_dir(spec: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(spec);
    if direct.is_dir() && direct.join("config.json").is_file() {
        return Some(direct);
    }

    // ModelScope specs resolve to ModelScope's own (flat) cache dir.
    if let Some(r) = spec.strip_prefix("modelscope:") {
        let (owner, name) = r.split_once('/')?;
        let dir = crate::modelscope::model_cache_dir(owner, name)?;
        return dir.join("config.json").is_file().then_some(dir);
    }

    // LM Studio MLX models live under its store as plain HF-layout dirs; the spec is
    // `lmstudio:<repo-relative-to models/>` (produced by models::scan_lmstudio). Resolve to that dir.
    if let Some(rel) = spec.strip_prefix("lmstudio:") {
        let dir = crate::models::lmstudio_root().join("models").join(rel);
        return dir.join("config.json").is_file().then_some(dir);
    }

    // Normalize the spec's `org/name`, mirroring mistralrs_backend::normalize_spec.
    let repo = if let Some(r) = spec.strip_prefix("mlx-community:") {
        format!("mlx-community/{r}")
    } else if let Some(r) = spec.strip_prefix("hf:") {
        r.to_owned()
    } else {
        spec.to_owned()
    };
    let (org, name) = repo.split_once('/')?;

    let home = std::env::var_os("HOME")?;
    let cache = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{name}"))
        .join("snapshots");
    let snapshots = std::fs::read_dir(&cache).ok()?;
    for entry in snapshots.flatten() {
        let dir = entry.path();
        if dir.join("config.json").is_file() {
            return Some(dir);
        }
    }
    None
}

/// Resolve a model spec to a local model dir, **downloading it if absent**.
///
/// Tries the local cache first ([`resolve_model_dir`]); on a miss, fetches the
/// snapshot from the matching hub — `modelscope:<owner>/<repo>` → ModelScope,
/// otherwise `mlx-community:` / `hf:` / `owner/repo` → HuggingFace — but only
/// after `gate` accepts the fetched `config.json` (so an unsupported repo is
/// rejected before its multi-GB weights). The caller supplies its engine's gate,
/// keeping this module catalog-agnostic. Each hub writes its own native cache
/// layout so the download is shared with that hub's tools.
///
/// Returns `None` (chain falls through) when the spec isn't a hub repo or the
/// download fails.
pub async fn ensure_model_dir(
    spec: &str,
    gate: impl Fn(&serde_json::Value) -> Result<(), String>,
) -> Option<PathBuf> {
    if let Some(dir) = resolve_model_dir(spec) {
        return Some(dir);
    }
    let result = if let Some(repo) = spec.strip_prefix("modelscope:") {
        crate::modelscope::ensure_snapshot(repo, gate).await
    } else {
        let repo = spec_to_hf_repo(spec)?;
        crate::hf_hub::ensure_snapshot(&repo, gate).await
    };
    match result {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("rozum: auto-download of '{spec}' skipped: {e}");
            None
        }
    }
}

/// KV-cache element size in bytes (bf16 compute dtype).
const KV_DTYPE_BYTES: u64 = 2;

/// Bytes the KV cache grows **per context position** for a model, from its
/// `config.json`: `2 (k+v) * full_attn_layers * n_kv_heads * head_dim * dtype`.
/// Only full-attention layers hold KV — hybrid models keep it on every
/// `full_attention_interval`-th layer (GatedDeltaNet conv/recurrent state is O(1)
/// in context); dense models on all. Reads `text_config` (the multimodal hybrid
/// wrapper) if present, else the top level. `None` if the config lacks the needed
/// fields. Pure config math — engine- and hardware-agnostic, so any in-process
/// leaf's RAM preflight can reuse it to reject an OOM-bound context before loading
/// weights.
pub fn kv_bytes_per_position(cfg: &serde_json::Value) -> Option<u64> {
    let c = cfg.get("text_config").unwrap_or(cfg);
    let n_layers = c.get("num_hidden_layers")?.as_u64()?;
    // Hybrid: every `full_attention_interval`-th layer is full attention.
    // Dense models omit it -> all layers hold KV.
    let interval = c
        .get("full_attention_interval")
        .and_then(|v| v.as_u64())
        .filter(|&i| i > 0)
        .unwrap_or(1);
    let full_attn_layers = if interval > 1 {
        n_layers / interval
    } else {
        n_layers
    };
    let n_kv = c.get("num_key_value_heads")?.as_u64()?;
    let head_dim = c
        .get("head_dim")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            let hidden = c.get("hidden_size")?.as_u64()?;
            let heads = c.get("num_attention_heads")?.as_u64()?;
            (heads > 0).then(|| hidden / heads)
        })?;
    Some(2 * full_attn_layers * n_kv * head_dim * KV_DTYPE_BYTES)
}

/// Activation + cache reserve added on top of weights + KV, tied to the **real bounds**
/// (smmr-D-calibrated, 2026-06-22): (a) the MLX buffer **cache**, hard-bounded by
/// `set_cache_limit` (rozum default 4 GiB, `ROZUM_MLX_CACHE_GB`) — smmr-D saw it grow
/// under load and pin at exactly the limit; and (b) the prefill activation spike, bounded
/// by chunked prefill (smmr-D measured ~1.2 GiB at 14k ctx → a 1.5 GiB margin). So the
/// reserve is `set_cache_limit + 1.5 GiB` (≈ 5.5 GiB default), **not** the old arbitrary
/// `max(6 GiB, weights/4)` (which over-reserved and never even engaged the weights/4 term
/// for our ≤19 GiB-weight models — a flat 6 GiB). `set_memory_limit` is only a soft hint,
/// so conservative admission on this figure is the safety lever
/// ([[reference-mlx-memory-cap-semantics]]); it must stay ≥ the real cache+prefill peak.
/// If the cache cap is DISABLED (`ROZUM_MLX_CACHE_GB=0`) the cache is unbounded → no
/// footprint can bound it → fall back to a large conservative reserve.
/// Default MLX buffer-cache cap (GiB) for a model of `weight_bytes`, used when the operator hasn't
/// pinned `ROZUM_MLX_CACHE_GB`. A small model doesn't benefit from a big cache — smmr-D measured a 4B
/// pinning only ~1.2 GiB even at 14k ctx — so a flat 4 GiB over-reserves ~2 GiB of dead headroom on
/// small models. Scale it: `clamp(weights_GiB / 2, 2, 4)`. Big models (≥8 GiB weights) keep the full
/// 4 GiB cap; a 4B (~3 GiB weights) gets 2 GiB → ~2 GiB less reserved. Floored at 2 GiB, safely above
/// the measured cache+prefill peak. The memory win realizes under co-residency / bigger models and is
/// harmless for a single small model (just leaves more RAM free). `gw-cache-cap-by-size`.
pub fn default_cache_cap_gib(weight_bytes: u64) -> u64 {
    const GB: u64 = 1 << 30;
    (weight_bytes / GB / 2).clamp(2, 4)
}

fn activation_reserve_bytes(weight_bytes: u64) -> u64 {
    const GB: u64 = 1 << 30;
    // Reads the cap the load path already chose + published to the env (`fit_model_params` sets it,
    // size-scaled, so estimate and the real `set_cache_limit` agree). Falls back to a flat 4 GiB only
    // when nothing set it (adaptive-off) — matching the `cap_mlx_memory` fallback so the two stay
    // consistent. `gw-cache-cap-by-size` does the scaling in `fit_model_params`, not here.
    let cache_gb = std::env::var("ROZUM_MLX_CACHE_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(4);
    if cache_gb == 0 {
        // Cache cap off ⇒ cache unbounded ⇒ admission must be very cautious.
        return (weight_bytes / 4).max(8 * GB);
    }
    cache_gb.saturating_mul(GB).saturating_add(GB + GB / 2) // cache bound + ~1.5 GiB prefill
}

/// A model's resident RAM **need** — weights + the KV cache at `n_ctx` + an activation
/// + cache reserve. This is the figure the host residency ledger (BUG-003 v2) admits
/// against; admission (refuse-before-load) is the structural safety lever, because no
/// MLX API hard-caps a process below physical RAM — `set_memory_limit` is soft and only
/// `set_cache_limit` bounds the cache ([[reference-mlx-memory-cap-semantics]]). So this
/// must be ≥ the model's real resident peak (active weights+KV+prefill **plus** the
/// bounded cache), or two models the ledger admits could together overcommit the host.
///
/// `weight_bytes` is the catalog on-disk size (≈ resident quantized weights). The KV
/// term reads the cached `config.json` via [`kv_bytes_per_position`]; an unreadable
/// config folds KV into the reserve (the caller keeps its own conservative floor).
/// Conservative throughout (saturating, rounds up). See
/// `docs/specs/safe-multi-model-residency.md`.
pub fn runtime_footprint_bytes(spec: &str, n_ctx: u32, weight_bytes: u64) -> u64 {
    runtime_active_bytes(spec, n_ctx, weight_bytes).saturating_add(process_reserve_bytes(weight_bytes))
}

/// A model's **per-model** resident bytes — weights + KV cache at `n_ctx`, WITHOUT the
/// process-shared activation reserve. This is the part that genuinely scales with the number
/// of co-resident models: each model holds its own weights and its own KV cache.
///
/// Split out from [`runtime_footprint_bytes`] for the **shared-reserve** accounting: when N
/// models co-reside in ONE process they share a single MLX buffer cache (`set_cache_limit` is
/// process-global) and serialize prefill (`max_num_seqs`), so the activation reserve
/// ([`process_reserve_bytes`]) is real ONCE per process — not per model. The correct
/// in-process multi-model peak is therefore `Σ runtime_active_bytes(model_i) +
/// process_reserve_bytes(max weight)`, which counts the cache+prefill pool a single time.
/// For a single-model process this + the reserve is exactly `runtime_footprint_bytes`.
pub fn runtime_active_bytes(spec: &str, n_ctx: u32, weight_bytes: u64) -> u64 {
    let kv = resolve_model_dir(spec)
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|cfg| kv_bytes_per_position(&cfg))
        .map(|per| per.saturating_mul(n_ctx as u64))
        .unwrap_or(0);
    weight_bytes.saturating_add(kv)
}

/// The **process-shared** activation reserve (MLX buffer cache + prefill spike), counted ONCE
/// per process no matter how many models co-reside (see [`runtime_active_bytes`]). Public wrapper
/// over the calibrated [`activation_reserve_bytes`]. `max_weight_bytes` only matters for the
/// cache-DISABLED fallback (`ROZUM_MLX_CACHE_GB=0`), where the unbounded cache scales with the
/// largest resident's weights; pass the biggest co-resident weight (or `0` for the smallest,
/// always-safe floor). With the default cache cap the reserve is a weight-independent constant.
pub fn process_reserve_bytes(max_weight_bytes: u64) -> u64 {
    activation_reserve_bytes(max_weight_bytes)
}

/// **Adaptive loading**: the best model params that fit `available` RAM while keeping `min_free` free,
/// or `None` if the model can't fit even at the floor (its weights are too big for this host).
///
/// Returns `(n_ctx, cache_gib)` to load with. The policy maximizes the user-facing **context window**:
///  1. Keep the requested `req_n_ctx` if it fits — preferring the largest MLX cache cap (4 → 2 → 1 GiB).
///  2. If even `req_n_ctx` with a 1 GiB cache overflows, the host is tight: pin the cache at 1 GiB (most
///     room for KV) and reduce `n_ctx` to the largest multiple of 1024 that fits, down to `n_ctx_floor`.
///  3. If `n_ctx_floor` + 1 GiB cache still overflows → `None` (weights alone don't fit; refuse).
///
/// `weight_bytes` is the catalog/on-disk weights; KV-per-position is read from the model's `config.json`
/// (`spec` resolves the dir). This only shrinks — it never returns a larger n_ctx than requested. The
/// caller applies `cache_gib` (e.g. `ROZUM_MLX_CACHE_GB`) and `n_ctx` to BOTH the footprint estimate and
/// the actual load, so the residency gate still admits and never overcommits.
pub fn fit_model_params(
    spec: &str,
    weight_bytes: u64,
    req_n_ctx: u32,
    available: u64,
    min_free: u64,
    n_ctx_floor: u32,
) -> Option<(u32, u64)> {
    let kv_per_pos = resolve_model_dir(spec)
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|cfg| kv_bytes_per_position(&cfg))
        .unwrap_or(0);
    // Start from the operator's cache preference, else the size-scaled default (a small model gets a
    // smaller cap — see `default_cache_cap_gib`), and only ever shrink it to fit.
    let max_cache_gib = std::env::var("ROZUM_MLX_CACHE_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| default_cache_cap_gib(weight_bytes))
        .clamp(1, 8);
    fit_params_with_kv(weight_bytes, kv_per_pos, req_n_ctx, available, min_free, n_ctx_floor, max_cache_gib)
}

/// The pure fitting math (see [`fit_model_params`]) over an explicit `kv_per_pos` + cache ceiling —
/// unit-testable without a model on disk. The cache is tried from `max_cache_gib` down (halving) to
/// 1 GiB, so it never exceeds the caller's preference.
fn fit_params_with_kv(
    weight_bytes: u64,
    kv_per_pos: u64,
    req_n_ctx: u32,
    available: u64,
    min_free: u64,
    n_ctx_floor: u32,
    max_cache_gib: u64,
) -> Option<(u32, u64)> {
    const GB: u64 = 1 << 30;
    let budget = available.saturating_sub(min_free);
    let reserve = |cache_gib: u64| cache_gib.saturating_mul(GB).saturating_add(GB + GB / 2);
    let footprint = |n_ctx: u64, cache_gib: u64| {
        weight_bytes
            .saturating_add(kv_per_pos.saturating_mul(n_ctx))
            .saturating_add(reserve(cache_gib))
    };
    // 1. Requested context fits → keep it; prefer the largest cache (≤ the ceiling) that still fits.
    let mut cache_gib = max_cache_gib.max(1);
    loop {
        if footprint(req_n_ctx as u64, cache_gib) <= budget {
            return Some((req_n_ctx, cache_gib));
        }
        if cache_gib <= 1 {
            break;
        }
        cache_gib = (cache_gib / 2).max(1);
    }
    // 2. RAM-constrained: smallest cache (most KV room), shrink n_ctx to the largest 1024-multiple.
    if kv_per_pos == 0 {
        return None; // can't shrink without a KV term, and the request didn't fit
    }
    let room = budget.saturating_sub(weight_bytes.saturating_add(reserve(1)));
    let max_n_ctx = (room / kv_per_pos / 1024) * 1024;
    let n_ctx = max_n_ctx.min(req_n_ctx as u64);
    (n_ctx >= n_ctx_floor as u64).then_some((n_ctx as u32, 1))
}

#[cfg(test)]
mod tests {
    use super::{
        activation_reserve_bytes, config_model_type, default_cache_cap_gib, fit_params_with_kv,
        kv_bytes_per_position, process_reserve_bytes, resolve_model_dir, runtime_active_bytes,
        runtime_footprint_bytes, same_model, spec_to_hf_repo,
    };

    const GB: u64 = 1 << 30;

    // Adaptive loading: pick the best (largest n_ctx, then largest cache) params that fit available
    // RAM; refuse only if the weights themselves don't fit. KV = 128 KiB/token (so n_ctx 8192 ⇒ 1 GiB).
    #[test]
    fn fit_params_keeps_context_then_shrinks_cache_then_n_ctx() {
        const KV: u64 = 128 * 1024; // 128 KiB/token → 8192 ctx = exactly 1 GiB KV
        let w = 17 * GB; // weights
        let min_free = 3 * GB;
        let floor = 4096;
        // Roomy: 8192 ctx fits with the full 4 GiB cache.
        assert_eq!(fit_params_with_kv(w, KV, 8192, 28 * GB, min_free, floor, 4), Some((8192, 4)));
        // Tighter: 8192 fits only once the cache drops to 2 GiB (keeps full context).
        assert_eq!(fit_params_with_kv(w, KV, 8192, 26 * GB, min_free, floor, 4), Some((8192, 2)));
        // RAM-constrained: a 32768 request can't fit even at 1 GiB cache → shrink n_ctx to the largest
        // 1024-multiple, cache pinned at 1 GiB. 15 GiB weights, budget 19 GiB → room 1.5 GiB → 12288.
        assert_eq!(fit_params_with_kv(15 * GB, KV, 32768, 22 * GB, min_free, floor, 4), Some((12288, 1)));
        // Never returns MORE than requested.
        assert_eq!(fit_params_with_kv(w, KV, 4096, 28 * GB, min_free, floor, 4), Some((4096, 4)));
        // Weights too big for the host (even floor n_ctx + 1 GiB cache overflows) → refuse.
        assert_eq!(fit_params_with_kv(30 * GB, KV, 8192, 22 * GB, min_free, floor, 4), None);
        // Unknown KV (config unreadable): can only keep-or-refuse, never shrink n_ctx.
        assert_eq!(fit_params_with_kv(10 * GB, 0, 8192, 22 * GB, min_free, floor, 4), Some((8192, 4)));
        assert_eq!(fit_params_with_kv(30 * GB, 0, 8192, 22 * GB, min_free, floor, 4), None);
    }

    // The active/reserve split must reconstruct the original footprint exactly (no behavior
    // change for the single-model admission gate), and the reserve must be the process-shared
    // part so co-resident models count it ONCE. `spec` here is unresolvable ⇒ KV folds to 0, so
    // active == weight; the identity still holds whatever the KV term is.
    #[test]
    fn active_plus_reserve_equals_footprint() {
        let weight = 3 * GB;
        let spec = "definitely/not-a-real-model-xyzzy"; // unresolvable ⇒ KV = 0
        let active = runtime_active_bytes(spec, 8192, weight);
        let reserve = process_reserve_bytes(weight);
        assert_eq!(active.saturating_add(reserve), runtime_footprint_bytes(spec, 8192, weight));
        // Active is strictly the per-model part (weights + KV), below the full footprint.
        assert!(active < runtime_footprint_bytes(spec, 8192, weight));
        assert_eq!(active, weight); // KV unresolved ⇒ 0
        // process_reserve_bytes(0) is the smallest possible reserve (always-safe subtraction floor):
        // with the default cache cap the reserve is weight-independent, so it equals the full one.
        assert!(process_reserve_bytes(0) <= reserve);
    }

    #[test]
    fn spec_to_hf_repo_forms() {
        assert_eq!(spec_to_hf_repo("mlx-community:Qwen3-4B-4bit").as_deref(), Some("mlx-community/Qwen3-4B-4bit"));
        assert_eq!(spec_to_hf_repo("hf:org/model").as_deref(), Some("org/model"));
        assert_eq!(spec_to_hf_repo("org/model").as_deref(), Some("org/model"));
        // Non-HF specs are not repos.
        assert_eq!(spec_to_hf_repo("ollama:qwen3:8b"), None);
        assert_eq!(spec_to_hf_repo("/abs/path"), None);
    }

    #[test]
    fn cache_cap_scales_with_model_size() {
        // A 4B (~3 GiB weights) → floored at 2 GiB (not the flat 4) → ~2 GiB less reserved.
        assert_eq!(default_cache_cap_gib(3 * GB), 2);
        assert_eq!(default_cache_cap_gib(512 * 1024 * 1024), 2); // sub-GiB → floor 2
        assert_eq!(default_cache_cap_gib(6 * GB), 3); // mid → weights/2
        // Big models keep the full 4 GiB cap (capped).
        assert_eq!(default_cache_cap_gib(8 * GB), 4);
        assert_eq!(default_cache_cap_gib(20 * GB), 4);
    }

    #[test]
    fn same_model_tolerates_surface_form() {
        // The canonical catalog colon form matches the slash + hf: forms a user might pass.
        assert!(same_model("mlx-community:Qwen3.5-4B-MLX-4bit", "mlx-community/Qwen3.5-4B-MLX-4bit"));
        assert!(same_model("mlx-community:Qwen3.5-4B-MLX-4bit", "hf:mlx-community/Qwen3.5-4B-MLX-4bit"));
        assert!(same_model("mlx-community/M", "mlx-community:M"));
        // Distinct models don't collide; unrelated / unsizeable specs fall back to exact eq.
        assert!(!same_model("mlx-community:A", "mlx-community:B"));
        assert!(!same_model("mlx-community:A", "/some/raw/path"));
        assert!(same_model("/same/path", "/same/path"));
    }

    #[test]
    fn config_model_type_reads_top_level_and_text_config() {
        let top = serde_json::json!({ "model_type": "qwen3" });
        assert_eq!(config_model_type(&top), Some("qwen3"));
        // Multimodal wrapper (Qwen3.6) nests it under text_config.
        let wrapped = serde_json::json!({ "text_config": { "model_type": "qwen3_5_text" } });
        assert_eq!(config_model_type(&wrapped), Some("qwen3_5_text"));
        assert_eq!(config_model_type(&serde_json::json!({})), None);
    }

    #[test]
    fn resolve_missing_spec_is_none() {
        assert!(resolve_model_dir("definitely/not-a-real-model-xyzzy").is_none());
    }

    // KV bytes/position: only full-attention layers count (hybrid uses
    // full_attention_interval), head_dim derived from hidden/heads if absent.
    #[test]
    fn kv_bytes_per_position_estimate() {
        // Hybrid wrapper: 64 layers, interval 4 -> 16 full-attn; kv heads 4,
        // head_dim 256, bf16. 2*16*4*256*2 = 65536.
        let hybrid = serde_json::json!({
            "text_config": {
                "num_hidden_layers": 64, "full_attention_interval": 4,
                "num_key_value_heads": 4, "head_dim": 256
            }
        });
        assert_eq!(kv_bytes_per_position(&hybrid), Some(65_536));
        // Dense (no interval -> all 28 layers), head_dim from hidden/heads.
        let dense = serde_json::json!({
            "num_hidden_layers": 28, "num_key_value_heads": 8,
            "hidden_size": 4096, "num_attention_heads": 32
        });
        // head_dim = 4096/32 = 128; 2*28*8*128*2 = 114688.
        assert_eq!(kv_bytes_per_position(&dense), Some(114_688));
        // Missing fields -> None.
        assert_eq!(kv_bytes_per_position(&serde_json::json!({})), None);
    }

    #[test]
    fn activation_reserve_is_cache_tied_and_weight_independent() {
        // Reserve = set_cache_limit + 1.5 GiB prefill, INDEPENDENT of weights (the old
        // weights/4 over-reserved). Exact value asserted per the env state to stay
        // deterministic across runners.
        let small = activation_reserve_bytes(2 * GB);
        let big = activation_reserve_bytes(40 * GB);
        match std::env::var("ROZUM_MLX_CACHE_GB") {
            Ok(v) if v.trim() == "0" => {
                // Cache cap disabled → conservative weight-scaled fallback.
                assert_eq!(activation_reserve_bytes(40 * GB), 10 * GB); // 40/4
                assert_eq!(activation_reserve_bytes(2 * GB), 8 * GB); // floor
            }
            Ok(_) => assert_eq!(small, big, "weight-independent when cache is bounded"),
            Err(_) => {
                // Default cache 4 GiB → 4 + 1.5 = 5.5 GiB, weight-independent.
                assert_eq!(small, 5 * GB + GB / 2);
                assert_eq!(big, 5 * GB + GB / 2);
            }
        }
    }

    #[test]
    fn runtime_footprint_is_weights_plus_reserve_when_config_absent() {
        // An unknown spec has no config dir → KV folds to 0; footprint = weights + the
        // (cache-tied) reserve. Always ≥ weights, and strictly more (the model's real
        // resident NEED = active + the bounded cache, not just its on-disk size).
        let w = 2 * GB;
        let fp = runtime_footprint_bytes("definitely/not-a-real-model-xyzzy", 32_768, w);
        assert_eq!(fp, w + activation_reserve_bytes(w), "weights + reserve, KV=0 without config");
        assert!(fp > w);
    }

    #[test]
    fn runtime_footprint_grows_with_context_when_config_present() {
        // With a real cached model the KV term scales linearly with n_ctx, so a larger
        // context yields a strictly larger need. Skip when the model isn't cached here.
        let spec = "mlx-community:Qwen3-4B-4bit";
        if resolve_model_dir(spec).is_some() {
            let w = 2 * GB;
            let small = runtime_footprint_bytes(spec, 4_096, w);
            let large = runtime_footprint_bytes(spec, 40_960, w);
            assert!(large > small, "KV grows with n_ctx: {large} !> {small}");
            assert!(small >= w + activation_reserve_bytes(w), "≥ weights + reserve (+ KV)");
        }
    }
}
