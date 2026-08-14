//! Minimal HuggingFace snapshot downloader — just `reqwest`, no `hf-hub` crate.
//!
//! Used by the native MLX backend to auto-fetch an MLX safetensors repo when it
//! isn't already cached. It writes the **exact `huggingface_hub` cache layout**
//! so the download is shared both ways with the rest of the HF ecosystem
//! (`mlx_lm`, `transformers`, `huggingface-cli`) — no double-downloads:
//!
//! ```text
//! ~/.cache/huggingface/hub/models--<org>--<name>/
//!   refs/main                       commit sha
//!   blobs/<oid|sha256>              file content, named by the file's etag
//!   snapshots/<commit>/<path>  ->   ../../blobs/<oid>   (symlink)
//! ```
//!
//! Blob names come from the repo tree API (`oid` for regular files, `lfs.oid` =
//! sha256 for LFS), which is exactly what `huggingface_hub` uses as the etag, so
//! an already-present blob (from either tool) is reused instead of re-fetched.
//! `config.json` is fetched first and passed to a caller gate, so an unsupported
//! repo is rejected before the multi-GB weights. `HF_TOKEN` is honored.

use std::path::{Path, PathBuf};

const HF_ENDPOINT: &str = "https://huggingface.co";

/// Files we keep for an MLX model: config + tokenizer + weights + chat template.
/// Skips READMEs, images, `.gitattributes`, GGUF, etc.
pub(crate) fn wanted(file: &str) -> bool {
    let f = file.rsplit('/').next().unwrap_or(file);
    f.ends_with(".safetensors")
        || f.ends_with(".json")
        || f.ends_with(".jinja")
        || f == "tokenizer.model"
        || f == "merges.txt"
        || f == "vocab.txt"
}

/// HF hub cache dir for `<org>/<name>` (mirrors `resolve_model_dir`).
fn model_cache_dir(org: &str, name: &str) -> Option<PathBuf> {
    Some(
        rozum_paths::home_dir()?
            .join(".cache")
            .join("huggingface")
            .join("hub")
            .join(format!("models--{org}--{name}")),
    )
}

fn with_auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// One repo file from the tree API: its path, its etag/blob name, and size.
struct RepoFile {
    path: String,
    blob: String,
    size: u64,
}

/// Shared context for the per-file fetch (keeps `fetch_one` within the arg budget).
struct DlCtx<'a> {
    client: &'a reqwest::Client,
    repo: &'a str,
    sha: &'a str,
    token: &'a Option<String>,
    blobs: &'a Path,
    snap: &'a Path,
}

/// Download `repo`'s MLX files into the HF cache and return the snapshot dir.
/// `config_gate` is called with the parsed `config.json` before any weights are
/// pulled; returning `Err` aborts the download (e.g. unsupported `model_type`).
pub async fn ensure_snapshot(
    repo: &str,
    config_gate: impl Fn(&serde_json::Value) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let (org, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("hf: not an org/name repo id: '{repo}'"))?;
    let client = reqwest::Client::builder()
        .user_agent("rozum-mlx")
        .build()
        .map_err(|e| format!("hf: client: {e}"))?;
    let token = std::env::var("HF_TOKEN").ok();

    // 1. Resolve the commit sha (the snapshot dir name + refs/main).
    let api = format!("{HF_ENDPOINT}/api/models/{repo}");
    let meta: serde_json::Value = with_auth(client.get(&api), &token)
        .send()
        .await
        .map_err(|e| format!("hf: query {repo}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("hf: query {repo}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("hf: parse {repo} metadata: {e}"))?;
    let sha = meta
        .get("sha")
        .and_then(|s| s.as_str())
        .ok_or_else(|| format!("hf: {repo} metadata has no commit sha"))?
        .to_owned();

    // 2. Tree → per-file blob name (etag): `lfs.oid` (sha256) or `oid` (git sha).
    let tree: serde_json::Value = with_auth(
        client.get(format!("{HF_ENDPOINT}/api/models/{repo}/tree/{sha}?recursive=true")),
        &token,
    )
    .send()
    .await
    .map_err(|e| format!("hf: tree {repo}: {e}"))?
    .error_for_status()
    .map_err(|e| format!("hf: tree {repo}: {e}"))?
    .json()
    .await
    .map_err(|e| format!("hf: parse {repo} tree: {e}"))?;
    let files: Vec<RepoFile> = tree
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("file"))
        .filter_map(|e| {
            let path = e.get("path")?.as_str()?.to_owned();
            if !wanted(&path) {
                return None;
            }
            // LFS files carry the sha256 under `lfs.oid`; the rest use git `oid`.
            let blob = e
                .get("lfs")
                .and_then(|l| l.get("oid"))
                .and_then(|o| o.as_str())
                .or_else(|| e.get("oid").and_then(|o| o.as_str()))?
                .to_owned();
            let size = e
                .get("lfs")
                .and_then(|l| l.get("size"))
                .or_else(|| e.get("size"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(RepoFile { path, blob, size })
        })
        .collect();
    let model_dir = model_cache_dir(org, name).ok_or_else(|| "hf: no HOME".to_owned())?;
    let blobs = model_dir.join("blobs");
    let snap = model_dir.join("snapshots").join(&sha);
    std::fs::create_dir_all(&blobs).map_err(|e| format!("hf: mkdir blobs: {e}"))?;
    std::fs::create_dir_all(&snap).map_err(|e| format!("hf: mkdir snapshot: {e}"))?;
    let ctx = DlCtx {
        client: &client,
        repo,
        sha: &sha,
        token: &token,
        blobs: &blobs,
        snap: &snap,
    };

    // 3. config.json first → reject unsupported repos before the weights.
    let cfg_file = files
        .iter()
        .find(|f| f.path == "config.json")
        .ok_or_else(|| format!("hf: {repo} has no config.json (not a model repo?)"))?;
    fetch_one(&ctx, cfg_file, "[config]").await?;
    let cfg_text = std::fs::read_to_string(snap.join("config.json"))
        .map_err(|e| format!("hf: read config.json: {e}"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&cfg_text).map_err(|e| format!("hf: parse config.json: {e}"))?;
    config_gate(&cfg)?;

    // 4. Everything else (weights, tokenizer, template).
    let rest: Vec<&RepoFile> = files.iter().filter(|f| f.path != "config.json").collect();
    let total = rest.len();
    eprintln!(
        "rozum mlx: downloading {repo} — {} file(s) → {}",
        total + 1,
        model_dir.display()
    );
    for (i, f) in rest.into_iter().enumerate() {
        let prefix = format!("[{}/{total}]", i + 1);
        fetch_one(&ctx, f, &prefix).await?;
    }

    // 5. refs/main — written last, so a partial cache is never advertised complete.
    let refs = model_dir.join("refs");
    std::fs::create_dir_all(&refs).map_err(|e| format!("hf: mkdir refs: {e}"))?;
    std::fs::write(refs.join("main"), &sha).map_err(|e| format!("hf: write refs/main: {e}"))?;
    Ok(snap)
}

/// Ensure `<blobs>/<file.blob>` exists (download if missing/short) and link
/// `<snap>/<file.path>` to it, reproducing the `huggingface_hub` layout. An
/// already-present blob (from a prior run or the `huggingface_hub` tool) is
/// reused, not re-fetched.
async fn fetch_one(ctx: &DlCtx<'_>, file: &RepoFile, prefix: &str) -> Result<(), String> {
    let blob = ctx.blobs.join(&file.blob);
    let have = std::fs::metadata(&blob)
        .map(|m| file.size == 0 || m.len() == file.size)
        .unwrap_or(false);
    if have {
        eprintln!("  {prefix} {}  cached", file.path);
    } else {
        let url = format!("{HF_ENDPOINT}/{}/resolve/{}/{}", ctx.repo, ctx.sha, file.path);
        stream_download(ctx.client, &url, &blob, file.size, ctx.token, prefix, &file.path).await?;
    }
    symlink_into_snapshot(ctx.snap, &file.path, &file.blob)
}

/// Symlink `<snap>/<path>` → relative `../…/blobs/<blob>` (extra `..` per nested
/// dir in `path`), matching `huggingface_hub`. Falls back to a copy off-unix.
fn symlink_into_snapshot(snap: &Path, path: &str, blob: &str) -> Result<(), String> {
    let link = snap.join(path);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("hf: mkdir {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&link); // replace a stale link
    #[cfg(unix)]
    {
        let ups = 2 + path.matches('/').count();
        let target = format!("{}blobs/{blob}", "../".repeat(ups));
        std::os::unix::fs::symlink(&target, &link)
            .map_err(|e| format!("hf: symlink {}: {e}", link.display()))
    }
    #[cfg(not(unix))]
    {
        let blob_abs = snap.join("..").join("..").join("blobs").join(blob);
        std::fs::copy(&blob_abs, &link)
            .map(|_| ())
            .map_err(|e| format!("hf: copy {}: {e}", link.display()))
    }
}

/// Human-readable byte size (MiB/GiB), for progress lines.
pub(crate) fn fmt_bytes(n: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = n as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

/// Stream a URL to `dest` (atomic: `.part` + rename) with a live progress line
/// (`<prefix> <label>  <done>/<total> (NN%)`), throttled to ~4 MiB. Shared by the
/// HF and ModelScope downloaders.
pub(crate) async fn stream_download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected: u64,
    token: &Option<String>,
    prefix: &str,
    label: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    use futures::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("hf: mkdir {}: {e}", parent.display()))?;
    }
    let resp = with_auth(client.get(url), token)
        .send()
        .await
        .map_err(|e| format!("hf: GET {label}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("hf: GET {label}: {e}"))?;
    let total = if expected > 0 { Some(expected) } else { resp.content_length() };

    let tmp = dest.with_extension("part");
    let mut out = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("hf: create {}: {e}", tmp.display()))?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_tick: u64 = 0;
    let render = |done: u64| {
        let body = match total {
            Some(t) if t > 0 => format!(
                "{}/{} ({}%)",
                fmt_bytes(done),
                fmt_bytes(t),
                done.saturating_mul(100) / t
            ),
            _ => fmt_bytes(done),
        };
        eprint!("\r  {prefix} {label}  {body}\x1b[K");
        let _ = std::io::stderr().flush();
    };
    render(0);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("hf: download {label}: {e}"))?;
        out.write_all(&chunk)
            .await
            .map_err(|e| format!("hf: write {label}: {e}"))?;
        done += chunk.len() as u64;
        if done - last_tick >= 4 * 1024 * 1024 {
            last_tick = done;
            render(done);
        }
    }
    out.flush().await.map_err(|e| format!("hf: flush {label}: {e}"))?;
    drop(out);
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| format!("hf: finalize {label}: {e}"))?;
    render(done);
    eprintln!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_keeps_model_files_skips_noise() {
        for f in ["config.json", "model.safetensors", "model.safetensors.index.json",
                  "tokenizer.json", "tokenizer_config.json", "chat_template.jinja",
                  "tokenizer.model", "merges.txt"] {
            assert!(wanted(f), "{f} should be kept");
        }
        for f in ["README.md", ".gitattributes", "model.gguf", "logo.png", "LICENSE"] {
            assert!(!wanted(f), "{f} should be skipped");
        }
    }

    // Network smoke test: queries a real repo + downloads only config.json, then the
    // gate rejects it — so no multi-GB weights are pulled. Run:
    //   cargo test --release hf_hub:: -- --ignored --nocapture config_first_gate
    #[tokio::test]
    #[ignore = "network: hits huggingface.co"]
    async fn config_first_gate_rejects_before_weights() {
        let err = ensure_snapshot("Qwen/Qwen2.5-0.5B-Instruct", |_cfg| {
            Err("nope (test gate)".to_owned())
        })
        .await
        .unwrap_err();
        assert!(err.contains("nope"), "expected the gate error, got: {err}");
    }

    // Full download of a small real MLX model: verifies the proper hf_hub layout —
    // blobs + snapshot symlinks + refs/main. Pulls ~0.4 GB. Run:
    //   cargo test --release hf_hub:: -- --ignored --nocapture full_download
    #[tokio::test]
    #[ignore = "network: downloads ~0.4 GB from huggingface.co"]
    async fn full_download_writes_proper_hf_layout() {
        let snap = ensure_snapshot("mlx-community/Qwen3-0.6B-4bit", |_cfg| Ok(()))
            .await
            .expect("download");
        // config.json resolves through a symlink to a blob.
        let cfg = snap.join("config.json");
        assert!(cfg.is_file(), "config.json missing");
        assert!(
            std::fs::symlink_metadata(&cfg).unwrap().file_type().is_symlink(),
            "config.json should be a symlink into blobs/"
        );
        // model_dir = snapshots/<sha>/.. /.. ; refs/main + blobs/ exist.
        let model_dir = snap.parent().unwrap().parent().unwrap();
        assert!(model_dir.join("refs/main").is_file(), "refs/main missing");
        assert!(model_dir.join("blobs").is_dir(), "blobs/ missing");
        assert!(
            std::fs::read_dir(&snap)
                .unwrap()
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "safetensors")),
            "no safetensors symlink in snapshot"
        );
    }
}
