//! The blast radius. Every tool that touches the filesystem or spawns a process goes
//! through here first — the agent SDK never performs a side effect itself, so this is
//! the single place where "what the model asked for" becomes "what actually happens".
//!
//! Two mechanisms, deliberately independent:
//!
//! 1. **Path jail** ([`Sandbox::resolve`]) — every path argument is canonicalized and
//!    checked against the workspace root. Escapes are *refused*, never clamped: a
//!    clamped path silently writes somewhere the model did not intend, which is worse
//!    than an error the model can read and correct.
//! 2. **Exec confinement** ([`Sandbox::exec`]) — `bash` runs with cwd pinned to the
//!    root, a hard timeout, and (macOS) a seatbelt profile that denies writes outside
//!    the root and denies network unless opted in.
//!
//! The seatbelt profile confines **writes only**. Confining reads is what aborts dyld
//! before the child ever runs (paid for once already in this project) — and it buys
//! little, since the threat here is an agent mangling the machine, not exfiltration
//! from a process that already has the operator's shell.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the agent is allowed to act, and how far.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// Canonical workspace root. Nothing outside it is writable.
    root: PathBuf,
    /// Allow the confined shell to reach the network (`--allow-net`). Off by default:
    /// a coding task that needs the network is the exception, and an agent that can
    /// both write files and reach the network is a different risk class.
    pub allow_net: bool,
    /// Wrap `bash` in `sandbox-exec` (macOS). On by default; `--no-confine` turns it
    /// off for environments where the profile fights the toolchain.
    pub confine: bool,
    /// Default per-command wall-clock ceiling.
    pub timeout: Duration,
}

/// What a finished command produced. Mirrors `std::process::Output` minus the raw bytes.
pub struct Exec {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// True when the command was killed at the deadline rather than exiting on its own.
    pub timed_out: bool,
}

impl Sandbox {
    /// Pin the sandbox to `root`, which must already exist. The root is canonicalized
    /// once here so every later comparison is against a real path, not a spelling of one.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| format!("workspace root {}: {e}", root.as_ref().display()))?;
        if !root.is_dir() {
            return Err(format!("workspace root {} is not a directory", root.display()));
        }
        Ok(Self {
            root,
            allow_net: false,
            confine: cfg!(target_os = "macos"),
            timeout: Duration::from_secs(120),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path against the workspace, or refuse.
    ///
    /// The path need not exist yet (`write_file` creates files), so the canonical form is
    /// built from the deepest ancestor that *does* exist plus the remaining, lexically
    /// normalized tail. That closes the two escapes a naive `root.join(rel)` leaves open:
    /// `..` climbing out, and a symlinked ancestor pointing elsewhere.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        if rel.trim().is_empty() {
            return Err("path is empty".into());
        }
        let candidate = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            self.root.join(rel)
        };

        // Split into (deepest existing ancestor, remaining tail).
        let mut existing = candidate.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        let real = loop {
            match existing.canonicalize() {
                Ok(p) => break p,
                Err(_) => match (existing.parent(), existing.file_name()) {
                    (Some(parent), Some(name)) => {
                        tail.push(name);
                        existing = parent;
                    }
                    _ => return Err(format!("path {rel} does not resolve")),
                },
            }
        };
        let mut resolved = real;
        for name in tail.iter().rev() {
            resolved.push(name);
        }

        // Lexical normalization of the non-existent tail — `a/../../etc/passwd` must not
        // survive just because `a/..` was never canonicalized.
        let mut normalized = PathBuf::new();
        for c in resolved.components() {
            match c {
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(format!("path {rel} escapes the workspace"));
                    }
                }
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }

        if !normalized.starts_with(&self.root) {
            return Err(format!(
                "path {rel} is outside the workspace ({}) — refused",
                self.root.display()
            ));
        }
        Ok(normalized)
    }

    /// Run one shell command inside the workspace.
    ///
    /// The command string is passed to `bash -lc` verbatim: the model needs pipes,
    /// redirection and `&&` to be useful, and an allowlist of argv[0] does not survive
    /// contact with `cargo test 2>&1 | tail -20`. The containment is therefore the
    /// sandbox profile and the timeout, not command parsing — parsing a shell string to
    /// decide whether it is safe is a losing game.
    pub fn exec(&self, command: &str, timeout: Option<Duration>) -> Result<Exec, String> {
        let timeout = timeout.unwrap_or(self.timeout);
        // Say it once when confinement was wanted and this platform has none. `exec` runs per
        // command, so this is a one-time notice rather than a line per invocation — but it is not
        // silent, because "the sandbox is on" and "the sandbox exists here" are different claims
        // and the second one is false everywhere except macOS (BUG-044).
        if self.confine && !cfg!(target_os = "macos") {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "nadia: sandbox confinement was requested but this platform has none \
                     (seatbelt is macOS-only) — commands run UNCONFINED in {}",
                    self.root.display()
                );
            }
        }
        let mut cmd = if self.confine && cfg!(target_os = "macos") {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(self.seatbelt_profile()).arg("/bin/bash").arg("-lc").arg(command);
            c
        } else {
            let mut c = Command::new("/bin/bash");
            c.arg("-lc").arg(command);
            c
        };
        cmd.current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
        let deadline = Instant::now() + timeout;
        let timed_out = loop {
            match child.try_wait().map_err(|e| format!("wait: {e}"))? {
                Some(_) => break false,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        let out = child.wait_with_output().map_err(|e| format!("collect output: {e}"))?;
        Ok(Exec {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            // A killed child has no exit code; 124 is what `timeout(1)` reports, so the
            // model sees the convention it was trained on rather than a nadia-ism.
            exit_code: if timed_out { 124 } else { out.status.code().unwrap_or(-1) },
            timed_out,
        })
    }

    /// Deny writes outside the workspace; deny network unless opted in. Reads stay open
    /// (see the module note on dyld). The allowlist is the minimum a Rust toolchain needs
    /// to run at all: the cargo home (its package-cache lock is taken even for a build
    /// with no dependencies) and the per-user temp area every compiler writes through.
    ///
    /// `/tmp` is deliberately NOT on the list. It is world-writable and shared, so
    /// allowing it hands the agent a channel out of the workspace for free — and on
    /// macOS `/tmp` is a symlink to `/private/tmp`, so allowing either allows both.
    /// The toolchain does not need it: `$TMPDIR` is a private `/var/folders/…` path.
    ///
    /// **The root itself cannot be unlinked.** `(subpath root)` covers the directory node
    /// as well as its contents, so `rm -rf <root>` from inside the jail SUCCEEDED — measured
    /// 2026-08-04, a run that deleted its own workspace and then reported
    /// `verify command failed to run`, because there was no longer a directory to run it in.
    /// Deleting files inside is ordinary work and stays allowed; deleting the workspace is
    /// not work, and everything downstream of it — the check, the file list, the report —
    /// becomes meaningless the moment it happens.
    fn seatbelt_profile(&self) -> String {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let cargo_home =
            std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{home}/.cargo"));
        let net = if self.allow_net { "" } else { "(deny network*)" };
        format!(
            "(version 1)\
             (allow default)\
             (deny file-write*)\
             (allow file-write* (subpath \"{root}\") (subpath \"{cargo_home}\") \
                                (subpath \"{tmp}\") (subpath \"/private/var/folders\") \
                                (literal \"/dev/null\") (literal \"/dev/stdout\") \
                                (literal \"/dev/stderr\"))\
             (deny file-write-unlink (literal \"{root}\"))\
             {net}",
            root = self.root.display(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> (tempfile::TempDir, Sandbox) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let s = Sandbox::new(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn resolves_paths_inside_the_workspace() {
        let (_d, s) = sb();
        assert!(s.resolve("src/main.rs").is_ok());
        // A file that does not exist yet must still resolve — write_file creates it.
        assert!(s.resolve("src/new_module.rs").is_ok());
        assert!(s.resolve("deeply/nested/new.txt").is_ok());
    }

    #[test]
    fn refuses_escapes() {
        let (_d, s) = sb();
        for bad in ["../outside.txt", "src/../../outside.txt", "/etc/passwd", "  "] {
            assert!(s.resolve(bad).is_err(), "should have refused {bad:?}");
        }
    }

    #[test]
    fn refuses_rather_than_clamps() {
        // The failure mode this guards: a jail that strips `..` would turn
        // `../secrets` into `<root>/secrets` and write to a path nobody asked for.
        let (_d, s) = sb();
        let err = s.resolve("../secrets").unwrap_err();
        assert!(err.contains("escapes") || err.contains("outside"), "{err}");
    }

    #[test]
    fn exec_runs_in_the_workspace_and_reports_exit_code() {
        let (_d, mut s) = sb();
        s.confine = false; // the profile is exercised separately; this asserts the plumbing
        let ok = s.exec("pwd && echo marker", None).unwrap();
        assert_eq!(ok.exit_code, 0);
        assert!(ok.stdout.contains("marker"), "{}", ok.stdout);
        let bad = s.exec("exit 3", None).unwrap();
        assert_eq!(bad.exit_code, 3);
    }

    /// The confined profile is the DEFAULT on macOS, so a profile that fails to load, or
    /// that denies something the toolchain needs, breaks every `bash` call at once — and
    /// it fails as a plausible-looking command error, not as a nadia error. Assert both
    /// directions: work inside the root succeeds, writes outside it are denied.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_allows_the_workspace_and_denies_the_rest() {
        let (_d, s) = sb();
        assert!(s.confine, "confinement must be on by default on macOS");

        let inside = s.exec("echo hello > inside.txt && cat inside.txt", None).unwrap();
        assert_eq!(inside.exit_code, 0, "stderr: {}", inside.stderr);
        assert!(inside.stdout.contains("hello"), "{}", inside.stdout);

        let outside = s.exec("echo nope > /tmp/nadia-should-not-exist.txt", None).unwrap();
        assert_ne!(outside.exit_code, 0, "a write to /tmp escaped the sandbox");
        assert!(!std::path::Path::new("/tmp/nadia-should-not-exist.txt").exists());
    }

    /// macOS ONLY, and the reason is a gap rather than a test artifact: exec confinement is
    /// `sandbox-exec` (seatbelt), which exists on no other platform, so `confine` defaults to
    /// false off macOS and this property is simply not true there. Ungated, this test asserted
    /// a guarantee the platform does not make and kept the Linux CI job red from before
    /// 2026-08-14 — and a permanently red job is one nobody reads, which is how two REAL
    /// breakages went unnoticed the same day (BUG-043).
    ///
    /// The gap itself is `nadia-linux-confinement` in `BACKLOG.md`; on Linux an agent can still
    /// delete its own workspace, which is BUG-017 unfixed there.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_agent_cannot_delete_its_own_workspace() {
        // Measured 2026-08-04: `(subpath root)` covers the directory node itself, so an agent
        // told to reorganize a project ran `rm -rf` over its own root and succeeded. Everything
        // after that is nonsense — the acceptance check reports "verify command failed to run"
        // because there is no directory to run it in, and the work is simply gone.
        let (d, s) = sb();
        let root = d.path().to_path_buf();
        std::fs::write(root.join("keep.txt"), "x").unwrap();

        let r = s.exec(&format!("cd / && rm -rf {}", root.display()), None).unwrap();
        assert_ne!(r.exit_code, 0, "deleting the workspace root must be refused: {r:?}", r = r.stderr);
        assert!(root.is_dir(), "the workspace root must survive its own agent");

        // Ordinary work inside is untouched — deleting files it created is the job.
        let ok = s.exec("mkdir -p sub && echo y > sub/a.txt && rm -rf sub && echo done", None).unwrap();
        assert_eq!(ok.exit_code, 0, "stderr: {}", ok.stderr);
        assert!(!root.join("sub").exists());
    }

    #[test]
    fn exec_kills_at_the_deadline() {
        let (_d, mut s) = sb();
        s.confine = false;
        let r = s.exec("sleep 30", Some(Duration::from_millis(300))).unwrap();
        assert!(r.timed_out);
        assert_eq!(r.exit_code, 124);
    }
}
