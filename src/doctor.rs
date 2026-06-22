//! Lightweight read-only demo readiness checks.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::meeting::daemon::{daemon_alive, daemon_rooms};
use crate::meeting::room_path::meeting_sock;
use crate::sandbox::{NetPolicy, SandboxBackend};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
            CheckStatus::Skip => "skip",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub hint: Option<String>,
}

impl Check {
    fn new(
        name: &'static str,
        status: CheckStatus,
        detail: impl Into<String>,
        hint: Option<impl Into<String>>,
    ) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
            hint: hint.map(Into::into),
        }
    }

    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Ok, detail, None::<String>)
    }

    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Warn, detail, Some(hint))
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Fail, detail, Some(hint))
    }

    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Skip, detail, None::<String>)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoctorOptions {
    pub web_url: Option<String>,
    pub strict: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }

    pub fn has_warnings(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Warn)
    }

    pub fn should_fail(&self, strict: bool) -> bool {
        self.has_failures() || (strict && self.has_warnings())
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut ok = 0;
        let mut warn = 0;
        let mut fail = 0;
        let mut skip = 0;
        for c in &self.checks {
            match c.status {
                CheckStatus::Ok => ok += 1,
                CheckStatus::Warn => warn += 1,
                CheckStatus::Fail => fail += 1,
                CheckStatus::Skip => skip += 1,
            }
        }
        (ok, warn, fail, skip)
    }

    pub fn render(&self) -> String {
        let mut out = String::from("rozum doctor\n");
        for check in &self.checks {
            out.push_str(&format!(
                "  [{:<4}] {:<20} {}\n",
                check.status.label(),
                check.name,
                check.detail
            ));
            if let Some(hint) = &check.hint {
                out.push_str(&format!("         hint: {hint}\n"));
            }
        }
        let (ok, warn, fail, skip) = self.counts();
        out.push_str(&format!(
            "summary: {ok} ok, {warn} warn, {fail} fail, {skip} skip\n"
        ));
        out
    }
}

pub async fn run(options: DoctorOptions) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(check_demo_launcher());
    checks.push(check_tailscale_cli());
    checks.push(check_meeting_daemon().await);
    checks.push(check_gateway().await);
    checks.extend(check_sandbox());
    match options.web_url.as_deref() {
        Some(url) if !url.trim().is_empty() => checks.extend(check_web_url(url).await),
        _ => checks.push(Check::skip(
            "web-pwa",
            "not probed (pass --web-url http://host:port)",
        )),
    }
    DoctorReport { checks }
}

async fn check_meeting_daemon() -> Check {
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        return Check::fail(
            "meeting-daemon",
            format!("not reachable ({})", sock.display()),
            "run: rozum meetings start",
        );
    }
    match daemon_rooms(&sock).await {
        Ok(rooms) => Check::ok(
            "meeting-daemon",
            format!(
                "running; {} room(s) listed ({})",
                rooms.len(),
                sock.display()
            ),
        ),
        Err(e) => Check::warn(
            "meeting-daemon",
            format!("reachable but rooms.list failed: {e}"),
            "run: rozum meetings status",
        ),
    }
}

async fn check_gateway() -> Check {
    let Some(active) = crate::share::read_active() else {
        return Check::warn(
            "gateway",
            format!(
                "no active registry at {}",
                crate::share::active_path().display()
            ),
            "run: rozum gateway --model <model>",
        );
    };
    if crate::share::health_ok(active.port).await {
        Check::ok(
            "gateway",
            format!(
                "healthy on :{}; model={} pid={} generation={}",
                active.port, active.model, active.pid, active.generation
            ),
        )
    } else {
        Check::fail(
            "gateway",
            format!(
                "registry exists but health check failed on :{}; model={} pid={}",
                active.port, active.model, active.pid
            ),
            "run: rozum gateway status; remove stale registry or restart the gateway",
        )
    }
}

fn check_sandbox() -> Vec<Check> {
    let cfg = crate::RuntimeConfig::load()
        .map(|c| c.sandbox)
        .unwrap_or_default();
    let backend = select_sandbox_backend(
        std::env::var("ROZUM_SANDBOX_BACKEND").ok().as_deref(),
        cfg.backend.as_deref(),
    );
    let network = select_sandbox_network(
        std::env::var("ROZUM_SANDBOX_NETWORK").ok().as_deref(),
        cfg.network.as_deref(),
    );
    let enabled = sandbox_enabled(std::env::var_os("ROZUM_SANDBOX").as_deref());

    let mut checks = Vec::new();
    if !enabled {
        checks.push(Check::warn(
            "sandbox",
            format!("disabled by ROZUM_SANDBOX=0; backend={backend:?} network={network:?}"),
            "unset ROZUM_SANDBOX or set it to 1 before a sandboxed demo",
        ));
        checks.push(Check::skip("docker-image", "sandbox disabled"));
        return checks;
    }

    match backend {
        SandboxBackend::Seatbelt if cfg!(target_os = "macos") => checks.push(Check::ok(
            "sandbox",
            format!("enabled; backend=seatbelt network={network:?}"),
        )),
        SandboxBackend::Seatbelt => checks.push(Check::warn(
            "sandbox",
            format!("backend=seatbelt network={network:?}, but Seatbelt is macOS-only"),
            "use ROZUM_SANDBOX_BACKEND=docker on non-macOS hosts",
        )),
        SandboxBackend::Docker => checks.push(Check::ok(
            "sandbox",
            format!("enabled; backend=docker network={network:?}"),
        )),
    }

    if backend == SandboxBackend::Docker {
        checks.extend(check_docker_image());
    } else {
        checks.push(Check::skip(
            "docker-image",
            "not selected (sandbox backend is seatbelt)",
        ));
    }
    checks
}

fn check_docker_image() -> Vec<Check> {
    let mut checks = Vec::new();
    let version = Command::new("docker")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match version {
        Ok(status) if status.success() => {}
        Ok(status) => {
            checks.push(Check::fail(
                "docker-cli",
                format!("docker --version exited with {status}"),
                "start Docker Desktop or fix the docker CLI",
            ));
            return checks;
        }
        Err(e) => {
            checks.push(Check::fail(
                "docker-cli",
                format!("docker command unavailable: {e}"),
                "install/start Docker before selecting the Docker sandbox backend",
            ));
            return checks;
        }
    }
    checks.push(Check::ok("docker-cli", "docker command available"));

    let image = crate::sandbox::default_docker_image();
    let inspect = Command::new("docker")
        .args(["image", "inspect", &image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match inspect {
        Ok(status) if status.success() => checks.push(Check::ok(
            "docker-image",
            format!("image exists locally: {image}"),
        )),
        Ok(status) => checks.push(Check::fail(
            "docker-image",
            format!("image missing or unreadable: {image} ({status})"),
            "run: scripts/build-agent-image.sh",
        )),
        Err(e) => checks.push(Check::fail(
            "docker-image",
            format!("could not inspect image {image}: {e}"),
            "run: scripts/build-agent-image.sh",
        )),
    }
    checks
}

fn check_demo_launcher() -> Check {
    let path = Path::new("scripts/demo-conference.sh");
    let Ok(meta) = std::fs::metadata(path) else {
        return Check::warn(
            "demo-launcher",
            "scripts/demo-conference.sh is missing",
            "restore the demo launcher before running the conference demo",
        );
    };
    if !meta.is_file() {
        return Check::warn(
            "demo-launcher",
            "scripts/demo-conference.sh exists but is not a file",
            "replace it with the demo launcher script",
        );
    }
    if is_executable(&meta) {
        Check::ok(
            "demo-launcher",
            "scripts/demo-conference.sh exists and is executable",
        )
    } else {
        Check::warn(
            "demo-launcher",
            "scripts/demo-conference.sh exists but is not executable",
            "run: chmod +x scripts/demo-conference.sh",
        )
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    meta.is_file()
}

fn check_tailscale_cli() -> Check {
    match Command::new("tailscale")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Check::ok("tailscale", "tailscale CLI available"),
        Ok(status) => Check::warn(
            "tailscale",
            format!("tailscale version exited with {status}"),
            "check tailscale status before phone/PWA demo",
        ),
        Err(e) => Check::warn(
            "tailscale",
            format!("tailscale CLI unavailable: {e}"),
            "install/start Tailscale or skip phone-over-tailnet demo",
        ),
    }
}

async fn check_web_url(url: &str) -> Vec<Check> {
    let base = normalize_url(url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut checks = Vec::new();
    checks.push(check_http(&client, "web-root", &base, true).await);
    checks.push(
        check_http(
            &client,
            "web-manifest",
            &join_url(&base, "manifest.webmanifest"),
            false,
        )
        .await,
    );
    checks.push(check_http(&client, "web-sw", &join_url(&base, "sw.js"), false).await);
    checks
}

async fn check_http(
    client: &reqwest::Client,
    name: &'static str,
    url: &str,
    auth_ok: bool,
) -> Check {
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            Check::ok(name, format!("{url} -> {}", resp.status()))
        }
        Ok(resp) if auth_ok && resp.status() == reqwest::StatusCode::UNAUTHORIZED => Check::ok(
            name,
            format!("{url} reachable but requires auth ({})", resp.status()),
        ),
        Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => Check::warn(
            name,
            format!("{url} requires auth; route existence not fully confirmed"),
            "use ROZUM_WEB_NO_AUTH=1 for a tailnet-gated PWA demo, or test after login",
        ),
        Ok(resp) => Check::fail(
            name,
            format!("{url} -> {}", resp.status()),
            "start/restart the .ssc meeting client (rozum-meeting-ssc; launchd com.rozum.meeting-ssc)",
        ),
        Err(e) => Check::fail(
            name,
            format!("{url} unreachable: {e}"),
            "start the .ssc meeting client (rozum-meeting-ssc; launchd com.rozum.meeting-ssc)",
        ),
    }
}

pub fn select_sandbox_backend(
    env_backend: Option<&str>,
    config_backend: Option<&str>,
) -> SandboxBackend {
    env_backend
        .map(SandboxBackend::parse)
        .or_else(|| config_backend.map(SandboxBackend::parse))
        .unwrap_or_default()
}

pub fn select_sandbox_network(
    env_network: Option<&str>,
    config_network: Option<&str>,
) -> NetPolicy {
    env_network
        .map(NetPolicy::parse)
        .or_else(|| config_network.map(NetPolicy::parse))
        .unwrap_or_default()
}

pub fn sandbox_enabled(value: Option<&OsStr>) -> bool {
    !matches!(value.and_then(OsStr::to_str).map(str::trim), Some("0"))
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", normalize_url(base), path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mode_fails_on_warning() {
        let report = DoctorReport {
            checks: vec![Check::warn("gateway", "missing", "start it")],
        };
        assert!(!report.should_fail(false));
        assert!(report.should_fail(true));
    }

    #[test]
    fn render_includes_statuses_and_summary() {
        let report = DoctorReport {
            checks: vec![
                Check::ok("meeting-daemon", "running"),
                Check::skip("web-pwa", "not probed"),
            ],
        };
        let rendered = report.render();
        assert!(rendered.contains("[ok  ] meeting-daemon"));
        assert!(rendered.contains("[skip] web-pwa"));
        assert!(rendered.contains("summary: 1 ok, 0 warn, 0 fail, 1 skip"));
    }

    #[test]
    fn sandbox_selection_prefers_env_over_config() {
        assert_eq!(
            select_sandbox_backend(Some("docker"), Some("seatbelt")),
            SandboxBackend::Docker
        );
        assert_eq!(
            select_sandbox_network(Some("none"), Some("full")),
            NetPolicy::None
        );
    }

    #[test]
    fn sandbox_enabled_only_zero_disables() {
        assert!(!sandbox_enabled(Some(OsStr::new("0"))));
        assert!(sandbox_enabled(Some(OsStr::new("false"))));
        assert!(sandbox_enabled(None));
    }

    #[test]
    fn join_url_trims_double_slashes() {
        assert_eq!(
            join_url("http://localhost:8400/", "/manifest.webmanifest"),
            "http://localhost:8400/manifest.webmanifest"
        );
    }
}
