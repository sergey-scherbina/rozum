//! Agent processes: launch one, list what is live, stop it.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). `require_perm_agents` stayed behind with
//! its five sibling permission middlewares — taking one of six would split that family and point
//! this module back at its parent, the same call made for `require_perm_matrix`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::defaults::default_policy;
use crate::errors::json_err;
use crate::paths::state_dir;
use crate::spawn_support::*;
use crate::wire_body::*;

/// One launched chat-agent we track. Persisted to the registry; `alive` is computed per status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub model: String,
    pub room: String,
    pub handle: String,
    pub policy: String,
    /// 0 until the background launch has spawned the process.
    pub pid: u32,
    pub started_at: u64,
    /// "starting…" / "running" / "failed: …"; empty on legacy records (= running).
    #[serde(default)]
    pub status: String,
}

/// Display brief for a running agent (registry entry + liveness), in `ControlStatus.agents`.
#[derive(Debug, Clone, Serialize)]
pub struct AgentBrief {
    pub id: String,
    pub model: String,
    pub room: String,
    pub handle: String,
    pub policy: String,
    pub pid: u32,
    pub alive: bool,
    pub status: String,
}

pub(crate) fn agents_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-agents.json"))
}

pub(crate) fn load_agents() -> Vec<AgentRecord> {
    agents_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub(crate) fn save_agents(agents: &[AgentRecord]) {
    if let Some(p) = agents_registry_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(body) = serde_json::to_vec_pretty(agents) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

/// Agents for the UCC table: live processes plus in-flight ("starting…") and failed launches.
/// Only records whose launch COMPLETED are pruned when their pid dies; a failed row stays
/// visible until the user stops it — that's how launch errors reach the phone.
pub(crate) fn live_agents() -> Vec<AgentBrief> {
    let _g = registry_lock();
    let now = crate::share::now_unix();
    let all = load_agents();
    let (keep, dead): (Vec<_>, Vec<_>) = all.into_iter().partition(|a| {
        // A `starting…` row is kept only while its launch task could still be running (TTL) —
        // past that it's an orphan from a control-serve restart mid-launch.
        let starting_fresh = a.status.starts_with("starting")
            && now.saturating_sub(a.started_at) < STARTING_TTL_SECS;
        starting_fresh || a.status.starts_with("failed") || pid_alive(a.pid)
    });
    if !dead.is_empty() {
        save_agents(&keep); // self-heal: drop processes that have exited
    }
    keep.into_iter()
        .map(|a| {
            let alive = pid_alive(a.pid);
            let status = if !a.status.is_empty() { a.status.clone() } else { "running".into() };
            AgentBrief {
                id: a.id, model: a.model, room: a.room, handle: a.handle, policy: a.policy,
                pid: a.pid, alive, status,
            }
        })
        .collect()
}

/// Rewrite one agent record in place under the registry lock; returns whether a record was found
/// (false ⇒ it was stopped meanwhile — the caller must clean up anything it already spawned).
pub(crate) fn update_agent_record(id: &str, f: impl FnOnce(&mut AgentRecord)) -> bool {
    let _g = registry_lock();
    let mut agents = load_agents();
    if let Some(a) = agents.iter_mut().find(|a| a.id == id) {
        f(a);
        save_agents(&agents);
        true
    } else {
        false
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentLaunchReq {
    pub(crate) model: String,
    pub(crate) room: String,
    #[serde(default = "default_policy")]
    pub(crate) policy: String,
    #[serde(default)]
    pub(crate) persona: String,
    #[serde(default)]
    pub(crate) handle: String,
}

pub(crate) async fn agent_launch_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: AgentLaunchReq = match parse_action_json(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let model = req.model.trim().to_string();
    let room = req.room.trim().to_string();
    if model.is_empty() || room.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model and room required");
    }
    // Record NOW ("starting…"), do the slow model load + spawn in the background — the phone's
    // fetch returns instantly and errors land in the row's status (same pattern as sessions).
    let h = req.handle.trim().to_string();
    let handle = if h.is_empty() { derive_handle(&model) } else { h };
    let id = format!("{}-{}-{}", sanitize(&room), crate::share::now_unix(), next_launch_seq());
    {
        let _g = registry_lock();
        let mut agents = load_agents();
        agents.push(AgentRecord {
            id: id.clone(), model: model.clone(), room: room.clone(),
            handle: handle.clone(), policy: req.policy.clone(), pid: 0,
            started_at: crate::share::now_unix(),
            status: "starting…".into(),
        });
        save_agents(&agents);
    }
    let policy = req.policy.clone();
    let persona = req.persona.trim().to_string();
    let spawn_model = model.clone();
    spawn_launch_task(
        model,
        id.clone(),
        |id| load_agents().iter().any(|a| a.id == id),
        |id, s| { update_agent_record(id, |a| a.status = s); },
        move |task_id, port| async move {
            match spawn_participant(&spawn_model, &room, &policy, &persona, port) {
                Ok(pid) => {
                    // If stop removed the record while we were spawning, update_* returns false —
                    // kill the just-spawned participant so it isn't orphaned (it holds the model).
                    let kept = update_agent_record(&task_id, |a| {
                        a.pid = pid;
                        a.status = "running".into();
                    });
                    if !kept {
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                    }
                }
                Err(e) => { update_agent_record(&task_id, |a| a.status = format!("failed: spawn: {e}")); }
            }
        },
    );
    axum::Json(serde_json::json!({ "ok": true, "id": id, "handle": handle, "status": "starting" })).into_response()
}

pub(crate) async fn agent_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let _g = registry_lock();
    let mut agents = load_agents();
    let Some(pos) = agents.iter().position(|a| a.id == id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such agent");
    };
    let a = agents.remove(pos);
    if a.pid != 0 {
        unsafe { libc::kill(a.pid as libc::pid_t, libc::SIGTERM); }
    }
    save_agents(&agents);
    axum::Json(serde_json::json!({ "ok": true, "id": a.id })).into_response()
}

/// The agent's own invocation for a non-interactive run: program + flags + the task prompt. claude runs
/// autonomously (skip-permissions + a turn cap) so it never blocks on a phone-launched run.
pub(crate) fn agent_invocation(agent: &str, prompt: &str) -> Vec<String> {
    match agent {
        "claude" => vec![
            "claude".into(), "-p".into(), prompt.into(),
            "--dangerously-skip-permissions".into(), "--max-turns".into(), "40".into(),
        ],
        "codex" => vec!["codex".into(), "exec".into(), prompt.into()],
        "opencode" => vec!["opencode".into(), "run".into(), prompt.into()],
        // nadia headless = `nadia run <task>`. No autonomy flag: its batch mode already starts in
        // auto-approve (asking would deadlock on a stdin nobody is at) and the sandbox is the
        // containment there. A bare `nadia <prompt>` would read the prompt as the MODE and die.
        "nadia" => vec!["nadia".into(), "run".into(), prompt.into()],
        other => vec![other.into(), prompt.into()],
    }
}

