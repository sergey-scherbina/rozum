//! The agentic matrix: run it, watch it live, read its cells.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). `control.rs` is not one component but
//! ~260 small route handlers sharing helpers, so it splits by SUBJECT rather than by leaf. This is
//! the first subject out: the job queue and worker, the live panel persisted across restarts, the
//! CSV reader, and the public read-only views.
//!
//! Measured before moving: this family calls exactly three things outside itself — `json_err`,
//! `safe_path_seg`, `state_dir` — which is why those went to `errors.rs`/`paths.rs` in the commit
//! before this one. `require_perm_matrix` deliberately stayed behind: it is one of six sibling
//! permission middlewares, so taking it would split that family AND point this module back at its
//! parent.

pub(crate) const DONE_TTL_SECS: u64 = 300; // keep panel visible 5 min after completion
/// Stale on-disk state older than this is ignored on startup (the job is long gone).
pub(crate) const LIVE_STALE_SECS: u64 = 1800;

use serde::{Deserialize, Serialize};

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::errors::json_err;
use crate::paths::{safe_path_seg, state_dir};
// The public matrix routes are gated on a view token. That check used to come from `control.rs`,
// which made this module point back at its parent; the `view_tokens` slice moved it to a module of
// its own, so the dependency now runs downward.
use crate::view_tokens::check_view_token;
// The shared log-tail default: this module's matrix-log route and control's coder-log route are
// the same kind of endpoint and must not drift to different numbers.
use crate::defaults::default_tail;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MatrixJobStatus { Queued, Running, Paused, Done, Failed, Stopped }

// Live state for the currently running matrix job (cleared when job finishes).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct MatrixLive {
    pub(crate) job_id: String,
    pub(crate) pgid: i32,
    pub(crate) paused: bool,
    pub(crate) done: bool,       // true after exit, panel stays visible for DONE_TTL_SECS
    pub(crate) done_at: u64,
    pub(crate) started_at: u64,
    pub(crate) total_cells: usize,
    pub(crate) log_path: String,
    pub(crate) result_dir: String,
}

pub(crate) fn matrix_live_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("matrix-live.json"))
}

pub(crate) fn persist_matrix_live(live: &Option<MatrixLive>) {
    let Some(path) = matrix_live_path() else { return };
    match live {
        None => { let _ = std::fs::remove_file(&path); }
        Some(l) => {
            if let Ok(bytes) = serde_json::to_vec(l) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, &bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
    }
}

pub(crate) fn load_matrix_live_from_disk() -> Option<MatrixLive> {
    let path = matrix_live_path()?;
    let bytes = std::fs::read(&path).ok()?;
    let l: MatrixLive = serde_json::from_slice(&bytes).ok()?;
    // Ignore stale state: done jobs older than LIVE_STALE_SECS, or in-progress jobs
    // from a previous process that is no longer running (pgid dead).
    let now = crate::share::now_unix();
    if l.done && now.saturating_sub(l.done_at) > LIVE_STALE_SECS {
        return None;
    }
    // For in-progress jobs, check if the process group is still alive.
    if !l.done {
        let alive = unsafe { libc::kill(-l.pgid, 0) } == 0;
        if !alive {
            return None;
        }
    }
    Some(l)
}

pub(crate) fn matrix_live() -> &'static Mutex<Option<MatrixLive>> {
    use std::sync::OnceLock;
    static L: OnceLock<Mutex<Option<MatrixLive>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(load_matrix_live_from_disk()))
}

pub(crate) fn matrix_live_data() -> serde_json::Value {
    let live = matrix_live().lock().unwrap();
    let Some(ref l) = *live else {
        return serde_json::json!({ "status": "idle" });
    };
    let now = crate::share::now_unix();
    // If job finished and TTL has expired, treat as idle
    if l.done && now.saturating_sub(l.done_at) > DONE_TTL_SECS {
        drop(live);
        let cleared = None;
        persist_matrix_live(&cleared);
        *matrix_live().lock().unwrap() = cleared;
        return serde_json::json!({ "status": "idle" });
    }
    let elapsed_s = if l.done { l.done_at.saturating_sub(l.started_at) } else { now.saturating_sub(l.started_at) };
    // Count done cells from partial CSV
    let result_path = PathBuf::from(&l.result_dir).join("per-run.csv");
    let done = parse_matrix_csv(&result_path).len();
    // Log tail
    let log_tail = std::fs::read_to_string(&l.log_path).unwrap_or_default();
    let tail_lines: Vec<&str> = log_tail.lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail_start = tail_lines.len().saturating_sub(8);
    let log_recent = tail_lines[tail_start..].join("\n");
    // Current task hint: prefer model/agent/task lines (contain "/"), fall back to TASK keyword lines
    let current_hint = if l.done {
        String::new()
    } else {
        tail_lines.iter().rev()
            .find(|l| {
                let t = l.trim();
                !t.chars().all(|c| c == '=' || c == ' ' || c == '-')
                    && (t.contains('/') || t.contains("TASK") || t.contains("task"))
            })
            .map(|s| s.trim().trim_matches('=').trim().to_string())
            .unwrap_or_default()
    };
    // Memory from residency
    let report = crate::share::dry_run_admission(0);
    let available_gib = report.available.unwrap_or(0) as f64 / 1024.0 / 1024.0 / 1024.0;
    let committed_gib = report.in_use as f64 / 1024.0 / 1024.0 / 1024.0;
    let status = if l.done { "done" } else if l.paused { "paused" } else { "running" };
    serde_json::json!({
        "status": status,
        "elapsed_s": elapsed_s,
        "started_at": l.started_at,
        "progress": { "done": done, "total": l.total_cells },
        "memory": { "available_gib": (available_gib * 10.0).round() / 10.0,
                    "committed_gib": (committed_gib * 10.0).round() / 10.0 },
        "current_hint": current_hint,
        "log_tail": log_recent,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MatrixJob {
    pub(crate) id: String,
    pub(crate) models: Option<Vec<String>>,
    pub(crate) agents: Option<Vec<String>>,
    pub(crate) tasks: Option<Vec<String>>,
    /// REPS: how many runs per cell (default 1 = single-run pass/fail).
    #[serde(default)]
    pub(crate) reps: Option<u32>,
    pub(crate) status: MatrixJobStatus,
    pub(crate) queued_at: u64,
    pub(crate) started_at: Option<u64>,
    pub(crate) finished_at: Option<u64>,
    pub(crate) log_path: Option<String>,
    pub(crate) result_dir: Option<String>,
    pub(crate) exit_code: Option<i32>,
}

pub(crate) fn matrix_queue() -> &'static Mutex<Vec<MatrixJob>> {
    use std::sync::OnceLock;
    static Q: OnceLock<Mutex<Vec<MatrixJob>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn matrix_notify() -> &'static tokio::sync::Notify {
    use std::sync::OnceLock;
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

pub(crate) fn bench_script() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join("scripts/bench/agentic.sh")
}

pub(crate) fn bench_results_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join("scripts/bench/results")
}

pub(crate) fn latest_matrix_result() -> Option<(String, PathBuf)> {
    let dir = bench_results_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            // only directories named agentic-*, skip .console/.log files
            n.starts_with("agentic-") && !n.contains('.')
        })
        .filter(|e| e.path().join("per-run.csv").exists())
        .collect();
    entries.sort_by_key(|e| {
        e.metadata().and_then(|m| m.modified()).ok()
    });
    let last = entries.last()?;
    let stamp = last.file_name().to_string_lossy().to_string();
    Some((stamp, last.path().join("per-run.csv")))
}

pub(crate) fn parse_matrix_csv(csv_path: &PathBuf) -> Vec<serde_json::Value> {
    let text = match std::fs::read_to_string(csv_path) { Ok(t) => t, Err(_) => return vec![] };
    let mut lines = text.lines();
    let header: Vec<String> = match lines.next() {
        Some(h) => h.split(',').map(|s| s.to_string()).collect(),
        None => return vec![],
    };
    lines.filter(|l| !l.trim().is_empty()).map(|line| {
        let vals: Vec<&str> = line.split(',').collect();
        let mut obj = serde_json::Map::new();
        for (i, key) in header.iter().enumerate() {
            let v = vals.get(i).copied().unwrap_or("");
            if let Ok(n) = v.parse::<i64>() {
                obj.insert(key.clone(), serde_json::json!(n));
            } else if let Ok(f) = v.parse::<f64>() {
                obj.insert(key.clone(), serde_json::json!(f));
            } else {
                obj.insert(key.clone(), serde_json::json!(v));
            }
        }
        serde_json::Value::Object(obj)
    }).collect()
}

pub(crate) async fn matrix_status_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let queue: Vec<MatrixJob> = matrix_queue().lock().unwrap().clone();
    let last = latest_matrix_result().map(|(stamp, csv)| {
        let cells = parse_matrix_csv(&csv);
        let total = cells.len();
        let passed = cells.iter()
            .filter(|c| c.get("pass").and_then(|v| v.as_i64()).unwrap_or(0) == 1)
            .count();
        serde_json::json!({ "stamp": stamp, "cells": cells, "total": total, "passed": passed })
    });
    let installed: Vec<String> = rozum_models::models::scan_all_installed()
        .into_iter().map(|m| m.spec).collect();
    axum::Json(serde_json::json!({ "queue": queue, "last": last, "installed": installed })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct MatrixRunReq {
    pub(crate) models: Option<Vec<String>>,
    pub(crate) agents: Option<Vec<String>>,
    pub(crate) tasks: Option<Vec<String>>,
    /// REPS: how many times to repeat each cell (default 1). Passed as REPS=N to agentic.sh.
    #[serde(default)]
    pub(crate) reps: Option<u32>,
}

pub(crate) async fn matrix_run_route(axum::Json(req): axum::Json<MatrixRunReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = format!("job-{}", crate::share::now_unix());
    let job = MatrixJob {
        id: id.clone(),
        models: req.models,
        agents: req.agents,
        tasks: req.tasks,
        reps: req.reps,
        status: MatrixJobStatus::Queued,
        queued_at: crate::share::now_unix(),
        started_at: None, finished_at: None, log_path: None, result_dir: None, exit_code: None,
    };
    matrix_queue().lock().unwrap().push(job);
    matrix_notify().notify_one();
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct MatrixLogQuery { job_id: String, #[serde(default = "default_tail")] tail: usize }

pub(crate) async fn matrix_log_route(axum::extract::Query(q): axum::extract::Query<MatrixLogQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let log_path = matrix_queue().lock().unwrap()
        .iter().find(|j| j.id == q.job_id)
        .and_then(|j| j.log_path.clone());
    match log_path {
        None => json_err(axum::http::StatusCode::NOT_FOUND, "no log for job"),
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(q.tail.min(2000));
            axum::Json(serde_json::json!({ "log": lines[start..].join("\n") })).into_response()
        }
    }
}

pub(crate) async fn matrix_worker() {
    loop {
        matrix_notify().notified().await;
        loop {
            let job_id = {
                let mut q = matrix_queue().lock().unwrap();
                q.iter().position(|j| j.status == MatrixJobStatus::Queued)
                    .map(|i| {
                        q[i].status = MatrixJobStatus::Running;
                        q[i].started_at = Some(crate::share::now_unix());
                        q[i].id.clone()
                    })
            };
            let Some(id) = job_id else { break };
            run_matrix_job(&id).await;
        }
    }
}

pub(crate) async fn run_matrix_job(job_id: &str) {
    let (models, agents, tasks, reps) = {
        let q = matrix_queue().lock().unwrap();
        let Some(j) = q.iter().find(|j| j.id == job_id) else { return };
        (j.models.clone(), j.agents.clone(), j.tasks.clone(), j.reps.unwrap_or(1))
    };

    let log_dir = state_dir().map(|d| d.join("matrix-logs")).unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{job_id}.log"));

    let stamp = crate::share::now_unix();
    let result_dir = bench_results_dir().join(format!("agentic-ucc-{stamp}"));
    let _ = std::fs::create_dir_all(&result_dir);

    {
        let mut q = matrix_queue().lock().unwrap();
        if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
            j.log_path = Some(log_path.to_string_lossy().into_owned());
            j.result_dir = Some(result_dir.to_string_lossy().into_owned());
        }
    }

    let log_out = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f, Err(_) => { matrix_fail(job_id); return; }
    };
    let log_err = match log_out.try_clone() { Ok(f) => f, Err(_) => { matrix_fail(job_id); return; } };

    let all_models: Vec<String> = rozum_models::models::scan_all_installed().into_iter().map(|m| m.spec).collect();
    let models_vec = models.unwrap_or(all_models);
    // The fallback for API callers that omit `agents`. The UCC no longer relies on it —
    // it sends the list explicitly precisely so that adding an agent to the matrix does
    // not require rebuilding and restarting the gateway to take effect.
    let agents_vec = agents
        .unwrap_or_else(|| vec!["claude".into(), "codex".into(), "opencode".into(), "nadia".into()]);
    let tasks_vec  = tasks.unwrap_or_else(|| vec!["greet".into(), "build".into(), "fix".into(), "test".into(), "debug".into(), "rpn".into()]);
    let total_cells = models_vec.len() * agents_vec.len() * tasks_vec.len() * reps as usize;
    let models_str = models_vec.join(" ");
    let agents_str = agents_vec.join(" ");
    let tasks_str  = tasks_vec.join(" ");

    let started_at = {
        let q = matrix_queue().lock().unwrap();
        q.iter().find(|j| j.id == job_id).and_then(|j| j.started_at).unwrap_or_else(crate::share::now_unix)
    };

    use std::os::unix::process::CommandExt as _;
    let mut cmd = Command::new("bash");
    cmd.arg(bench_script())
        .env("AGENTIC_MODELS", &models_str)
        .env("AGENTS", &agents_str)
        .env("TASKS", &tasks_str)
        .env("BENCH_OUT", &result_dir)
        .env("ROZUM_SAMPLING_SEED", "1234")
        // Argmax decode: for these DETERMINISTIC coding tasks (one correct behaviour) greedy is the
        // most reliable single decode and removes the gateway's sampling RNG — so a capable model's
        // cell reflects capability, not a lucky/unlucky sample (matrix-reliability-greedy-repair).
        .env("ROZUM_FORCE_GREEDY", "1")
        .env("KEEP", "1") // preserve workdirs so we can archive cell logs
        .env("REPS", reps.to_string())
        // TWO verify-repair retries: a verified FAIL feeds the real compiler/test error back for up to
        // two fresh attempts before the cell is recorded RED. Only costs wall-clock on cells that fail;
        // absorbs the residual run-to-run variance (agent CLIs inject fresh session-id+ts per prompt) so
        // a single unlucky sample on a capable model no longer reads as a hard red.
        .env("REPAIR", "2")
        .env("GEN_TIMEOUT", "120") // 120s per-generation; frozen model fails fast, leaves room for retry
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .process_group(0);

    let exit_code = match cmd.spawn() {
        Ok(mut child) => {
            let pgid = child.id() as i32;
            {
                let new_live = Some(MatrixLive {
                    job_id: job_id.to_string(), pgid, paused: false,
                    done: false, done_at: 0,
                    started_at, total_cells,
                    log_path: log_path.to_string_lossy().into_owned(),
                    result_dir: result_dir.to_string_lossy().into_owned(),
                });
                persist_matrix_live(&new_live);
                *matrix_live().lock().unwrap() = new_live;
            }
            let code = tokio::task::spawn_blocking(move || child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)).await.unwrap_or(-1);
            let finish_ts = crate::share::now_unix();
            {
                let mut lock = matrix_live().lock().unwrap();
                if let Some(ref mut l) = *lock {
                    l.done = true;
                    l.done_at = finish_ts;
                }
                persist_matrix_live(&*lock);
            }
            code
        }
        Err(e) => { eprintln!("matrix worker: spawn failed: {e}"); -1 }
    };

    // Archive kept workdirs into $OUT/cells/<agent>/<model_safe>/<task>/
    archive_matrix_cells(&result_dir);

    let status = if exit_code == 0 { MatrixJobStatus::Done } else { MatrixJobStatus::Failed };
    let mut q = matrix_queue().lock().unwrap();
    if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
        j.status = status;
        j.finished_at = Some(crate::share::now_unix());
        j.exit_code = Some(exit_code);
    }
}

/// After a KEEP=1 run: scan /tmp/rozum-agentic-* for workdirs whose agentic.meta
/// matches this run, then copy them into $OUT/cells/<agent>/<model_safe>/<task>/.
pub(crate) fn archive_matrix_cells(result_dir: &PathBuf) {
    let cells_root = result_dir.join("cells");
    let tmp = PathBuf::from("/tmp");
    let entries = match std::fs::read_dir(&tmp) {
        Ok(e) => e, Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("rozum-agentic-") { continue; }
        let meta_path = entry.path().join("agentic.meta");
        let Ok(meta_text) = std::fs::read_to_string(&meta_path) else { continue };
        // Parse key=value
        let kv: std::collections::HashMap<&str, &str> = meta_text.lines()
            .filter_map(|l| l.split_once('='))
            .collect();
        let (agent, model, task) = match (kv.get("agent"), kv.get("model"), kv.get("task")) {
            (Some(&a), Some(&m), Some(&t)) if !a.is_empty() && !t.is_empty() => (a, m, t),
            _ => continue,
        };
        let safe_model = model.replace(['/', ':', ' '], "_");
        let dest = cells_root.join(agent).join(&safe_model).join(task);
        if dest.exists() { continue; } // already archived
        let _ = std::fs::create_dir_all(&dest);
        for file in ["agent.log", "verify.out", "triage.out", "agentic.meta", "cargo.err"] {
            let src = entry.path().join(file);
            if src.exists() { let _ = std::fs::copy(&src, dest.join(file)); }
        }
    }
}

/// Hardcoded task prompts and expected outputs matching agentic.sh's prompt_for().
pub(crate) fn matrix_task_info(task: &str) -> serde_json::Value {
    match task {
        "greet" => serde_json::json!({
            "label": "Greet",
            "difficulty": 1,
            "prompt": "Reply with exactly the single word: pong  (nothing else, no punctuation).",
            "expected": "output contains \"pong\""
        }),
        "build" => serde_json::json!({
            "label": "Build",
            "difficulty": 2,
            "prompt": "Create a minimal Rust binary project in the CURRENT directory: reverse-cli — reads its first CLI arg and prints it reversed. Then run `cargo run -- hello` and confirm it prints \"olleh\".",
            "expected": "cargo run -- hello == olleh"
        }),
        "fix" => serde_json::json!({
            "label": "Fix",
            "difficulty": 3,
            "prompt": "There is a Rust project in the current directory. `cargo run -- hello` prints \"hello\" instead of \"olleh\". Find and fix the one-line bug in src/main.rs, then confirm it prints \"olleh\".",
            "expected": "cargo run -- hello == olleh"
        }),
        "test" => serde_json::json!({
            "label": "Test",
            "difficulty": 4,
            "prompt": "Extend the reverse-cli project: implement a reverse() function and a #[test] that asserts reverse(\"hello\") == \"olleh\". Run `cargo test` (green) and `cargo run -- hello` (olleh).",
            "expected": "cargo test green AND cargo run -- hello == olleh"
        }),
        "debug" => serde_json::json!({
            "label": "Debug",
            "difficulty": 5,
            "prompt": "There is a Rust library in the current directory. `cargo test` fails due to a bug in src/lib.rs. Fix the bug (do NOT modify the test) so `cargo test` passes.",
            "expected": "cargo test green"
        }),
        "rpn" => serde_json::json!({
            "label": "RPN calc",
            "difficulty": 6,
            "prompt": "Create a Rust binary `rpn-calc` that evaluates a Reverse Polish Notation expression from its first CLI arg (space-separated tokens, integer arithmetic). Verify: `cargo run -- \"3 4 + 5 *\"` == 35 and `cargo run -- \"5 1 2 + 4 * + 3 -\"` == 14.",
            "expected": "both RPN expressions produce correct integer results"
        }),
        _ => serde_json::json!({ "label": task, "difficulty": null, "prompt": null, "expected": null }),
    }
}

#[derive(Deserialize)]
pub(crate) struct MatrixCellQuery {
    pub(crate) stamp: String,
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) task: String,
    #[serde(default = "default_tail")]
    pub(crate) tail: usize,
}

pub(crate) async fn matrix_cell_route(axum::extract::Query(q): axum::extract::Query<MatrixCellQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;

    // Reject path-traversal in the segments joined onto the results dir below (stamp/agent/task raw,
    // model after its `/`→`_` normalization) — a crafted `../…` must not walk outside bench_results_dir.
    let safe_model = q.model.replace(['/', ':', ' '], "_");
    if !(safe_path_seg(&q.stamp) && safe_path_seg(&q.agent) && safe_path_seg(&q.task) && safe_path_seg(&safe_model)) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "invalid stamp/agent/model/task segment");
    }

    // Find cell in the specified result dir
    let csv_path = bench_results_dir().join(&q.stamp).join("per-run.csv");
    let cell = parse_matrix_csv(&csv_path).into_iter().find(|c| {
        c.get("agent").and_then(|v| v.as_str()) == Some(&q.agent) &&
        c.get("model").and_then(|v| v.as_str()) == Some(&q.model) &&
        c.get("task").and_then(|v| v.as_str()) == Some(&q.task)
    });

    // Check for archived cell logs
    let cell_dir = bench_results_dir().join(&q.stamp).join("cells").join(&q.agent).join(&safe_model).join(&q.task);

    let agent_log = if cell_dir.join("agent.log").exists() {
        let text = std::fs::read_to_string(cell_dir.join("agent.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(q.tail.min(3000));
        Some(lines[start..].join("\n"))
    } else { None };

    let verify_out = if cell_dir.join("verify.out").exists() {
        std::fs::read_to_string(cell_dir.join("verify.out")).ok()
    } else { None };

    let triage_out = if cell_dir.join("triage.out").exists() {
        std::fs::read_to_string(cell_dir.join("triage.out")).ok()
    } else { None };

    axum::Json(serde_json::json!({
        "cell": cell,
        "task_info": matrix_task_info(&q.task),
        "agent_log": agent_log,
        "verify_out": verify_out,
        "triage_out": triage_out,
        "has_logs": cell_dir.exists(),
    })).into_response()
}

pub(crate) fn matrix_fail(job_id: &str) {
    let mut q = matrix_queue().lock().unwrap();
    if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
        j.status = MatrixJobStatus::Failed;
        j.finished_at = Some(crate::share::now_unix());
    }
}

pub(crate) fn signal_matrix(sig: libc::c_int) -> bool {
    let live = matrix_live().lock().unwrap();
    if let Some(ref l) = *live {
        unsafe { libc::killpg(l.pgid, sig) == 0 }
    } else { false }
}

pub(crate) async fn matrix_pause_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = signal_matrix(libc::SIGSTOP);
    if ok {
        let id = {
            let mut live = matrix_live().lock().unwrap();
            if let Some(ref mut l) = *live {
                l.paused = true;
                let id = l.job_id.clone();
                persist_matrix_live(&*live);
                id
            } else { String::new() }
        };
        if !id.is_empty() {
            let mut q = matrix_queue().lock().unwrap();
            if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Paused; }
        }
    }
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

pub(crate) async fn matrix_resume_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = signal_matrix(libc::SIGCONT);
    if ok {
        let id = {
            let mut live = matrix_live().lock().unwrap();
            if let Some(ref mut l) = *live {
                l.paused = false;
                let id = l.job_id.clone();
                persist_matrix_live(&*live);
                id
            } else { String::new() }
        };
        if !id.is_empty() {
            let mut q = matrix_queue().lock().unwrap();
            if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Running; }
        }
    }
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

pub(crate) async fn matrix_stop_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    // SIGCONT first to unfreeze if paused, then SIGTERM
    signal_matrix(libc::SIGCONT);
    let ok = signal_matrix(libc::SIGTERM);
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

pub(crate) async fn matrix_live_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(matrix_live_data()).into_response()
}

pub(crate) async fn public_matrix_live_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    axum::Json(matrix_live_data()).into_response()
}

pub(crate) async fn public_matrix_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    // Re-use the same matrix status logic
    let queue: Vec<MatrixJob> = matrix_queue().lock().unwrap().clone();
    let last = latest_matrix_result().map(|(stamp, csv)| {
        let cells = parse_matrix_csv(&csv);
        let total = cells.len();
        let passed = cells.iter().filter(|c| c.get("pass").and_then(|v| v.as_i64()).unwrap_or(0) == 1).count();
        serde_json::json!({ "stamp": stamp, "cells": cells, "total": total, "passed": passed })
    });
    axum::Json(serde_json::json!({ "queue": queue, "last": last })).into_response()
}

pub(crate) async fn public_matrix_cell_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    let stamp = q.get("stamp").map(|s| s.as_str()).unwrap_or("");
    let agent = q.get("agent").map(|s| s.as_str()).unwrap_or("");
    let model = q.get("model").map(|s| s.as_str()).unwrap_or("");
    let task  = q.get("task").map(|s| s.as_str()).unwrap_or("");
    let tail: usize = q.get("tail").and_then(|s| s.parse().ok()).unwrap_or(200);
    // A view token grants read of the matrix results ONLY — reject path-traversal segments so an
    // anonymous token holder cannot walk `cell_dir`/`csv_path` outside bench_results_dir (arbitrary
    // file read + a dir-existence oracle otherwise).
    let safe_model = model.replace(['/', ':', ' '], "_");
    if !(safe_path_seg(stamp) && safe_path_seg(agent) && safe_path_seg(task) && safe_path_seg(&safe_model)) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "invalid stamp/agent/model/task segment");
    }
    let csv_path = bench_results_dir().join(stamp).join("per-run.csv");
    let cell = parse_matrix_csv(&csv_path).into_iter().find(|c| {
        c.get("agent").and_then(|v| v.as_str()) == Some(agent) &&
        c.get("model").and_then(|v| v.as_str()) == Some(model) &&
        c.get("task").and_then(|v| v.as_str()) == Some(task)
    });
    let cell_dir = bench_results_dir().join(stamp).join("cells").join(agent).join(&safe_model).join(task);
    let agent_log = if cell_dir.join("agent.log").exists() {
        let text = std::fs::read_to_string(cell_dir.join("agent.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(tail.min(3000));
        Some(lines[start..].join("\n"))
    } else { None };
    let verify_out = std::fs::read_to_string(cell_dir.join("verify.out")).ok();
    let triage_out = std::fs::read_to_string(cell_dir.join("triage.out")).ok();
    axum::Json(serde_json::json!({
        "cell": cell, "task_info": matrix_task_info(task),
        "agent_log": agent_log, "verify_out": verify_out,
        "triage_out": triage_out, "has_logs": cell_dir.exists(),
    })).into_response()
}

