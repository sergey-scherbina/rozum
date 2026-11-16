//! `rozum setup` — is a plain `claude` on this machine actually getting what rozum offers?
//!
//! Distinct from [`crate::doctor`], which answers "is the demo path ready" and is read-only by
//! contract. This one answers a different question — "does an agent started from a console see
//! the meeting rooms, the retrieval, and the skills that describe them" — and it can repair what
//! it finds, because every gap it looks for has exactly one correct fix.
//!
//! The gaps are not hypothetical. When this was first written, three skills shipped in the repo
//! (`rag`, `replay`, `task-state`) had never been copied into `~/.claude/commands`, and a fourth
//! (`rozum`) was a stale copy predating a section added the same week. Nothing reported that: a
//! skill that is merely absent produces no error, just an agent that never knows the feature
//! exists. That silence is what this command exists to break.

use std::path::{Path, PathBuf};

use crate::doctor::{Check, CheckStatus, DoctorReport};

/// Where the skills are read FROM. The binary cannot know the checkout it was built in, so the
/// source tree is located rather than assumed: `ROZUM_PLUGINS_DIR` wins, else the plugin
/// submodule under the current git root. Not found is reported, never guessed at.
pub fn plugins_dir(cwd: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ROZUM_PLUGINS_DIR") {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let mut dir = cwd;
    loop {
        let candidate = dir.join("vendor/agent-plugins");
        if candidate.join(".claude-plugin").is_dir() || candidate.join("install.sh").is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Where skills are installed TO: the agent-neutral place (`~/.agents/skills`, read by opencode, Codex and others)
/// and Claude Code's own (`~/.claude/skills`). A skill is a directory with a `SKILL.md` (the open Agent Skills
/// format) — the same in both, tied to no agent.
pub fn skills_dirs() -> Vec<PathBuf> {
    let home = rozum_paths::home_dir().unwrap_or_else(|| PathBuf::from("."));
    vec![home.join(".agents").join("skills"), home.join(".claude").join("skills")]
}

/// Where the OLD installer copied skills, as Claude Code commands: a copy there is removed when the skill is
/// installed (it would show twice, and go stale).
pub fn legacy_commands_dir() -> PathBuf {
    rozum_paths::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".claude").join("commands")
}

/// What a single skill's installed copy is, relative to the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillState {
    /// Installed and byte-identical to the source.
    Current,
    /// Installed but different — an older copy, which is the WORSE failure of the two: the agent
    /// reads it and acts on instructions that have since changed.
    Stale,
    /// Never installed. The agent simply never learns the feature exists.
    Missing,
}

/// A skill's source file: `<name>/SKILL.md`, or — a checkout from before the Agent Skills layout —
/// `<name>/commands/<name>.md`.
fn source(plugins: &Path, name: &str) -> Option<PathBuf> {
    let skill = plugins.join(name).join("SKILL.md");
    if skill.is_file() {
        return Some(skill);
    }
    let old = plugins.join(name).join("commands").join(format!("{name}.md"));
    old.is_file().then_some(old)
}

/// Compare every skill in `plugins` against what is installed in `skills`.
///
/// Pure and directory-driven so it is testable without a home directory, and so adding a skill to
/// the repo needs no edit here — the recurring way a list like this goes wrong is by being a list.
pub fn skill_states(plugins: &Path, skills: &Path) -> Vec<(String, SkillState)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugins) else { return out };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| source(plugins, name).is_some())
        .collect();
    names.sort();
    for name in names {
        let Some(src) = source(plugins, &name) else { continue };
        let dst = skills.join(&name).join("SKILL.md");
        let state = match (std::fs::read(&src), std::fs::read(&dst)) {
            (Ok(a), Ok(b)) if a == b => SkillState::Current,
            (Ok(_), Ok(_)) => SkillState::Stale,
            (Ok(_), Err(_)) => SkillState::Missing,
            _ => continue,
        };
        out.push((name, state));
    }
    out
}

/// Install (or refresh) one skill in `skills`: a LINK to the source's directory (a pull of the checkout is then the
/// update — nothing to go stale), or, for a source in the old layout, a directory with a copy as its `SKILL.md`. A
/// link or a file in the way is replaced; a real directory keeps what else it holds. Returns the destination.
pub fn install_skill(plugins: &Path, skills: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(skills)?;
    let src = source(plugins, name)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("no skill {name}")))?;
    let dst = skills.join(name);
    let meta = std::fs::symlink_metadata(&dst);
    let is_dir = meta.as_ref().map(|m| m.file_type().is_dir()).unwrap_or(false);
    if meta.is_ok() && !is_dir {
        std::fs::remove_file(&dst)?;
    }
    if src.file_name().is_some_and(|f| f == "SKILL.md") && !is_dir {
        let target = std::fs::canonicalize(plugins.join(name))?;
        std::os::unix::fs::symlink(target, &dst)?;
    } else {
        std::fs::create_dir_all(&dst)?;
        std::fs::copy(&src, dst.join("SKILL.md"))?;
    }
    Ok(dst)
}

/// The skills half of the report for every place, and — when `fix` — the repair (the old copies in `legacy`
/// removed with it).
pub fn check_skills(plugins: Option<&Path>, dirs: &[PathBuf], legacy: &Path, fix: bool) -> Vec<Check> {
    let Some(plugins) = plugins else {
        return vec![Check::warn(
            "agent skills",
            "plugin source not found",
            "run from inside the rozum checkout, or set ROZUM_PLUGINS_DIR to the agent-plugins \
             directory",
        )];
    };
    dirs.iter().map(|skills| check_place(plugins, skills, legacy, fix)).collect()
}

fn check_place(plugins: &Path, skills: &Path, legacy: &Path, fix: bool) -> Check {
    // a check's name is static: the two places by name, any other as plain "agent skills"
    let label: &'static str = if skills.ends_with(".agents/skills") {
        "agent skills (~/.agents/skills)"
    } else if skills.ends_with(".claude/skills") {
        "agent skills (~/.claude/skills)"
    } else {
        "agent skills"
    };
    let states = skill_states(plugins, skills);
    if states.is_empty() {
        return Check::warn_bare(label, format!("no skills found under {}", plugins.display()));
    }
    let behind: Vec<String> = states
        .iter()
        .filter(|(_, s)| *s != SkillState::Current)
        .map(|(n, s)| format!("{n} ({})", if *s == SkillState::Stale { "stale" } else { "missing" }))
        .collect();
    if behind.is_empty() {
        return Check::ok(label, format!("{} installed and current", states.len()));
    }
    if !fix {
        return Check::fail(
            label,
            format!("{} of {} not current: {}", behind.len(), states.len(), behind.join(", ")),
            "rozum setup --fix",
        );
    }
    let mut failed = Vec::new();
    for (name, _) in states.iter().filter(|(_, s)| *s != SkillState::Current) {
        if let Err(e) = install_skill(plugins, skills, name) {
            failed.push(format!("{name}: {e}"));
        }
        let old = legacy.join(format!("{name}.md"));
        if std::fs::symlink_metadata(&old).is_ok_and(|m| m.file_type().is_file()) {
            let _ = std::fs::remove_file(&old);
        }
    }
    if failed.is_empty() {
        Check::ok(label, "installed/refreshed the ones that were behind")
    } else {
        Check::fail_bare(label, failed.join("; "))
    }
}

/// The whole report.
pub fn run(cwd: &Path, fix: bool) -> DoctorReport {
    let plugins = plugins_dir(cwd);
    let mut checks = check_skills(plugins.as_deref(), &skills_dirs(), &legacy_commands_dir(), fix);
    checks.extend(check_mcp());
    checks.extend(check_meetings());
    checks.extend(check_rag(cwd));
    checks.extend(check_launch_path());
    DoctorReport { checks }
}

/// Is the rozum MCP server registered for Claude Code — AND does the thing it names actually
/// exist?
///
/// Read from the agent's own config, because what matters is what the AGENT will load, not what we
/// believe we wrote. And checked past the registration: a first cut only looked for the name in the
/// file, which would pass for an entry pointing at a dead port or at a binary that has since moved.
/// "Registered" and "working" are different claims, and only the second one is what the operator
/// asked about.
fn check_mcp() -> Vec<Check> {
    let cfg = rozum_paths::home_dir().unwrap_or_default().join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&cfg) else {
        return vec![Check::warn(
            "rozum mcp server",
            format!("{} unreadable", cfg.display()),
            "rozum mcp install",
        )];
    };
    let Some(entry) = mcp_entry(&text) else {
        return Check::fail(
            "rozum mcp server",
            "not registered — the agent gets no meeting tools and no rag.search",
            "rozum mcp install",
        )
        .into_vec();
    };
    match entry {
        McpEntry::Http(url) => match probe_http(&url) {
            Some(code) => Check::ok(
                "rozum mcp server",
                format!("registered over http and answering ({url}, http {code})"),
            )
            .into_vec(),
            None => Check::fail(
                "rozum mcp server",
                format!("registered at {url} but nothing answers there — the tools will be absent"),
                "rozum service install, or check com.rozum.mcp-http",
            )
            .into_vec(),
        },
        McpEntry::Stdio(cmd) => {
            if which_exists(&cmd) {
                Check::ok("rozum mcp server", format!("registered over stdio ({cmd})")).into_vec()
            } else {
                Check::fail(
                    "rozum mcp server",
                    format!("registered as `{cmd}`, which is not on PATH — the tools will be absent"),
                    "rozum mcp install",
                )
                .into_vec()
            }
        }
    }
}

/// How the agent is configured to reach rozum's MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEntry {
    Http(String),
    Stdio(String),
}

/// The rozum entry in an agent's MCP config, parsed rather than grepped.
///
/// Grepping for the name was the first cut and it answers the wrong question: it is true of a
/// config that names rozum and points nowhere.
pub fn mcp_entry(config_json: &str) -> Option<McpEntry> {
    let v: serde_json::Value = serde_json::from_str(config_json).ok()?;
    let e = v.get("mcpServers")?.get("rozum")?;
    if let Some(url) = e.get("url").and_then(|u| u.as_str()) {
        return Some(McpEntry::Http(url.to_string()));
    }
    e.get("command")
        .and_then(|c| c.as_str())
        .map(|c| McpEntry::Stdio(c.to_string()))
}

/// Any HTTP answer means something is listening and speaking. An MCP endpoint answers `406` to a
/// plain GET (wrong `Accept`), which is a HEALTHY reply — treating only 2xx as alive would report
/// a working server as broken.
fn probe_http(url: &str) -> Option<u16> {
    let out = std::process::Command::new("curl")
        .args(["-s", "-m", "3", "-o", "/dev/null", "-w", "%{http_code}", url])
        .output()
        .ok()?;
    let code: u16 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (code > 0).then_some(code)
}

fn which_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The `rozum launch` path: it points the agent at the local gateway, so the gateway has to be
/// reachable and there has to be a model to serve. Warnings, not failures — a machine that only
/// ever uses the MCP tools with a cloud model is not broken for lacking these.
fn check_launch_path() -> Vec<Check> {
    let mut out = Vec::new();
    let port = crate::share::read_active().map(|g| g.port);
    out.push(match port.and_then(|p| probe_http(&format!("http://127.0.0.1:{p}/v1/models"))) {
        Some(code) if (200..300).contains(&code) => {
            Check::ok("gateway (for launch)", format!("answering on 127.0.0.1:{}", port.unwrap()))
        }
        Some(code) => Check::warn(
            "gateway (for launch)",
            format!("registered but answered http {code}"),
            "rozum gateway status",
        ),
        None => Check::warn(
            "gateway (for launch)",
            "not running — `rozum launch` will start one on demand",
            "rozum service install (keeps it warm)",
        ),
    });
    let installed = rozum_models::models::scan_all_installed();
    // An embedding model cannot answer a chat request; counting it would report a machine that
    // cannot serve `launch` at all as ready.
    let chat: Vec<&str> = installed
        .iter()
        .map(|m| m.spec.as_str())
        .filter(|s| !s.to_lowercase().contains("embedding"))
        .collect();
    out.push(if chat.is_empty() {
        Check::warn(
            "local model (for launch)",
            "no chat model on disk — `rozum launch` has nothing to serve",
            "rozum models pull <spec>  (rozum models list --remote)",
        )
    } else {
        Check::ok("local model (for launch)", format!("{} on disk", chat.len()))
    });
    out
}

fn check_meetings() -> Vec<Check> {
    let sock = crate::meeting::room_path::meeting_sock();
    if sock.exists() {
        Check::ok("meeting daemon", "socket present").into_vec()
    } else {
        Check::warn(
            "meeting daemon",
            "not running — rooms and the inbox are unavailable",
            "rozum meetings start",
        )
        .into_vec()
    }
}

fn check_rag(cwd: &Path) -> Vec<Check> {
    let idx = cwd.join(".rozum").join("rag-index.json");
    if idx.is_file() {
        Check::ok("rag index", "present for this project").into_vec()
    } else {
        Check::warn(
            "rag index",
            "absent — rag.search has nothing to answer from in this project",
            "rozum rag index",
        )
        .into_vec()
    }
}

trait IntoVec {
    fn into_vec(self) -> Vec<Check>;
}
impl IntoVec for Check {
    fn into_vec(self) -> Vec<Check> {
        vec![self]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(root: &Path, name: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    fn installed(dst: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dst.join(name)).unwrap();
        std::fs::write(dst.join(name).join("SKILL.md"), body).unwrap();
    }

    /// Behavior: the three states are distinguished, and STALE is distinguished from CURRENT.
    ///
    /// Missing is the loud one and stale is the dangerous one: the agent reads a stale skill and
    /// acts on instructions that have since changed. Both were live on this machine when this was
    /// written — three skills never installed and one predating a section added the same week —
    /// and nothing reported either, because an absent skill produces no error at all.
    #[test]
    fn skill_states_tell_missing_from_stale_from_current() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        skill(src.path(), "alpha", "same");
        skill(src.path(), "beta", "new text");
        skill(src.path(), "gamma", "never installed");
        installed(dst.path(), "alpha", "same");
        installed(dst.path(), "beta", "OLD text");

        let states = skill_states(src.path(), dst.path());
        assert_eq!(
            states,
            vec![
                ("alpha".to_string(), SkillState::Current),
                ("beta".to_string(), SkillState::Stale),
                ("gamma".to_string(), SkillState::Missing),
            ]
        );
    }

    /// Behavior: the check FAILS while anything is behind, and the fix makes it pass — including
    /// the stale one, which a copy-if-absent installer would silently leave wrong.
    #[test]
    fn fix_installs_the_missing_and_refreshes_the_stale() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        skill(src.path(), "alpha", "current");
        skill(src.path(), "beta", "new text");
        installed(dst.path(), "beta", "OLD text");
        let legacy = tempfile::tempdir().unwrap();
        std::fs::write(legacy.path().join("alpha.md"), "an old command copy").unwrap();
        let dirs = vec![dst.path().to_path_buf()];

        let before = check_skills(Some(src.path()), &dirs, legacy.path(), false);
        assert_eq!(before[0].status, CheckStatus::Fail, "{before:?}");
        assert!(before[0].detail.contains("alpha (missing)"), "{before:?}");
        assert!(before[0].detail.contains("beta (stale)"), "{before:?}");

        let fixed = check_skills(Some(src.path()), &dirs, legacy.path(), true);
        assert_eq!(fixed[0].status, CheckStatus::Ok, "{fixed:?}");
        assert_eq!(std::fs::read_to_string(dst.path().join("beta/SKILL.md")).unwrap(), "new text");
        assert_eq!(std::fs::read_to_string(dst.path().join("alpha/SKILL.md")).unwrap(), "current");
        // the missing one LINKED to its source: a change there is the installed skill at once
        assert!(std::fs::symlink_metadata(dst.path().join("alpha")).unwrap().file_type().is_symlink());
        std::fs::write(src.path().join("alpha/SKILL.md"), "edited").unwrap();
        assert_eq!(std::fs::read_to_string(dst.path().join("alpha/SKILL.md")).unwrap(), "edited");
        // the old command copy gone with it
        assert!(!legacy.path().join("alpha.md").exists());

        // And a second run is quiet: the report must not keep claiming work it already did.
        let again = check_skills(Some(src.path()), &dirs, legacy.path(), false);
        assert_eq!(again[0].status, CheckStatus::Ok, "{again:?}");
    }

    /// Behavior: a source that cannot be located is a WARNING naming what to do, not a silent
    /// pass. Reporting "all current" while having found no skills at all would be the worst
    /// possible answer — indistinguishable from a healthy machine.
    /// Behavior: a checkout from before the Agent Skills layout (`<name>/commands/<name>.md`) still installs — as a
    /// directory with that file as its SKILL.md.
    #[test]
    fn the_old_layout_still_installs() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let d = src.path().join("old").join("commands");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("old.md"), "old body").unwrap();
        assert_eq!(skill_states(src.path(), dst.path()), vec![("old".to_string(), SkillState::Missing)]);
        install_skill(src.path(), dst.path(), "old").unwrap();
        assert_eq!(std::fs::read_to_string(dst.path().join("old/SKILL.md")).unwrap(), "old body");
        assert_eq!(skill_states(src.path(), dst.path()), vec![("old".to_string(), SkillState::Current)]);
    }

    #[test]
    fn a_missing_plugin_source_is_reported_not_assumed_fine() {
        let dst = tempfile::tempdir().unwrap();
        let dirs = vec![dst.path().to_path_buf()];
        let none = check_skills(None, &dirs, dst.path(), false);
        assert_eq!(none[0].status, CheckStatus::Warn);
        assert!(none[0].hint.as_deref().unwrap_or_default().contains("ROZUM_PLUGINS_DIR"));

        let empty = tempfile::tempdir().unwrap();
        let no_skills = check_skills(Some(empty.path()), &dirs, dst.path(), false);
        assert_eq!(no_skills[0].status, CheckStatus::Warn, "an empty source is not 'all current'");
    }

    /// Behavior: the rozum MCP entry is PARSED, both transports, and absence is absence.
    ///
    /// The first cut grepped the config for the name, which answers a different question: it is
    /// true of a config that names rozum and points nowhere. "Registered" and "reachable" are
    /// separate claims and the operator asked about the second.
    #[test]
    fn the_mcp_entry_is_parsed_not_grepped() {
        let http = r#"{"mcpServers":{"rozum":{"type":"http","url":"http://127.0.0.1:8779/mcp"}}}"#;
        assert_eq!(mcp_entry(http), Some(McpEntry::Http("http://127.0.0.1:8779/mcp".into())));

        let stdio = r#"{"mcpServers":{"rozum":{"command":"rozum","args":["mcp-proxy"]}}}"#;
        assert_eq!(mcp_entry(stdio), Some(McpEntry::Stdio("rozum".into())));

        // Named elsewhere in the file, but not registered: a grep would have said yes.
        let decoy = r#"{"projects":{"/x":{"note":"rozum lives here"}},"mcpServers":{"other":{}}}"#;
        assert_eq!(mcp_entry(decoy), None);

        assert_eq!(mcp_entry("not json"), None);
        assert_eq!(mcp_entry("{}"), None);
    }

    /// Behavior: the plugin source is found by walking UP from the working directory, so the
    /// command works from any subdirectory of the checkout — which is where it will be run from.
    #[test]
    fn plugins_dir_walks_up_to_the_checkout() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("vendor/agent-plugins/.claude-plugin")).unwrap();
        let deep = root.path().join("crates").join("some-crate").join("src");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(plugins_dir(&deep), Some(root.path().join("vendor/agent-plugins")));

        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(plugins_dir(elsewhere.path()), None);
    }
}
