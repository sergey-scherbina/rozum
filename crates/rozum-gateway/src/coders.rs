//! Coder processes: an agent invocation with a workspace and a log.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). It imports `agent_invocation` from
//! `agents` — SIDEWAYS, not back at the parent — and that edge is the honest shape of the thing: a
//! coder IS an agent invocation, given a directory to work in and a file to write to.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::agents::agent_invocation;
use crate::defaults::{default_tail, default_true};
use crate::errors::json_err;
use crate::paths::state_dir;
use crate::spawn_support::*;
use crate::wire_body::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoderRecord {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub log: String,
    /// 0 until the background launch has spawned the process.
    pub pid: u32,
    pub started_at: u64,
    /// "starting…" / "running" / "failed: …"; empty on legacy records (= running).
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoderBrief {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub pid: u32,
    pub alive: bool,
    pub status: String,
}

pub(crate) fn coders_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-coders.json"))
}

pub(crate) fn load_coders() -> Vec<CoderRecord> {
    coders_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub(crate) fn save_coders(coders: &[CoderRecord]) {
    if let Some(p) = coders_registry_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(body) = serde_json::to_vec_pretty(coders) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

/// Running coders with a fresh liveness check; coders that have exited STAY in the registry (so their
/// log is still reachable) but report `alive=false`. The UI lets the operator clear a finished one.
/// Status: "starting…"/"failed: …" verbatim from the record; a completed launch shows "running"
/// while the process lives and "exited" once it's done (a coder run finishing is normal).
pub(crate) fn live_coders() -> Vec<CoderBrief> {
    let _g = registry_lock();
    let now = crate::share::now_unix();
    load_coders()
        .into_iter()
        .map(|c| {
            let alive = pid_alive(c.pid);
            let status = if c.status.starts_with("starting") {
                // A launch task that never spawned (pid 0) past the TTL is a dead cold-start —
                // show it as failed rather than an eternal "starting…".
                if c.pid == 0 && now.saturating_sub(c.started_at) >= STARTING_TTL_SECS {
                    "failed: launch interrupted".into()
                } else {
                    c.status.clone()
                }
            } else if c.status.starts_with("failed") {
                c.status.clone()
            } else if alive {
                "running".into()
            } else {
                "exited".into()
            };
            CoderBrief {
                alive,
                id: c.id, agent: c.agent, model: c.model, workdir: c.workdir, prompt: c.prompt,
                pid: c.pid, status,
            }
        })
        .collect()
}

/// Rewrite one coder record in place under the registry lock; returns whether a record was found
/// (false ⇒ stopped meanwhile — the caller must clean up anything it already spawned).
pub(crate) fn update_coder_record(id: &str, f: impl FnOnce(&mut CoderRecord)) -> bool {
    let _g = registry_lock();
    let mut coders = load_coders();
    if let Some(c) = coders.iter_mut().find(|c| c.id == id) {
        f(c);
        save_coders(&coders);
        true
    } else {
        false
    }
}

/// Spawn `rozum launch --model <m> <agent invocation>` DETACHED in `workdir`, output → a log file.
/// Returns (pid, log_path).
pub(crate) fn spawn_coder(agent: &str, model: &str, workdir: &str, prompt: &str, verify: bool) -> std::io::Result<(u32, PathBuf)> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let log_dir = state_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(rozum_paths::temp_dir);
    let _ = std::fs::create_dir_all(&log_dir);
    let stamp = crate::share::now_unix();
    let log_path = log_dir.join(format!("coder-{}-{}.log", sanitize(agent), stamp));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log2 = log.try_clone()?;
    let mut args: Vec<String> = vec!["launch".into(), "--model".into(), model.into()];
    // Chat turns declare a 32k window instead of the model's max (262k on Qwen3.5-4B). This is an
    // ADMISSION lever, not a speed one: the residency gate reserves weights + KV(n_ctx) + reserve,
    // and KV at 262k is ~8 GiB (vs ~1 GiB at 32k) — so an uncapped chat turn asks for ~14 GiB it
    // will never touch (the KV cache itself grows lazily per token, it is not pre-allocated). On a
    // busy host that oversized request is what makes a chat turn WAIT in the admission queue behind
    // a resident model. 32k is ample for one conversational turn that reads a few files.
    if !verify {
        args.push("--n-ctx".into());
        args.push("32768".into());
        // …and no room presence. `rozum launch` carries the meeting room for an agent that has no
        // MCP client (nadia), which is right for a Coders run — a task the operator started and
        // wants to see land. A chat turn is not a task: every message would post a `working:` and a
        // `done:` into the project's room, and room chatter would be folded into a conversational
        // turn that never asked for it. The Coders path (verify:true) keeps the presence.
        args.push("--no-room-bridge".into());
    }
    args.extend(agent_invocation(agent, prompt));
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    crate::procctl::own_process_group(&mut cmd);
    // Chat turns (verify:false) opt out of the post-agent cargo verify-gate AND decode GREEDILY
    // (argmax) — the same focus lever the matrix uses (ROZUM_FORCE_GREEDY). Both env vars propagate
    // to the shared gateway `rozum launch` spawns, so a chat's model runs deterministic + focused
    // rather than sampled + rambly. Proven: greedy + an "explore-then-answer" prompt makes the 4B
    // read the repo and summarise it accurately.
    if !verify {
        cmd.env("ROZUM_VERIFY", "0");
        cmd.env("ROZUM_FORCE_GREEDY", "1");
    }
    Ok((cmd.spawn()?.id(), log_path))
}

#[derive(Deserialize)]
pub(crate) struct CoderLaunchReq {
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) workdir: String,
    pub(crate) prompt: String,
    /// Run `rozum launch`'s post-agent verify-gate (`cargo build && cargo test`, with repair rounds).
    /// Default true keeps the coder-view behaviour. The chat app passes `false`: a conversational
    /// turn is not a cargo task, so verifying the whole repo after every message is wrong + slow.
    #[serde(default = "default_true")]
    pub(crate) verify: bool,
}

pub(crate) async fn coder_launch_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: CoderLaunchReq = match parse_action_json(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let agent = req.agent.trim().to_string();
    let model = req.model.trim().to_string();
    let workdir = req.workdir.trim().to_string();
    let prompt = req.prompt.trim().to_string();
    if agent.is_empty() || model.is_empty() || workdir.is_empty() || prompt.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "agent, model, workdir, prompt required");
    }
    if !std::path::Path::new(&workdir).is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "workdir is not a directory");
    }
    if let Some(why) = agent_missing_reason(&agent) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, &why);
    }
    // Record NOW ("starting…"), do the slow model load + spawn in the background — the phone's
    // fetch returns instantly and errors land in the row's status (same pattern as sessions).
    let id = format!("{}-{}-{}", sanitize(&agent), crate::share::now_unix(), next_launch_seq());
    {
        let _g = registry_lock();
        let mut coders = load_coders();
        coders.push(CoderRecord {
            id: id.clone(),
            agent: agent.clone(),
            model: model.clone(),
            workdir: workdir.clone(),
            prompt: prompt.clone(),
            log: String::new(),
            pid: 0,
            started_at: crate::share::now_unix(),
            status: "starting…".into(),
        });
        save_coders(&coders);
    }
    let spawn_model = model.clone();
    let verify = req.verify;
    spawn_launch_task(
        model,
        id.clone(),
        |id| load_coders().iter().any(|c| c.id == id),
        |id, s| { update_coder_record(id, |c| c.status = s); },
        move |task_id, _port| async move {
            match spawn_coder(&agent, &spawn_model, &workdir, &prompt, verify) {
                Ok((pid, log)) => {
                    // Stopped mid-spawn → kill the orphan (it holds the model + a live agent run).
                    let kept = update_coder_record(&task_id, |c| {
                        c.pid = pid;
                        c.log = log.to_string_lossy().into_owned();
                        c.status = "running".into();
                    });
                    if !kept {
                        crate::procctl::terminate(pid);
                    }
                }
                Err(e) => { update_coder_record(&task_id, |c| c.status = format!("failed: spawn: {e}")); }
            }
        },
    );
    axum::Json(serde_json::json!({ "ok": true, "id": id, "status": "starting" })).into_response()
}

pub(crate) async fn coder_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let _g = registry_lock();
    let mut coders = load_coders();
    let Some(pos) = coders.iter().position(|c| c.id == id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such coder");
    };
    let c = coders.remove(pos);
    crate::procctl::terminate(c.pid);
    save_coders(&coders);
    axum::Json(serde_json::json!({ "ok": true, "id": c.id })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct CoderLogQuery {
    pub(crate) id: String,
    #[serde(default = "default_tail")]
    pub(crate) tail: usize,
}

pub(crate) async fn coder_log_route(axum::extract::Query(q): axum::extract::Query<CoderLogQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(c) = load_coders().into_iter().find(|c| c.id == q.id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such coder");
    };
    let text = std::fs::read_to_string(&c.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(q.tail.min(2000));
    let tail = lines[start..].join("\n");
    axum::Json(serde_json::json!({ "id": c.id, "alive": pid_alive(c.pid), "log": tail })).into_response()
}

