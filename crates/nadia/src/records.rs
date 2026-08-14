//! What an agent did, on disk, so it outlives the process that ran it.
//!
//! Agents live in `nadia serve`'s memory. `serve` is started by whatever front-end needs it — on
//! this machine, the Telegram bridge — and a deploy that restarts the front-end used to take the
//! agents with it: no result, no `/status`, no way to answer "what happened in that directory".
//! It happened twice in one evening, both times unnoticed, because the only symptom is that the
//! next agent comes back as `#1`.
//!
//! Records also make ids unique for the life of the machine rather than the life of a process.
//! That matters beyond tidiness: a front-end that remembers "agent #3 is mine" and meets a
//! restarted `serve` handing #3 to somebody else's work will deliver one operator's result into
//! another's chat. The Telegram watcher carries a whole branch to survive that; with ids that do
//! not restart, there is nothing to survive.
//!
//! One file per agent, written at the moments that change what an outside reader would be told:
//! when it starts, when the gate reports, and when it stops. Not on every tool call — that is a
//! write per second per agent for a field nobody reads after the fact.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::supervisor::{AgentId, Phase, Status};

/// Where the records live. `NADIA_STATE` overrides, so a test never writes into the operator's
/// own history and two `serve`s in a test can be given separate ones.
pub fn dir() -> PathBuf {
    if let Some(p) = std::env::var_os("NADIA_STATE") {
        return PathBuf::from(p);
    }
    rozum_paths::home_dir()
        .unwrap_or_else(rozum_paths::temp_dir)
        .join(".nadia")
        .join(".agents")
}

fn path_of(id: AgentId) -> PathBuf {
    dir().join(format!("{id}.json"))
}

/// Write one agent's record. Best-effort by design: a run must not fail because its history
/// could not be written, and the caller has nothing useful to do about it either.
pub fn save(s: &Status) {
    let d = dir();
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    let v = json!({
        "id": s.id,
        "parent": s.parent,
        "task": s.task,
        "workspace": s.workspace,
        "phase": s.phase.label(),
        "tool_calls": s.tool_calls,
        "last_tool": s.last_tool,
        "elapsed_secs": s.elapsed.as_secs(),
        "result": s.result,
        "touched": s.touched,
        "check": s.report.check,
        "checked": s.report.passed,
        "check_detail": s.report.detail,
        "repairs": s.report.rounds,
    });
    if let Ok(text) = serde_json::to_vec_pretty(&v) {
        let p = path_of(s.id);
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// Every record on disk, oldest id first.
///
/// A record that was not terminal when it was written is [`Phase::Interrupted`]: the process that
/// was running it is gone, so nothing can say whether the work finished. Calling that `done` would
/// claim a result we do not have and calling it `failed` would claim a failure we did not see —
/// the same rule the verify gate lives by, one layer out.
pub fn load_all() -> Vec<Status> {
    let Ok(rd) = std::fs::read_dir(dir()) else { return Vec::new() };
    let mut out: Vec<Status> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter_map(|v| from_json(&v))
        .collect();
    out.sort_by_key(|s| s.id);
    out
}

fn from_json(v: &Value) -> Option<Status> {
    let id = v.get("id")?.as_u64()?;
    let phase = v
        .get("phase")
        .and_then(|p| p.as_str())
        .and_then(Phase::from_label)
        .unwrap_or(Phase::Interrupted);
    let strings = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(Status {
        id,
        parent: v.get("parent").and_then(|x| x.as_u64()),
        task: text("task").unwrap_or_default(),
        workspace: PathBuf::from(text("workspace").unwrap_or_default()),
        // A live phase in a record means the process died holding it.
        phase: if phase.is_terminal() { phase } else { Phase::Interrupted },
        tool_calls: v.get("tool_calls").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        last_tool: text("last_tool"),
        elapsed: std::time::Duration::from_secs(
            v.get("elapsed_secs").and_then(|x| x.as_u64()).unwrap_or(0),
        ),
        result: text("result"),
        touched: strings("touched"),
        report: crate::gate::Report {
            check: text("check"),
            passed: v.get("checked").and_then(|x| x.as_bool()),
            detail: text("check_detail").unwrap_or_default(),
            rounds: v.get("repairs").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point, in one test: a record survives the process, and one that was still
    /// running when the process ended says so instead of claiming an outcome.
    #[test]
    fn a_record_outlives_the_process_and_an_unfinished_one_says_so() {
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("NADIA_STATE", d.path()) };

        let base = Status {
            id: 3,
            parent: None,
            task: "напиши программу".into(),
            workspace: PathBuf::from("/w/tasks/x"),
            phase: Phase::Done,
            tool_calls: 4,
            last_tool: Some("bash".into()),
            elapsed: std::time::Duration::from_secs(27),
            result: Some("готово".into()),
            touched: vec!["src/main.rs".into()],
            report: crate::gate::Report {
                check: Some("cargo build -q".into()),
                passed: Some(true),
                detail: String::new(),
                rounds: 0,
            },
        };
        save(&base);
        save(&Status { id: 4, phase: Phase::Running, result: None, ..base.clone() });

        let loaded = load_all();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, 3);
        assert_eq!(loaded[0].phase, Phase::Done, "a finished run must keep its verdict");
        assert_eq!(loaded[0].report.passed, Some(true), "the gate's verdict is part of the record");
        assert_eq!(loaded[0].touched, vec!["src/main.rs".to_string()]);
        assert_eq!(loaded[0].task, "напиши программу");

        // The one that was still running when the process ended: not done, not failed, not gone.
        assert_eq!(loaded[1].phase, Phase::Interrupted);
        assert!(loaded[1].phase.is_terminal(), "an interrupted agent is not still running");

        unsafe { std::env::remove_var("NADIA_STATE") };
    }
}
