//! Stamp every binary built from this workspace with the commit it was built from.
//!
//! WHY. `doctor --services` reported every service healthy while `~/.cargo/bin/rozum-gateway` was
//! three days behind `master` — three times in two days (2026-08-07..08). Once that meant a feature
//! was "shipped" for a day while the daemon serving it had never heard of it. Every other kind of
//! drift in this repo has a check; the gap between what is MERGED and what is RUNNING had none.
//!
//! The stamp is a plain marker string in the binary rather than a subcommand, so the check reads a
//! FILE and never has to start a service to ask it what it is — a binary that cannot run is exactly
//! the case worth reporting, and spawning the resident-model gateway to ask its version would cost
//! a model load.
use std::process::Command;

fn main() {
    let repo = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(["-C", &repo]).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    };

    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let root = git(&["rev-parse", "--show-toplevel"]).unwrap_or_else(|| repo.clone());

    // Rebuild when HEAD moves. `--git-path` resolves through a worktree, where `.git` is a file and
    // `HEAD` does not live where a naive `.git/HEAD` would look.
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rustc-env=ROZUM_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=ROZUM_BUILD_REPO={root}");
}
