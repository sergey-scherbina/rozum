//! Tier-3 "gateway piggyback" wakeup (`docs/specs/rozum-native-channels.md`).
//!
//! The last-resort wakeup for agents that support neither Tier-1 Claude-Code
//! channels nor a Tier-2 `wait_my_turn` loop: surface room activity by injecting
//! it into whatever model traffic the agent *does* send through our gateway.
//!
//! Two processes cooperate through a per-agent drop file keyed by **project +
//! agent name** (the same identity an agent carries in a room, `<project>-<agent>`):
//! the `mcp-proxy` (which holds the room connection) *appends* each new transcript
//! line; the launch-local HTTP proxy *drains* the project's drops into the next
//! chat request as an out-of-band system note. Reaches the agent at its next
//! inference call — never a truly idle one — which is why it is the last rung.
//!
//! On by default; disable per launch with `rozum launch --no-piggyback` or the
//! env `ROZUM_PIGGYBACK=0`. Room text enters the model context (a prompt-injection
//! surface), but only ever for an agent that has joined a meeting room — when no
//! room is joined nothing is ever written, so the default is inert for ordinary
//! use. `rozum launch` decides once and propagates the choice to both ends: it
//! threads the flag into the launch-local proxy (the reader) and exports the
//! matching `ROZUM_PIGGYBACK` to the agent, which the mcp-proxy writer inherits.

use std::path::{Path, PathBuf};

use super::room_path::rozum_runtime_dir;

/// Per-injection cap: at most this many bytes of pending room text reach the
/// model context, so a busy room cannot flood a single request.
const MAX_INJECT_BYTES: usize = 4096;
/// Per-agent drop-file cap: an undrained file (agent never makes a request) is
/// trimmed to its tail rather than growing without bound.
const MAX_FILE_BYTES: u64 = 16 * 1024;

/// Tier-3 is on by default; `ROZUM_PIGGYBACK` in `{0,false,off,no}` disables it.
/// This is the env half of the switch (read by the mcp-proxy writer); the
/// launch-local proxy reader is gated by an explicit flag threaded from
/// `rozum launch`, which also exports the matching env so both ends agree.
pub fn enabled() -> bool {
    !matches!(
        std::env::var("ROZUM_PIGGYBACK").ok().as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// The *explicit* `ROZUM_PIGGYBACK` setting, or `None` when unset/unrecognized so
/// the caller can apply its own default. `rozum launch` uses this to let an
/// operator override the automatic "off when Tier-1 channels are active" rule.
pub fn env_override() -> Option<bool> {
    match std::env::var("ROZUM_PIGGYBACK").ok().as_deref() {
        Some("0" | "false" | "off" | "no") => Some(false),
        Some("1" | "true" | "on" | "yes") => Some(true),
        _ => None,
    }
}

/// Basename of the current working directory — the project scope shared by the
/// launch process and the agent's mcp-proxy (both run in the project dir). Same
/// derivation as the room display-name prefix, so the keys line up.
pub fn project_slug() -> String {
    let raw = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.file_name().and_then(|s| s.to_str()).map(str::to_owned))
        .unwrap_or_default();
    sanitize(&raw, "project")
}

/// Replace path-hostile characters so a name is safe as a single path segment.
fn sanitize(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Root holding every project's drop dirs: `$XDG_RUNTIME_DIR/rozum/piggyback`.
fn piggyback_root() -> PathBuf {
    rozum_runtime_dir().join("piggyback")
}

fn project_dir(root: &Path, project: &str) -> PathBuf {
    root.join(project)
}

fn agent_file(root: &Path, project: &str, agent: &str) -> PathBuf {
    project_dir(root, project).join(format!("{}.log", sanitize(agent, "agent")))
}

/// Append one rendered room line (`"<from>: <text>"`, possibly multi-line) to
/// this agent's drop file. Best-effort: any IO error is swallowed — the wakeup
/// is additive over the always-correct `wait_my_turn` path. Trims the file to
/// its tail if it has grown past `MAX_FILE_BYTES` (agent isn't draining it).
pub fn append(project: &str, agent: &str, line: &str) {
    append_in(&piggyback_root(), project, agent, line);
}

fn append_in(root: &Path, project: &str, agent: &str, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let path = agent_file(root, project, agent);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
        trim_to_tail(&path);
    }
}

/// Keep only the trailing `MAX_FILE_BYTES` of a drop file (whole lines).
fn trim_to_tail(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else { return };
    let tail = tail_bytes(&content, MAX_FILE_BYTES as usize);
    let _ = std::fs::write(path, tail);
}

/// Drain every drop file under `project`, returning the pending lines (oldest
/// first), capped to `MAX_INJECT_BYTES` of the most recent text. Draining is a
/// rename-then-read so a concurrent `append` to the recreated file is preserved
/// for the next drain instead of being lost. Returns empty when nothing pends.
pub fn drain(project: &str) -> Vec<String> {
    drain_in(&piggyback_root(), project)
}

fn drain_in(root: &Path, project: &str) -> Vec<String> {
    let dir = project_dir(root, project);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        // Atomically claim the file: a rename on the same dir is atomic, so an
        // append racing us either lands before (we read it) or recreates the
        // original name (next drain gets it) — never a torn read.
        let taken = path.with_extension("draining");
        if std::fs::rename(&path, &taken).is_err() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&taken) {
            for l in content.lines() {
                if !l.trim().is_empty() {
                    lines.push(l.to_owned());
                }
            }
        }
        let _ = std::fs::remove_file(&taken);
    }
    cap_lines(lines, MAX_INJECT_BYTES)
}

/// Keep the most recent lines that fit in `max_bytes` (order preserved).
fn cap_lines(lines: Vec<String>, max_bytes: usize) -> Vec<String> {
    let mut total = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for l in lines.into_iter().rev() {
        let cost = l.len() + 1;
        if total + cost > max_bytes && !kept.is_empty() {
            break;
        }
        total += cost;
        kept.push(l);
    }
    kept.reverse();
    kept
}

/// Last `max_bytes` of `s`, snapped forward to a line boundary.
fn tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // `s.len() - max_bytes` can land INSIDE a multi-byte UTF-8 char (e.g. an em-dash `—`, 3 bytes) —
    // slicing `s[start..]` there panics ("byte index N is not a char boundary"), which on the
    // mcp-http bridge kills a tokio worker and drops the session (agents then reconnect and re-default
    // to the pinned project room — the room-flap). Walk `start` forward to the next char boundary
    // first; it drops at most a couple of leading bytes and we snap to the next line boundary anyway.
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let snap = s[start..].find('\n').map(|i| start + i + 1).unwrap_or(start);
    s[snap..].to_owned()
}

/// Wrap drained lines into the out-of-band system note injected into a request.
/// Clearly delimited and self-describing so the agent treats it as a preview,
/// not the turn API. `None` when there is nothing pending.
pub fn render_note(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut note = String::from(
        "[rozum] Room activity arrived while you were busy (you were not polling):\n",
    );
    for l in lines {
        note.push_str(l);
        note.push('\n');
    }
    note.push_str(
        "Use meeting.wait_my_turn to fetch the authoritative thread, then meeting.submit. \
         This note is a preview, not the turn API.",
    );
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unique temp root per test — no global env mutation, so these never race
    // the Unix-socket-path tests that read `XDG_RUNTIME_DIR`.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rozum_pb_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn append_then_drain_round_trips_oldest_first() {
        let root = temp_root("rt");
        append_in(&root, "projX", "alice", "bob: hello");
        append_in(&root, "projX", "alice", "carol: ping");
        let drained = drain_in(&root, "projX");
        assert_eq!(drained, vec!["bob: hello".to_string(), "carol: ping".to_string()]);
        // Second drain is empty — draining removed the file.
        assert!(drain_in(&root, "projX").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drain_coalesces_multiple_agents_in_a_project() {
        let root = temp_root("coalesce");
        append_in(&root, "p", "a1", "x: one");
        append_in(&root, "p", "a2", "y: two");
        let mut drained = drain_in(&root, "p");
        drained.sort();
        assert_eq!(drained, vec!["x: one".to_string(), "y: two".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cap_keeps_most_recent_within_budget() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i:03}")).collect();
        let kept = cap_lines(lines, 40);
        // Order preserved, only the tail kept, within budget.
        assert!(kept.len() < 100);
        assert_eq!(kept.last().unwrap(), "line 099");
        assert!(kept.iter().map(|l| l.len() + 1).sum::<usize>() <= 40);
    }

    #[test]
    fn render_note_is_delimited_and_lists_lines() {
        assert!(render_note(&[]).is_none());
        let note = render_note(&["bob: hi".to_string()]).unwrap();
        assert!(note.starts_with("[rozum]"));
        assert!(note.contains("bob: hi"));
        assert!(note.contains("meeting.wait_my_turn"));
    }

    #[test]
    fn tail_bytes_snaps_to_line_boundary() {
        let s = "aaaa\nbbbb\ncccc\n";
        let t = tail_bytes(s, 6);
        assert!(t.ends_with("cccc\n"));
        assert!(!t.contains("aaaa"));
    }

    #[test]
    fn tail_bytes_never_splits_a_multibyte_char() {
        // Regression: `—` (em-dash, 3 bytes) landing at the `s.len() - max_bytes` cut point used to
        // panic "byte index N is not a char boundary", killing an mcp-http worker → dropped session →
        // room-flap. Exercise EVERY cut position over a string full of em-dashes; none may panic and
        // each result must be a valid-UTF-8 suffix of the input.
        let s = "line one — with an em-dash\nsecond — line here\nthird — final line\n";
        for max in 1..=s.len() {
            let t = tail_bytes(s, max); // must not panic at any cut position
            assert!(s.ends_with(&t), "result must be a suffix of the input (max={max})");
        }
    }
}
