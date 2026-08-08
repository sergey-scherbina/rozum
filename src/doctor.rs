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
    /// Also report on the `com.rozum.*` launchd jobs and the endpoints they serve
    /// (`docs/specs/service-liveness.md`).
    pub services: bool,
    /// ONLY the service section — no demo-path checks.
    ///
    /// The periodic job runs from launchd, which starts it in `/` with a minimal `PATH`, so the
    /// demo checks report `scripts/demo-conference.sh is missing` and `tailscale unavailable`
    /// every five minutes. Neither is true of the machine; both are true of that environment. A
    /// watcher that cries wolf twice a tick is the failure this whole check exists to remove.
    pub services_only: bool,
    /// Post a line to this room when a service CHANGES verdict, and stay silent otherwise. For the
    /// periodic job: every tick would be noise, a transition is news.
    pub post_room: Option<String>,
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
    if options.services_only {
        return DoctorReport { checks: check_services().await };
    }
    checks.push(check_demo_launcher());
    checks.push(check_tailscale_cli());
    checks.push(check_meeting_daemon().await);
    checks.push(check_gateway().await);
    checks.extend(check_sandbox());
    if options.services {
        checks.extend(check_services().await);
    }
    match options.web_url.as_deref() {
        Some(url) if !url.trim().is_empty() => checks.extend(check_web_url(url).await),
        _ => checks.push(Check::skip(
            "web-pwa",
            "not probed (pass --web-url http://host:port)",
        )),
    }
    DoctorReport { checks }
}

/// Remember the last verdict per service, and return only what CHANGED.
///
/// A line every five minutes is a line nobody reads, which is the same failure as no line at all
/// arrived at from the other side. State lives next to the rest of rozum's state; a missing file
/// means "first run", and a first run announces nothing — it would otherwise shout the whole
/// roster at whoever installs the job.
pub fn transitions(report: &DoctorReport) -> Vec<String> {
    let path = crate::meeting::rozum_state_dir().join("service-liveness.json");
    let previous: std::collections::HashMap<String, String> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now: std::collections::HashMap<String, String> = report
        .checks
        .iter()
        .filter(|c| SERVICES.iter().any(|(_, n, _, _, _, _)| *n == c.name))
        .map(|c| (c.name.to_string(), c.status.label().to_string()))
        .collect();

    let mut lines = Vec::new();
    if !previous.is_empty() {
        let mut names: Vec<&String> = now.keys().collect();
        names.sort();
        for name in names {
            let before = previous.get(name).map(String::as_str).unwrap_or("unknown");
            let after = now[name].as_str();
            if before == after {
                continue;
            }
            let detail = report
                .checks
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.detail.as_str())
                .unwrap_or("");
            lines.push(match (before, after) {
                (_, "ok") => format!("✅ {name}: back ({before} → ok) — {detail}"),
                _ => format!("⚠️ {name}: {before} → {after} — {detail}"),
            });
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(&now) {
        let _ = std::fs::write(&path, text);
    }
    lines
}

/// What this machine runs, and what each of those is supposed to answer.
///
/// `endpoint: None` is deliberate and is reported as `skip`: the bridges talk outward to Telegram
/// and the participant pools talk to the daemon over a socket they hold open, so there is nothing
/// here to ask. Inventing a probe that cannot fail would be worse than saying "not probed" — see
/// `docs/specs/service-liveness.md`.
/// How to ask a service whether it is doing its job.
#[derive(Clone, Copy)]
enum Probe {
    /// A plain GET whose status is the answer.
    Get(&'static str),
    /// An MCP `initialize` over HTTP. The proxy answers 404 to every path but `/mcp` and 406 to a
    /// GET without the streaming `Accept`, so a "does the port respond" probe reports a healthy
    /// server as broken — measured on the first live run of this check. Speaking its protocol is
    /// the only probe that means anything.
    McpInitialize(&'static str),
    /// Nothing to ask: the bridges talk outward to Telegram, the pools hold a socket to the
    /// daemon. Reported as `skip`, because inventing a probe that cannot fail is worse than
    /// admitting there is none (`docs/specs/service-liveness.md`).
    None,
}

/// A `StartInterval` job is healthy when it ran recently, whatever the process table says.
///
/// "Recently" is two intervals: one tick can be missed to a busy machine without it meaning
/// anything, two in a row means it is not running. The evidence is the state file the doctor
/// writes on EVERY run — the log would do, but a log can be rotated or redirected, while that file
/// is written by the work itself.
fn periodic_check(name: &'static str, last_exit: i64, every_secs: u64, what: &str) -> Check {
    let path = crate::meeting::rozum_state_dir().join("service-liveness.json");
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    match age {
        None => Check::warn(
            name,
            format!("installed, but it has never written its state — {what}"),
            format!("run it once by hand: rozum-gateway doctor --services (writes {})", path.display()),
        ),
        Some(a) if a <= every_secs * 2 => {
            Check::ok(name, format!("ran {a}s ago (every {every_secs}s), last exit {last_exit}"))
        }
        Some(a) => Check::fail(
            name,
            format!(
                "silent for {a}s — more than two intervals of {every_secs}s, so {what} is NOT \
                 watching anything"
            ),
            format!("launchctl kickstart -k gui/$UID/com.rozum.doctor, then read ~/.rozum-doctor.log"),
        ),
    }
}

/// What a launchd exit code MEANS, for the two that this project has actually been bitten by.
///
/// `78` is the one that cost four days (BUG-013): launchd itself refuses to exec the program and
/// reports `EX_CONFIG`. **Our binaries never exit 78** — they use 0/1/2 — so seeing it means the
/// process never ran at all, which is also why the job's log stays empty while `KeepAlive`
/// respawns it tens of thousands of times. A reader who does not know that sees a number and moves
/// on; naming it is the difference between a four-day silence and a sentence.
///
/// `-9` is the other: launchd had to SIGKILL it, which is what both Telegram bridges looked like
/// after a bad install on 2026-08-05.
fn exit_meaning(code: i64) -> Option<&'static str> {
    match code {
        78 => Some(
            "EX_CONFIG — launchd REFUSED to exec it, so the program never ran and its log is \
             empty however many times it respawned (BUGS.md BUG-013)",
        ),
        -9 => Some("SIGKILL — launchd had to kill it; a fresh install that cannot start looks like this"),
        _ => None,
    }
}

/// How a job is expected to be found.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// A service: it should be RUNNING. Not running is the failure this check exists for.
    Resident,
    /// A `StartInterval` job: between ticks it is correctly NOT running, and "no pid" says
    /// nothing. What matters is that it RAN recently — measured by the state file it writes on
    /// every run. Without this the watcher could not be watched: adding `com.rozum.doctor` to the
    /// resident list would have made it permanently, wrongly red.
    Periodic { every_secs: u64 },
}

/// Who actually serves, when that can be established independently of the job.
#[derive(Clone, Copy)]
enum Owner {
    /// The holder of the lock beside the unix socket (`docs/specs/meeting-socket-ownership.md`).
    /// A launchd job can be alive and WAITING while a client-spawned daemon holds this and serves —
    /// observed on the host 2026-08-05: job pid 42206, lock and listener on 42132.
    SocketLock,
    /// Nothing to ask: the job's own process is the server, or there is no server.
    JobItself,
}

/// `(launchd label, row name, probe, owner, what it serves)`.
///
/// The row name is `svc:*` and not the bare service name ON PURPOSE: `rozum doctor` already has
/// checks called `gateway` and `meeting-daemon` (the demo-path section), and sharing a name made
/// the transition line quote the wrong check's detail — measured on the first live post, where a
/// service transition was reported with the demo check's text. Two rows with one name are two
/// facts a lookup cannot tell apart.
const SERVICES: &[(&str, &str, Probe, Owner, Shape, &str)] = &[
    ("com.rozum.gateway", "svc:gateway", Probe::Get("http://127.0.0.1:8089/v1/models"), Owner::JobItself, Shape::Resident, "the resident model"),
    ("com.rozum.ucc-control", "svc:ucc-control", Probe::Get("http://127.0.0.1:8411/control/auth/status"), Owner::JobItself, Shape::Resident, "the control plane"),
    ("com.rozum.meeting-daemon", "svc:meeting-daemon", Probe::Get("http://127.0.0.1:8401/rooms"), Owner::SocketLock, Shape::Resident, "meeting rooms over REST"),
    ("com.rozum.meeting-ssc", "svc:meeting-ssc", Probe::Get("http://127.0.0.1:8405/"), Owner::JobItself, Shape::Resident, "the meeting PWA"),
    ("com.rozum.mcp-http", "svc:mcp-http", Probe::McpInitialize("http://127.0.0.1:8779/mcp"), Owner::JobItself, Shape::Resident, "MCP over HTTP"),
    ("com.rozum.telegram", "svc:telegram", Probe::None, Owner::JobItself, Shape::Resident, "the Telegram bridge (private)"),
    ("com.rozum.telegram-groups", "svc:telegram-groups", Probe::None, Owner::JobItself, Shape::Resident, "the Telegram bridge (groups)"),
    ("com.rozum.assistant", "svc:assistant", Probe::None, Owner::JobItself, Shape::Resident, "the participant pool"),
    ("com.rozum.assistant-groups", "svc:assistant-groups", Probe::None, Owner::JobItself, Shape::Resident, "the participant pool (groups)"),
    // The watcher, watched by the same list it walks. It is a StartInterval job, so between ticks
    // it is correctly not running; what would be wrong is silence for longer than its interval.
    ("com.rozum.doctor", "svc:doctor", Probe::None, Owner::JobItself, Shape::Periodic { every_secs: 300 }, "this liveness check itself"),
];

/// The pid holding the lock beside the meeting socket, i.e. the process that is actually serving
/// (`docs/specs/meeting-socket-ownership.md`). `None` when nothing holds it or `lsof` is absent —
/// unknown is reported as unknown, never as agreement.
/// `…/meeting.sock` → `…/meeting.sock.lock`. Appended, not substituted: `with_extension` alone
/// would turn `meeting.sock` into `meeting.lock` and silently look at a file nobody writes, which
/// would make this check answer "unknown" forever without ever saying so.
fn socket_lock_path(sock: &Path) -> std::path::PathBuf {
    let mut p = sock.as_os_str().to_os_string();
    p.push(".lock");
    std::path::PathBuf::from(p)
}

fn socket_owner_pid() -> Option<i64> {
    let lock = socket_lock_path(&meeting_sock());
    let out = Command::new("lsof").arg("-t").arg(&lock).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse::<i64>().ok()
}

/// One line per service: is the job running, and does the thing it serves answer.
///
/// The four-day outage this exists for (BUG-013) had a job that was LOADED, was being restarted by
/// `KeepAlive` 36,000 times, and served nothing. `launchctl` alone cannot tell that from health;
/// only asking the endpoint can.
async fn check_services() -> Vec<Check> {
    let jobs = launchctl_jobs();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut out = Vec::new();
    for (label, name, probe, owner, shape, what) in SERVICES {
        out.push(check_service(&jobs, label, name, *probe, *owner, *shape, what, &client).await);
    }
    out
}

/// `label -> (pid, last exit status)`. `pid` is `None` when the job is loaded but not running,
/// which is exactly the state two bridges sat in for several minutes today while everything else
/// looked fine.
fn launchctl_jobs() -> std::collections::HashMap<String, (Option<i64>, i64)> {
    let Ok(out) = Command::new("launchctl").arg("list").stderr(Stdio::null()).output() else {
        return std::collections::HashMap::new();
    };
    parse_launchctl(&String::from_utf8_lossy(&out.stdout))
}

/// Split out from the process call so the shape can be tested: `-` in the PID column is the state
/// that matters (loaded, not running) and it is the one a bare exit code hides.
fn parse_launchctl(text: &str) -> std::collections::HashMap<String, (Option<i64>, i64)> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (Some(pid), Some(status), Some(label)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !label.starts_with("com.rozum.") {
            continue;
        }
        map.insert(
            label.to_string(),
            (pid.parse::<i64>().ok(), status.parse::<i64>().unwrap_or(0)),
        );
    }
    map
}


async fn check_service(
    jobs: &std::collections::HashMap<String, (Option<i64>, i64)>,
    label: &'static str,
    name: &'static str,
    probe: Probe,
    owner: Owner,
    shape: Shape,
    what: &str,
    client: &reqwest::Client,
) -> Check {
    let Some((pid, last_exit)) = jobs.get(label).copied() else {
        return Check::skip(name, format!("not installed on this machine ({label})"));
    };
    // A periodic job is judged by WHEN IT LAST RAN, not by whether it holds a pid right now.
    if let Shape::Periodic { every_secs } = shape {
        return periodic_check(name, last_exit, every_secs, what);
    }
    let Some(pid) = pid else {
        // The job is down — but something else may be serving in its place. Measured the first day
        // this check existed: `com.rozum.meeting-daemon` was not running while `:8401` answered,
        // because a bridge had spawned its own daemon on demand and won the socket. Calling that
        // `fail` would be a red the operator cannot clear; calling it `ok` would hide that nothing
        // will restart it. It is its own state, and it says which.
        let served = match probe {
            Probe::None => None,
            Probe::Get(u) => client.get(u).send().await.ok().map(|r| r.status().as_u16()),
            Probe::McpInitialize(u) => mcp_initialize(client, u).await.ok().map(|_| 200),
        };
        return match served {
            Some(code) => Check::warn(
                name,
                format!(
                    "launchd's copy is NOT running (last exit {last_exit}), yet {what} answers \
                     ({code}) — served by {}",
                    match owner {
                        Owner::SocketLock => socket_owner_pid()
                            .map(|p| format!("pid {p}, which holds the socket"))
                            .unwrap_or_else(|| "something unmanaged".into()),
                        Owner::JobItself => "something unmanaged".to_string(),
                    }
                ),
                format!(
                    "find it (pgrep -f {name}) and decide who owns it: while launchd's copy is \
                     down, nothing restarts this if the unmanaged one dies"
                ),
            ),
            None => Check::fail(
                name,
                format!(
                    "loaded but NOT running (last exit {last_exit}{}) — {what} is down",
                    exit_meaning(last_exit).map(|m| format!(", {m}")).unwrap_or_default()
                ),
                format!("launchctl bootout gui/$UID/{label}; launchctl bootstrap gui/$UID ~/Library/LaunchAgents/{label}.plist"),
            ),
        };
    };
    let url = match probe {
        // No endpoint to ask. Say what IS known and do not dress it up as health.
        Probe::None => {
            return Check::skip(name, format!("running (pid {pid}), no endpoint to probe — {what}"))
        }
        Probe::Get(u) | Probe::McpInitialize(u) => u,
    };
    let answered = match probe {
        Probe::Get(u) => match client.get(u).send().await {
            // 401 is an answer: the control plane demanding a session is proof it is alive.
            Ok(r) if r.status().is_success() || r.status().as_u16() == 401 => {
                Ok(format!("answers {}", r.status().as_u16()))
            }
            Ok(r) => Err(format!("answered {}", r.status().as_u16())),
            Err(e) => Err(format!("did not answer ({e})")),
        },
        Probe::McpInitialize(u) => mcp_initialize(client, u).await,
        Probe::None => unreachable!("handled above"),
    };
    // WHO serves. With the socket-ownership fix (BUG-025) the job can be alive and merely waiting
    // while a client-spawned daemon holds the lock and the listener — observed on the host, job pid
    // 42206 against owner 42132. "running (pid 42206), :8401 answers 200" is then two true halves
    // that together say something false, which is the failure this whole check exists to remove.
    let served_by = match owner {
        Owner::JobItself => None,
        Owner::SocketLock => socket_owner_pid(),
    };
    match answered {
        Ok(detail) => match served_by {
            Some(other) if other != pid as i64 => Check::warn(
                name,
                format!(
                    "job pid {pid} is alive but serves nothing — {url} {detail}, served by pid \
                     {other}, which holds the socket"
                ),
                format!(
                    "that is a client-spawned daemon, not launchd's: it works, but the job cannot \
                     restart what it does not own (BUGS.md BUG-025)"
                ),
            ),
            Some(_) => Check::ok(name, format!("running (pid {pid}) and owns the socket, {url} {detail}")),
            None => Check::ok(name, format!("running (pid {pid}), {url} {detail}")),
        },
        // The BUG-013 shape exactly: the process table says yes, the service says nothing.
        Err(why) => Check::fail(
            name,
            format!("running (pid {pid}) but {url} {why} — {what} is not being served"),
            format!("a job that cannot serve is indistinguishable from a healthy one here — read its log, then bootout+bootstrap {label}"),
        ),
    }
}

/// Speak MCP at it: an `initialize` whose reply carries a `serverInfo` is the service doing its
/// actual job, which is what this check is for.
async fn mcp_initialize(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "rozum-doctor", "version": "1"}
        }
    });
    let resp = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("did not answer ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if text.contains("serverInfo") {
        let name = text
            .split("\"name\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("mcp");
        Ok(format!("speaks MCP ({name})"))
    } else {
        Err(format!("answered {status} but not with an MCP initialize result"))
    }
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
    fn the_lock_sits_beside_the_socket_not_instead_of_it() {
        // `with_extension` would produce `meeting.lock` — a path nothing writes, so the owner would
        // read as unknown forever and the check would go back to reporting the job as the server.
        assert_eq!(
            socket_lock_path(Path::new("/run/rozum/meeting.sock")),
            Path::new("/run/rozum/meeting.sock.lock")
        );
        assert_eq!(socket_lock_path(Path::new("/x/sock")), Path::new("/x/sock.lock"));
    }

    #[test]
    fn the_exit_code_that_cost_four_days_is_named() {
        // 78 is not ours: this project's binaries exit 0/1/2, so EX_CONFIG means launchd never got
        // the program started — which is why 36,301 respawns wrote nothing to the log.
        assert!(exit_meaning(78).unwrap().contains("REFUSED"));
        assert!(exit_meaning(-9).unwrap().contains("SIGKILL"));
        // Ordinary codes get no invented meaning.
        assert_eq!(exit_meaning(0), None);
        assert_eq!(exit_meaning(1), None);
        assert_eq!(exit_meaning(2), None);
    }

    /// The two states `launchctl list` prints that this check exists to tell apart.
    #[test]
    fn a_loaded_job_that_is_not_running_is_not_a_running_one() {
        // Real output, including the shapes that bit us: `-` for a job that is loaded and dead,
        // and a negative last-exit for one launchd had to SIGKILL.
        let jobs = parse_launchctl(
            "PID\tStatus\tLabel\n             83185\t0\tcom.rozum.gateway\n             -\t0\tcom.rozum.meeting-daemon\n             -\t-9\tcom.rozum.telegram\n             1234\t0\tcom.apple.something\n",
        );
        assert_eq!(jobs.get("com.rozum.gateway"), Some(&(Some(83185), 0)));
        // Loaded, exit code 0, and DOWN. A reader who only looks at the status column sees a zero
        // and moves on — that is how a four-day outage stayed invisible (BUG-013).
        assert_eq!(jobs.get("com.rozum.meeting-daemon"), Some(&(None, 0)));
        // Killed by launchd, which is what both bridges looked like after a bad install today.
        assert_eq!(jobs.get("com.rozum.telegram"), Some(&(None, -9)));
        // Not ours, not our business.
        assert!(!jobs.contains_key("com.apple.something"));
    }

    /// Every service either has a probe or says out loud that it has none.
    #[test]
    fn no_service_is_reported_healthy_on_the_process_table_alone() {
        for (label, name, probe, _owner, _shape, what) in SERVICES {
            assert!(label.starts_with("com.rozum."), "{label}");
            assert!(name.starts_with("svc:"), "{name} must not collide with a demo-path check");
            assert!(!what.is_empty(), "{label} must say what it serves");
            if let Probe::Get(u) | Probe::McpInitialize(u) = probe {
                assert!(u.starts_with("http://127.0.0.1:"), "{label} probes off-machine: {u}");
            }
        }
        // And the ones with no probe are exactly the ones that serve nothing locally: a bridge
        // talks outward to Telegram, a pool holds a socket to the daemon. If this list grows, the
        // new entry needs a probe or a reason.
        let unprobed: Vec<&str> = SERVICES
            .iter()
            .filter(|(_, _, p, _, sh, _)| matches!(p, Probe::None) && *sh == Shape::Resident)
            .map(|(l, _, _, _, _, _)| *l)
            .collect();
        assert_eq!(
            unprobed,
            vec![
                "com.rozum.telegram",
                "com.rozum.telegram-groups",
                "com.rozum.assistant",
                "com.rozum.assistant-groups"
            ]
        );
    }

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
