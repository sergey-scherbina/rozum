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

#[cfg(test)]
mod tests {
    use super::{config_model_type, kv_bytes_per_position, resolve_model_dir, spec_to_hf_repo};

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
}
