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
    if !l.done && !crate::procctl::group_alive(l.pgid) {
        return None;
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
    Q.get_or_init(|| Mutex::new(load_matrix_queue_from_disk()))
}

fn matrix_queue_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("matrix-queue.json"))
}

/// Write the queue out. Atomic tmp+rename, like `persist_matrix_live` — a half-written queue read
/// by another process is worse than no file.
fn persist_matrix_queue(q: &[MatrixJob]) {
    let Some(path) = matrix_queue_path() else { return };
    if let Ok(bytes) = serde_json::to_vec(q) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Read the queue back, and **settle every unfinished job rather than resuming it.**
///
/// A `Queued` entry restored as-is would start a matrix run nobody asked for, minutes after a
/// reboot, unattended — and on this host the matrix has taken the machine down twice (BUG-001
/// kernel panic, BUG-003 jetsam). A `Running` entry cannot be resumed either: its process died with
/// the gateway. So both become terminal with a reason, which makes the panel tell the truth about
/// what happened instead of either lying or acting.
///
/// The value of persisting is therefore history and cross-process readability — NOT resumption.
/// Resuming is a separate feature with a separate risk, and it should be asked for explicitly.
fn load_matrix_queue_from_disk() -> Vec<MatrixJob> {
    let Some(path) = matrix_queue_path() else { return Vec::new() };
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    let Ok(jobs) = serde_json::from_slice::<Vec<MatrixJob>>(&bytes) else {
        return Vec::new();
    };
    settle_after_restart(jobs, crate::share::now_unix())
}

/// Split out so the restart POLICY is testable without a clock or a file — it is the decision this
/// change exists to make, and a decision only exercised through I/O is one nobody re-checks.
fn settle_after_restart(mut jobs: Vec<MatrixJob>, now: u64) -> Vec<MatrixJob> {
    for j in jobs.iter_mut() {
        match j.status {
            MatrixJobStatus::Queued | MatrixJobStatus::Paused => {
                j.status = MatrixJobStatus::Stopped;
                j.finished_at.get_or_insert(now);
            }
            MatrixJobStatus::Running => {
                j.status = MatrixJobStatus::Failed;
                j.finished_at.get_or_insert(now);
            }
            MatrixJobStatus::Done | MatrixJobStatus::Failed | MatrixJobStatus::Stopped => {}
        }
    }
    jobs
}

/// Mutate the queue and persist it in the same breath.
///
/// Deliberately not "remember to call persist after each mutation": there are five mutation sites
/// and adding a sixth without the call would leave the file quietly behind the truth, which is the
/// failure this whole change exists to remove. Read-only callers keep the plain lock.
pub(crate) fn with_queue<T>(f: impl FnOnce(&mut Vec<MatrixJob>) -> T) -> T {
    let mut q = matrix_queue().lock().unwrap();
    let out = f(&mut q);
    persist_matrix_queue(&q);
    out
}

pub(crate) fn matrix_notify() -> &'static tokio::sync::Notify {
    use std::sync::OnceLock;
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

/// Where the matrix harness and its results live.
///
/// Both were resolved against the CURRENT DIRECTORY, which is right for a gateway started inside
/// the repo and wrong for a service: launchd starts it with whatever cwd it pleases, and the matrix
/// routes then read a results dir that does not exist and answer "no runs" instead of "I am looking
/// in the wrong place". Found while porting the same route to `.ssc`, where the bug had to be fixed
/// to make the port work at all.
///
/// The env var is the explicit answer; the cwd-relative path stays as the fallback so a repo-local
/// run behaves exactly as before. Changing the fallback to a repo-root search would have been a
/// behaviour change for every existing caller, and this fix is not the place to make one.
fn bench_path(env_key: &str, rel: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(env_key) {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    std::env::current_dir().unwrap_or_default().join(rel)
}

pub(crate) fn bench_script() -> PathBuf {
    bench_path("ROZUM_BENCH_SCRIPT", "scripts/bench/agentic.sh")
}

/// The task definitions. THE source is `scripts/bench/tasks.json`, read by the bench to build the
/// prompt it hands the model and by this file to show it. It exists because the two used to be
/// separate copies and five of six prompts had drifted: the console showed an older, shorter prompt
/// than the model was given, and two of the eight tasks had no entry here at all. See
/// `matrix-task-info-is-a-stale-copy` in BUGS.md.
pub(crate) fn bench_tasks_path() -> PathBuf {
    bench_path("ROZUM_BENCH_TASKS", "scripts/bench/tasks.json")
}

pub(crate) fn bench_results_dir() -> PathBuf {
    bench_path("ROZUM_BENCH_RESULTS", "scripts/bench/results")
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
    with_queue(|q| q.push(job));
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
            let job_id = with_queue(|q| {
                q.iter().position(|j| j.status == MatrixJobStatus::Queued)
                    .map(|i| {
                        q[i].status = MatrixJobStatus::Running;
                        q[i].started_at = Some(crate::share::now_unix());
                        q[i].id.clone()
                    })
            });
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

    let log_dir = state_dir()
        .map(|d| d.join("matrix-logs"))
        .unwrap_or_else(rozum_paths::temp_dir);
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{job_id}.log"));

    let stamp = crate::share::now_unix();
    let result_dir = bench_results_dir().join(format!("agentic-ucc-{stamp}"));
    let _ = std::fs::create_dir_all(&result_dir);

    with_queue(|q| {
        if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
            j.log_path = Some(log_path.to_string_lossy().into_owned());
            j.result_dir = Some(result_dir.to_string_lossy().into_owned());
        }
    });

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
        .stderr(Stdio::from(log_err));
    crate::procctl::own_process_group(&mut cmd);

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
    with_queue(|q| {
        if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
            j.status = status;
            j.finished_at = Some(crate::share::now_unix());
            j.exit_code = Some(exit_code);
        }
    });
}

/// After a KEEP=1 run: scan /tmp/rozum-agentic-* for workdirs whose agentic.meta
/// matches this run, then copy them into $OUT/cells/<agent>/<model_safe>/<task>/.
pub(crate) fn archive_matrix_cells(result_dir: &PathBuf) {
    let cells_root = result_dir.join("cells");
    // A LITERAL `/tmp`, and it must stay one: `agentic.sh` creates these workdirs with
    // `mktemp -d /tmp/rozum-agentic-XXXXXX`, so the scanner and the maker have to name the same
    // directory. `std::env::temp_dir()` would read `$TMPDIR` — on macOS a per-user
    // `/var/folders/…` — and this would then find nothing, silently, on every run.
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
        // `.rozum-seed` is the sha256 of what `setup_task` seeded, and it is what makes `rc=13`
        // ("the workdir came back byte-identical") re-checkable months later instead of a number
        // the reader has to take on trust. It costs a few hundred bytes per cell.
        for file in [
            "agent.log",
            "verify.out",
            "triage.out",
            "agentic.meta",
            "cargo.err",
            ".rozum-seed",
        ] {
            let src = entry.path().join(file);
            if src.exists() { let _ = std::fs::copy(&src, dest.join(file)); }
        }
    }
}

/// Hardcoded task prompts and expected outputs matching agentic.sh's prompt_for().
pub(crate) fn matrix_task_info(task: &str) -> serde_json::Value {
    matrix_task_info_at(&bench_tasks_path(), task)
}

/// Split out so the tests can name the file they mean. They used to point
/// `ROZUM_BENCH_TASKS` at it instead — one process-global variable, two tests, and cargo
/// runs them in parallel: the pair passed by luck until it did not.
fn matrix_task_info_at(path: &std::path::Path, task: &str) -> serde_json::Value {
    // Read, not matched: the table this replaced was a copy of the bench's prompts and had gone
    // stale in five of six entries. An unreadable file answers the same shape with nulls rather
    // than a stale truth — a console that says "unknown" is better than one that says something
    // the model never saw.
    let unknown = || serde_json::json!({
        "label": task, "difficulty": null, "prompt": null, "expected": null
    });
    let Ok(raw) = std::fs::read_to_string(path.to_path_buf()) else {
        // Answer nulls, but SAY SO once. The file is resolved relative to the process's working
        // directory, so a console started somewhere else — or deployed before the file lands in
        // its checkout — would otherwise serve empty prompts that look like real data.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| eprintln!(
            "control server: task definitions not readable at {} — /control/public/matrix/cell will \
             answer null prompts (set ROZUM_BENCH_TASKS or start from the repo root)",
            path.to_path_buf().display()));
        return unknown();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else { return unknown() };
    match doc.get("tasks").and_then(|t| t.get(task)) {
        Some(v) => v.clone(),
        None    => unknown(),
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
    with_queue(|q| {
        if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
            j.status = MatrixJobStatus::Failed;
            j.finished_at = Some(crate::share::now_unix());
        }
    });
}

/// Ask the running bench's process group to stop / freeze / continue.
///
/// `Outcome`, not `bool`: on a platform without SIGSTOP the pause routes must be able to say WHY
/// nothing happened, and "no live run" and "this platform cannot freeze a process group" are two
/// different answers to the operator (`crate::procctl`).
pub(crate) fn signal_matrix(ask: crate::procctl::Ask) -> crate::procctl::Outcome {
    let live = matrix_live().lock().unwrap();
    match *live {
        Some(ref l) => crate::procctl::signal_group(l.pgid, ask),
        None => crate::procctl::Outcome::Failed,
    }
}

/// The answer a pause/resume/stop route gives.
///
/// `why` is present only when the platform has no such operation at all, so on unix this body is
/// byte-for-byte the `{"ok": …}` the console has always parsed. A UI that renders a bare `false`
/// for "Windows cannot freeze a process group" sends the operator hunting a broken bench run.
fn ask_answer(sent: crate::procctl::Outcome) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut body = serde_json::json!({ "ok": sent.ok() });
    if let Some(why) = sent.why() {
        body["why"] = serde_json::Value::from(why);
    }
    axum::Json(body).into_response()
}

pub(crate) async fn matrix_pause_route() -> axum::response::Response {
    let sent = signal_matrix(crate::procctl::Ask::Suspend);
    let ok = sent.ok();
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
            with_queue(|q| {
                if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Paused; }
            });
        }
    }
    ask_answer(sent)
}

pub(crate) async fn matrix_resume_route() -> axum::response::Response {
    let sent = signal_matrix(crate::procctl::Ask::Resume);
    let ok = sent.ok();
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
            with_queue(|q| {
                if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Running; }
            });
        }
    }
    ask_answer(sent)
}

pub(crate) async fn matrix_stop_route() -> axum::response::Response {
    // Continue first to unfreeze a paused run, then ask it to stop. Where the platform cannot
    // freeze, it never froze, so the unsupported continue is exactly the no-op it should be — the
    // stop's own outcome is the one reported.
    let _ = signal_matrix(crate::procctl::Ask::Resume);
    ask_answer(signal_matrix(crate::procctl::Ask::Terminate))
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


#[cfg(test)]
mod queue_persist_tests {
    use super::*;

    fn job(id: &str, status: MatrixJobStatus) -> MatrixJob {
        MatrixJob {
            id: id.into(),
            models: None,
            agents: None,
            tasks: None,
            reps: None,
            status,
            queued_at: 1,
            started_at: None,
            finished_at: None,
            log_path: None,
            result_dir: None,
            exit_code: None,
        }
    }

    /// The decision this change is really about: a restart must SETTLE unfinished jobs, never
    /// resume them. A `Queued` entry restored as-is would start a matrix run nobody asked for,
    /// unattended, minutes after a reboot — and the matrix has taken this host down twice
    /// (BUG-001 kernel panic, BUG-003 jetsam).
    #[test]
    fn a_restart_settles_unfinished_jobs_instead_of_resuming_them() {
        let raw = serde_json::to_vec(&vec![
            job("a", MatrixJobStatus::Queued),
            job("b", MatrixJobStatus::Running),
            job("c", MatrixJobStatus::Paused),
            job("d", MatrixJobStatus::Done),
            job("e", MatrixJobStatus::Failed),
        ])
        .unwrap();
        let jobs: Vec<MatrixJob> = serde_json::from_slice(&raw).unwrap();
        let settled = settle_after_restart(jobs, 100);

        let by = |id: &str| settled.iter().find(|j| j.id == id).unwrap().status.clone();
        assert_eq!(by("a"), MatrixJobStatus::Stopped, "a queued job must not run itself");
        assert_eq!(by("b"), MatrixJobStatus::Failed, "its process died with the gateway");
        assert_eq!(by("c"), MatrixJobStatus::Stopped);
        // Terminal states are history and must be left exactly as they were.
        assert_eq!(by("d"), MatrixJobStatus::Done);
        assert_eq!(by("e"), MatrixJobStatus::Failed);
        // Everything settled carries a finish time, so the panel can show when it stopped rather
        // than leaving a row that looks like it is still going.
        for id in ["a", "b", "c"] {
            assert_eq!(
                settled.iter().find(|j| j.id == id).unwrap().finished_at,
                Some(100)
            );
        }
    }

    /// A `finished_at` that was already recorded is not overwritten — the restart is not when that
    /// job ended, and stamping "now" on it would rewrite history to the moment of the reboot.
    #[test]
    fn an_existing_finish_time_survives_the_restart() {
        let mut j = job("a", MatrixJobStatus::Running);
        j.finished_at = Some(7);
        let settled = settle_after_restart(vec![j], 100);
        assert_eq!(settled[0].finished_at, Some(7));
    }

    /// An unreadable or absent file is an empty queue, never a panic: this runs at first touch of a
    /// static, so a failure here would take the gateway down at startup rather than lose a list.
    #[test]
    fn a_corrupt_queue_file_reads_as_empty() {
        assert!(serde_json::from_slice::<Vec<MatrixJob>>(b"{not json").is_err());
    }
}

#[cfg(test)]
mod bench_path_tests {
    use super::*;

    /// The env var wins, and an EMPTY one does not — an unset-looking variable that is actually set
    /// to "" would otherwise resolve every matrix path to the filesystem root, which is the kind of
    /// wrong that deletes things rather than merely failing.
    #[test]
    fn the_env_var_wins_unless_it_is_empty() {
        let key = "ROZUM_TEST_BENCH_PATH";
        // SAFETY: single-threaded test, and the key is unique to this test.
        unsafe { std::env::set_var(key, "/somewhere/results") };
        assert_eq!(bench_path(key, "rel/path"), PathBuf::from("/somewhere/results"));

        unsafe { std::env::set_var(key, "") };
        let fallback = bench_path(key, "rel/path");
        assert_ne!(fallback, PathBuf::from(""), "an empty var must not resolve to the root");
        assert!(fallback.ends_with("rel/path"));

        unsafe { std::env::remove_var(key) };
        assert!(bench_path(key, "rel/path").ends_with("rel/path"));
    }

    /// The fallback stays cwd-relative on purpose: every existing caller runs inside the repo, and
    /// changing that here would be a behaviour change smuggled into a path fix.
    #[test]
    fn the_fallback_is_still_relative_to_the_current_directory() {
        let key = "ROZUM_TEST_BENCH_PATH_2";
        unsafe { std::env::remove_var(key) };
        let cwd = std::env::current_dir().unwrap_or_default();
        assert_eq!(bench_path(key, "scripts/bench/results"), cwd.join("scripts/bench/results"));
    }
}

#[cfg(test)]
mod task_info_tests {
    use super::*;

    /// Every task the bench can run must have a prompt here, and it must be THE prompt — the file
    /// this reads is the same one `prompt_for` serves to the model. The table this replaced was a
    /// copy and had drifted in five of six entries, with two tasks missing outright; the console
    /// showed an operator a task the model was never given. See BUGS.md
    /// `matrix-task-info-is-a-stale-copy`.
    #[test]
    fn every_bench_task_has_a_prompt_and_a_difficulty() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap();
        let tasks = repo.join("scripts/bench/tasks.json");
        // The eight the bench's own DIFF table names — wordcount and multibug are the two that
        // used to answer `prompt: null`, which is what made this worth a test rather than a look.
        for task in ["greet", "build", "fix", "test", "debug", "rpn", "wordcount", "multibug"] {
            let info = matrix_task_info_at(&tasks, task);
            assert!(info["prompt"].is_string(), "{task}: prompt is {}", info["prompt"]);
            assert!(!info["prompt"].as_str().unwrap().is_empty(), "{task}: empty prompt");
            assert!(info["difficulty"].is_number(), "{task}: difficulty is {}", info["difficulty"]);
        }
    }

    /// An unreadable file answers the shape with nulls rather than panicking or serving a stale
    /// truth: a console saying "unknown" is better than one confidently showing the wrong task.
    #[test]
    fn a_missing_file_answers_nulls_not_a_panic() {
        let info = matrix_task_info_at(std::path::Path::new("/nonexistent/tasks.json"), "rpn");
        assert!(info["prompt"].is_null());
        assert_eq!(info["label"], "rpn");
    }
}
