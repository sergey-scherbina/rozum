//! Structural sandbox for agentic model runs — see `docs/specs/model-sandbox.md`.
//!
//! A sandbox is a SET of `(path, mode)` rules rendered to an OS jail so a model's
//! agent loop **cannot do anything harmful without per-action prompts** — the
//! safety is the kernel, not interactive confirmation. v1 enforcer: macOS Seatbelt
//! (`sandbox-exec`); the policy is built once (the `rust-coding` profile) and the
//! launch wrapper runs the agent child under it (P1b).
//!
//! Design **validated empirically on macOS (M4)**:
//! - **Writes** are strictly confined to the workspace(s) + toolchain caches + a
//!   few device nodes; everything else is denied.
//! - **Reads** are allow-all **minus a secret denylist** — an allow-LIST for reads
//!   breaks `dyld`/binary startup (the shared-cache path moves between macOS
//!   versions), so we allow reads broadly and carve out secrets (`~/.ssh`, cloud
//!   creds, keychains, …). Most-specific-rule-wins makes the deny override.
//! - **Network** is deny-by-default; `GatewayOnly` adds loopback so the child can
//!   still reach the local model gateway, nothing off-box.

use std::path::PathBuf;

/// Network policy for the sandboxed child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetPolicy {
    /// No network at all (subsumed by `(deny default)`).
    None,
    /// Loopback only — reach the local model gateway, nothing off-box.
    GatewayOnly,
    /// Unrestricted (escape hatch; not for untrusted/experimental models).
    Full,
}

/// A structural sandbox policy: confined writes + a secret-read denylist + network.
/// Rendered to a backend profile (Seatbelt today). Paths should already be the real
/// (symlink-resolved) locations so the kernel subpath-matches them.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    /// Roots the child may read+write+exec (the workspace(s) + toolchain caches).
    pub writable: Vec<PathBuf>,
    /// Secret roots carved OUT of the otherwise-readable filesystem.
    pub secret_deny: Vec<PathBuf>,
    /// Network policy.
    pub network: NetPolicy,
}

impl SandboxPolicy {
    /// The v1 `rust-coding` profile: `workspaces` (rw, may be several) + the Rust
    /// toolchain caches (rw, so builds work) + a default secret denylist, with the
    /// given network policy. Env-derived (`CARGO_HOME`/`RUSTUP_HOME`/`TMPDIR`/
    /// `HOME`); paths are canonicalized so Seatbelt matches the real location
    /// (`/tmp` → `/private/tmp`, etc.).
    pub fn rust_coding(workspaces: &[PathBuf], network: NetPolicy) -> Self {
        let mut writable: Vec<PathBuf> = workspaces.iter().map(|p| resolve(p)).collect();
        writable.extend(toolchain_paths());
        Self {
            writable: dedup(writable),
            secret_deny: dedup(default_secret_paths()),
            network,
        }
    }

    /// Render to a macOS Seatbelt (`sandbox-exec -f`) profile (SBPL).
    pub fn to_seatbelt_profile(&self) -> String {
        let mut p = String::from("(version 1)\n(deny default)\n");
        // The minimum a child process needs to fork/exec and load libraries.
        p.push_str("(allow process-fork)\n");
        p.push_str("(allow process-exec*)\n");
        p.push_str("(allow sysctl-read)\n");
        p.push_str("(allow mach-lookup)\n");
        // Reads: broad (so dyld + tools load), minus the secret denylist below.
        p.push_str("(allow file-read*)\n");
        for s in &self.secret_deny {
            p.push_str(&format!("(deny file-read* (subpath {}))\n", sbpl_quote(s)));
        }
        // Writes (+ the exec targets are covered by file-read* above): workspace(s)
        // + toolchain caches + the device nodes tools expect.
        p.push_str("(allow file-write*\n");
        for w in &self.writable {
            p.push_str(&format!("  (subpath {})\n", sbpl_quote(w)));
        }
        for dev in [
            "/dev/null",
            "/dev/zero",
            "/dev/random",
            "/dev/urandom",
            "/dev/dtracehelper",
            "/dev/tty",
        ] {
            p.push_str(&format!("  (literal \"{dev}\")\n"));
        }
        p.push_str(")\n");
        // Network.
        match self.network {
            NetPolicy::None => {} // (deny default) already blocks all network
            NetPolicy::GatewayOnly => {
                p.push_str("(allow network* (local ip) (remote ip \"localhost:*\"))\n");
            }
            NetPolicy::Full => p.push_str("(allow network*)\n"),
        }
        p
    }
}

/// Write `policy`'s Seatbelt profile to a temp file and return its path, for
/// `sandbox-exec -f <path> <program> <args…>`. The file is intentionally left in
/// place (the launch wrapper `exec`s into `sandbox-exec`, so no cleanup code runs;
/// the OS clears the temp dir). One file per process (pid-named).
pub fn write_seatbelt_profile_temp(policy: &SandboxPolicy) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("rozum-sandbox-{}.sb", std::process::id()));
    std::fs::write(&path, policy.to_seatbelt_profile())?;
    Ok(path)
}

/// Canonicalize a path (resolve symlinks like `/tmp`→`/private/tmp`); fall back to
/// the path as given if it does not exist yet.
fn resolve(p: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The Rust toolchain + temp caches a build must read/write.
fn toolchain_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let from_env_or_home = |env: &str, sub: &str| -> Option<PathBuf> {
        std::env::var_os(env)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(sub)))
    };
    if let Some(c) = from_env_or_home("CARGO_HOME", ".cargo") {
        v.push(resolve(&c));
    }
    if let Some(r) = from_env_or_home("RUSTUP_HOME", ".rustup") {
        v.push(resolve(&r));
    }
    if let Some(t) = std::env::var_os("TMPDIR").map(PathBuf::from) {
        v.push(resolve(&t));
    }
    // `/tmp` is the conventional scratch; canonicalizes to `/private/tmp` on macOS.
    v.push(resolve(std::path::Path::new("/tmp")));
    v
}

/// Default secret roots to deny reads of, relative to `$HOME`. Best-effort: covers
/// the common credential stores; the operator can extend it via config.
fn default_secret_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".ssh",
        ".aws",
        ".gnupg",
        ".netrc",
        ".kube",
        ".docker",
        ".config/gh",
        ".config/gcloud",
        ".npmrc",
        "Library/Keychains",
    ]
    .iter()
    .map(|s| home.join(s))
    .collect()
}

/// Quote a path as an SBPL string literal: `(subpath "…")`, escaping `\` and `"`.
fn sbpl_quote(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn dedup(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_confines_writes_and_denies_secrets() {
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/private/tmp/ws")],
            secret_deny: vec![PathBuf::from("/Users/x/.ssh")],
            network: NetPolicy::GatewayOnly,
        };
        let p = policy.to_seatbelt_profile();
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        assert!(p.contains("(allow file-read*)\n"));
        assert!(p.contains("(deny file-read* (subpath \"/Users/x/.ssh\"))"));
        assert!(p.contains("(subpath \"/private/tmp/ws\")")); // writable
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(p.contains("localhost")); // gateway-only network rule
    }

    #[test]
    fn network_variants() {
        let base = |n| SandboxPolicy {
            writable: vec![PathBuf::from("/ws")],
            secret_deny: vec![],
            network: n,
        }
        .to_seatbelt_profile();
        assert!(!base(NetPolicy::None).contains("allow network")); // deny-default only
        assert!(base(NetPolicy::GatewayOnly).contains("(allow network* (local ip) (remote ip \"localhost:*\"))"));
        assert!(base(NetPolicy::Full).contains("(allow network*)\n"));
    }

    #[test]
    fn sbpl_quote_escapes() {
        assert_eq!(sbpl_quote(std::path::Path::new("/a/b")), "\"/a/b\"");
        // Quotes and backslashes in a path must be escaped so the profile parses.
        assert_eq!(
            sbpl_quote(std::path::Path::new("/a/\"q\"/b")),
            "\"/a/\\\"q\\\"/b\""
        );
    }

    #[test]
    fn rust_coding_includes_workspace_and_toolchain() {
        let ws = PathBuf::from("/private/tmp/ws");
        let pol = SandboxPolicy::rust_coding(&[ws.clone()], NetPolicy::GatewayOnly);
        // The workspace is writable, plus at least one toolchain/temp path.
        assert!(pol.writable.iter().any(|p| p == &ws));
        assert!(pol.writable.len() >= 2, "toolchain caches should be added");
        assert_eq!(pol.network, NetPolicy::GatewayOnly);
    }

    // Integration (macOS only; runs sandbox-exec): the GENERATED profile must
    // actually parse and let a real binary execute under it — string assertions
    // alone don't prove the SBPL is valid. Ignored by default (spawns a process,
    // macOS-only); run with `--include-ignored` on a Mac.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "spawns sandbox-exec; macOS only"]
    fn generated_profile_parses_and_runs() {
        let pol = SandboxPolicy::rust_coding(&[std::env::temp_dir()], NetPolicy::GatewayOnly);
        let profile = pol.to_seatbelt_profile();
        let f = std::env::temp_dir().join(format!("rozum-sbx-{}.sb", std::process::id()));
        std::fs::write(&f, &profile).unwrap();
        let status = std::process::Command::new("sandbox-exec")
            .arg("-f")
            .arg(&f)
            .arg("/usr/bin/true")
            .status()
            .expect("spawn sandbox-exec");
        let _ = std::fs::remove_file(&f);
        assert!(status.success(), "generated profile failed to parse/run:\n{profile}");
    }

    // Full e2e (macOS only; runs cargo + sandbox-exec; ignored — slow): build a
    // real crate INSIDE the rozum-generated jail (proves the toolchain paths are
    // right, so a coding model can actually build) AND prove a write OUTSIDE the
    // workspace is denied (proves confinement). This is the spec's P1 "Done when"
    // (docs/specs/model-sandbox.md). Validated on M4 2026-06-19.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "runs cargo build under sandbox-exec; macOS only, slow"]
    fn cargo_build_runs_in_jail_and_escape_denied() {
        use std::process::Command;
        let id = std::process::id();
        let ws = std::env::temp_dir().join(format!("rozum-sbx-e2e-{id}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let proj = ws.join("sbxdemo");
        // A depless crate builds offline — no registry/network needed.
        let new = Command::new("cargo")
            .args(["new", "--bin", "--quiet", "--name", "sbxdemo"])
            .arg(&proj)
            .status()
            .expect("spawn cargo new");
        assert!(new.success(), "cargo new failed");

        // The REAL rozum-generated profile for this workspace (not a hand copy).
        let policy = SandboxPolicy::rust_coding(&[proj.clone()], NetPolicy::GatewayOnly);
        let profile = write_seatbelt_profile_temp(&policy).unwrap();

        // 1. `cargo build` must SUCCEED in the jail (toolchain paths correct).
        let build = Command::new("sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .args(["cargo", "build", "--offline", "--quiet"])
            .current_dir(&proj)
            .status()
            .expect("spawn sandbox-exec cargo");
        assert!(build.success(), "cargo build failed inside the jail (toolchain paths?)");
        assert!(
            proj.join("target/debug/sbxdemo").exists(),
            "no build output produced inside the jail"
        );

        // 2. a write OUTSIDE the workspace/toolchain ($HOME root) must be DENIED.
        let home = std::env::var("HOME").expect("HOME set");
        let escape = std::path::Path::new(&home).join(format!(".rozum-sbx-escape-{id}.txt"));
        let _ = std::fs::remove_file(&escape);
        let _ = Command::new("sandbox-exec")
            .arg("-f")
            .arg(&profile)
            .args(["/bin/sh", "-c", &format!("echo escaped > {}", escape.display())])
            .status();
        let leaked = escape.exists();
        // Cleanup before asserting.
        let _ = std::fs::remove_file(&escape);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_file(&profile);
        assert!(!leaked, "SANDBOX ESCAPE: wrote outside the workspace");
    }
}
