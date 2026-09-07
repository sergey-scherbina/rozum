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

/// Where a skill is installed TO for Claude Code.
pub fn commands_dir() -> PathBuf {
    rozum_paths::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("commands")
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

/// Compare every skill in `plugins` against what is installed in `commands`.
///
/// Pure and directory-driven so it is testable without a home directory, and so adding a skill to
/// the repo needs no edit here — the recurring way a list like this goes wrong is by being a list.
pub fn skill_states(plugins: &Path, commands: &Path) -> Vec<(String, SkillState)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugins) else { return out };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            e.path().join("commands").join(format!("{name}.md")).is_file().then_some(name)
        })
        .collect();
    names.sort();
    for name in names {
        let src = plugins.join(&name).join("commands").format_md(&name);
        let dst = commands.join(format!("{name}.md"));
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

trait JoinMd {
    fn format_md(&self, name: &str) -> PathBuf;
}
impl JoinMd for PathBuf {
    fn format_md(&self, name: &str) -> PathBuf {
        self.join(format!("{name}.md"))
    }
}

/// Install (or refresh) one skill. Returns the destination on success.
pub fn install_skill(plugins: &Path, commands: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(commands)?;
    let src = plugins.join(name).join("commands").format_md(name);
    let dst = commands.join(format!("{name}.md"));
    std::fs::copy(&src, &dst)?;
    Ok(dst)
}

/// The skills half of the report, and — when `fix` — the repair.
pub fn check_skills(plugins: Option<&Path>, commands: &Path, fix: bool) -> Vec<Check> {
    let Some(plugins) = plugins else {
        return vec![Check::warn(
            "agent skills",
            "plugin source not found",
            "run from inside the rozum checkout, or set ROZUM_PLUGINS_DIR to the agent-plugins \
             directory",
        )];
    };
    let states = skill_states(plugins, commands);
    if states.is_empty() {
        return vec![Check::warn_bare(
            "agent skills",
            format!("no skills found under {}", plugins.display()),
        )];
    }
    let mut behind: Vec<String> = states
        .iter()
        .filter(|(_, s)| *s != SkillState::Current)
        .map(|(n, s)| format!("{n} ({})", if *s == SkillState::Stale { "stale" } else { "missing" }))
        .collect();
    if behind.is_empty() {
        return vec![Check::ok(
            "agent skills",
            format!("{} installed and current", states.len()),
        )];
    }
    if !fix {
        return vec![Check::fail(
            "agent skills",
            format!("{} of {} not current: {}", behind.len(), states.len(), behind.join(", ")),
            "rozum setup --fix",
        )];
    }
    let mut failed = Vec::new();
    for (name, state) in states.iter().filter(|(_, s)| *s != SkillState::Current) {
        if let Err(e) = install_skill(plugins, commands, name) {
            failed.push(format!("{name}: {e}"));
        }
        let _ = state;
    }
    behind.clear();
    if failed.is_empty() {
        vec![Check::ok("agent skills", "installed/refreshed the ones that were behind")]
    } else {
        vec![Check::fail_bare("agent skills", failed.join("; "))]
    }
}

/// The whole report.
pub fn run(cwd: &Path, fix: bool) -> DoctorReport {
    let plugins = plugins_dir(cwd);
    let commands = commands_dir();
    let mut checks = check_skills(plugins.as_deref(), &commands, fix);
    checks.extend(check_mcp());
    checks.extend(check_meetings());
    checks.extend(check_rag(cwd));
    DoctorReport { checks }
}

/// Is the rozum MCP server registered for Claude Code? Read from the agent's own config rather
/// than from ours: what matters is what the AGENT will load, not what we believe we wrote.
fn check_mcp() -> Vec<Check> {
    let cfg = rozum_paths::home_dir().unwrap_or_default().join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&cfg) else {
        return vec![Check::warn(
            "rozum mcp server",
            format!("{} unreadable", cfg.display()),
            "rozum mcp install",
        )];
    };
    if text.contains("\"rozum\"") {
        Check::ok("rozum mcp server", "registered for claude").into_vec()
    } else {
        Check::fail(
            "rozum mcp server",
            "not registered — the agent gets no meeting tools and no rag.search",
            "rozum mcp install",
        )
        .into_vec()
    }
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
        let d = root.join(name).join("commands");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{name}.md")), body).unwrap();
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
        std::fs::write(dst.path().join("alpha.md"), "same").unwrap();
        std::fs::write(dst.path().join("beta.md"), "OLD text").unwrap();

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
        std::fs::write(dst.path().join("beta.md"), "OLD text").unwrap();

        let before = check_skills(Some(src.path()), dst.path(), false);
        assert_eq!(before[0].status, CheckStatus::Fail, "{before:?}");
        assert!(before[0].detail.contains("alpha (missing)"), "{before:?}");
        assert!(before[0].detail.contains("beta (stale)"), "{before:?}");

        let fixed = check_skills(Some(src.path()), dst.path(), true);
        assert_eq!(fixed[0].status, CheckStatus::Ok, "{fixed:?}");
        assert_eq!(std::fs::read_to_string(dst.path().join("beta.md")).unwrap(), "new text");
        assert_eq!(std::fs::read_to_string(dst.path().join("alpha.md")).unwrap(), "current");

        // And a second run is quiet: the report must not keep claiming work it already did.
        let again = check_skills(Some(src.path()), dst.path(), false);
        assert_eq!(again[0].status, CheckStatus::Ok, "{again:?}");
    }

    /// Behavior: a source that cannot be located is a WARNING naming what to do, not a silent
    /// pass. Reporting "all current" while having found no skills at all would be the worst
    /// possible answer — indistinguishable from a healthy machine.
    #[test]
    fn a_missing_plugin_source_is_reported_not_assumed_fine() {
        let dst = tempfile::tempdir().unwrap();
        let none = check_skills(None, dst.path(), false);
        assert_eq!(none[0].status, CheckStatus::Warn);
        assert!(none[0].hint.as_deref().unwrap_or_default().contains("ROZUM_PLUGINS_DIR"));

        let empty = tempfile::tempdir().unwrap();
        let no_skills = check_skills(Some(empty.path()), dst.path(), false);
        assert_eq!(no_skills[0].status, CheckStatus::Warn, "an empty source is not 'all current'");
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
