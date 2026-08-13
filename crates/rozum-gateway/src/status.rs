//! The machine snapshot: what is running, what fits, what is installed.
//!
//! Split out of `control.rs` on 2026-08-09 so it survives without the control PLANE. The two had
//! been one file since the console was written, and it cost portability: `control.rs` needs
//! `webauthn-rs` for Face ID, `webauthn-rs` needs OpenSSL, and OpenSSL is what stops the whole
//! binary building on Windows — for a machine that may only ever want the model server. Everything
//! here is pure reading (`share`, the model catalog, the footprint cache) with no HTTP, no auth and
//! no dependency the engine does not already carry.
//!
//! `rozum-gateway status --json` and `doctor` read this; the console serves it over HTTP when the
//! `ucc` feature is on.

use serde::Serialize;

/// True while the model's HF cache still has `*.incomplete` shards — i.e. it's downloading, not yet
/// loading. Best-effort: an unmapped spec (ollama/lmstudio/path) just reports "not downloading".
fn model_is_downloading(spec: &str) -> bool {
    let Some(rest) = spec.strip_prefix("mlx-community:").map(|r| format!("mlx-community/{r}"))
        .or_else(|| spec.strip_prefix("hf:").map(|s| s.to_owned()))
        .or_else(|| (!spec.contains(':') && spec.contains('/')).then(|| spec.to_owned()))
    else {
        return false;
    };
    let Some((org, name)) = rest.split_once('/') else { return false };
    let Some(home) = std::env::var_os("HOME") else { return false };
    let dir = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{name}"));
    fn has_incomplete(dir: &std::path::Path) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "incomplete") {
                return true;
            }
            if p.is_dir() && has_incomplete(&p) {
                return true;
            }
        }
        false
    }
    has_incomplete(&dir)
}

use std::path::PathBuf;

use crate::agents::{AgentBrief, live_agents};
use crate::gateway_control::loading_models;
use crate::coders::{CoderBrief, live_coders};
use crate::projects::{ProjectBrief, list_projects};
use crate::sessions::{SessionBrief, live_sessions};

/// A coherent snapshot of the models/gateway service.
#[derive(Debug, Clone, Serialize)]
pub struct ControlStatus {
    /// The active shared gateway, or `None` if none is running.
    pub gateway: Option<GatewayStatus>,
    /// Host residency (RAM budget / committed / available / resident set).
    pub residency: ResidencyStatus,
    /// Installed local models (the catalog).
    pub installed: Vec<InstalledBrief>,
    /// Flat, display-ready residency metrics (gateway label + GiB-formatted RAM). An ARRAY so a
    /// declarative table (`remoteTable(st, cols, "residency_metrics")`) renders them identically on
    /// web AND tui — no client-side `computedSignal` (which the tui backend can't recompute).
    pub residency_metrics: Vec<MetricBrief>,
    /// The meeting rooms (read-only, from the meeting daemon's on-disk registry) so the UCC can list
    /// them with a link to each room's web view. Read directly from `rooms.json` to avoid a
    /// rozum-gateway → rozum-meeting crate dependency.
    pub meetings: Vec<MeetingBrief>,
    /// The chat-agents (model-participants) launched through the control API, with liveness — so the
    /// UCC can show a cross-room "running agents" view and stop them.
    pub agents: Vec<AgentBrief>,
    /// The coding-agents (`rozum launch`) launched through the control API, with liveness — so the UCC
    /// can show running coders, tail their logs, and stop them.
    pub coders: Vec<CoderBrief>,
    /// The live interactive terminal sessions (tmux-backed) — so the UCC can list them, open the
    /// xterm.js terminal, and stop them.
    pub sessions: Vec<SessionBrief>,
    /// Known project directories (from rooms.json), for workdir selection in the UCC launch forms.
    pub projects: Vec<ProjectBrief>,
    /// All installed models merged with live residency, sorted by size desc.
    /// Each row carries `stop_label`/`load_spec` for per-row conditional action buttons in the UCC.
    pub models: Vec<UnifiedModelBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    pub model: String,
    pub port: u16,
    pub pid: u32,
    pub n_ctx: u32,
    pub generation: u64,
    pub uptime_secs: u64,
    pub clients: usize,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidencyStatus {
    pub host_budget_bytes: Option<u64>,
    pub committed_bytes: u64,
    pub available_bytes: Option<u64>,
    pub residents: Vec<ResidentBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidentBrief {
    pub pid: u32,
    pub model: String,
    pub size_gib: String,
    /// smmr-D: steady-state active footprint (weights + KV, no transient activations) in GiB.
    /// None if no prior run has been measured yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_peak_gib: Option<String>,
    /// smmr-D: process-wide total peak (active + cache + prefill spike) in GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_peak_gib: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledBrief {
    pub spec: String,
    pub size_bytes: u64,
    /// GiB-formatted size for direct display in a declarative table column.
    pub size_gib: String,
    /// "★★★★" adequacy rating for direct display (empty when unrated). See `model_stars`.
    pub stars: String,
    /// Short operator-facing driver hint. Capability is model × agent-driver, not model alone.
    pub driver_hint: String,
    /// Combined rating + driver for a single stacked sub-line under the model name in the pickers
    /// (e.g. "★★★★  claude") — keeps the phone model picker to two columns so it fits without a
    /// separate Driver column squeezing the name.
    pub rating_line: String,
}

/// Live matrix-derived ratings: `~/.rozum/ucc/model-ratings.json`, written by
/// `scripts/bench/export_model_ratings.py` after a matrix run (claude-driver pass-rate over the
/// real coding tasks; greet + rc=2 excluded). Loaded once per status request; exact spec keys.
pub(crate) fn load_live_ratings() -> std::collections::HashMap<String, u8> {
    let mut out = std::collections::HashMap::new();
    let Some(home) = std::env::var_os("HOME") else { return out };
    let path = PathBuf::from(home).join(".rozum/ucc/model-ratings.json");
    let Ok(body) = std::fs::read_to_string(path) else { return out };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else { return out };
    if let Some(models) = doc.get("models").and_then(|m| m.as_object()) {
        for (spec, v) in models {
            if let Some(stars) = v.get("stars").and_then(|s| s.as_u64()) {
                out.insert(spec.clone(), stars.min(5) as u8);
            }
        }
    }
    out
}

/// UCC model "adequacy" rating (1–5): the live matrix export when present (exact spec match),
/// else the static fallback distilled from past matrix sessions — how reliably the model completes
/// agentic coding tasks under its best driver on this machine. Unknown models get `None` (sorted
/// last, shown unrated). The UCC model pickers sort by this and show it as stars.
pub(crate) fn model_stars(spec: &str, live: &std::collections::HashMap<String, u8>) -> Option<u8> {
    if let Some(s) = live.get(spec) {
        return Some(*s);
    }
    const RATINGS: &[(&str, u8)] = &[
        ("GLM-4.7-Flash", 5),          // 15/15 full matrix, in DEFAULT_MODELS
        ("Qwen3.6-35B-A3B", 4),        // reliable under claude after the delivery fixes
        ("gpt-oss-20b", 4),            // claude 5/5; codex fails are model-ceiling (rc10)
        ("GLM-4-32B", 4),              // reliable edit/debug/chat; artifact-synth covers create
        ("Qwen3.6-27B", 3),
        ("DeepSeek-Coder-V2-Lite", 3),
        ("Qwen3-Coder-30B", 3),        // post-parser-fix needs a fresh full matrix before promotion
        ("Devstral", 3),               // good under claude; poor codex/opencode match (B3)
        ("Qwen2.5-Coder-7B", 2),
        ("GLM-4-9B", 2),
        ("Qwen3-4B", 2),               // fine cascade planner, weak solo coder
        ("Qwen3-0.6B", 1),
    ];
    RATINGS.iter().find(|(k, _)| spec.contains(k)).map(|(_, s)| *s)
}

/// Short UCC model-picker hint: the agent driver that best matches this model's tool dialect and
/// measured matrix behaviour. Keep terse so the table remains scannable on mobile.
pub(crate) fn model_driver_hint(spec: &str) -> &'static str {
    if spec.contains("Qwen3.6-35B") {
        "any"
    } else if spec.contains("gpt-oss-20b") {
        "claude/opencode"
    } else if spec.contains("Qwen3-Coder")
        || spec.contains("Qwen3-30B")
        || spec.contains("Qwen3.6-27B")
        || spec.contains("GLM-4.7")
        || spec.contains("GLM-4-32B")
    {
        "claude"
    } else if spec.contains("Devstral") || spec.contains("Mistral") {
        "claude"
    } else {
        ""
    }
}

/// One row in the unified models panel — installed catalog merged with live residency.
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedModelBrief {
    pub spec: String,
    pub size_gib: String,
    pub size_bytes: u64,
    /// "выгрузить" when the model is currently resident; "" when not (hides the stop button via CSS).
    pub stop_label: String,
    /// spec value when the model is not loaded (drives the load URL); "" when already resident.
    pub load_spec: String,
    /// Async-load status for the panel: "" idle, "downloading…"/"loading…" while a background load is
    /// in flight (both action buttons hidden), or "✗ <error>" if the last load failed (load button
    /// shown again so the user can retry). Driven by `loading_models()`.
    pub load_status: String,
}

/// A flat `{metric, value}` pair — a row in the display-ready `residency_metrics` table.
#[derive(Debug, Clone, Serialize)]
pub struct MetricBrief {
    pub metric: String,
    pub value: String,
}

/// A meeting room — its `room` name is the path segment of the meeting web view (`/r/<room>`).
#[derive(Debug, Clone, Serialize)]
pub struct MeetingBrief {
    pub room: String,
}


/// List the meeting rooms from the daemon's on-disk registry (`$XDG_STATE_HOME|~/.local/state` →
/// `rozum/rooms.json`). Read-only, best-effort: a missing/garbled file yields an empty list. Test and
/// worktree rooms (project under `/tmp` or `/.worktrees/`) are filtered out so the dashboard lists the
/// real rooms (global + project).
fn list_meetings() -> Vec<MeetingBrief> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")));
    let Some(path) = base.map(|b| b.join("rozum/rooms.json")) else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    let Ok(rooms) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
        return Vec::new();
    };
    rooms
        .iter()
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?;
            let project = r.get("project").and_then(|p| p.as_str()).unwrap_or("");
            if project.contains("/tmp/") || project.contains("/.worktrees/") {
                return None;
            }
            Some(MeetingBrief { room: name.to_string() })
        })
        .collect()
}








/// Format a byte count as a one-decimal GiB string (e.g. `"25.1 GiB"`).
fn fmt_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
}

/// Aggregate the live models/gateway control status — the active gateway (if any), the host residency
/// ledger, and the installed catalog. Read-only; never loads a model or invokes an engine.
pub async fn status() -> ControlStatus {
    use crate::share;
    let gateway = match share::read_active() {
        Some(a) => Some(GatewayStatus {
            healthy: share::health_ok(a.port).await,
            uptime_secs: share::now_unix().saturating_sub(a.started_at),
            clients: share::live_lease_count(share::LEASE_FRESH_SECS),
            model: a.model,
            port: a.port,
            pid: a.pid,
            n_ctx: a.n_ctx,
            generation: a.generation,
        }),
        None => None,
    };
    let installed_catalog = rozum_models::models::scan_all_installed();
    let residency = ResidencyStatus {
        host_budget_bytes: share::host_ram_budget_bytes(),
        committed_bytes: share::committed_by_others_bytes(0), // skip nothing → the whole ledger
        available_bytes: share::available_ram_for_admission(),
        residents: share::list_residents()
            .into_iter()
            .map(|(pid, model)| {
                let size_gib = installed_catalog
                    .iter()
                    .find(|m| rozum_models::model_source::same_model(&m.spec, &model))
                    .map(|m| fmt_gib(m.size_bytes))
                    .unwrap_or_default();
                let active_peak_gib = rozum_core::footprint::cached_active_peak(&model).map(fmt_gib);
                let total_peak_gib = rozum_core::footprint::cached_peak(&model).map(fmt_gib);
                ResidentBrief { pid, model, size_gib, active_peak_gib, total_peak_gib }
            })
            .collect(),
    };
    // Model pickers show this list top-down: best-rated first (see `model_stars`), unrated last.
    let live_ratings = load_live_ratings();
    let mut installed: Vec<InstalledBrief> = installed_catalog
        .iter()
        .map(|m| {
            let stars = model_stars(&m.spec, &live_ratings).map(|n| "★".repeat(n as usize)).unwrap_or_default();
            let driver_hint = model_driver_hint(&m.spec).to_string();
            let rating_line = match (stars.is_empty(), driver_hint.is_empty()) {
                (false, false) => format!("{stars}  {driver_hint}"),
                (false, true) => stars.clone(),
                (true, false) => driver_hint.clone(),
                (true, true) => String::new(),
            };
            InstalledBrief {
                size_gib: fmt_gib(m.size_bytes),
                stars,
                driver_hint,
                rating_line,
                spec: m.spec.clone(),
                size_bytes: m.size_bytes,
            }
        })
        .collect();
    installed.sort_by(|a, b| {
        model_stars(&b.spec, &live_ratings)
            .unwrap_or(0)
            .cmp(&model_stars(&a.spec, &live_ratings).unwrap_or(0))
            .then_with(|| a.spec.cmp(&b.spec))
    });
    let residency_metrics = vec![
        MetricBrief {
            metric: "gateway".into(),
            value: gateway.as_ref().map(|g| g.model.clone()).unwrap_or_else(|| "none running".into()),
        },
        MetricBrief {
            metric: "available".into(),
            value: residency.available_bytes.map(fmt_gib).unwrap_or_else(|| "—".into()),
        },
        MetricBrief {
            metric: "host budget".into(),
            value: residency.host_budget_bytes.map(fmt_gib).unwrap_or_else(|| "—".into()),
        },
        MetricBrief { metric: "committed".into(), value: fmt_gib(residency.committed_bytes) },
        MetricBrief { metric: "residents".into(), value: residency.residents.len().to_string() },
    ];
    let meetings = list_meetings();
    let agents = live_agents();
    let coders = live_coders();
    let sessions = live_sessions();
    let projects = list_projects();
    let resident_specs: std::collections::HashSet<&str> =
        residency.residents.iter().map(|r| r.model.as_str()).collect();
    // Snapshot in-flight loads (drop stale failures so a week-old error doesn't haunt the panel).
    let load_snapshot: std::collections::HashMap<String, Option<String>> = {
        let mut l = loading_models().lock().unwrap();
        let now = crate::share::now_unix();
        l.retain(|_, s| s.error.is_none() || now.saturating_sub(s.started) < 600);
        l.iter().map(|(k, s)| (k.clone(), s.error.clone())).collect()
    };
    let mut models: Vec<UnifiedModelBrief> = installed_catalog.iter().map(|m| {
        let loaded = resident_specs.contains(m.spec.as_str());
        // Async-load state: actively loading (no error) hides both buttons and shows a status; a failed
        // load shows "✗ <err>" AND leaves the load button so the user can retry.
        let load_entry = load_snapshot.get(&m.spec);
        let loading_now = matches!(load_entry, Some(None));
        let load_status = match load_entry {
            Some(Some(err)) => format!("✗ {err}"),
            Some(None) if model_is_downloading(&m.spec) => "downloading…".to_string(),
            Some(None) => "loading…".to_string(),
            None => String::new(),
        };
        UnifiedModelBrief {
            spec: m.spec.clone(),
            size_gib: fmt_gib(m.size_bytes),
            size_bytes: m.size_bytes,
            stop_label: if loaded && !loading_now { "выгрузить".to_string() } else { String::new() },
            load_spec: if !loaded && !loading_now { m.spec.clone() } else { String::new() },
            load_status,
        }
    }).collect();
    models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    ControlStatus { gateway, residency, installed, residency_metrics, meetings, agents, coders, sessions, projects, models }
}

// ── Messenger admin console ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_snapshots_and_serializes() {
        // Env-independent: it aggregates a coherent snapshot (residency + catalog) without panicking,
        // and serializes to the JSON contract the HTTP/UCC consumer reads.
        let s = status().await;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"residency\""));
        assert!(json.contains("\"installed\""));
        // residents in the ledger each have a non-empty model name.
        assert!(s.residency.residents.iter().all(|r| !r.model.is_empty()));
    }

    #[test]
    fn model_driver_hints_capture_known_model_driver_matches() {
        assert_eq!(model_driver_hint("mlx-community:Qwen3.6-35B-A3B-4bit-DWQ"), "any");
        assert_eq!(model_driver_hint("mlx-community:gpt-oss-20b-MXFP4-Q4"), "claude/opencode");
        assert_eq!(model_driver_hint("mlx-community:Qwen3-Coder-30B-A3B-Instruct-4bit"), "claude");
        assert_eq!(model_driver_hint("mlx-community:GLM-4.7-Flash-4bit"), "claude");
        assert_eq!(model_driver_hint("mlx-community:Devstral-Small-2507-4bit"), "claude");
        assert_eq!(model_driver_hint("unknown:model"), "");
    }
}
