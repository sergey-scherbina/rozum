//! Minimal ModelScope (modelscope.cn) snapshot downloader — just `reqwest`.
//!
//! ModelScope is the other hub that carries MLX safetensors (popular for Qwen in
//! CN). Native MLX auto-fetches from it for `modelscope:<owner>/<name>` specs.
//! Files land in ModelScope's own cache layout so its Python tool reuses them:
//!
//! ```text
//! $MODELSCOPE_CACHE/hub/<owner>/<name>/<path>   (flat files, mirroring the repo)
//! ```
//!
//! (default `MODELSCOPE_CACHE` = `~/.cache/modelscope`). ModelScope's newer
//! sidecar metadata may differ across versions, so reuse by the `modelscope`
//! library is best-effort; loading by rozum is exact. `config.json` is fetched
//! first and passed to the caller gate. `MODELSCOPE_API_TOKEN` is honored.

use std::path::PathBuf;

use crate::hf_hub::{stream_download, wanted};

const MS_ENDPOINT: &str = "https://www.modelscope.cn";
const MS_REVISION: &str = "master";

/// ModelScope cache dir for `<owner>/<name>` (mirrors `modelscope`'s layout).
pub fn model_cache_dir(owner: &str, name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("MODELSCOPE_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/modelscope")))?;
    Some(root.join("hub").join(owner).join(name))
}

fn with_auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        // ModelScope reads a cookie/header token; the bearer form works for the
        // open API and is ignored for public repos.
        Some(t) => req.header("Authorization", format!("Bearer {t}")),
        None => req,
    }
}

struct RepoFile {
    path: String,
    size: u64,
}

/// Download `repo` (`<owner>/<name>`) from ModelScope into its cache, returning
/// the model dir. `config_gate` runs on `config.json` before the weights.
pub async fn ensure_snapshot(
    repo: &str,
    config_gate: impl Fn(&serde_json::Value) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("modelscope: not an owner/name repo id: '{repo}'"))?;
    let client = reqwest::Client::builder()
        .user_agent("rozum-mlx")
        .build()
        .map_err(|e| format!("modelscope: client: {e}"))?;
    let token = std::env::var("MODELSCOPE_API_TOKEN").ok();

    // 1. List files (recursive).
    let api = format!(
        "{MS_ENDPOINT}/api/v1/models/{repo}/repo/files?Revision={MS_REVISION}&Recursive=true"
    );
    let meta: serde_json::Value = with_auth(client.get(&api), &token)
        .send()
        .await
        .map_err(|e| format!("modelscope: list {repo}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("modelscope: list {repo}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("modelscope: parse {repo}: {e}"))?;
    let files: Vec<RepoFile> = meta
        .get("Data")
        .and_then(|d| d.get("Files"))
        .and_then(|f| f.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|e| e.get("Type").and_then(|t| t.as_str()) == Some("blob"))
        .filter_map(|e| {
            let path = e.get("Path")?.as_str()?.to_owned();
            if !wanted(&path) {
                return None;
            }
            let size = e.get("Size").and_then(serde_json::Value::as_u64).unwrap_or(0);
            Some(RepoFile { path, size })
        })
        .collect();
    if !files.iter().any(|f| f.path == "config.json") {
        return Err(format!("modelscope: {repo} has no config.json"));
    }

    let dir = model_cache_dir(owner, name).ok_or_else(|| "modelscope: no cache dir".to_owned())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("modelscope: mkdir {}: {e}", dir.display()))?;

    // 2. config.json first → gate before the weights.
    download(&client, repo, "config.json", 0, &dir, &token, "[config]").await?;
    let cfg_text = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|e| format!("modelscope: read config.json: {e}"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&cfg_text).map_err(|e| format!("modelscope: parse config.json: {e}"))?;
    config_gate(&cfg)?;

    // 3. The rest.
    let rest: Vec<&RepoFile> = files.iter().filter(|f| f.path != "config.json").collect();
    let total = rest.len();
    eprintln!(
        "rozum mlx: downloading {repo} from ModelScope — {} file(s) → {}",
        total + 1,
        dir.display()
    );
    for (i, f) in rest.into_iter().enumerate() {
        let prefix = format!("[{}/{total}]", i + 1);
        download(&client, repo, &f.path, f.size, &dir, &token, &prefix).await?;
    }
    Ok(dir)
}

/// Fetch one repo file to `<dir>/<path>` (flat real file — ModelScope's layout).
async fn download(
    client: &reqwest::Client,
    repo: &str,
    path: &str,
    size: u64,
    dir: &std::path::Path,
    token: &Option<String>,
    prefix: &str,
) -> Result<(), String> {
    let dest = dir.join(path);
    // Skip an already-complete file (idempotent re-runs; shared with `modelscope`).
    if std::fs::metadata(&dest).map(|m| size != 0 && m.len() == size).unwrap_or(false) {
        eprintln!("  {prefix} {path}  cached");
        return Ok(());
    }
    let url =
        format!("{MS_ENDPOINT}/api/v1/models/{repo}/repo?Revision={MS_REVISION}&FilePath={path}");
    stream_download(client, &url, &dest, size, token, prefix, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_under_modelscope_root() {
        // SAFETY: single-threaded test; sets + restores the env.
        unsafe { std::env::set_var("MODELSCOPE_CACHE", "/tmp/ms_test_root") };
        let d = model_cache_dir("Qwen", "Qwen2.5-0.5B-Instruct").unwrap();
        assert_eq!(d, PathBuf::from("/tmp/ms_test_root/hub/Qwen/Qwen2.5-0.5B-Instruct"));
        unsafe { std::env::remove_var("MODELSCOPE_CACHE") };
    }

    // Network smoke test: lists a real ModelScope repo + downloads config.json,
    // then the gate rejects it (no weights pulled). Run:
    //   cargo test --release modelscope:: -- --ignored --nocapture config_first_gate
    #[tokio::test]
    #[ignore = "network: hits modelscope.cn"]
    async fn config_first_gate_rejects_before_weights() {
        let err = ensure_snapshot("Qwen/Qwen2.5-0.5B-Instruct", |_cfg| {
            Err("nope (test gate)".to_owned())
        })
        .await
        .unwrap_err();
        assert!(err.contains("nope"), "expected the gate error, got: {err}");
    }
}
