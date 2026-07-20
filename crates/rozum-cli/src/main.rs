//! `rozum` — the thin umbrella dispatcher (binary split, `docs/specs/binary-split.md`).
//!
//! It links nothing (pure std): it inspects the first argument and dispatches to the right backend.
//! On Unix it replaces its process image with `exec`, so stdio/exit-codes/signals pass through
//! unchanged — essential for the stdio MCP proxy. Windows has no `exec`; there it waits for the
//! child and returns its exit code. This keeps `rozum <cmd>` working everywhere while the heavy
//! engine code lives only in `rozum-gateway`, so a frontend fix never triggers the MLX rebuild.
//!
//! Routing:
//! - `mcp-proxy` / `mpc-proxy` / `mcp-http` / `telegram` / `discord` → `rozum-meet`
//!   (engine-free meeting bridges)
//! - everything else (incl. no subcommand → TUI, `--topic …`, etc.) → `rozum-gateway` (full CLI)
//!
//! Each target is resolved next to this dispatcher first (so an uninstalled `target/…/rozum`
//! finds its siblings), then falls back to `PATH`.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (target, fwd) = route(&args);
    let bin = resolve(target);
    dispatch(&bin, target, &fwd)
}

/// Select the sibling binary without spawning it. Kept pure so command routing is testable offline.
fn route(args: &[String]) -> (&'static str, Vec<String>) {
    match args.get(1).map(String::as_str) {
        // `mpc-proxy` is a long-standing typo'd alias clap accepted on the monolith; normalize it.
        Some("mcp-proxy") | Some("mpc-proxy") => {
            let mut v = vec!["mcp-proxy".to_string()];
            v.extend(args[2..].iter().cloned());
            ("rozum-meet", v)
        }
        Some("mcp-http") | Some("telegram") | Some("discord") => ("rozum-meet", args[1..].to_vec()),
        // Everything else — gateway, launch, meetings, service, web, tui (bare), list, control,
        // mcp install … — goes to the full engine-linking binary unchanged.
        _ => ("rozum-gateway", args[1..].to_vec()),
    }
}

#[cfg(unix)]
fn dispatch(bin: &std::path::Path, target: &str, args: &[String]) -> std::process::ExitCode {
    // `exec` replaces this process — no extra hop, stdio/signals/exit code are the child's.
    let err = Command::new(bin).args(args).exec();
    eprintln!("rozum: failed to exec {} ({target}): {err}", bin.display());
    std::process::ExitCode::from(127)
}

#[cfg(not(unix))]
fn dispatch(bin: &std::path::Path, target: &str, args: &[String]) -> std::process::ExitCode {
    match Command::new(bin).args(args).status() {
        // Windows has no process-image replacement. Waiting preserves inherited stdio and the
        // child's conventional process exit code; `exit` also avoids narrowing it to `u8`.
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("rozum: failed to spawn {} ({target}): {err}", bin.display());
            std::process::ExitCode::from(127)
        }
    }
}

/// Resolve `name` next to the running dispatcher first (covers `target/release/rozum` finding its
/// just-built siblings without an install), else bare `name` so the OS searches `PATH`.
fn resolve(name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            if cand.is_file() {
                return cand;
            }
        }
    }
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("rozum".to_owned())
            .chain(values.iter().map(|value| (*value).to_owned()))
            .collect()
    }

    #[test]
    fn messenger_commands_route_to_thin_meeting_binary_unchanged() {
        for command in ["telegram", "discord"] {
            let input = args(&[command, "--room", "ops", "--name", "relay"]);
            let (target, forwarded) = route(&input);
            assert_eq!(target, "rozum-meet");
            assert_eq!(forwarded, input[1..]);
        }
    }

    #[test]
    fn typoed_proxy_alias_is_still_normalized() {
        let input = args(&["mpc-proxy", "--flag"]);
        let (target, forwarded) = route(&input);
        assert_eq!(target, "rozum-meet");
        assert_eq!(forwarded, ["mcp-proxy", "--flag"]);
    }

    #[test]
    fn engine_and_legacy_commands_still_route_to_gateway() {
        for input in [args(&[]), args(&["gateway"]), args(&["meetings", "status"])] {
            let (target, forwarded) = route(&input);
            assert_eq!(target, "rozum-gateway");
            assert_eq!(forwarded, input[1..]);
        }
    }
}
