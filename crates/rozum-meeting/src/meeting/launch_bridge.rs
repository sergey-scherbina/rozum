//! Room presence for an agent that has **no MCP client at all** — the launch-side writer of Tier 3.
//!
//! `docs/specs/rozum-native-channels.md` builds the whole wakeup ladder on the mcp-proxy: it is what
//! holds the room connection, and it is what appends room deltas to the piggyback drop file the
//! launch-local proxy injects. An agent without an MCP client (nadia) therefore fell off the bottom
//! of the ladder — not because Tier 3 does not fit it (the injection point is our OWN gateway proxy,
//! which is agent-agnostic and needs nothing from the client) but because nothing was there to WRITE
//! the drops. This module is that writer, moved into `rozum launch` — the one process that is
//! already alive for the whole run, outlives every repair round, and knows how the run ended.
//!
//! It is deliberately not "MCP for nadia". Registering `rozum mcp-proxy` in an agent that cannot
//! speak MCP would be a config entry nothing reads; what the room actually needs from a participant
//! is two things, and both are doable from outside the agent:
//!
//! - **outward** — the `working:` / `done:` lines `AGENTS.md` asks of every agent, so the human sees
//!   a phone-launched run start and finish instead of silence;
//! - **inward** — every turn from someone ELSE appended to the drop file, `‹for you›`-prefixed when
//!   it addresses this handle, which is byte-for-byte what the mcp-proxy writer produces. From
//!   inside the model context the two paths are indistinguishable.
//!
//! What it is NOT: a way for the agent to answer. nadia cannot post back mid-turn (it has six tools
//! and none of them is a room), so the human gets presence and steering, not a conversation. Saying
//! that plainly here is cheaper than an operator discovering it by waiting for a reply.

use tokio::task::JoinHandle;

use super::piggyback;
use super::tui_client::MeetingClient;

/// Longest task text carried into the `working:` line. A room line is read at a glance on a phone;
/// the full prompt is in the coder log, which is one tap away.
const TASK_SUMMARY_MAX: usize = 160;

/// A live room presence for one `rozum launch` run. Dropping it without [`RoomBridge::finish`]
/// leaves the room without a closing line — the caller owns saying how the run ended.
pub struct RoomBridge {
    client: MeetingClient,
    /// The dedicated long-poll connection, and the task draining it into drop files.
    poll: JoinHandle<()>,
    pump: JoinHandle<()>,
    handle: String,
    room: String,
}

impl RoomBridge {
    /// The room this run announced itself in (for the launch's own stderr line).
    pub fn room(&self) -> &str {
        &self.room
    }

    /// The handle the run posts under — what the human types to address it.
    pub fn handle(&self) -> &str {
        &self.handle
    }

    /// Post the closing line and tear the connections down. Best-effort: a daemon that died
    /// mid-run must not turn a finished agent run into a failed one.
    pub async fn finish(mut self, outcome: &str) {
        self.pump.abort();
        self.poll.abort();
        let _ = self.client.submit(outcome).await;
    }
}

/// Join the cwd project's room as `agent` and start bridging it, or `None` when there is no room to
/// join (no project, no daemon, connect refused). Every failure is silent by design: room presence
/// is additive, and an agent run must never fail because a meeting daemon was down.
///
/// `inject` is the piggyback decision `rozum launch` already resolved — when it is off (`--no-piggyback`,
/// which is what `scripts/bench/agentic.sh` passes) NOTHING is written to the drop files, so a
/// benchmark cell can never have room chatter injected into the context it is being measured on.
/// The outward `working:`/`done:` lines are independent of it: they change no model input.
pub async fn start(agent: &str, task: Option<&str>, inject: bool) -> Option<RoomBridge> {
    let sock = super::room_path::meeting_sock();
    if !super::daemon::daemon_alive(&sock).await {
        super::daemon_proxy::spawn_daemon().await;
    }
    let handle = handle_for(agent);
    let project = super::daemon_proxy::detect_project()?;
    let mut client = MeetingClient::connect(&sock, &handle).await.ok()?;
    let room = client.enter_project(&project).await.ok()?;

    let _ = client.submit(&working_line(&handle, task)).await;

    // The poll connection reuses this client's session token, so both sockets bind to ONE roster
    // participant — which is what makes suppressing our own `working:`/`done:` lines by
    // participant id reliable rather than a guess about display names.
    let (mut rx, poll) = client.spawn_poll();
    let self_pid = client.participant_id().map(str::to_owned);
    let agent_key = agent.to_owned();
    let mention_handle = handle.clone();
    let pump = tokio::spawn(async move {
        let project = piggyback::project_slug();
        while let Some(turns) = rx.recv().await {
            if !inject {
                continue;
            }
            for t in turns {
                if self_pid.as_deref() == Some(t.participant_id.as_str()) {
                    continue;
                }
                let Some(line) = render_turn(&t.display_name, &t.participant_id, &t.content, &mention_handle)
                else {
                    continue;
                };
                piggyback::append(&project, &agent_key, &line);
            }
        }
    });

    Some(RoomBridge { client, poll, pump, handle, room })
}

/// The handle a launched agent posts under: `$ROZUM_MEETING_AS` when the caller named it (the UCC
/// can give two concurrent runs distinct names), else the agent's own name — the same key the
/// piggyback drop file uses, so a reader of the room and a reader of the drops see one identity.
fn handle_for(agent: &str) -> String {
    std::env::var("ROZUM_MEETING_AS")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| agent.to_owned())
}

/// `working: <handle> — <task>`; the task clipped to one glanceable line. An interactive launch has
/// no task (the REPL gets one typed into it later), and says so rather than inventing one.
fn working_line(handle: &str, task: Option<&str>) -> String {
    match task.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("working: {handle} — {}", clip(t, TASK_SUMMARY_MAX)),
        None => format!("working: {handle} — interactive session"),
    }
}

/// Clip to `max` chars on a char boundary, marking that it was clipped (silent truncation is how a
/// reader ends up confidently wrong about what was asked).
fn clip(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let head: String = one_line.chars().take(max).collect();
    format!("{head}…")
}

/// One room turn as the drop-file line the launch-local proxy injects: `"<from>: <text>"`, with the
/// `‹for you›` prefix when it addresses this handle — identical to the mcp-proxy writer's rendering
/// (`daemon_proxy::render_stored_delta` + the mention prefix), because the model must not be able to
/// tell which writer produced a note.
fn render_turn(display_name: &str, participant_id: &str, content: &str, handle: &str) -> Option<String> {
    if content.trim().is_empty() {
        return None;
    }
    let from = if display_name.is_empty() { participant_id } else { display_name };
    let line = format!("{from}: {content}");
    if super::mention::addresses(content, handle) {
        Some(format!("‹for you› {line}"))
    } else {
        Some(line)
    }
}

/// How a finished run reports itself. `verified` is the launch's verify-gate verdict (`None` when
/// the project had no gate to run), `code` the agent's exit code.
pub fn outcome_line(handle: &str, verified: Option<bool>, code: i32) -> String {
    match verified {
        Some(true) => format!("done: {handle} — verify passed"),
        Some(false) => format!("blocked: {handle} — verify failed (rc={code})"),
        None if code == 0 => format!("done: {handle} — finished (rc=0)"),
        None => format!("blocked: {handle} — exited rc={code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_line_clips_and_names_the_interactive_case() {
        assert_eq!(working_line("nadia", Some("fix the flaky test")), "working: nadia — fix the flaky test");
        assert_eq!(working_line("nadia", None), "working: nadia — interactive session");
        assert_eq!(working_line("nadia", Some("   ")), "working: nadia — interactive session");
        // Multi-line prompts collapse to one room line.
        assert_eq!(working_line("nadia", Some("do X\nthen Y")), "working: nadia — do X then Y");
        let long = "x".repeat(TASK_SUMMARY_MAX + 40);
        let line = working_line("nadia", Some(&long));
        assert!(line.ends_with('…'), "a clipped task must SAY it was clipped: {line}");
        assert_eq!(line.chars().count(), "working: nadia — ".chars().count() + TASK_SUMMARY_MAX + 1);
    }

    #[test]
    fn rendered_turn_matches_the_mcp_writer_and_flags_mentions() {
        assert_eq!(render_turn("alice", "p1", "hello", "nadia").unwrap(), "alice: hello");
        // No display name → the participant id, same fallback as render_stored_delta.
        assert_eq!(render_turn("", "p1", "hello", "nadia").unwrap(), "p1: hello");
        assert_eq!(
            render_turn("alice", "p1", "@nadia stop", "nadia").unwrap(),
            "‹for you› alice: @nadia stop"
        );
        assert!(render_turn("alice", "p1", "   ", "nadia").is_none());
    }

    #[test]
    fn outcome_lines_distinguish_verified_from_merely_exited() {
        assert_eq!(outcome_line("nadia", Some(true), 0), "done: nadia — verify passed");
        assert_eq!(outcome_line("nadia", Some(false), 3), "blocked: nadia — verify failed (rc=3)");
        assert_eq!(outcome_line("nadia", None, 0), "done: nadia — finished (rc=0)");
        assert_eq!(outcome_line("nadia", None, 2), "blocked: nadia — exited rc=2");
    }

    #[test]
    fn handle_prefers_an_explicit_name() {
        // Serial with the env var: set → explicit wins; unset → the agent's own name.
        unsafe { std::env::set_var("ROZUM_MEETING_AS", "nadia-phone") };
        assert_eq!(handle_for("nadia"), "nadia-phone");
        unsafe { std::env::set_var("ROZUM_MEETING_AS", "  ") };
        assert_eq!(handle_for("nadia"), "nadia");
        unsafe { std::env::remove_var("ROZUM_MEETING_AS") };
        assert_eq!(handle_for("nadia"), "nadia");
    }
}
