//! What makes a failure reproducible, gathered from a working tree — and what must never leave it.
//!
//! The rules and the reasoning are in `docs/specs/incident-repro.md`. In short: tracked diff only,
//! never untracked or ignored files; a secret in the diff REFUSES the capture rather than scrubbing
//! it; bounded and truncated loudly; manual, never automatic.

use std::path::Path;
use std::process::{Command, Stdio};

/// How much diff a permanent, append-only room will carry. Past this the metadata survives and the
/// body does not — and the bundle says so, because half a diff a reader cannot identify is worse
/// than none.
pub const DIFF_CAP_BYTES: usize = 256 * 1024;

/// The outcome of an attempted capture.
#[derive(Debug, PartialEq)]
pub enum Repro {
    /// Ready to post, as the body of one `event` message.
    Bundle(String),
    /// Refused, with the reason in the reporter's terms. Never posted, never partially posted.
    Refused(String),
}

/// Gather the repro for `workdir`, naming the failing command when the reporter knows it and
/// carrying the named environment variables (by EXACT name — never the environment).
pub fn capture(workdir: &Path, cmd: Option<&str>, env_names: &[String]) -> Repro {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(workdir)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    let Some(commit) = git(&["rev-parse", "HEAD"]) else {
        return Repro::Refused(format!(
            "not a git repository (or no commits yet): {}\nThis capture is a diff against a commit; \
             without one there is nothing bounded to take.",
            workdir.display()
        ));
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let origin = git(&["rev-parse", "origin/master"]);
    let ahead_behind = origin
        .as_deref()
        .and_then(|_| git(&["rev-list", "--left-right", "--count", "HEAD...origin/master"]));

    // TRACKED files only, staged and unstaged. `git diff HEAD` is exactly that set: it never sees an
    // untracked or ignored file, which is the whole safety property and the reason not to assemble
    // this by walking the directory.
    let diff = git(&["diff", "HEAD"]).unwrap_or_default();
    // Named separately so the bundle can say what it did NOT take, rather than leaving the reader to
    // assume the tree was clean.
    let untracked = git(&["ls-files", "--others", "--exclude-standard"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);

    if let Some(hit) = first_secret(&diff) {
        return Repro::Refused(format!(
            "refused: the tracked diff looks like it contains a secret — {hit}\n\
             Nothing was posted. Redaction would NOT make this safe: `meeting redact` hides content \
             at read time and the bytes stay in the room's log on disk, so the only safe moment to \
             stop is before the write.\n\
             Remove it from the diff (or capture from a tree without it) and run this again."
        ));
    }

    let mut env_lines = Vec::new();
    for name in env_names {
        match std::env::var(name) {
            Ok(v) if first_secret(&format!("{name}={v}")).is_some() => {
                return Repro::Refused(format!(
                    "refused: --env {name} looks like a secret. Nothing was posted. Name only the \
                     variables that change BEHAVIOUR (a flag, a path, a mode); a credential is not \
                     part of a repro."
                ));
            }
            Ok(v) => env_lines.push(format!("  {name}={v}")),
            Err(_) => env_lines.push(format!("  {name}=(unset)")),
        }
    }

    let mut b = String::from("repro capture\n");
    b.push_str(&format!("  workdir: {}\n", workdir.display()));
    b.push_str(&format!("  commit:  {commit}{}\n", if branch.is_empty() { String::new() } else { format!(" ({branch})") }));
    if let Some(ab) = ahead_behind {
        let mut it = ab.split_whitespace();
        if let (Some(ahead), Some(behind)) = (it.next(), it.next()) {
            b.push_str(&format!("  vs origin/master: {ahead} ahead, {behind} behind\n"));
        }
    }
    if let Some(c) = cmd {
        b.push_str(&format!("  command: {c}\n"));
    }
    if !env_lines.is_empty() {
        b.push_str("  env (named explicitly):\n");
        for l in &env_lines {
            b.push_str(l);
            b.push('\n');
        }
    }
    // Say what was left behind. A bundle that mentions only what it took reads as complete.
    if untracked > 0 {
        b.push_str(&format!(
            "  NOT captured: {untracked} untracked/ignored file(s) — never included, by policy \
             (docs/specs/incident-repro.md)\n"
        ));
    }

    if diff.trim().is_empty() {
        b.push_str("  tracked diff: none — the tree matches the commit above\n");
        return Repro::Bundle(b);
    }
    if diff.len() > DIFF_CAP_BYTES {
        b.push_str(&format!(
            "  tracked diff: {} bytes — OVER the {DIFF_CAP_BYTES}-byte cap, so the body is not \
             included. Files in it:\n",
            diff.len()
        ));
        for f in git(&["diff", "--name-only", "HEAD"]).unwrap_or_default().lines() {
            b.push_str(&format!("    {f}\n"));
        }
        return Repro::Bundle(b);
    }
    b.push_str(&format!("  tracked diff: {} bytes\n\n{diff}\n", diff.len()));
    Repro::Bundle(b)
}

/// The first thing in `text` that looks like a credential, described in a way a reporter can act on.
///
/// Heuristic AND SAID TO BE: callers must print that a clean scan is not proof. The point is not to
/// be exhaustive — it cannot be — but to stop the obvious cases before a permanent write.
pub fn first_secret(text: &str) -> Option<String> {
    for (n, line) in text.lines().enumerate() {
        let at = || format!("line {}", n + 1);
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            return Some(format!("a private key block at {}", at()));
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("authorization:") && lower.contains("bearer ") {
            return Some(format!("an Authorization: Bearer header at {}", at()));
        }
        // `NAME=value` / `NAME: value` where the name says credential and the value is not obviously
        // a placeholder. Diff markers are stripped so a `+TOKEN=…` line is caught like any other.
        let body = line.trim_start_matches(['+', '-', ' ']);
        if let Some((name, value)) = body.split_once(['=', ':']) {
            let n_low = name.trim().to_ascii_lowercase();
            let looks_credential = ["token", "secret", "password", "passwd", "api_key", "apikey", "access_key", "private_key"]
                .iter()
                .any(|k| n_low.ends_with(k) || n_low.contains(k));
            let v = value.trim().trim_matches(['"', '\'', ',']);
            // A placeholder is what a repo is FULL of, and refusing on those would make the guard
            // useless by making it constant. Length plus "not obviously a stand-in" is the line.
            let placeholder = v.is_empty()
                || v.len() < 12
                || v.contains("...")
                || v.contains("<")
                || v.contains("$")
                || v.eq_ignore_ascii_case("none")
                || v.to_ascii_lowercase().contains("example")
                || v.to_ascii_lowercase().contains("placeholder")
                || v.to_ascii_lowercase().contains("changeme")
                || v.to_ascii_lowercase().contains("your_");
            if looks_credential && !placeholder {
                return Some(format!("`{}` assigned a {}-character value at {}", name.trim(), v.len(), at()));
            }
        }
        // A Telegram bot token has a shape of its own, and this project has leaked one before:
        // `<digits>:<35+ base64-ish>`. Worth catching by shape, not only by variable name.
        if let Some((left, right)) = body.split_once(':') {
            // The TRAILING digit run, not the whole left side: in the real leak the line was
            // `TELEGRAM="8412345678:AAH…"`, so anchoring at the start found `TELEGRAM="84…` and
            // matched nothing. The token's digits are what sits immediately before the colon.
            let digits: String = left
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let digits = digits.as_str();
            let tail: String = right.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if digits.len() >= 8 && digits.chars().all(|c| c.is_ascii_digit()) && tail.len() >= 30 {
                return Some(format!("something shaped like a Telegram bot token at {}", at()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    fn repo(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rozum-repro-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q", "-b", "master", "."]);
        git(&d, &["config", "user.email", "t@t"]);
        git(&d, &["config", "user.name", "t"]);
        std::fs::write(d.join("a.rs"), "fn main() {}\n").unwrap();
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-qm", "one"]);
        d
    }

    /// The safety property of the whole feature, and the one worth a test that reads the OUTPUT
    /// rather than trusting the flag: an untracked file must not appear in the bundle, and its
    /// absence must be stated rather than left to look like a clean tree.
    #[test]
    fn untracked_files_never_enter_the_bundle_and_their_absence_is_said() {
        let d = repo("untracked");
        std::fs::write(d.join("a.rs"), "fn main() { println!(\"changed\") }\n").unwrap();
        std::fs::write(d.join(".env"), "SUPER_SECRET_TOKEN=abcdef0123456789abcdef\n").unwrap();
        let Repro::Bundle(b) = capture(&d, Some("cargo test"), &[]) else {
            panic!("expected a bundle");
        };
        assert!(b.contains("changed"), "the tracked change must be in it");
        assert!(!b.contains("SUPER_SECRET_TOKEN"), "an untracked file must NEVER be in it");
        assert!(b.contains("NOT captured: 1 untracked"), "silence would read as a clean tree");
        assert!(b.contains("command: cargo test"));
        std::fs::remove_dir_all(&d).ok();
    }

    /// A secret in a TRACKED file is the case redaction cannot save, so the capture must not happen
    /// at all — not a scrubbed bundle, not a partial one.
    #[test]
    fn a_secret_in_the_tracked_diff_refuses_the_whole_capture() {
        let d = repo("secret");
        std::fs::write(d.join("a.rs"), "const API_KEY: &str = \"sk-live-9f2b7c1d4e6a8b0c2d4f6a8b\";\n").unwrap();
        let out = capture(&d, None, &[]);
        match out {
            Repro::Refused(why) => {
                assert!(why.contains("secret"), "{why}");
                assert!(why.contains("redact"), "must say why redaction is not the answer: {why}");
            }
            Repro::Bundle(b) => panic!("must refuse, got a bundle:\n{b}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// Placeholders are what a repository is full of. A guard that fires on them is a guard that
    /// gets disabled.
    #[test]
    fn placeholders_do_not_trip_the_guard() {
        for line in [
            "TOKEN=",
            "API_KEY=<your_key_here>",
            "password: changeme",
            "SECRET=$ROZUM_SECRET",
            "api_key = \"example-key-value\"",
            "token: none",
        ] {
            assert_eq!(first_secret(line), None, "false positive on {line:?}");
        }
        assert!(first_secret("-----BEGIN OPENSSH PRIVATE KEY-----").is_some());
        assert!(first_secret("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9").is_some());
        assert!(
            first_secret("TELEGRAM=\"8412345678:AAH9xQwErTyUiOpAsDfGhJkLzXcVbNm1234\"").is_some(),
            "a bot token has a shape of its own, and this project has leaked one"
        );
    }

    /// A cap that truncates silently is worse than no capture: the reader cannot tell which half
    /// they are missing.
    #[test]
    fn an_oversized_diff_is_dropped_loudly_with_its_file_list() {
        let d = repo("big");
        let big: String = std::iter::repeat("// filler line to make this diff large\n")
            .take(DIFF_CAP_BYTES / 30)
            .collect();
        std::fs::write(d.join("a.rs"), big).unwrap();
        let Repro::Bundle(b) = capture(&d, None, &[]) else { panic!("expected a bundle") };
        assert!(b.contains("OVER the"), "must say it was dropped: {}", &b[..b.len().min(400)]);
        assert!(b.contains("a.rs"), "must name the files that were in it");
        assert!(!b.contains("filler line"), "the body must not be there");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Not-a-repo is refused with the reason, not captured as an empty bundle that reads like a
    /// clean tree.
    #[test]
    fn a_directory_that_is_not_a_repo_is_refused_with_the_reason() {
        let d = std::env::temp_dir().join(format!("rozum-repro-plain-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        match capture(&d, None, &[]) {
            Repro::Refused(why) => assert!(why.contains("not a git repository"), "{why}"),
            Repro::Bundle(b) => panic!("must refuse:\n{b}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }
}
