//! Minimal HuggingFace snapshot downloader — just `reqwest`, no `hf-hub` crate.
//!
//! Used by the native MLX backend to auto-fetch an MLX safetensors repo when it
//! isn't already in the HF cache. Files land in the standard cache layout
//! (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<sha>/`) so
//! `resolve_model_dir` finds them on the next run, and a later `hf-hub`/`mlx_lm`
//! run reuses the same snapshot. `config.json` is fetched first and passed to a
//! caller gate, so an unsupported repo is rejected before the multi-GB weights.

use std::path::PathBuf;

const HF_ENDPOINT: &str = "https://huggingface.co";

/// Files we keep for an MLX model: config + tokenizer + weights + chat template.
/// Skips READMEs, images, `.gitattributes`, GGUF, etc.
fn wanted(file: &str) -> bool {
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
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache/huggingface/hub")
            .join(format!("models--{org}--{name}")),
    )
}

fn with_auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// Download `repo`'s MLX files into the HF cache and return the snapshot dir.
/// `config_gate` is called with the parsed `config.json` before any weights are
/// pulled; returning `Err` aborts the download (e.g. unsupported `model_type`).
/// `HF_TOKEN` is honored for gated repos.
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

    // 1. List files + resolve the commit sha (the snapshot dir name).
    let api = format!("{HF_ENDPOINT}/api/models/{repo}");
    let meta: serde_json::Value = with_auth(client.get(&api), &token)
        .send()
        .await
        .map_err(|e| format!("hf: list {repo}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("hf: list {repo}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("hf: parse {repo} metadata: {e}"))?;
    let sha = meta
        .get("sha")
        .and_then(|s| s.as_str())
        .unwrap_or("main")
        .to_owned();
    let files: Vec<String> = meta
        .get("siblings")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("rfilename").and_then(|r| r.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if !files.iter().any(|f| f == "config.json") {
        return Err(format!("hf: {repo} has no config.json (not a model repo?)"));
    }

    let snap = model_cache_dir(org, name)
        .ok_or_else(|| "hf: no HOME for cache dir".to_owned())?
        .join("snapshots")
        .join(&sha);
    std::fs::create_dir_all(&snap).map_err(|e| format!("hf: mkdir {}: {e}", snap.display()))?;

    // 2. config.json first → let the caller reject unsupported repos cheaply.
    download_file(&client, repo, &sha, "config.json", &snap, &token, "[config]").await?;
    let cfg_text = std::fs::read_to_string(snap.join("config.json"))
        .map_err(|e| format!("hf: read config.json: {e}"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&cfg_text).map_err(|e| format!("hf: parse config.json: {e}"))?;
    config_gate(&cfg)?;

    // 3. Everything else (weights, tokenizer, template).
    let rest: Vec<&String> = files
        .iter()
        .filter(|f| wanted(f) && f.as_str() != "config.json")
        .collect();
    let total = rest.len();
    eprintln!(
        "rozum mlx: downloading {repo} — {total} file(s) → {}",
        snap.display()
    );
    for (i, f) in rest.into_iter().enumerate() {
        let prefix = format!("[{}/{total}]", i + 1);
        download_file(&client, repo, &sha, f, &snap, &token, &prefix).await?;
    }
    Ok(snap)
}

/// Human-readable byte size (MiB/GiB), for progress lines.
fn fmt_bytes(n: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = n as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

/// Stream one repo file to `<snap>/<file>` (atomic: temp + rename), printing a
/// live progress line on stderr (`<prefix> <file>  <done>/<total> (NN%)`),
/// throttled to whole-percent / ~4 MiB steps. Nested paths create parent dirs.
async fn download_file(
    client: &reqwest::Client,
    repo: &str,
    sha: &str,
    file: &str,
    snap: &std::path::Path,
    token: &Option<String>,
    prefix: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    use tokio::io::AsyncWriteExt as _;

    let dest = snap.join(file);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("hf: mkdir {}: {e}", parent.display()))?;
    }
    let url = format!("{HF_ENDPOINT}/{repo}/resolve/{sha}/{file}");
    let resp = with_auth(client.get(&url), token)
        .send()
        .await
        .map_err(|e| format!("hf: GET {file}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("hf: GET {file}: {e}"))?;
    let total = resp.content_length();

    let tmp = dest.with_extension("part");
    let mut out = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("hf: create {}: {e}", tmp.display()))?;
    let mut stream = resp.bytes_stream();
    use futures::StreamExt as _;
    let mut done: u64 = 0;
    let mut last_tick: u64 = 0; // bytes at last redraw
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
        // `\r` + clear-to-EOL so a shorter line doesn't leave stale chars.
        eprint!("\r  {prefix} {file}  {body}\x1b[K");
        let _ = std::io::stderr().flush();
    };
    render(0);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("hf: download {file}: {e}"))?;
        out.write_all(&chunk)
            .await
            .map_err(|e| format!("hf: write {file}: {e}"))?;
        done += chunk.len() as u64;
        // Redraw at most every ~4 MiB to avoid flooding the terminal.
        if done - last_tick >= 4 * 1024 * 1024 {
            last_tick = done;
            render(done);
        }
    }
    out.flush().await.map_err(|e| format!("hf: flush {file}: {e}"))?;
    drop(out);
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| format!("hf: finalize {file}: {e}"))?;
    render(done);
    eprintln!(); // finish the progress line
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

    // Network smoke test: lists a real repo + downloads only config.json, then the
    // gate rejects it — so no multi-GB weights are pulled. Validates the HF API
    // listing, the config-first download, and the gate. Run:
    //   cargo test --release hf_hub:: -- --ignored --nocapture config_first_gate
    #[tokio::test]
    #[ignore = "network: hits huggingface.co"]
    async fn config_first_gate_rejects_before_weights() {
        // Bare `org/name` (production maps the spec via `spec_to_hf_repo` first).
        let err = ensure_snapshot("Qwen/Qwen2.5-0.5B-Instruct", |_cfg| {
            Err("nope (test gate)".to_owned())
        })
        .await
        .unwrap_err();
        assert!(err.contains("nope"), "expected the gate error, got: {err}");
    }

    // Full download of a small real MLX model: exercises the weights loop + the
    // GiB-scale progress line. Pulls ~0.4 GB. Run:
    //   cargo test --release hf_hub:: -- --ignored --nocapture full_download
    #[tokio::test]
    #[ignore = "network: downloads ~0.4 GB from huggingface.co"]
    async fn full_download_fetches_a_loadable_snapshot() {
        let dir = ensure_snapshot("mlx-community/Qwen3-0.6B-4bit", |_cfg| Ok(()))
            .await
            .expect("download");
        assert!(dir.join("config.json").is_file());
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "safetensors")),
            "no safetensors in {}",
            dir.display()
        );
    }
}
