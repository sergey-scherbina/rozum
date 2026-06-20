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

use std::path::{Path, PathBuf};

/// Network policy for the sandboxed child.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NetPolicy {
    /// No network at all (subsumed by `(deny default)`). True zero-egress.
    None,
    /// Loopback only — reach the local model gateway, nothing off-box (the default).
    /// NOTE: under the Docker backend this is best-effort — the container reaches the
    /// host gateway, but the default bridge also permits general egress (no native
    /// Docker egress allowlist). Use `GatewayStrict` (Docker) or `None` for a guarantee.
    #[default]
    GatewayOnly,
    /// Reach the gateway and **nothing else** — a true egress allowlist. On Seatbelt
    /// this is identical to `GatewayOnly` (the SBPL rule already allows only localhost).
    /// On Docker it adds an in-container iptables egress filter (the `rozum-agent`
    /// entrypoint, gated by `--cap-add=NET_ADMIN` + `ROZUM_EGRESS=strict`) that drops
    /// all output except the host gateway — closing the bridge-egress gap.
    GatewayStrict,
    /// Unrestricted (escape hatch; not for untrusted/experimental models).
    Full,
}

impl NetPolicy {
    /// Pick the network policy from `ROZUM_SANDBOX_NETWORK` (case-insensitive):
    /// `none` | `gateway-only` (default; aliases `gateway`/`loopback`) |
    /// `gateway-strict` (alias `strict`) | `full`. Applies to BOTH backends. Unknown /
    /// unset → `GatewayOnly`.
    pub fn from_env() -> Self {
        std::env::var("ROZUM_SANDBOX_NETWORK")
            .ok()
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    /// Pure parse of a network-policy name (the testable core of `from_env`).
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => NetPolicy::None,
            "gateway-strict" | "strict" => NetPolicy::GatewayStrict,
            "full" | "all" | "unrestricted" => NetPolicy::Full,
            _ => NetPolicy::GatewayOnly, // "gateway-only" / "gateway" / "loopback" / unknown
        }
    }
}

/// Which OS mechanism enforces the `(path, mode)` policy. The policy is written
/// once (`rust_coding`); each backend renders it to its own jail. Selected with
/// `ROZUM_SANDBOX_BACKEND` (`seatbelt` | `docker`); default `Seatbelt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SandboxBackend {
    /// macOS `sandbox-exec` + generated SBPL. The v1 default (macOS-only).
    #[default]
    Seatbelt,
    /// Run the agent in a `docker` container — writable paths become volume
    /// mounts, the rest of the host FS is simply absent (stronger isolation than
    /// a deny rule), the gateway is reached over the host loopback. Cross-platform
    /// (the only jail available off macOS), heavier. model-sandbox P3.
    Docker,
}

impl SandboxBackend {
    /// Pick the backend from `ROZUM_SANDBOX_BACKEND` (case-insensitive). Unknown /
    /// unset → `Seatbelt`. `container` is accepted as an alias for `docker`.
    pub fn from_env() -> Self {
        std::env::var("ROZUM_SANDBOX_BACKEND")
            .ok()
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    /// Pure parse of a backend name (the testable core of `from_env`).
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "docker" | "container" => SandboxBackend::Docker,
            _ => SandboxBackend::Seatbelt,
        }
    }
}

/// Hostname a container uses to reach a service on the host (the rozum gateway /
/// MLX on the host GPU). Docker Desktop (macOS/Windows) and Docker Engine with
/// `--add-host …:host-gateway` (Linux) both resolve this to the host. Inside a
/// container the host's `127.0.0.1` is the container itself, so the gateway URL
/// must use this name instead — see `to_docker_run_args` (`--add-host`).
pub const CONTAINER_GATEWAY_HOST: &str = "host.docker.internal";

/// The container image a Docker-backend launch runs the agent in. Operator-supplied
/// (it must contain the agent CLI — `claude`/`codex`/`opencode` — and a Rust
/// toolchain if the task builds): `ROZUM_SANDBOX_DOCKER_IMAGE`, default
/// `rozum-agent:latest`.
pub fn default_docker_image() -> String {
    std::env::var("ROZUM_SANDBOX_DOCKER_IMAGE")
        .unwrap_or_else(|_| "rozum-agent:latest".to_owned())
}

/// Container resource limits — the spec's out-of-scope-for-v1 DoS/kernel threats
/// (a runaway model exhausting host RAM/CPU or fork-bombing). Rendered to
/// `docker run` flags; only emitted when set, so they never silently change a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DockerLimits {
    /// `--memory` (e.g. `"8g"`). Opt-in: a hard memory ceiling triggers an OOM-kill
    /// of the container rather than the host. None = no limit (heavy builds untouched).
    pub memory: Option<String>,
    /// `--cpus` (e.g. `"4"`). Opt-in CPU-time cap. None = no limit.
    pub cpus: Option<String>,
    /// `--pids-limit`. A cheap fork-bomb guard; a generous default is safe for builds.
    /// `Some(-1)` = unlimited (Docker's convention); None = omit the flag entirely.
    pub pids: Option<i64>,
}

impl DockerLimits {
    /// No limits at all (the renderer adds nothing).
    pub fn none() -> Self {
        Self::default()
    }

    /// Read limits from the environment:
    /// - `ROZUM_SANDBOX_DOCKER_MEMORY` → `--memory` (opt-in, e.g. `8g`)
    /// - `ROZUM_SANDBOX_DOCKER_CPUS`   → `--cpus` (opt-in, e.g. `4`)
    /// - `ROZUM_SANDBOX_DOCKER_PIDS`   → `--pids-limit` (default **2048** — a fork-bomb
    ///   guard generous enough for parallel `cargo` builds; set `-1` to disable, or a
    ///   number to tighten).
    pub fn from_env() -> Self {
        let trimmed = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let pids = match trimmed("ROZUM_SANDBOX_DOCKER_PIDS") {
            Some(s) => s.parse::<i64>().ok(),    // explicit (incl. -1 = unlimited)
            None => Some(2048),                  // default fork-bomb guard
        };
        Self {
            memory: trimmed("ROZUM_SANDBOX_DOCKER_MEMORY"),
            cpus: trimmed("ROZUM_SANDBOX_DOCKER_CPUS"),
            pids,
        }
    }

    /// Append the limit flags to a `docker run` arg vector (in place).
    fn push_args(&self, a: &mut Vec<String>) {
        if let Some(m) = &self.memory {
            a.push("--memory".into());
            a.push(m.clone());
        }
        if let Some(c) = &self.cpus {
            a.push("--cpus".into());
            a.push(c.clone());
        }
        if let Some(p) = self.pids {
            a.push("--pids-limit".into());
            a.push(p.to_string());
        }
    }
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
    /// toolchain caches (rw, so builds work) + the supported agent CLIs' own state
    /// dirs (rw, so a launched agent persists its session/history mid-task instead
    /// of crashing on a denied write) + a default secret denylist, with the given
    /// network policy. Env-derived (`CARGO_HOME`/`RUSTUP_HOME`/`TMPDIR`/`HOME`);
    /// paths are canonicalized so Seatbelt matches the real location (`/tmp` →
    /// `/private/tmp`, etc.).
    pub fn rust_coding(workspaces: &[PathBuf], network: NetPolicy) -> Self {
        let mut writable: Vec<PathBuf> = workspaces.iter().map(|p| resolve(p)).collect();
        writable.extend(toolchain_paths());
        writable.extend(agent_state_paths());
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
        // Reads: broad (so dyld + tools load); secrets carved out LAST below.
        p.push_str("(allow file-read*)\n");
        // Writes: workspace(s) + toolchain caches + the device nodes tools expect.
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
        // Secrets LAST (Seatbelt is last-match-wins): never readable AND never
        // writable — even when the workspace (e.g. a cwd at `$HOME` under the
        // default-on jail) would otherwise grant write access to a subpath of it.
        for s in &self.secret_deny {
            let q = sbpl_quote(s);
            p.push_str(&format!("(deny file-read* (subpath {q}))\n"));
            p.push_str(&format!("(deny file-write* (subpath {q}))\n"));
        }
        // Network.
        match self.network {
            NetPolicy::None => {} // (deny default) already blocks all network
            // Seatbelt's loopback-only rule is ALREADY a strict egress allowlist, so
            // GatewayOnly and GatewayStrict render identically here (the strict/best-
            // effort distinction only matters for Docker's bridge).
            NetPolicy::GatewayOnly | NetPolicy::GatewayStrict => {
                p.push_str("(allow network* (local ip) (remote ip \"localhost:*\"))\n");
            }
            NetPolicy::Full => p.push_str("(allow network*)\n"),
        }
        p
    }

    /// Render the policy to the `docker run` argument vector **up to and including
    /// the image** — the caller appends the program + its args (they become the
    /// container's command). Mapping of the `(path, mode)` set onto Docker:
    ///
    /// - **writable** → `-v <path>:<path>:rw` (host path == container path so the
    ///   workspace/cwd in args line up); the toolchain caches mount too, so builds
    ///   reuse `~/.cargo`/`~/.rustup` and work offline.
    /// - **everything else** → simply NOT mounted ⇒ absent in the container
    ///   (stronger than a Seatbelt deny: there is no path to reach).
    /// - **secrets under a writable mount** (e.g. `~/.ssh` when the workspace is
    ///   `$HOME`) → masked with `--tmpfs <path>`: an empty in-container fs shadows
    ///   the real dir so the mounted secret is unreadable. Secrets not under any
    ///   mount need no flag (already absent).
    /// - **network**: `None` → `--network none`; `GatewayOnly`/`Full` → the default
    ///   bridge plus `--add-host host.docker.internal:host-gateway` so the agent can
    ///   reach the host gateway. NOTE: the bridge still allows general egress, so
    ///   Docker `GatewayOnly` is weaker than Seatbelt's loopback-only — use `None`
    ///   for strict no-egress, or a firewalled custom network (future hardening).
    /// - `forward_env`: env var **names** turned into `-e <NAME>` so `docker`
    ///   forwards their values from this process's environment into the container
    ///   (the caller sets them on the `docker` command). Order is preserved.
    /// - `limits`: `--memory`/`--cpus`/`--pids-limit` (only the set ones are emitted)
    ///   — DoS/kernel containment for a runaway model.
    pub fn to_docker_run_args(
        &self,
        image: &str,
        workdir: &Path,
        forward_env: &[&str],
        limits: &DockerLimits,
    ) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--init".into(), // reap the agent's child processes
        ];
        limits.push_args(&mut a);
        // Writable paths → rw bind mounts (host path == container path).
        let writable: Vec<PathBuf> = self.writable.iter().map(|p| resolve(p)).collect();
        for w in &writable {
            a.push("-v".into());
            a.push(format!("{p}:{p}:rw", p = w.to_string_lossy()));
        }
        // Mask any secret that sits UNDER a writable mount (else it'd be mounted
        // too) with an empty tmpfs; secrets not under a mount are already absent.
        for s in &self.secret_deny {
            let s = resolve(s);
            if writable.iter().any(|w| s != *w && s.starts_with(w)) {
                a.push("--tmpfs".into());
                a.push(s.to_string_lossy().into_owned());
            }
        }
        a.push("-w".into());
        a.push(workdir.to_string_lossy().into_owned());
        match self.network {
            NetPolicy::None => a.push("--network=none".into()),
            NetPolicy::GatewayOnly | NetPolicy::Full => {
                a.push("--add-host".into());
                a.push(format!("{CONTAINER_GATEWAY_HOST}:host-gateway"));
            }
            NetPolicy::GatewayStrict => {
                // Reach the host gateway like GatewayOnly, but ALSO grant NET_ADMIN so
                // the image entrypoint can install an iptables egress filter that drops
                // everything except the host gateway. `ROZUM_EGRESS=strict` turns the
                // filter on; the entrypoint resolves the gateway IP from /etc/hosts.
                a.push("--add-host".into());
                a.push(format!("{CONTAINER_GATEWAY_HOST}:host-gateway"));
                a.push("--cap-add=NET_ADMIN".into());
                a.push("-e".into());
                a.push("ROZUM_EGRESS=strict".into());
            }
        }
        for key in forward_env {
            a.push("-e".into());
            a.push((*key).to_owned());
        }
        a.push(image.to_owned());
        a
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

/// Config/state dirs the supported agent CLIs write to **operate** (session,
/// history, todos, project state) — claude `~/.claude`, codex `~/.codex`, opencode
/// `~/.config/opencode` + `~/.local/{share,state}/opencode` + `~/.cache/opencode`.
/// The sandbox is for agentic runs, so the agent's OWN working state must be
/// writable or it breaks mid-task; these are the agent's tool dirs, not the
/// operator's project data. (Under `rozum launch` the agent auths to the local
/// gateway, not its real creds, and network is loopback-only — so even its own
/// creds under `~/.claude` can't be exfiltrated.)
fn agent_state_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".claude",
        ".codex",
        ".config/opencode",
        ".local/share/opencode",
        ".local/state/opencode",
        ".cache/opencode",
    ]
    .iter()
    .map(|s| resolve(&home.join(s)))
    .collect()
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
        // Secrets are also write-denied (so they're safe even if the workspace
        // encompasses them under default-on), and that deny comes AFTER the
        // allow-write so last-match-wins keeps them protected.
        assert!(p.contains("(deny file-write* (subpath \"/Users/x/.ssh\"))"));
        let allow_w = p.find("(allow file-write*").unwrap();
        let deny_secret = p.find("(deny file-write* (subpath \"/Users/x/.ssh\"))").unwrap();
        assert!(deny_secret > allow_w, "secret write-deny must come after allow-write");
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
        // Seatbelt loopback-only is already strict → GatewayStrict renders identically.
        assert_eq!(base(NetPolicy::GatewayStrict), base(NetPolicy::GatewayOnly));
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

    #[test]
    fn rust_coding_includes_agent_state_dirs() {
        // A launched agent must persist its own state (session/history/todos) or it
        // crashes mid-task on a denied write — its state dir must be writable. The
        // deny/allow *mechanism* is proven by the escape-denied integration test; this
        // guards that the agent dirs are actually in the writable set.
        if std::env::var_os("HOME").is_some() {
            let pol =
                SandboxPolicy::rust_coding(&[PathBuf::from("/private/tmp/ws")], NetPolicy::None);
            assert!(
                pol.writable.iter().any(|p| p.ends_with(".claude")),
                "agent state dir ~/.claude must be writable so the agent can run"
            );
            assert!(
                pol.writable.iter().any(|p| p.ends_with(".codex")),
                "agent state dir ~/.codex must be writable"
            );
        }
    }

    #[test]
    fn backend_parse_selects_docker_only_for_known_aliases() {
        assert_eq!(SandboxBackend::parse("docker"), SandboxBackend::Docker);
        assert_eq!(SandboxBackend::parse("Docker"), SandboxBackend::Docker);
        assert_eq!(SandboxBackend::parse(" CONTAINER "), SandboxBackend::Docker);
        // Anything else (incl. "seatbelt", typos, empty) → the default backend.
        for s in ["seatbelt", "", "podman", "vm", "nonsense"] {
            assert_eq!(SandboxBackend::parse(s), SandboxBackend::Seatbelt, "{s:?}");
        }
        assert_eq!(SandboxBackend::default(), SandboxBackend::Seatbelt);
    }

    #[test]
    fn docker_args_mount_workspace_and_map_network() {
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/private/tmp/ws")],
            secret_deny: vec![],
            network: NetPolicy::GatewayOnly,
        };
        let args =
            policy.to_docker_run_args("img:1", Path::new("/private/tmp/ws"), &[], &DockerLimits::none());
        // Starts a one-shot interactive run.
        assert_eq!(&args[0..4], &["run", "--rm", "-i", "--init"]);
        // Workspace is a rw bind, host path == container path.
        assert!(window_has(&args, &["-v", "/private/tmp/ws:/private/tmp/ws:rw"]));
        // Working dir set to the workspace.
        assert!(window_has(&args, &["-w", "/private/tmp/ws"]));
        // GatewayOnly → host-gateway alias so the container reaches the host gateway.
        assert!(window_has(
            &args,
            &["--add-host", "host.docker.internal:host-gateway"]
        ));
        // Image is last (program + args get appended after it by the caller).
        assert_eq!(args.last().unwrap(), "img:1");
        assert!(!args.iter().any(|a| a == "--network=none"));
    }

    #[test]
    fn docker_args_no_network_and_env_forwarding() {
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/ws")],
            secret_deny: vec![],
            network: NetPolicy::None,
        };
        let args =
            policy.to_docker_run_args("img", Path::new("/ws"), &["FOO", "BAR"], &DockerLimits::none());
        assert!(args.iter().any(|a| a == "--network=none"));
        assert!(!args.iter().any(|a| a == "--add-host"));
        // Env names become `-e NAME` forwards (value comes from the docker process).
        assert!(window_has(&args, &["-e", "FOO"]));
        assert!(window_has(&args, &["-e", "BAR"]));
    }

    #[test]
    fn docker_args_mask_secret_under_a_writable_mount() {
        // Workspace == $HOME-ish root that ENCOMPASSES a secret: the secret would be
        // bind-mounted too, so it must be shadowed with an empty tmpfs.
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/home/u")],
            secret_deny: vec![
                PathBuf::from("/home/u/.ssh"), // under the mount → masked
                PathBuf::from("/elsewhere/.aws"), // not under any mount → absent, no flag
            ],
            network: NetPolicy::None,
        };
        let args = policy.to_docker_run_args("img", Path::new("/home/u"), &[], &DockerLimits::none());
        assert!(window_has(&args, &["--tmpfs", "/home/u/.ssh"]));
        assert!(!args.iter().any(|a| a == "/elsewhere/.aws"));
    }

    #[test]
    fn docker_args_render_resource_limits_only_when_set() {
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/ws")],
            secret_deny: vec![],
            network: NetPolicy::None,
        };
        // none() → no resource flags at all.
        let bare = policy.to_docker_run_args("img", Path::new("/ws"), &[], &DockerLimits::none());
        for f in ["--memory", "--cpus", "--pids-limit"] {
            assert!(!bare.iter().any(|a| a == f), "{f} must be absent when unset");
        }
        // Set ones are emitted with their values.
        let limits = DockerLimits {
            memory: Some("8g".into()),
            cpus: Some("4".into()),
            pids: Some(2048),
        };
        let args = policy.to_docker_run_args("img", Path::new("/ws"), &[], &limits);
        assert!(window_has(&args, &["--memory", "8g"]));
        assert!(window_has(&args, &["--cpus", "4"]));
        assert!(window_has(&args, &["--pids-limit", "2048"]));
    }

    #[test]
    fn net_policy_parse_maps_aliases_and_defaults() {
        assert_eq!(NetPolicy::parse("none"), NetPolicy::None);
        assert_eq!(NetPolicy::parse("OFF"), NetPolicy::None);
        assert_eq!(NetPolicy::parse("full"), NetPolicy::Full);
        assert_eq!(NetPolicy::parse(" all "), NetPolicy::Full);
        assert_eq!(NetPolicy::parse("gateway-strict"), NetPolicy::GatewayStrict);
        assert_eq!(NetPolicy::parse(" STRICT "), NetPolicy::GatewayStrict);
        // default + unknown + the explicit gateway aliases → GatewayOnly.
        for s in ["gateway-only", "gateway", "loopback", "", "nonsense"] {
            assert_eq!(NetPolicy::parse(s), NetPolicy::GatewayOnly, "{s:?}");
        }
        assert_eq!(NetPolicy::default(), NetPolicy::GatewayOnly);
    }

    #[test]
    fn docker_args_strict_egress_adds_cap_and_marker() {
        let policy = SandboxPolicy {
            writable: vec![PathBuf::from("/ws")],
            secret_deny: vec![],
            network: NetPolicy::GatewayStrict,
        };
        let args = policy.to_docker_run_args("img", Path::new("/ws"), &[], &DockerLimits::none());
        // Still reaches the host gateway, plus the NET_ADMIN cap + the strict marker the
        // entrypoint reads to install the iptables egress filter.
        assert!(window_has(&args, &["--add-host", "host.docker.internal:host-gateway"]));
        assert!(args.iter().any(|a| a == "--cap-add=NET_ADMIN"));
        assert!(window_has(&args, &["-e", "ROZUM_EGRESS=strict"]));
        assert!(!args.iter().any(|a| a == "--network=none"));
    }

    /// True if `needle` appears as a contiguous run inside `hay` (a flat argv).
    fn window_has(hay: &[String], needle: &[&str]) -> bool {
        hay.windows(needle.len())
            .any(|w| w.iter().zip(needle).all(|(a, b)| a == b))
    }

    // Real Docker e2e (ignored; needs a running daemon — pulls `busybox`). Proves
    // the rozum-generated `docker run` argv actually (1) round-trips a write in the
    // mounted workspace to the host, (2) leaves a non-mounted host path untouched
    // (confinement), and (3) masks a secret that sits under the workspace mount.
    #[test]
    #[ignore = "runs docker; needs the daemon + pulls busybox"]
    fn docker_run_confines_writes_and_masks_secret() {
        use std::process::Command;
        const IMG: &str = "busybox:latest";
        let id = std::process::id();
        let ws = resolve(&std::env::temp_dir()).join(format!("rozum-docker-e2e-{id}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join(".ssh")).unwrap();
        std::fs::write(ws.join(".ssh/id_rsa"), b"TOP-SECRET-KEY").unwrap();

        // Workspace = ws (rw), with ws/.ssh declared a secret → must be masked.
        let policy = SandboxPolicy {
            writable: vec![ws.clone()],
            secret_deny: vec![ws.join(".ssh")],
            network: NetPolicy::None,
        };
        let wsd = ws.to_string_lossy().into_owned();
        // 1+3: write inside the workspace, and try to read the masked secret.
        let run = Command::new("docker")
            .args(policy.to_docker_run_args(IMG, &ws, &[], &DockerLimits::none()))
            .args([
                "sh",
                "-c",
                &format!("echo built > {wsd}/out.txt; cat {wsd}/.ssh/id_rsa 2>/dev/null || true"),
            ])
            .output()
            .expect("spawn docker run");
        assert!(
            run.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        // The in-workspace write reached the host.
        assert_eq!(
            std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
            "built"
        );
        // The masked secret read back EMPTY inside the container (tmpfs shadow),
        // even though it's still on the host disk.
        let seen = String::from_utf8_lossy(&run.stdout);
        assert!(
            !seen.contains("TOP-SECRET-KEY"),
            "secret leaked into the container: {seen:?}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join(".ssh/id_rsa")).unwrap(),
            "TOP-SECRET-KEY",
            "host secret must be untouched"
        );

        // 2: a write to a host path OUTSIDE any mount must not reach the host.
        let escape = resolve(&std::env::temp_dir()).join(format!("rozum-docker-escape-{id}.txt"));
        let _ = std::fs::remove_file(&escape);
        let _ = Command::new("docker")
            .args(policy.to_docker_run_args(IMG, &ws, &[], &DockerLimits::none()))
            .args(["sh", "-c", &format!("echo escaped > {}", escape.display())])
            .output();
        let leaked = escape.exists();
        let _ = std::fs::remove_file(&escape);
        let _ = std::fs::remove_dir_all(&ws);
        assert!(!leaked, "SANDBOX ESCAPE: wrote to a non-mounted host path");
    }

    // Real coding-workload e2e (ignored; needs the `rozum-agent` image —
    // `scripts/build-agent-image.sh` — and a running daemon). The Docker analog of
    // `cargo_build_runs_in_jail_and_escape_denied`: a fresh crate must BUILD and RUN
    // inside the container jail (proves the agent image's toolchain works through the
    // rozum-generated `docker run`), and the build output must land on the host (the
    // workspace bind round-trips). Validated on M4 2026-06-20 (rozum-agent built from
    // docker/rozum-agent.Dockerfile). Uses the operator's configured image.
    #[test]
    #[ignore = "runs cargo build in the rozum-agent container; needs the image built"]
    fn agent_image_builds_a_crate_in_the_docker_jail() {
        use std::process::Command;
        let image = default_docker_image();
        let id = std::process::id();
        let ws = resolve(&std::env::temp_dir()).join(format!("rozum-agent-build-{id}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();

        let policy = SandboxPolicy::rust_coding(&[ws.clone()], NetPolicy::None);
        let wsd = ws.to_string_lossy().into_owned();
        let out = Command::new("docker")
            .args(policy.to_docker_run_args(&image, &ws, &[], &DockerLimits::none()))
            // A login shell, like an agent's Bash tool — exercises the PATH fix.
            .args([
                "bash",
                "-lc",
                &format!(
                    "set -e; cd {wsd}; cargo new --bin demo >/dev/null 2>&1; cd demo; \
                     cargo build --offline >/dev/null 2>&1; ./target/debug/demo"
                ),
            ])
            .output()
            .expect("spawn docker run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let leaked = ws.join("demo/target/debug/demo").exists();
        let _ = std::fs::remove_dir_all(&ws);
        assert!(
            out.status.success(),
            "cargo build failed in the agent jail: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("Hello, world!"), "binary did not run: {stdout:?}");
        assert!(leaked, "build output did not round-trip to the host workspace mount");
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
