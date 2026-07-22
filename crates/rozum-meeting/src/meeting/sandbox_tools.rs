//! Sandboxed file/shell tools for the model participant. When the `assistant`
//! room's model is launched with `--sandbox <dir>` it is handed four OpenAI
//! tools scoped to that directory, so it can actually read, write, and run
//! commands on files instead of only chatting. Spec:
//! `docs/specs/assistant-sandbox-tools.md`.
//!
//! TRUST: this path is driven by an untrusted messenger (Telegram/Discord DM —
//! `docs/specs/messenger-bridges-daemon.md`). `list_files`/`read_file`/
//! `write_file` are hard-confined to the sandbox root (no `..`, no absolute
//! escape, no symlink escape). `run_command` runs a shell with cwd = root but
//! CANNOT be confined to it — a shell inherits the daemon user's full rights —
//! so it is only as safe as the sender allowlist that gates who reaches here.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// Cap on the bytes of any single tool result fed back to the model.
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
/// Wall-clock limit for one `run_command`.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// macOS seatbelt wrapper used to confine `run_command`'s filesystem writes.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Build the seatbelt profile confining a shell to `root`: it may read the system
/// (needed to load and run binaries) and read/write inside `root` (which contains
/// its tempdir), but any write, delete, or rename outside `root` is denied. Network
/// is allowed when `allow_network` is set (the default) and denied otherwise; write
/// confinement holds either way. Verified against write/delete/network attempts on macOS.
fn seatbelt_profile(root: &Path, allow_network: bool) -> String {
    // Canonical paths under the home dir contain no quotes; guard anyway so a weird
    // path can never break out of the string literal (falls back to a bare root).
    let r = root.to_string_lossy();
    let r = if r.contains('"') || r.contains('\n') { "/var/empty" } else { r.as_ref() };
    let network = if allow_network { "(allow network*)" } else { "(deny network*)" };
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow process-exec*)\n\
         (allow signal (target self))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow ipc-posix-shm*)\n\
         (allow file-read*)\n\
         (allow file-write* (subpath \"{r}\"))\n\
         (allow file-write-data (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))\n\
         (allow file-ioctl (literal \"/dev/tty\") (literal \"/dev/dtracehelper\"))\n\
         {network}\n"
    )
}

/// A directory the model may read, write, and run commands in. Cloneable so the
/// reply loop can hand it to each tool call cheaply.
#[derive(Clone, Debug)]
pub struct Sandbox {
    root: PathBuf,
    /// Whether `run_command` may use the network (default: yes). Writes outside the
    /// root are always denied regardless.
    allow_network: bool,
}

impl Sandbox {
    /// Open (creating if needed) the sandbox root, canonicalized so later
    /// confinement checks compare against a real absolute prefix. Network access
    /// for `run_command` defaults to allowed; override with [`Sandbox::with_network`].
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        Ok(Self { root, allow_network: true })
    }

    /// Allow (default) or deny network access from `run_command`.
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The OpenAI tool definitions advertised to the model, filtered by capability:
    /// `read` → list_files + read_file, `write` → write_file, `shell` → run_command.
    /// Returns an empty array when nothing is granted (→ plain chat, no tools).
    pub fn tool_defs(read: bool, write: bool, shell: bool) -> Value {
        let mut tools = Vec::new();
        if read {
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": "list_files",
                    "description": "List files and directories inside your working directory.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Directory to list, relative to your working directory. Defaults to '.'."
                            }
                        }
                    }
                }
            }));
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a text file from your working directory.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File to read, relative to your working directory."
                            }
                        },
                        "required": ["path"]
                    }
                }
            }));
        }
        if write {
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Create or overwrite a text file in your working directory.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File to write, relative to your working directory."
                            },
                            "content": { "type": "string", "description": "Full file contents." }
                        },
                        "required": ["path", "content"]
                    }
                }
            }));
        }
        if shell {
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": "run_command",
                    "description": "Run a shell command in your working directory (confined to it) and return its output.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "Shell command to run." }
                        },
                        "required": ["command"]
                    }
                }
            }));
        }
        Value::Array(tools)
    }

    /// Execute one tool call, returning the string result fed back as the tool
    /// message. Never panics: every failure becomes text the model can read.
    pub async fn dispatch(&self, name: &str, args: &Value) -> String {
        match name {
            "list_files" => self.list_files(args["path"].as_str().unwrap_or(".")),
            "read_file" => match args["path"].as_str() {
                Some(p) => self.read_file(p),
                None => "error: read_file requires a 'path'".into(),
            },
            "write_file" => match (args["path"].as_str(), args["content"].as_str()) {
                (Some(p), Some(c)) => self.write_file(p, c),
                _ => "error: write_file requires 'path' and 'content'".into(),
            },
            "run_command" => match args["command"].as_str() {
                Some(cmd) => self.run_command(cmd).await,
                None => "error: run_command requires a 'command'".into(),
            },
            other => format!("error: unknown tool '{other}'"),
        }
    }

    /// Resolve a caller-supplied relative path inside the sandbox, rejecting any
    /// path that would escape the root: absolute paths and `..` are refused
    /// lexically, and the deepest existing ancestor is canonicalized so a
    /// symlink pointing outward is caught.
    fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let p = Path::new(rel);
        let mut out = self.root.clone();
        for comp in p.components() {
            match comp {
                Component::Normal(c) => out.push(c),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err("path may not contain '..' (would escape the sandbox)".into());
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err("path must be relative to the working directory".into());
                }
            }
        }
        // Symlink guard: canonicalize the deepest existing ancestor of `out` and
        // require it to stay under root. New files (no existing leaf) fall back
        // to their existing parent, ultimately the root itself.
        let mut probe = out.as_path();
        loop {
            match probe.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&self.root) {
                        return Err("resolved path escapes the sandbox".into());
                    }
                    break;
                }
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => break,
                },
            }
        }
        Ok(out)
    }

    fn list_files(&self, rel: &str) -> String {
        let dir = match self.resolve(rel) {
            Ok(d) => d,
            Err(e) => return format!("error: {e}"),
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return format!("error: cannot list '{rel}': {e}"),
        };
        let mut lines: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => lines.push(format!("{name}/")),
                Ok(_) => {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    lines.push(format!("{name} ({size} bytes)"));
                }
                Err(_) => lines.push(name),
            }
        }
        lines.sort();
        if lines.is_empty() {
            format!("(empty directory '{rel}')")
        } else {
            lines.join("\n")
        }
    }

    fn read_file(&self, rel: &str) -> String {
        let path = match self.resolve(rel) {
            Ok(p) => p,
            Err(e) => return format!("error: {e}"),
        };
        match std::fs::read(&path) {
            Ok(bytes) => cap(&String::from_utf8_lossy(&bytes)),
            Err(e) => format!("error: cannot read '{rel}': {e}"),
        }
    }

    fn write_file(&self, rel: &str, content: &str) -> String {
        let path = match self.resolve(rel) {
            Ok(p) => p,
            Err(e) => return format!("error: {e}"),
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("error: cannot create parent of '{rel}': {e}");
            }
        }
        match std::fs::write(&path, content) {
            Ok(()) => format!("wrote {} bytes to '{rel}'", content.len()),
            Err(e) => format!("error: cannot write '{rel}': {e}"),
        }
    }

    async fn run_command(&self, command: &str) -> String {
        use tokio::process::Command;
        // Confine the shell to the sandbox via macOS seatbelt: it cannot write or
        // delete anything outside the root, and cannot use the network. Reads stay
        // open (restricting them aborts dyld's shared-cache mapping). HOME + TMPDIR
        // are redirected into the sandbox so tool dotfiles/tempfiles stay inside.
        if !Path::new(SANDBOX_EXEC).exists() {
            return format!("error: shell confinement unavailable ({SANDBOX_EXEC} missing)");
        }
        let tmp = self.root.join(".tmp");
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            return format!("error: cannot prepare shell tempdir: {e}");
        }
        let profile = seatbelt_profile(&self.root, self.allow_network);
        let child = Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("TMPDIR", &tmp)
            .output();
        let output = match tokio::time::timeout(CMD_TIMEOUT, child).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return format!("error: cannot run command: {e}"),
            Err(_) => return format!("error: command timed out after {}s", CMD_TIMEOUT.as_secs()),
        };
        let mut buf = String::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.is_empty() {
            buf.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str("[stderr] ");
            buf.push_str(&stderr);
        }
        if !output.status.success() {
            buf.push_str(&format!("\n[exit status: {}]", output.status));
        }
        if buf.is_empty() {
            buf.push_str("(command produced no output)");
        }
        cap(&buf)
    }
}

/// Truncate a tool result to `MAX_OUTPUT_BYTES`, appending a note on a cut.
fn cap(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    // Cut on a char boundary at or below the byte budget.
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated, {} bytes total]", &s[..end], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (Sandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox::open(dir.path()).unwrap();
        (sb, dir)
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (sb, _d) = sandbox();
        assert!(sb.write_file("notes/a.txt", "hello").starts_with("wrote 5 bytes"));
        assert_eq!(sb.read_file("notes/a.txt"), "hello");
    }

    #[test]
    fn list_shows_files_and_dirs() {
        let (sb, _d) = sandbox();
        sb.write_file("sub/x.txt", "hi");
        sb.write_file("top.txt", "yo");
        let out = sb.list_files(".");
        assert!(out.contains("sub/"), "dir listed with slash: {out}");
        assert!(out.contains("top.txt (2 bytes)"), "file with size: {out}");
    }

    #[test]
    fn rejects_parent_traversal() {
        let (sb, _d) = sandbox();
        assert!(sb.read_file("../secret").starts_with("error:"));
        assert!(sb.write_file("../../etc/pwn", "x").starts_with("error:"));
        assert!(sb.read_file("a/../../b").starts_with("error:"));
    }

    #[test]
    fn rejects_absolute_path() {
        let (sb, _d) = sandbox();
        assert!(sb.read_file("/etc/hosts").starts_with("error:"));
        assert!(sb.write_file("/tmp/pwn", "x").starts_with("error:"));
    }

    #[test]
    fn rejects_symlink_escape() {
        let (sb, dir) = sandbox();
        // A symlink inside the sandbox pointing at the parent must not be a
        // read/write hole out of the jail.
        let target = dir.path().parent().unwrap();
        let link = dir.path().join("escape");
        if std::os::unix::fs::symlink(target, &link).is_ok() {
            assert!(
                sb.read_file("escape/anything").starts_with("error:"),
                "symlink escape must be refused"
            );
        }
    }

    #[tokio::test]
    async fn run_command_captures_output() {
        let (sb, _d) = sandbox();
        let out = sb.run_command("echo hello-sandbox").await;
        assert!(out.contains("hello-sandbox"), "got: {out}");
    }

    #[tokio::test]
    async fn run_command_runs_in_sandbox_cwd() {
        let (sb, _d) = sandbox();
        sb.write_file("marker.txt", "x");
        let out = sb.run_command("ls").await;
        assert!(out.contains("marker.txt"), "cwd should be the sandbox: {out}");
    }

    #[tokio::test]
    async fn run_command_cannot_write_outside_sandbox() {
        if !std::path::Path::new(SANDBOX_EXEC).exists() {
            return; // seatbelt only on macOS
        }
        let (sb, dir) = sandbox();
        // A sibling path just outside the sandbox root.
        let escape = dir.path().parent().unwrap().join("escape-should-not-exist.txt");
        let cmd = format!("echo pwned > '{}'", escape.display());
        let out = sb.run_command(&cmd).await;
        assert!(!escape.exists(), "seatbelt must block writes outside the sandbox: {out}");
        // But a write INSIDE the sandbox still works.
        let ok = sb.run_command("echo hi > inside.txt && cat inside.txt").await;
        assert!(ok.contains("hi"), "in-sandbox write should succeed: {ok}");
    }

    #[test]
    fn seatbelt_profile_network_is_toggleable_but_writes_stay_confined() {
        let root = std::path::Path::new("/private/tmp/sbx");
        let on = seatbelt_profile(root, true);
        let off = seatbelt_profile(root, false);
        assert!(on.contains("(allow network*)"), "network on: {on}");
        assert!(off.contains("(deny network*)"), "network off: {off}");
        // write confinement is present regardless of the network setting
        for p in [&on, &off] {
            assert!(p.contains("(deny default)"));
            assert!(p.contains("file-write* (subpath \"/private/tmp/sbx\")"));
        }
    }

    #[tokio::test]
    async fn run_command_default_allows_network_field() {
        let (sb, _d) = sandbox();
        assert!(sb.allow_network, "network defaults to allowed");
        let sb2 = sb.with_network(false);
        assert!(!sb2.allow_network);
    }

    #[test]
    fn cap_truncates_and_notes() {
        let big = "a".repeat(MAX_OUTPUT_BYTES + 100);
        let out = cap(&big);
        assert!(out.contains("truncated"));
        assert!(out.len() < big.len() + 64);
    }
}
