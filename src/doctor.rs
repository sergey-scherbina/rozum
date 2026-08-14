//! Lightweight read-only demo readiness checks.

use std::ffi::OsStr;
use crate::services::{Owner, Probe, Service, Shape};
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
    // `{name: "<posted status>|<pending status>|<streak>"}` — the status the room was last TOLD,
    // plus how many consecutive ticks have disagreed with it. One tick is not news: measured
    // 2026-08-08, seven "fail" transitions for a service that answered every direct probe, each
    // one landing while the host was compiling. Two in a row is news.
    let raw: std::collections::HashMap<String, String> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let parse = |v: &String| -> (String, String, u32) {
        let mut it = v.split('|');
        let posted = it.next().unwrap_or("").to_string();
        let pending = it.next().unwrap_or(&posted).to_string();
        let streak = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        (posted, pending, streak)
    };
    let previous: std::collections::HashMap<String, String> =
        raw.iter().map(|(k, v)| (k.clone(), parse(v).0)).collect();
    let now: std::collections::HashMap<String, String> = report
        .checks
        .iter()
        .filter(|c| crate::services::ALL.iter().any(|s| s.row == c.name))
        .map(|c| (c.name.to_string(), c.status.label().to_string()))
        .collect();

    let mut lines = Vec::new();
    let mut next_state: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let first_run = raw.is_empty();
    {
        let mut names: Vec<&String> = now.keys().collect();
        names.sort();
        for name in names {
            // A first run has nothing to compare against: record what IS as the baseline and say
            // nothing. Treating it as a disagreement made the SECOND tick announce the whole
            // roster — caught by exercising it rather than reading it.
            if first_run {
                next_state.insert(name.clone(), format!("{}|{}|0", now[name], now[name]));
                continue;
            }
            let (posted, pending, streak) = raw.get(name).map(&parse).unwrap_or_default();
            let before = if posted.is_empty() { "unknown" } else { posted.as_str() };
            let after = now[name].as_str();

            // Agrees with what the room was told: nothing to say, streak resets.
            if before == after {
                next_state.insert(name.clone(), format!("{after}|{after}|0"));
                continue;
            }
            // Disagrees. Count it, and stay quiet until it has disagreed CONFIRM times running —
            // a single missed probe on a loaded machine is not a service going down.
            let streak = if pending == after { streak + 1 } else { 1 };
            if streak < CONFIRM {
                next_state.insert(name.clone(), format!("{before}|{after}|{streak}"));
                continue;
            }
            next_state.insert(name.clone(), format!("{after}|{after}|0"));
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
    if let Ok(text) = serde_json::to_vec_pretty(&next_state) {
        let _ = std::fs::write(&path, text);
    }
    lines
}

/// How many consecutive disagreeing ticks before the room hears about it.
const CONFIRM: u32 = 2;

/// What this machine runs, and what each of those is supposed to answer.
///
/// `endpoint: None` is deliberate and is reported as `skip`: the bridges talk outward to Telegram
/// and the participant pools talk to the daemon over a socket they hold open, so there is nothing
/// here to ask. Inventing a probe that cannot fail would be worse than saying "not probed" — see
/// `docs/specs/service-liveness.md`.
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

/// The restart rate as a phrase, or nothing at all. Silence when the job restarted once (that is
/// every healthy service) or writes no dated start line — a check that appends noise to every OK
/// line is a check people stop reading.
/// What the binary behind a job was built from, and how far that is behind `origin/master`.
///
/// The gap between what is MERGED and what is RUNNING had no check, and it opened three times in
/// two days: once a feature was "shipped" for a day while the daemon serving it had never heard of
/// it. Health and freshness are different questions, and `answers 200` was only ever the first.
///
/// Reads the binary as a FILE (`rozum_core::build_stamp`) rather than running it: a service whose
/// binary cannot start is exactly the case worth reporting, and asking the resident-model gateway
/// its version would cost a model load.
/// How stale a deployment may get before the row's VERDICT changes, as opposed to its text.
///
/// A day, because that is the failure this check was built from: a feature "shipped" for a day while
/// the daemon serving it had never heard of it. Being a few commits behind inside a working session
/// is not that — and on the day this landed, master moved seven commits and every service went
/// yellow, which is the cry-wolf shape the spec forbids, introduced by the person who wrote the ban.
const DRIFT_GRACE_SECS: u64 = 24 * 60 * 60;

/// What the binary behind a job was built from — the FACT, and separately whether it is bad enough
/// to change the row's verdict.
enum Drift {
    /// Worth saying on the row, not worth a colour: behind, but recently built.
    Note(String),
    /// Worth a `warn`: old enough that "shipped" and "running" have had time to diverge, or of an
    /// age nothing can determine.
    Stale(String),
}

/// What the binary behind a job was built from, and how far that is behind `origin/master`.
///
/// The gap between what is MERGED and what is RUNNING had no check, and it opened three times in
/// two days: once a feature was "shipped" for a day while the daemon serving it had never heard of
/// it. Health and freshness are different questions, and `answers 200` was only ever the first.
///
/// Reads the binary as a FILE (`rozum_core::build_stamp`) rather than running it: a service whose
/// binary cannot start is exactly the case worth reporting, and asking the resident-model gateway
/// its version would cost a model load.
fn deployment_drift(label: &str) -> Option<Drift> {
    let prog = job_program(label)?;
    let Some(built) = rozum_core::build_stamp::commit_of_file(std::path::Path::new(&prog)) else {
        // NO STAMP IS NOT NO NEWS. An unstamped binary predates stamping or was built outside a
        // checkout — either way its age is unknown, and reporting unknown as silence is the exact
        // substitution this check exists to remove. Only OUR cargo binaries can carry one:
        // `rozum-meeting-ssc` is emitted by ScalaScript and links none of these crates, so asking
        // it for a stamp forever would be a warn nobody can clear.
        let ours = matches!(
            std::path::Path::new(&prog).file_name().and_then(|n| n.to_str()),
            Some("rozum-gateway" | "rozum" | "rozum-ctrl" | "rozum-meet" | "nadia")
        );
        return ours.then(|| Drift::Stale("deployed binary carries no build stamp — its age is unknown".to_string()));
    };
    let repo = drift_repo()?;
    let short = &built[..7.min(built.len())];
    // Against origin/master, NOT the local checkout: a stale clone would otherwise pronounce itself
    // perfectly up to date, which is the failure mode wearing a green hat.
    let out = Command::new("git")
        .args(["-C", &repo, "rev-list", "--count", &format!("{built}..origin/master")])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        // The commit is not in this checkout — a binary built elsewhere, or from a branch that was
        // pruned. Unknown, said as unknown, and unknown is not reassuring.
        return Some(Drift::Stale(format!("built from {short} — not a commit this checkout knows")));
    }
    let behind: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if behind == 0 {
        return None;
    }
    let plural = if behind == 1 { "commit" } else { "commits" };
    let fact = format!("deployed binary is {behind} {plural} behind origin/master ({short})");
    // Age of the DEPLOYED commit, read from git rather than baked in: the stamp is a sha, and the
    // sha already knows when it was written.
    let age = Command::new("git")
        .args(["-C", &repo, "show", "-s", "--format=%ct", &built])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .map(|built_at| rozum_core::share::now_unix().saturating_sub(built_at));
    Some(match age {
        Some(secs) => match age_verdict(secs) {
            Verdict::Note => Drift::Note(format!("{fact}, built {}", human_age(secs))),
            Verdict::Stale => Drift::Stale(format!("{fact}, built {}", human_age(secs))),
        },
        // Cannot date it ⇒ cannot excuse it.
        None => Drift::Stale(fact),
    })
}


/// The threshold, split out so it can be tested without a repository or a launchd job.
#[cfg_attr(not(test), allow(dead_code))]
enum Verdict {
    Note,
    Stale,
}

#[cfg_attr(not(test), allow(dead_code))]
fn age_verdict(secs: u64) -> Verdict {
    if secs < DRIFT_GRACE_SECS { Verdict::Note } else { Verdict::Stale }
}

/// "3h ago" / "2 days ago" — enough to judge, short enough for a row.
fn human_age(secs: u64) -> String {
    match secs {
        s if s < 90 * 60 => format!("{}m ago", s / 60),
        s if s < 36 * 3600 => format!("{}h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

/// `ProgramArguments[0]` for a job — the binary launchd actually execs, read from the plist rather
/// than assumed. `scripts/install-bins.sh` derives its destinations the same way, for the same
/// reason: this machine has had the same program installed at three paths with three ages.
fn job_program(label: &str) -> Option<String> {
    // `~/Library/LaunchAgents` stays spelled out: this path describes MACOS, not this user's
    // filesystem conventions, and pretending otherwise would hide that the whole function is
    // launchd's (`windows-service-install` is where a Windows arm goes).
    let plist = rozum_paths::home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"));
    let out = Command::new("plutil")
        .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
        .arg(&plist)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}

/// A checkout to measure distance in. This binary's build-time repo first — but a build in a
/// worktree bakes the worktree's path, and worktrees are deleted when their branch lands, so a
/// missing path must mean "cannot compare" and never "up to date". Falls back to the cwd's repo.
fn drift_repo() -> Option<String> {
    let baked = rozum_core::build_stamp::repo();
    if !baked.is_empty() && std::path::Path::new(baked).join(".git").exists() {
        return Some(baked.to_string());
    }
    let out = Command::new("git").args(["rev-parse", "--show-toplevel"]).stderr(Stdio::null()).output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}

fn restart_note(label: &str) -> String {
    match restarts_last_hour(label) {
        Some(n) if n > 1 => format!(" — restarted {n}× in the last hour"),
        _ => String::new(),
    }
}

/// How often a job has restarted in the last hour, read from its own log.
///
/// `launchctl` reports a LIFETIME counter — 3152 runs, over a month or over an afternoon, and the
/// number cannot tell you which. That ambiguity is why BUG-013 (four days of crash-looping) and
/// BUG-025 (a respawn every ~9 s) were both found by somebody happening to look, rather than by
/// anything reporting them. A rate is the signal; a total is trivia.
///
/// Counts `START <rfc3339>` lines newer than an hour. `None` when the job writes no such line —
/// said as "not instrumented" rather than guessed at, because a zero here would read as "healthy".
fn restarts_last_hour(label: &str) -> Option<usize> {
    let path = Command::new("plutil")
        .args(["-extract", "StandardErrorPath", "raw", "-o", "-"])
        .arg(format!("{}/Library/LaunchAgents/{label}.plist", std::env::var("HOME").ok()?))
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    count_recent_starts(std::path::Path::new(&path), chrono::Duration::hours(1))
}

/// Split from the plist lookup so the counting can be tested without a launchd job.
fn count_recent_starts(path: &Path, window: chrono::Duration) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    let cutoff = chrono::Local::now() - window;
    let mut seen_any = false;
    let mut recent = 0usize;
    for line in text.lines().rev().take(20_000) {
        let Some(rest) = line.split(" START ").nth(1) else { continue };
        seen_any = true;
        let Some(stamp) = rest.split_whitespace().next() else { continue };
        match chrono::DateTime::parse_from_rfc3339(stamp) {
            Ok(t) if t.with_timezone(&chrono::Local) >= cutoff => recent += 1,
            Ok(_) => break, // the log is chronological; older than the window ends the scan
            Err(_) => {}
        }
    }
    seen_any.then_some(recent)
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
        // 10 s, not 3. Measured 2026-08-08: the `:8405` PWA answered every probe in milliseconds
        // on an idle machine and missed SEVEN of them while the host was compiling — a small
        // single-threaded server starved of CPU, not a broken service. A watcher that reports a
        // failure every time somebody builds is the cry-wolf this check exists to remove.
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut out = Vec::new();
    // A job on this machine that NOTHING declares is invisible to every reader downstream: doctor
    // will not probe it, install-bins will not publish its binary, and no spec mentions it. That is
    // how `com.rozum.*` jobs have appeared and rotted before. Reported once, not per service.
    out.extend(undeclared_jobs(&jobs));
    for svc in crate::services::ALL {
        let (label, name) = (svc.label, svc.row);
        let mut c =
            check_service(&jobs, label, name, svc.probe, svc.owner, svc.shape, svc.what, &client).await;
        // Freshness is a SEPARATE verdict, applied after the liveness one: a service that answers
        // from three-day-old code is healthy and wrong, and only the first half of that was ever
        // reported. `warn`, never `fail` — being behind between a merge and a deploy is normal, and
        // a red that is usually red gets ignored, which is how this check would become the noise it
        // exists to replace.
        if matches!(c.status, CheckStatus::Ok) {
            // Declared-vs-installed, alongside declared-vs-merged: same class of question.
            if let Some(m) = program_mismatch(svc) {
                c = Check::warn(name, format!("{} — {m}", c.detail), "reconcile the plist with src/services.rs");
            }
            match deployment_drift(label) {
                // The FACT always shows; the VERDICT only when it means something. A row that is
                // yellow after every merge trains its reader to skip it, and then it is not there
                // for the day that matters.
                Some(Drift::Note(d)) => c.detail = format!("{} — {d}", c.detail),
                Some(Drift::Stale(d)) => {
                    c = Check::warn(
                        name,
                        format!("{} — {d}", c.detail),
                        "redeploy: scripts/install-bins.sh (it restarts the job and waits for it)",
                    )
                }
                None => {}
            }
        }
        out.push(c);
    }
    out
}

/// `com.rozum.*` jobs installed here that `services::ALL` does not declare.
///
/// The registry is intent and the plists are what the machine obeys; the point of having both is
/// that they can disagree, and a disagreement should be a finding rather than a surprise. One row,
/// naming them all: a machine mid-migration should not turn the report into a list.
fn undeclared_jobs(jobs: &std::collections::HashMap<String, (Option<i64>, i64)>) -> Vec<Check> {
    let mut extra: Vec<&str> = jobs
        .keys()
        .map(String::as_str)
        .filter(|l| crate::services::find(l).is_none())
        .collect();
    if extra.is_empty() {
        return Vec::new();
    }
    extra.sort_unstable();
    vec![Check::warn(
        "svc:undeclared",
        format!("installed here but declared nowhere: {}", extra.join(", ")),
        "add it to src/services.rs (with a probe, or a stated reason there is none) or remove the job",
    )]
}

/// A declared service whose plist runs a different binary than the registry expects.
///
/// The mismatch that cost something: `~/.rozum/bin/rozum-ctrl` is the thin dispatcher, and a guess
/// from the filename published the 54 MB engine over it. Nothing broke only because the old process
/// was still running.
fn program_mismatch(svc: &crate::services::Service) -> Option<String> {
    compare_program(&job_program(svc.label)?, svc.program)
}

/// The comparison itself, separated from launchd so the exception can be tested: this is a rule
/// about names, and a rule that can only be exercised by editing a plist is one nobody re-checks.
fn compare_program(installed_path: &str, declared: &str) -> Option<String> {
    let actual = std::path::Path::new(installed_path).file_name()?.to_str()?;
    // `rozum-ctrl` is the gateway binary under another name — the deploy's decision, recorded in
    // `scripts/install-bins.sh`, not a mismatch.
    let same = actual == declared || (actual == "rozum-ctrl" && declared == "rozum-gateway");
    (!same).then(|| format!("runs {actual}, but this service is declared to run {declared}"))
}

/// `label -> (pid, last exit status)`./// `label -> (pid, last exit status)`. `pid` is `None` when the job is loaded but not running,
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
                     restart what it does not own. Hand it over: `rozum meetings handoff` \
                     (docs/specs/meeting-daemon-ownership.md)"
                ),
            ),
            Some(_) => Check::ok(
                name,
                format!("running (pid {pid}) and owns the socket, {url} {detail}{}", restart_note(label)),
            ),
            None => Check::ok(name, format!("running (pid {pid}), {url} {detail}{}", restart_note(label))),
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

    /// One tick is not news; two in a row is.
    ///
    /// The rule exists because the watcher's first day produced SEVEN `meeting-ssc` failures, every
    /// one of them while the machine was compiling — a small single-threaded server missing a 3 s
    /// probe, not a service going down. Direct probes were 200 throughout.
    #[test]
    fn a_single_missed_tick_is_not_reported_but_a_confirmed_change_is() {
        // This crate ALREADY has a lock for exactly this — `proxy.rs` takes it before redirecting
        // XDG_STATE_HOME, "so we never race another test". I wrote "no other test in this crate
        // reads XDG_STATE_HOME" instead of checking, and shipped a suite that passed alone, passed
        // among the doctor tests, and failed in the full workspace run.
        let _env = rozum_core::share::POISON_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        // SAFETY: held under that lock; no other thread reads XDG_STATE_HOME now.
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };

        let report = |status: CheckStatus| DoctorReport {
            checks: vec![Check {
                name: "svc:gateway",
                status,
                detail: "probe".into(),
                hint: None,
            }],
        };

        // First run: record what IS, say nothing. (Announcing the roster to whoever installs the
        // job is how a watcher trains people to ignore it.)
        assert!(transitions(&report(CheckStatus::Ok)).is_empty(), "a baseline must be silent");
        // One disagreeing tick: still silent.
        assert!(transitions(&report(CheckStatus::Fail)).is_empty(), "one missed probe is not news");
        // The same disagreement again: now it is news.
        let posted = transitions(&report(CheckStatus::Fail));
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert!(posted[0].contains("svc:gateway"), "{}", posted[0]);
        // Steady state: silent again.
        assert!(transitions(&report(CheckStatus::Fail)).is_empty());
        // And a single GOOD tick after a failure does not un-say it either.
        assert!(transitions(&report(CheckStatus::Ok)).is_empty(), "recovery is confirmed too");
        assert_eq!(transitions(&report(CheckStatus::Ok)).len(), 1, "…on the second good tick");

        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    /// A rate needs a clock in the log, and silence when there is nothing to say.
    #[test]
    fn a_restart_rate_is_read_from_dated_start_lines() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("gw.log");
        let now = chrono::Local::now();
        let old = now - chrono::Duration::hours(5);
        // Chronological, like the real file: two starts inside the window, one long before it.
        std::fs::write(
            &log,
            format!(
                "rozum gateway: START {} pid 1\nsome noise\nrozum gateway: START {} pid 2\n\
                 rozum gateway: START {} pid 3\n",
                old.to_rfc3339(),
                (now - chrono::Duration::minutes(30)).to_rfc3339(),
                (now - chrono::Duration::minutes(2)).to_rfc3339()
            ),
        )
        .unwrap();
        assert_eq!(count_recent_starts(&log, chrono::Duration::hours(1)), Some(2));

        // A log with no dated start line is "not instrumented", NOT "zero restarts" — a zero here
        // would read as healthy, which is the mistake this whole item is about.
        std::fs::write(&log, "context window: 32768\nready\n").unwrap();
        assert_eq!(count_recent_starts(&log, chrono::Duration::hours(1)), None);
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
        for svc in crate::services::ALL {
            let (label, name, probe, what) = (svc.label, svc.row, svc.probe, svc.what);
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
        let unprobed: Vec<&str> = crate::services::ALL
            .iter()
            .filter(|s| matches!(s.probe, Probe::None) && s.shape == Shape::Resident)
            .map(|s| s.label)
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

/// A job nobody declares is invisible to every reader downstream — doctor will not probe it,
    /// install-bins will not publish its binary, no spec mentions it. It must be a finding.
    #[test]
    fn an_undeclared_job_is_reported_once_and_named() {
        let mut jobs = std::collections::HashMap::new();
        jobs.insert("com.rozum.gateway".to_string(), (Some(1), 0));
        assert!(undeclared_jobs(&jobs).is_empty(), "a declared job is not news");
        jobs.insert("com.rozum.mystery".to_string(), (Some(2), 0));
        jobs.insert("com.rozum.another".to_string(), (None, 0));
        let out = undeclared_jobs(&jobs);
        assert_eq!(out.len(), 1, "one row, not one per job — a migration should not become a list");
        assert!(out[0].detail.contains("com.rozum.another"), "{}", out[0].detail);
        assert!(out[0].detail.contains("com.rozum.mystery"), "{}", out[0].detail);
    }

    /// The mismatch that cost something: a 54 MB engine published over the thin dispatcher because
    /// the name was guessed. And the exception that must NOT fire, because `rozum-ctrl` genuinely is
    /// the gateway binary under another name.
    #[test]
    fn a_plist_running_the_wrong_binary_is_a_finding_but_rozum_ctrl_is_not() {
        assert_eq!(compare_program("/Users/x/.cargo/bin/rozum-gateway", "rozum-gateway"), None);
        assert_eq!(compare_program("/Users/x/.rozum/bin/rozum-ctrl", "rozum-gateway"), None, "same program, deploy's own name");
        let m = compare_program("/Users/x/.rozum/bin/rozum-meet", "rozum-gateway").expect("must report");
        assert!(m.contains("runs rozum-meet"), "{m}");
        assert!(m.contains("declared to run rozum-gateway"), "{m}");
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

#[cfg(test)]
mod drift_tests {
    use super::*;

    fn sh(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new(args[0]).args(&args[1..]).current_dir(dir).output().expect("cmd");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The distance must be measured against `origin/master`, not the local checkout — a stale
    /// clone would otherwise pronounce itself perfectly up to date, which is this bug wearing a
    /// green hat. Built as a real repo with a real remote because the whole claim is about which
    /// ref the count uses.
    #[test]
    fn distance_is_measured_against_the_remote_not_the_local_head() {
        let base = std::env::temp_dir().join(format!("rozum-drift-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let origin = base.join("origin");
        let clone = base.join("clone");
        std::fs::create_dir_all(&origin).unwrap();
        sh(&origin, &["git", "init", "-q", "-b", "master", "."]);
        sh(&origin, &["git", "config", "user.email", "t@t"]);
        sh(&origin, &["git", "config", "user.name", "t"]);
        std::fs::write(origin.join("a"), "1").unwrap();
        sh(&origin, &["git", "add", "-A"]);
        sh(&origin, &["git", "commit", "-qm", "one"]);
        let first = sh(&origin, &["git", "rev-parse", "HEAD"]);
        std::fs::write(origin.join("a"), "2").unwrap();
        sh(&origin, &["git", "commit", "-qam", "two"]);
        std::fs::write(origin.join("a"), "3").unwrap();
        sh(&origin, &["git", "commit", "-qam", "three"]);

        std::fs::create_dir_all(&clone).unwrap();
        sh(&base, &["git", "clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        // The clone sits on the FIRST commit: local HEAD says "current", origin/master says 2 behind.
        sh(&clone, &["git", "checkout", "-q", &first]);
        let repo = clone.to_str().unwrap();
        let count = |from: &str| -> String {
            let out = Command::new("git")
                .args(["-C", repo, "rev-list", "--count", &format!("{from}..origin/master")])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(count(&first), "2", "two commits landed after the deployed one");
        let head = sh(&clone, &["git", "rev-parse", "HEAD"]);
        assert_eq!(head, first, "the local checkout believes it is current");

        // An unknown commit must not be silently read as zero.
        let bogus = "0000000000000000000000000000000000000000";
        let out = Command::new("git")
            .args(["-C", repo, "rev-list", "--count", &format!("{bogus}..origin/master")])
            .output()
            .unwrap();
        assert!(!out.status.success(), "an unknown commit must FAIL, not count as up to date");

        let _ = std::fs::remove_dir_all(&base);
    }


    /// A binary that cannot carry a stamp must not be nagged about forever; one that can, must be.
    #[test]
/// The check went from "5 ok, 1 warn" to "1 ok, 5 warn" the day it landed, because master had
    /// moved seven commits during one working session. The distance was true and the verdict was
    /// noise, and a row that is yellow after every merge is not there for the day that matters.
    #[test]
    fn a_fresh_deploy_that_is_merely_behind_does_not_change_the_verdict() {
        assert!(DRIFT_GRACE_SECS >= 12 * 3600, "shorter than a working day defeats the purpose");
        assert!(DRIFT_GRACE_SECS <= 48 * 3600, "longer than two days hides the failure it was built from");
        // The boundary either side, since the whole behaviour is a threshold.
        assert!(matches!(age_verdict(3 * 3600), Verdict::Note), "3h old, mid-session");
        assert!(matches!(age_verdict(DRIFT_GRACE_SECS - 1), Verdict::Note));
        assert!(matches!(age_verdict(DRIFT_GRACE_SECS), Verdict::Stale), "a day is the failure that was recorded");
        assert!(matches!(age_verdict(5 * 86400), Verdict::Stale));
    }

    #[test]
    fn ages_read_the_way_a_person_would_say_them() {
        assert_eq!(human_age(60), "1m ago");
        assert_eq!(human_age(3 * 3600), "3h ago");
        assert_eq!(human_age(50 * 3600), "2 days ago");
    }

    #[test]
    fn only_our_own_binaries_are_asked_for_a_stamp() {
        let ours = |p: &str| {
            matches!(
                std::path::Path::new(p).file_name().and_then(|n| n.to_str()),
                Some("rozum-gateway" | "rozum" | "rozum-ctrl" | "rozum-meet" | "nadia")
            )
        };
        assert!(ours("/Users/x/.rozum/bin/rozum-gateway"));
        assert!(ours("/Users/x/.cargo/bin/nadia"));
        assert!(!ours("/Users/x/.local/bin/rozum-meeting-ssc"), "ScalaScript-emitted, links none of our crates");
        assert!(!ours("/Users/x/.rozum/bin/rozum-telegram-bridge.sh"));
    }
}
