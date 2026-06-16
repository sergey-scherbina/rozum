/// Model discovery and inspection.
///
/// Powers `rozum list`, `rozum list --remote`, and `rozum info <spec>`.
/// Looks at three local caches without requiring the corresponding runtime
/// (Ollama / LMStudio / hf-hub) to be running:
///
/// - HuggingFace hub cache: `~/.cache/huggingface/hub/models--<owner>--<name>/`
/// - Ollama blobs:          `~/.ollama/models/manifests/.../<tag>` → `blobs/sha256-...`
/// - LMStudio downloads:    `~/.cache/lm-studio/models/<owner>/<repo>/*.gguf`
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub enum ModelSource {
    HuggingFace,
    Ollama,
    LMStudio,
}

impl ModelSource {
    pub fn label(self) -> &'static str {
        match self {
            ModelSource::HuggingFace => "hf",
            ModelSource::Ollama => "ollama",
            ModelSource::LMStudio => "lmstudio",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstalledModel {
    pub source: ModelSource,
    /// Spec the user would pass to `--model`.
    pub spec: String,
    /// Filesystem path of the weights (or directory).
    pub path: PathBuf,
    /// Total bytes on disk.
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RecommendedModel {
    pub spec: &'static str,
    pub display_name: &'static str,
    pub approx_size_gb: f32,
    pub notes: &'static str,
}

/// Hand-picked list optimised for Apple Silicon 24-36 GB unified memory, ordered for
/// **agentic coding** (Claude Code / Codex via `rozum launch`). The agentic benchmark
/// (`scripts/bench/agentic.sh`) found a sharp **7B→27B capability cliff**: below ~27B
/// (or a strong MoE) local quantized models can't reliably drive the multi-step tool
/// loop. Qwen3/Qwen3.6 emit the native `<tool_call>` format (always valid); Qwen2.5/Coder
/// need the gateway's JSON-repair. MoE (30B-A3B) is the best speed/capability/memory
/// balance. Update when stronger `mlx-community/` models appear.
pub const RECOMMENDED: &[RecommendedModel] = &[
    RecommendedModel {
        spec: "mlx-community:Qwen3-30B-A3B-4bit",
        display_name: "Qwen3 30B-A3B (MoE)",
        approx_size_gb: 16.0,
        notes: "BEST for local agentic coding. MoE ~3B active/token → ~100 t/s + strong; \
                completes tasks cleanly. The speed/capability/memory sweet spot.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen3.6-27B-4bit",
        display_name: "Qwen3.6 27B (dense)",
        approx_size_gb: 15.0,
        notes: "Most capable for agentic coding (5/5 in the matrix) and fits agentic use on a \
                36 GB Mac. DENSE → slower (~15 t/s) than the MoEs; prefer the 30B-A3B MoE for \
                interactive speed, this when you want the strongest single-shot result.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen3.6-35B-A3B-4bit",
        display_name: "Qwen3.6 35B-A3B (MoE)",
        approx_size_gb: 17.0,
        notes: "Strongest/fastest MoE, but HEAVY for agentic on 36 GB: peaks ~25 GB and the \
                gateway can OOM on a big agent prompt (the single-shot prefill spike hits the \
                MLX memory cap) → the worker dies mid-run. Use for chat / on ≥48 GB, or wait \
                for chunked prefill. Lower `ROZUM_MLX_CACHE_GB` to free headroom.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen3.6-35B-A3B-4bit-DWQ",
        display_name: "Qwen3.6 35B-A3B DWQ",
        approx_size_gb: 17.0,
        notes: "Distillation-Weighted Quant — same MoE, slightly better quality. Same ~25 GB / \
                OOM caveat as the 35B above on a 36 GB Mac.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen2.5-Coder-32B-Instruct-4bit",
        display_name: "Qwen2.5-Coder 32B",
        approx_size_gb: 19.0,
        notes: "Strong dense coder, native tool-use. Heavy (needs ~24 GB); dense → slower.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen2.5-Coder-7B-Instruct-4bit",
        display_name: "Qwen2.5-Coder 7B",
        approx_size_gb: 4.3,
        notes: "Smallest model that does agentic coding (4/5 build/fix/test/debug) — but only \
                with the gateway's loose-JSON repair; fits 16 GB. Below this, agentic fails.",
    },
    RecommendedModel {
        spec: "mlx-community:Qwen3-4B-4bit",
        display_name: "Qwen3 4B",
        approx_size_gb: 2.5,
        notes: "Punches above its weight for agentic with Claude Code (4/5 in the matrix, native \
                <tool_call> + the gateway's loop-breaker/termination fixes) at a ~360 MB agent \
                footprint. Great fast cascade start-tier / chat. Fits anywhere.",
    },
];

/// A hosted (cloud) model offered in the launch picker. Selecting one (or mixing it into a
/// multi-select) routes through the OpenAI/Anthropic HTTP backends; no local download. Uses the
/// provider's API key from the environment (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`).
#[derive(Debug, Clone, Copy)]
pub struct RemoteModel {
    pub spec: &'static str,
    pub provider: &'static str,
    pub display_name: &'static str,
}

/// Common hosted models (Anthropic + OpenAI), cheapest/fastest first within each provider. Pick any
/// in the launch picker; multi-select several (local and/or cloud) to form a cascade. You can also
/// pass an exact model id directly (e.g. `--model "qwen3-4b,gpt-4o"`), so this list need not be
/// exhaustive.
pub const RECOMMENDED_REMOTE: &[RemoteModel] = &[
    RemoteModel {
        spec: "claude-haiku-4-5",
        provider: "Anthropic",
        display_name: "Claude Haiku 4.5 — fast, cheap",
    },
    RemoteModel {
        spec: "claude-sonnet-4-6",
        provider: "Anthropic",
        display_name: "Claude Sonnet 4.6 — balanced",
    },
    RemoteModel {
        spec: "claude-opus-4-8",
        provider: "Anthropic",
        display_name: "Claude Opus 4.8 — most capable",
    },
    RemoteModel {
        spec: "gpt-4o-mini",
        provider: "OpenAI",
        display_name: "GPT-4o mini — fast, cheap",
    },
    RemoteModel { spec: "gpt-4o", provider: "OpenAI", display_name: "GPT-4o — balanced" },
    RemoteModel {
        spec: "o3-mini",
        provider: "OpenAI",
        display_name: "o3-mini — reasoning, economical",
    },
    RemoteModel { spec: "o1", provider: "OpenAI", display_name: "o1 — deep reasoning" },
];

// ─── Scanners ────────────────────────────────────────────────────────────────

pub fn scan_all_installed() -> Vec<InstalledModel> {
    let mut models = Vec::new();
    models.extend(scan_huggingface());
    models.extend(scan_ollama());
    models.extend(scan_lmstudio());
    models.sort_by(|a, b| a.spec.cmp(&b.spec));
    models
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&p);
        } else if meta.is_file() {
            total += meta.len();
        } else if meta.file_type().is_symlink() {
            // hf-hub snapshots are symlinks to blobs/; resolve.
            if let Ok(target_meta) = std::fs::metadata(&p) {
                if target_meta.is_file() {
                    total += target_meta.len();
                }
            }
        }
    }
    total
}

pub fn scan_huggingface() -> Vec<InstalledModel> {
    let hub = std::env::var_os("HF_HUB_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HF_HOME")
                .map(|h| PathBuf::from(h).join("hub"))
                .unwrap_or_else(|| home().join(".cache").join("huggingface").join("hub"))
        });
    let Ok(entries) = std::fs::read_dir(&hub) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("models--") else {
            continue;
        };
        let parts: Vec<&str> = rest.splitn(2, "--").collect();
        if parts.len() != 2 {
            continue;
        }
        let path = entry.path();
        // Only count the blobs/ directory (snapshots/ are symlinks to it).
        let size = dir_size(&path.join("blobs"));
        let owner = parts[0];
        let name = parts[1];
        let spec = if owner == "mlx-community" {
            format!("mlx-community:{name}")
        } else {
            format!("hf:{owner}/{name}")
        };
        out.push(InstalledModel {
            source: ModelSource::HuggingFace,
            spec,
            path,
            size_bytes: size,
        });
    }
    out
}

pub fn scan_ollama() -> Vec<InstalledModel> {
    let base = std::env::var_os("ROZUM_OLLAMA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".ollama"))
        .join("models");
    let manifests = base
        .join("manifests")
        .join("registry.ollama.ai")
        .join("library");
    let Ok(name_dirs) = std::fs::read_dir(&manifests) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name_entry in name_dirs.flatten() {
        let name = name_entry.file_name().to_string_lossy().to_string();
        let Ok(tag_files) = std::fs::read_dir(name_entry.path()) else {
            continue;
        };
        for tag_entry in tag_files.flatten() {
            let tag = tag_entry.file_name().to_string_lossy().to_string();
            let Ok(text) = std::fs::read_to_string(tag_entry.path()) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let layers = manifest["layers"].as_array().cloned().unwrap_or_default();

            // Case 1: monolithic GGUF model — one layer with mediaType "...image.model".
            // Path points at the blob; rozum can use it via the gguf backend.
            if let Some(digest) = layers.iter().find_map(|layer| {
                if layer["mediaType"].as_str() == Some("application/vnd.ollama.image.model") {
                    layer["digest"].as_str().map(str::to_owned)
                } else {
                    None
                }
            }) {
                let blob = base.join("blobs").join(digest.replace(':', "-"));
                let size = std::fs::metadata(&blob).map(|m| m.len()).unwrap_or(0);
                out.push(InstalledModel {
                    source: ModelSource::Ollama,
                    spec: format!("ollama:{name}:{tag}"),
                    path: blob,
                    size_bytes: size,
                });
                continue;
            }

            // Case 2: MLX model — many "image.tensor" layers, one per tensor.
            // Sum sizes for display. The path is the manifest itself since the
            // tensors are scattered across blobs/. Not directly loadable by our
            // GGUF backend; shown for awareness.
            let tensor_total: u64 = layers
                .iter()
                .filter(|l| l["mediaType"].as_str() == Some("application/vnd.ollama.image.tensor"))
                .filter_map(|l| l["size"].as_u64())
                .sum();
            if tensor_total > 0 {
                out.push(InstalledModel {
                    source: ModelSource::Ollama,
                    spec: format!("ollama:{name}:{tag}"),
                    path: tag_entry.path(),
                    size_bytes: tensor_total,
                });
            }
        }
    }
    out
}

pub fn scan_lmstudio() -> Vec<InstalledModel> {
    let root = std::env::var_os("ROZUM_LMSTUDIO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache").join("lm-studio"))
        .join("models");
    let mut out = Vec::new();
    walk_for_gguf(&root, &root, &mut out);
    out
}

fn walk_for_gguf(root: &Path, dir: &Path, out: &mut Vec<InstalledModel>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_for_gguf(root, &p, out);
        } else if p.extension().map_or(false, |e| e == "gguf") {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            // Spec = lmstudio:<repo-relative-to-root>, without the file name.
            let repo = p
                .parent()
                .and_then(|d| d.strip_prefix(root).ok())
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push(InstalledModel {
                source: ModelSource::LMStudio,
                spec: format!("lmstudio:{repo}"),
                path: p,
                size_bytes: size,
            });
        }
    }
}

// ─── HuggingFace API client (read-only metadata) ─────────────────────────────

#[derive(Debug, Clone)]
pub struct HfModelInfo {
    pub id: String,
    pub author: Option<String>,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub pipeline_tag: Option<String>,
    pub tags: Vec<String>,
    pub license: Option<String>,
    pub files: Vec<HfFile>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct HfFile {
    pub name: String,
    pub size: u64,
}

/// Resolve a model spec used in rozum CLI to a HuggingFace repo id.
pub fn spec_to_hf_id(spec: &str) -> Option<String> {
    if let Some(repo) = spec.strip_prefix("hf:") {
        Some(repo.to_owned())
    } else if let Some(repo) = spec.strip_prefix("mlx-community:") {
        Some(format!("mlx-community/{repo}"))
    } else if spec.contains('/') && !spec.starts_with('/') {
        // Bare "owner/repo" form
        Some(spec.to_owned())
    } else {
        None
    }
}

pub async fn fetch_hf_info(repo: &str) -> Result<HfModelInfo, String> {
    let url = format!("https://huggingface.co/api/models/{repo}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HF API returned {}: {repo}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let id = v["id"].as_str().unwrap_or(repo).to_owned();
    let author = v["author"].as_str().map(str::to_owned);
    let downloads = v["downloads"].as_u64();
    let likes = v["likes"].as_u64();
    let pipeline_tag = v["pipeline_tag"].as_str().map(str::to_owned);
    let tags: Vec<String> = v["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let license = v["cardData"]["license"]
        .as_str()
        .or_else(|| v["license"].as_str())
        .map(str::to_owned);

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    if let Some(siblings) = v["siblings"].as_array() {
        for s in siblings {
            let name = s["rfilename"].as_str().unwrap_or("").to_owned();
            let size = s["size"].as_u64().unwrap_or(0);
            if size > 0 {
                total_bytes += size;
            }
            files.push(HfFile { name, size });
        }
    }
    // If sibling sizes are missing, fall back to the tree API to get them.
    if total_bytes == 0 {
        let tree_url = format!("https://huggingface.co/api/models/{repo}/tree/main");
        if let Ok(tree_resp) = client.get(&tree_url).send().await {
            if tree_resp.status().is_success() {
                if let Ok(tree) = tree_resp.json::<serde_json::Value>().await {
                    if let Some(entries) = tree.as_array() {
                        files.clear();
                        for e in entries {
                            let name = e["path"].as_str().unwrap_or("").to_owned();
                            let size = e["size"].as_u64().unwrap_or(0);
                            total_bytes += size;
                            files.push(HfFile { name, size });
                        }
                    }
                }
            }
        }
    }

    Ok(HfModelInfo {
        id,
        author,
        downloads,
        likes,
        pipeline_tag,
        tags,
        license,
        files,
        total_bytes,
    })
}

// ─── Pretty formatting ───────────────────────────────────────────────────────

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000 * KB;
    const GB: u64 = 1_000 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_to_hf_id_variants() {
        assert_eq!(
            spec_to_hf_id("hf:Qwen/Qwen3-4B").as_deref(),
            Some("Qwen/Qwen3-4B")
        );
        assert_eq!(
            spec_to_hf_id("mlx-community:Qwen3.6-35B-A3B-4bit").as_deref(),
            Some("mlx-community/Qwen3.6-35B-A3B-4bit")
        );
        assert_eq!(
            spec_to_hf_id("Qwen/Qwen3-4B").as_deref(),
            Some("Qwen/Qwen3-4B")
        );
        assert_eq!(spec_to_hf_id("/abs/path").as_deref(), None);
        assert_eq!(spec_to_hf_id("ollama-bare-tag").as_deref(), None);
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(50_000), "50 KB");
        assert_eq!(format_size(5_000_000), "5.0 MB");
        assert_eq!(format_size(17_000_000_000), "17.00 GB");
    }
}
