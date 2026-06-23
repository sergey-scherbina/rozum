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
fn activation_reserve_bytes(weight_bytes: u64) -> u64 {
    const GB: u64 = 1 << 30;
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

#[cfg(test)]
mod tests {
    use super::{
        activation_reserve_bytes, config_model_type, kv_bytes_per_position, process_reserve_bytes,
        resolve_model_dir, runtime_active_bytes, runtime_footprint_bytes, spec_to_hf_repo,
    };

    const GB: u64 = 1 << 30;

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
