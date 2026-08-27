//! Coding-agent participant: a real agent (`claude`, `nadia`, `codex`, …) joins a
//! meeting room and answers turns by actually running that agent — not by calling a
//! chat-completions endpoint. Spec context: `docs/specs/agent-meeting-coordination.md`.
//!
//! Join/poll/reply-policy plumbing is shared with [`super::model_participant`] via
//! [`super::participant_loop`]; the only difference is *how a reply is produced*: one
//! turn in, one `rozum launch --model <local-spec> <agent> -p <prompt> …` subprocess
//! out, its stdout (that's what `claude -p`'s print mode IS — the final answer, not a
//! transcript) trimmed and posted back.
//!
//! This mirrors `crates/rozum-gateway/src/coders.rs::spawn_coder`'s `verify:false`
//! ("chat turn") recipe — a one-shot headless agent reply used conversationally, which
//! is exactly this shape — rather than its `verify:true` ("task") recipe, which is for
//! an operator-started job with its own room presence. Two crates, can't share the
//! function (`rozum-meeting` sits below `rozum-gateway` in the dependency graph), so
//! the tiny `agent`→argv match (`agents.rs::agent_invocation`) is duplicated here with
//! a pointer back to its twin — everything else (the loop, the risk story) is real
//! shared code via `participant_loop`.
//!
//! **This is a real risk increase over a chat-only participant.** Anyone who can post
//! in an ACL'd room/Telegram chat can make the agent edit files autonomously — no
//! per-action prompts, by design, same as `coders.rs`'s existing headless jobs.
//! Bounded by: the Seatbelt jail `rozum launch` puts the agent under by default
//! (confined to the workdir + toolchain caches, no write outside it, loopback-only
//! network — see `docs/specs/model-sandbox.md`), and by `--acl <path>` gating who can
//! even trigger a turn (reused from `model_participant`, checked against `caps.shell`
//! since this participant IS a shell-capable agent).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use super::participant_loop::{
    ParticipantConfig, ReplyGenerator, ReplyPolicy, base_handle, run_participant_loop,
    sender_id_from_content,
};
use super::store::StoredTurn;

/// `agent` → the argv `rozum launch` should exec, mirroring
/// `crates/rozum-gateway/src/agents.rs::agent_invocation` (kept in sync by hand — see
/// the module doc for why this can't be a shared function). `claude`'s
/// `--dangerously-skip-permissions` is the same flag `coders.rs` already uses for its
/// headless jobs; the containment is the sandbox, not a permission prompt nobody is
/// there to answer.
fn agent_invocation(agent: &str, prompt: &str) -> Vec<String> {
    match agent {
        "claude" => vec![
            "claude".into(),
            "-p".into(),
            prompt.into(),
            "--dangerously-skip-permissions".into(),
            "--max-turns".into(),
            "40".into(),
        ],
        "codex" => vec!["codex".into(), "exec".into(), prompt.into()],
        "opencode" => vec!["opencode".into(), "run".into(), prompt.into()],
        "nadia" => vec!["nadia".into(), "run".into(), prompt.into()],
        other => vec![other.into(), prompt.into()],
    }
}

/// Turn a room name into a filesystem-safe directory leaf: lowercase alnum, everything
/// else collapsed to `-`. Empty/all-punctuation input falls back to `room` so a workdir
/// is always derivable.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() { "room".to_string() } else { trimmed.to_string() }
}

/// `~/.local/state/rozum/agent-rooms/<sanitized-room>` (or `$XDG_STATE_HOME`
/// equivalent) — the auto-derived per-room workdir when `--workdir` is not given.
/// Created on first use; a room keeps the SAME workdir across restarts, so the agent's
/// own edits from a prior turn are still there on the next one.
fn default_workdir(room: &str) -> PathBuf {
    rozum_paths::state_dir()
        .unwrap_or_else(|| rozum_paths::temp_dir().join("rozum"))
        .join("agent-rooms")
        .join(sanitize(room))
}

/// Render recent history as flat "Name: text" lines for a `claude -p`-style prompt
/// (one string, not a messages array). Drops presence/redaction noise the same way
/// `model_participant::build_context` does; keeps the agent's OWN prior replies too,
/// labeled by `handle`, so it has continuity across turns despite each invocation
/// being a fresh subprocess with no memory of its own.
fn build_prompt(history: &[StoredTurn], handle: &str, agent: &str, persona: Option<&str>) -> String {
    let mut lines = Vec::new();
    let start = history.len().saturating_sub(24);
    for t in &history[start..] {
        let c = t.content.trim_start();
        if c.starts_with("joined:") || c.starts_with("left:") || c.starts_with("[redacted") {
            continue;
        }
        lines.push(format!("{}: {}", base_handle(&t.display_name), t.content.trim()));
    }
    let intro = match persona {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => format!(
            "You are {handle}, an AI participant in a multi-person group chat, running as {agent} \
             with real file and shell access in your current working directory."
        ),
    };
    format!(
        "{intro}\n\nRecent conversation:\n{}\n\nRespond to the latest message as {handle}. If it \
         asks you to do something with files or run a command, actually do it in your working \
         directory, then briefly say what you did. Keep the reply short — this is a chat, not a \
         report.",
        lines.join("\n")
    )
}

/// Wall-clock budget for one agent invocation before it's killed and reported as a
/// timeout — a stuck `claude` call must not hang the room loop forever.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Cap on how much of a reply gets posted to the room — a runaway dump would flood a
/// chat (and Telegram has its own message-size limit).
const MAX_REPLY_BYTES: usize = 4000;

struct AgentReplier {
    agent: String,
    model: String,
    handle: String,
    workdir: PathBuf,
    persona: Option<String>,
    acl_path: Option<PathBuf>,
    timeout_secs: u64,
}

impl ReplyGenerator for AgentReplier {
    fn reply<'a>(
        &'a mut self,
        history: &'a [StoredTurn],
        turn: &'a StoredTurn,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            // Same ACL shape as `model_participant`: gate on the triggering messenger user's
            // grants. This participant has no "chat-only, no tools" fallback — it IS the tool —
            // so an ungated/unpermitted sender gets silence rather than a degraded mode.
            if let Some(path) = &self.acl_path {
                let allowed = match sender_id_from_content(&turn.content) {
                    Some(id) => super::super::messenger_acl::Acl::load(path).caps_for(id).shell,
                    None => false,
                };
                if !allowed {
                    return None;
                }
            }

            let prompt = build_prompt(history, &self.handle, &self.agent, self.persona.as_deref());
            match run_agent(&self.agent, &self.model, &self.workdir, &prompt, self.timeout_secs).await
            {
                Ok(text) if !text.trim().is_empty() => {
                    let text = text.trim();
                    let text = if text.len() > MAX_REPLY_BYTES {
                        format!("{}… (truncated)", &text[..MAX_REPLY_BYTES])
                    } else {
                        text.to_string()
                    };
                    Some(text)
                }
                Ok(_) => None,
                Err(e) => Some(e), // failures are LOUD in the room — see module doc
            }
        })
    }
}

/// Run one `rozum launch --model <model> --no-room-bridge <agent-invocation>` in
/// `workdir`, bounded by `timeout_secs`. `Ok(text)` is the agent's stdout (its final
/// answer, per `-p`/print-mode contract); `Err(msg)` is a human-readable failure
/// already formatted for posting to the room.
async fn run_agent(
    agent: &str,
    model: &str,
    workdir: &std::path::Path,
    prompt: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    if let Err(e) = tokio::fs::create_dir_all(workdir).await {
        return Err(format!("⚠ {agent}: couldn't create workdir {}: {e}", workdir.display()));
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let mut args: Vec<String> =
        vec!["launch".into(), "--model".into(), model.into(), "--n-ctx".into(), "32768".into()];
    // This participant IS the room presence — `rozum launch`'s own bridge would double it.
    args.push("--no-room-bridge".into());
    args.extend(agent_invocation(agent, prompt));

    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args(&args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Deterministic + focused, same as coders.rs's chat-turn recipe — a conversational
        // reply wants the model's best single answer, not sampled variety.
        .env("ROZUM_VERIFY", "0")
        .env("ROZUM_FORCE_GREEDY", "1");

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(format!("⚠ {agent}: failed to spawn: {e}")),
    };
    let output = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("⚠ {agent}: {e}")),
        Err(_) => return Err(format!("⚠ {agent} timed out after {timeout_secs}s")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "⚠ {agent} exited {}: {}",
            output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
            if tail.trim().is_empty() { "(no stderr)".into() } else { tail }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run the bridge: join `room` as `handle`, then answer turns per `policy` by actually
/// running `agent` (default `claude`) against `model` (a local gateway spec) in
/// `workdir` (auto-derived from `room` when `None`).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent: String,
    model: String,
    room: String,
    handle: String,
    policy: ReplyPolicy,
    peers: Vec<String>,
    persona: Option<String>,
    workdir: Option<PathBuf>,
    acl_path: Option<PathBuf>,
    mention_alias: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workdir = workdir.unwrap_or_else(|| default_workdir(&room));
    std::fs::create_dir_all(&workdir)?;
    eprintln!(
        "[agent-participant] '{handle}' will run '{agent}' (model {model}) in {}",
        workdir.display()
    );
    if let Some(path) = &acl_path {
        eprintln!("[agent-participant] gated by ACL {} (needs the shell capability)", path.display());
    }

    let replier = AgentReplier {
        agent: agent.clone(),
        model: model.clone(),
        handle: handle.clone(),
        workdir,
        persona,
        acl_path,
        timeout_secs: timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
    };
    let cfg = ParticipantConfig {
        room,
        handle,
        label: format!("agent {agent} via {model}"),
        policy,
        peers,
        mention_alias,
        identity_prefix: "agent-participant",
    };
    run_participant_loop(cfg, replier).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_punctuation_and_lowercases() {
        assert_eq!(sanitize("My Room #1"), "my-room-1");
        assert_eq!(sanitize("assistant"), "assistant");
        assert_eq!(sanitize("---"), "room");
        assert_eq!(sanitize(""), "room");
    }

    #[test]
    fn agent_invocation_matches_the_known_agents() {
        assert_eq!(
            agent_invocation("claude", "do X"),
            vec!["claude", "-p", "do X", "--dangerously-skip-permissions", "--max-turns", "40"]
        );
        assert_eq!(agent_invocation("nadia", "do X"), vec!["nadia", "run", "do X"]);
        assert_eq!(agent_invocation("mystery", "do X"), vec!["mystery", "do X"]);
    }

    #[test]
    fn build_prompt_drops_presence_noise_and_keeps_own_turns() {
        let mk = |name: &str, content: &str| StoredTurn {
            date: "2026-06-20".into(),
            n: 1,
            participant_id: "x".into(),
            display_name: name.into(),
            content: content.into(),
            ts: 0,
            ..Default::default()
        };
        let history = vec![
            mk("claude", "joined: I'm claude"),
            mk("Sergiy", "write hello.txt"),
            mk("claude", "done, wrote hello.txt"),
        ];
        let prompt = build_prompt(&history, "claude", "claude", None);
        assert!(!prompt.contains("joined:"));
        assert!(prompt.contains("Sergiy: write hello.txt"));
        assert!(prompt.contains("claude: done, wrote hello.txt"));
    }
}
