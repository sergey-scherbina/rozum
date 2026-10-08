//! Finding the shared gateway, and starting it when it is not there (`docs/specs/gateway-ensure.md`).
//!
//! One function every client calls — in-process from Rust, through `rozum gateway ensure --json` from
//! anything else — so no client keeps a default port of its own. The Rust nadia kept `:8080` while the
//! gateway lived on `:8089` and announced so in `active.json`; that is the failure this removes.
//!
//! Order: the registry, if its port answers; the default port, if it answers; else start one — by
//! asking launchd where `com.rozum.gateway` is installed, and NEVER by spawning beside such a job
//! (`docs/specs/meeting-daemon-ownership.md`); by spawning a detached launch-managed daemon under the
//! spawn lock where it is not.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::share::{self, ActiveGateway, DEFAULT_GATEWAY_PORT};

/// The launchd job that owns the shared gateway where `rozum service install` was run.
pub const GATEWAY_LAUNCHD_LABEL: &str = "com.rozum.gateway";

/// How the gateway that answered came to be there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// It was already serving.
    Running,
    /// launchd's job was asked to start it.
    Launchd,
    /// This call spawned it.
    Spawned,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::Running => "running",
            How::Launchd => "started by launchd",
            How::Spawned => "spawned",
        }
    }
    /// The stable spelling for `--json`.
    pub fn key(self) -> &'static str {
        match self {
            How::Running => "running",
            How::Launchd => "launchd",
            How::Spawned => "spawned",
        }
    }
}

/// A gateway that answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub port: u16,
    /// The model it holds: from the registry, else from `/v1/models`; empty when neither says.
    pub model: String,
    /// 0 when the gateway answered on the default port with no registry to name its process.
    pub pid: u32,
    pub how: How,
    /// Whether the model is loaded now. `false` is not an error: the next request reloads it.
    pub resident: bool,
}

impl Found {
    /// The origin, without `/v1` — what `ROZUM_GATEWAY_URL` holds.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[derive(Clone, Debug)]
pub struct Opts {
    /// The model to start a gateway with, when one has to be started. `None` (or `local`) leaves it
    /// to `[runtime].model` in `rozum.toml`. Never used to switch a running gateway.
    pub model: Option<String>,
    /// `false`: find only, start nothing.
    pub start: bool,
    /// How long to wait for a started gateway to answer.
    pub wait: Duration,
    /// Told, once, why the wait is long — a gateway that is up but waiting for host RAM binds no
    /// port, and a client that says nothing for minutes looks hung (`starting.json`).
    pub notify: Option<fn(&str)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { model: None, start: true, wait: Duration::from_secs(300), notify: None }
    }
}

/// What to do when nothing answers. The rule in one place: where the job exists, launchd starts the
/// gateway, however long it takes; a client spawns one only where it does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartPlan {
    AskLaunchd,
    SpawnOwn,
    /// Someone else holds the spawn lock: they are starting one.
    WaitForOther,
}

pub fn start_plan(job_exists: bool, got_spawn_lock: bool) -> StartPlan {
    if job_exists {
        StartPlan::AskLaunchd
    } else if got_spawn_lock {
        StartPlan::SpawnOwn
    } else {
        StartPlan::WaitForOther
    }
}

/// Find the shared gateway; start it if allowed and nothing answers.
pub async fn ensure(opts: &Opts) -> Result<Found, String> {
    if let Some(found) = find().await {
        return Ok(found);
    }
    if let Some(st) = read_starting() {
        // Up, but not serving yet: starting it again would make a second gateway queue for the
        // same RAM. Wait for this one while it lives, and say why the wait is long; if it gives up
        // (admission refused, it exits), what is left is the ordinary "nothing answers" below.
        if !opts.start {
            return Err(st.reason());
        }
        if let Some(n) = opts.notify {
            n(&st.reason());
        }
        let deadline = Instant::now() + opts.wait;
        while read_starting().is_some() && Instant::now() < deadline {
            if let Some(f) = find().await {
                return Ok(f);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if let Some(f) = find().await {
            return Ok(f);
        }
        if let Some(st) = read_starting() {
            return Err(format!("still not serving after {}s — {}", opts.wait.as_secs(), st.reason()));
        }
    }
    if !opts.start {
        return Err(format!(
            "no gateway answers: looked at {} and :{DEFAULT_GATEWAY_PORT}",
            share::active_path().display()
        ));
    }
    let job = launchd_job_exists(GATEWAY_LAUNCHD_LABEL);
    // The lock is taken only where we might spawn; held until the spawned daemon answers.
    let lock = if job { None } else { share::try_spawn_lock(120) };
    match start_plan(job, lock.is_some()) {
        StartPlan::AskLaunchd => {
            launchd_kickstart(GATEWAY_LAUNCHD_LABEL);
            wait_for(opts.wait, || false, opts.notify).await.map(|f| Found { how: How::Launchd, ..f }).map_err(|e| {
                format!(
                    "launchd job {GATEWAY_LAUNCHD_LABEL} did not bring the gateway up ({e}); \
                     see `launchctl print gui/$(id -u)/{GATEWAY_LAUNCHD_LABEL}` and {}",
                    share::gateway_dir().join("service.log").display()
                )
            })
        }
        StartPlan::WaitForOther => wait_for(opts.wait, || false, opts.notify).await.map_err(|e| {
            format!("another client is starting the gateway and it did not come up ({e})")
        }),
        StartPlan::SpawnOwn => {
            let log = share::gateway_dir().join("gateway.log");
            let mut child = spawn_gateway(&gateway_binary(), spawn_model(opts.model.as_deref()), &log)?;
            let found = wait_for(opts.wait, || matches!(child.try_wait(), Ok(Some(_))), opts.notify).await;
            drop(lock);
            match found {
                Ok(f) => Ok(Found { how: How::Spawned, ..f }),
                Err(e) => {
                    let exited = child.try_wait().ok().flatten();
                    Err(match exited {
                        Some(st) if st.code() == Some(2) && spawn_model(opts.model.as_deref()).is_none() => format!(
                            "the gateway needs a model: pass --model, or set [runtime].model in rozum.toml (see {})",
                            log.display()
                        ),
                        Some(st) => format!("the gateway exited before answering ({st}); see {}", log.display()),
                        None => format!("{e}; see {}", log.display()),
                    })
                }
            }
        }
    }
}

/// The registry if it answers, else the default port if it answers.
pub async fn find() -> Option<Found> {
    if let Some(g) = share::read_active() {
        if let Some(models) = fetch_models(g.port).await {
            return Some(from_registry(&g, &models));
        }
    }
    let models = fetch_models(DEFAULT_GATEWAY_PORT).await?;
    let (model, resident) = held_model(&models, None);
    Some(Found { port: DEFAULT_GATEWAY_PORT, model, pid: 0, how: How::Running, resident })
}

fn from_registry(g: &ActiveGateway, models: &serde_json::Value) -> Found {
    let (_, resident) = held_model(models, Some(&g.model));
    Found { port: g.port, model: g.model.clone(), pid: g.pid, how: How::Running, resident }
}

/// The model a gateway holds and whether it is loaded, from its `/v1/models`. Each entry carries the
/// spec as `display_name` and a `resident` flag; with the registry's model known, that entry answers;
/// without it, the resident one, else the first.
pub fn held_model(models: &serde_json::Value, known: Option<&str>) -> (String, bool) {
    let data = models.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    let name = |m: &serde_json::Value| {
        m.get("display_name").or_else(|| m.get("id")).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let resident = |m: &serde_json::Value| m.get("resident").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Some(k) = known {
        let r = data.iter().find(|m| name(m) == k).map(resident).unwrap_or(false);
        return (k.to_string(), r);
    }
    match data.iter().find(|m| resident(m)).or(data.first()) {
        Some(m) => (name(m), resident(m)),
        None => (String::new(), false),
    }
}

/// `GET /v1/models` on a port: the body when it answers 2xx, the authoritative liveness signal.
async fn fetch_models(port: u16) -> Option<serde_json::Value> {
    let r = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// Poll `find` every 500 ms until it answers, `gave_up` says the start failed, or `max` passes. A
/// gateway seen waiting for RAM is reported once through `notify`, and is the reason given on timeout.
async fn wait_for(max: Duration, mut gave_up: impl FnMut() -> bool, notify: Option<fn(&str)>) -> Result<Found, String> {
    let deadline = Instant::now() + max;
    let mut told = false;
    loop {
        if let Some(f) = find().await {
            return Ok(f);
        }
        let starting = read_starting();
        if let (Some(st), Some(n), false) = (&starting, notify, told) {
            n(&st.reason());
            told = true;
        }
        if gave_up() {
            return Err("the start failed".into());
        }
        if Instant::now() >= deadline {
            return Err(match starting {
                Some(st) => format!("still not serving after {}s — {}", max.as_secs(), st.reason()),
                None => format!("nothing answered within {}s", max.as_secs()),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A gateway that is up but not serving yet: published by the daemon while it waits in the startup
/// RAM admission (`acquire_residency`, up to `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS`), removed as soon
/// as it is admitted or exits. Without it that wait is invisible — no port is bound, so to a client
/// it is the same as no gateway at all, for minutes, while launchd restarts it in a loop.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Starting {
    pub pid: u32,
    pub model: String,
    pub footprint_bytes: u64,
    pub min_free_bytes: u64,
    /// Free RAM as admission counts it, when the wait began; `None` when it cannot be measured.
    pub available_bytes: Option<u64>,
    pub since: u64,
}

impl Starting {
    pub fn reason(&self) -> String {
        let mb = |b: u64| b / 1_048_576;
        let free = self.available_bytes.map(|a| format!("~{} MB free", mb(a))).unwrap_or_else(|| "free RAM unknown".into());
        format!(
            "the gateway (pid {}) is up but waiting for host RAM to load {}: it needs ~{} MB + {} MB kept free, {free}, \
             for {}s — free memory, or start it with a smaller --n-ctx",
            self.pid,
            self.model,
            mb(self.footprint_bytes),
            mb(self.min_free_bytes),
            share::now_unix().saturating_sub(self.since),
        )
    }
}

pub fn starting_path() -> PathBuf {
    share::gateway_dir().join("starting.json")
}

/// Publish this process as a gateway waiting for admission (write-temp + rename).
pub fn write_starting(st: &Starting) {
    let _ = share::ensure_dir();
    let tmp = share::gateway_dir().join(format!("starting.json.tmp.{}", st.pid));
    if std::fs::write(&tmp, serde_json::to_vec(st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, starting_path());
    }
}

/// The waiting gateway, if its process is alive; a record left by a dead one is removed.
pub fn read_starting() -> Option<Starting> {
    let st: Starting = serde_json::from_slice(&std::fs::read(starting_path()).ok()?).ok()?;
    if share::pid_alive(st.pid) {
        Some(st)
    } else {
        let _ = std::fs::remove_file(starting_path());
        None
    }
}

/// Remove the record only if it is still this process's — never a newer gateway's.
pub fn clear_starting_if_mine(pid: u32) {
    let mine = std::fs::read(starting_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Starting>(&b).ok())
        .is_some_and(|st| st.pid == pid);
    if mine {
        let _ = std::fs::remove_file(starting_path());
    }
}

/// The model to spawn with: none for `local`, the resident-model placeholder, which names nothing.
pub fn spawn_model(model: Option<&str>) -> Option<&str> {
    model.filter(|m| !m.is_empty() && *m != "local")
}

/// The binary that runs the gateway: the current executable when it is one, else the one next to
/// it, else `rozum-gateway` from `PATH`. From nadia, `current_exe` is nadia.
pub fn gateway_binary() -> PathBuf {
    gateway_binary_for(std::env::current_exe().ok().as_deref())
}

pub fn gateway_binary_for(exe: Option<&Path>) -> PathBuf {
    let capable = |p: &Path| matches!(p.file_name().and_then(|n| n.to_str()), Some("rozum-gateway") | Some("rozum"));
    if let Some(exe) = exe {
        if capable(exe) {
            return exe.to_path_buf();
        }
        if let Some(dir) = exe.parent() {
            for name in ["rozum-gateway", "rozum"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return cand;
                }
            }
        }
    }
    PathBuf::from("rozum-gateway")
}

/// A detached launch-managed daemon on the default port: its own process group (it outlives this
/// client), stdio to the gateway log, idle-exit once no lease is held.
fn spawn_gateway(bin: &Path, model: Option<&str>, log: &Path) -> Result<std::process::Child, String> {
    let _ = share::ensure_dir();
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(bin);
    cmd.args(["gateway", "--port", &DEFAULT_GATEWAY_PORT.to_string()]);
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    cmd.env("ROZUM_GATEWAY_LAUNCH_MANAGED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| format!("could not start {}: {e}", bin.display()))
}

/// Is this label installed for this user? `launchctl print` answers for a loaded job whether or not
/// it is running — a crashed or idle job is still the owner. Always `false` off macOS.
pub fn launchd_job_exists(label: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let Some(uid) = current_uid() else { return false };
    Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `kickstart` WITHOUT `-k`: a job already running is not restarted because a client wanted it.
pub fn launchd_kickstart(label: &str) {
    if let Some(uid) = current_uid() {
        let _ = Command::new("launchctl")
            .args(["kickstart", &format!("gui/{uid}/{label}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn current_uid() -> Option<u32> {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
}

/// This process's lease on the gateway, heartbeated every 20 s, removed on drop: keeps a
/// launch-managed daemon from idle-exiting under a quiet client, and tells `gateway stop` someone is
/// attached. Needs a tokio runtime.
pub struct LeaseGuard {
    pid: u32,
    task: tokio::task::JoinHandle<()>,
}

impl LeaseGuard {
    pub fn hold() -> LeaseGuard {
        let pid = std::process::id();
        share::touch_lease(pid);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                share::touch_lease(pid);
            }
        });
        LeaseGuard { pid, task }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.task.abort();
        share::remove_lease(self.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn where_the_job_exists_launchd_starts_the_gateway_whatever_the_lock() {
        assert_eq!(start_plan(true, true), StartPlan::AskLaunchd);
        assert_eq!(start_plan(true, false), StartPlan::AskLaunchd);
    }

    #[test]
    fn without_the_job_the_lock_holder_spawns_and_the_rest_wait() {
        assert_eq!(start_plan(false, true), StartPlan::SpawnOwn);
        assert_eq!(start_plan(false, false), StartPlan::WaitForOther);
    }

    #[test]
    fn local_names_no_model_to_spawn_with() {
        assert_eq!(spawn_model(Some("local")), None);
        assert_eq!(spawn_model(Some("")), None);
        assert_eq!(spawn_model(None), None);
        assert_eq!(spawn_model(Some("org/repo")), Some("org/repo"));
    }

    #[test]
    fn the_gateway_binary_is_never_the_client_itself() {
        let dir = std::env::temp_dir().join(format!("gw-ensure-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // from rozum/rozum-gateway: itself
        assert_eq!(gateway_binary_for(Some(&dir.join("rozum-gateway"))), dir.join("rozum-gateway"));
        assert_eq!(gateway_binary_for(Some(&dir.join("rozum"))), dir.join("rozum"));
        // from nadia with no sibling: PATH
        assert_eq!(gateway_binary_for(Some(&dir.join("nadia"))), PathBuf::from("rozum-gateway"));
        // from nadia beside an installed rozum-gateway: the sibling
        std::fs::write(dir.join("rozum-gateway"), b"").unwrap();
        assert_eq!(gateway_binary_for(Some(&dir.join("nadia"))), dir.join("rozum-gateway"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn models() -> serde_json::Value {
        json!({"object":"list","data":[
            {"id":"claude-rozum-x","display_name":"mlx-community:Qwen3.5-4B-MLX-4bit","resident":false},
            {"id":"mlx-community:Qwen3-Embedding-0.6B-4bit-DWQ","display_name":"mlx-community:Qwen3-Embedding-0.6B-4bit-DWQ","resident":true}
        ]})
    }

    #[test]
    fn the_registrys_model_answers_with_its_own_residency() {
        assert_eq!(
            held_model(&models(), Some("mlx-community:Qwen3.5-4B-MLX-4bit")),
            ("mlx-community:Qwen3.5-4B-MLX-4bit".to_string(), false)
        );
    }

    #[test]
    fn without_a_registry_the_resident_model_else_the_first() {
        assert_eq!(held_model(&models(), None), ("mlx-community:Qwen3-Embedding-0.6B-4bit-DWQ".to_string(), true));
        let none_resident = json!({"data":[{"id":"a","resident":false},{"id":"b"}]});
        assert_eq!(held_model(&none_resident, None), ("a".to_string(), false));
        assert_eq!(held_model(&json!({"data":[]}), None), (String::new(), false));
    }

    #[test]
    fn a_waiting_gateway_says_what_it_waits_for() {
        let st = Starting {
            pid: 42,
            model: "org/m".into(),
            footprint_bytes: 12_135 * 1_048_576,
            min_free_bytes: 2048 * 1_048_576,
            available_bytes: Some(7_242 * 1_048_576),
            since: share::now_unix(),
        };
        let r = st.reason();
        for part in ["pid 42", "org/m", "~12135 MB", "2048 MB kept free", "~7242 MB free", "--n-ctx"] {
            assert!(r.contains(part), "{part} missing from: {r}");
        }
        let unknown = Starting { available_bytes: None, ..st };
        assert!(unknown.reason().contains("free RAM unknown"));
    }

    #[test]
    fn the_starting_record_round_trips_as_json() {
        let st = Starting { pid: 7, model: "m".into(), footprint_bytes: 1, min_free_bytes: 2, available_bytes: None, since: 3 };
        let back: Starting = serde_json::from_slice(&serde_json::to_vec(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn found_url_is_the_origin_without_v1() {
        let f = Found { port: 8089, model: String::new(), pid: 0, how: How::Running, resident: false };
        assert_eq!(f.url(), "http://127.0.0.1:8089");
    }
}
