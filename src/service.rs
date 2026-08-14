//! Install the shared gateway as a **user service** — a launchd LaunchAgent on macOS, a
//! `systemd --user` unit on Linux, or a per-user **Scheduled Task** on Windows — so it starts at
//! login and stays warm, instead of the lazy-spawn + idle-exit default (`shared-gateway-service`).
//! The CLI (`rozum service …`) writes the generated file and invokes `launchctl` / `systemctl` /
//! `schtasks`; this module is the **pure generation + path** layer (no side effects), so it's fully
//! unit-testable.
//!
//! **Why a Scheduled Task and not a Windows Service.** Both existing arms install a PER-USER thing:
//! a LaunchAgent runs as the logged-in user at login, and so does a `systemd --user` unit. `sc.exe`
//! installs a MACHINE service under `LocalSystem` — a different security posture for a process that
//! serves this user's models and reads this user's `~/.rozum` — and, more decisively, the Service
//! Control Manager kills any binary that does not report `SERVICE_RUNNING` over the service control
//! protocol within its start timeout. `rozum gateway` is a plain program; making it a Windows
//! Service means adding an SCM entry point to the binary, not adding an arm to this module. A
//! logon-triggered task needs neither, keeps the per-user semantics the other two arms have, and
//! restarts on failure. The trade-off, and the reason `sc.exe` stays written down rather than
//! dismissed: a task runs only while someone is logged on, so a headless box that reboots to a
//! login screen does not bring the gateway back. That is the case that would justify the SCM work.
//!
//! **UNVERIFIED ON WINDOWS.** Generated from what the platform documents; there is no Windows box
//! here. The generators are tested, the `schtasks` invocation is not.

use std::path::PathBuf;

/// launchd label / systemd unit base name.
pub const SERVICE_LABEL: &str = crate::services::GATEWAY_LABEL;
pub const SYSTEMD_UNIT: &str = "rozum-gateway.service";

fn home() -> PathBuf {
    rozum_paths::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `~/Library/LaunchAgents/com.rozum.gateway.plist`.
pub fn launchd_plist_path() -> PathBuf {
    home()
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"))
}

/// `$XDG_CONFIG_HOME/systemd/user/rozum-gateway.service` (or `~/.config/...`).
pub fn systemd_unit_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("systemd/user").join(SYSTEMD_UNIT)
}

/// Minimal XML escaping for the plist string values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A user LaunchAgent plist running `program args…` with `env`, started at load and kept alive.
/// Logs to `$XDG_STATE_HOME/rozum/gateway/service.{out,err}.log`-ish under the state dir.
pub fn launchd_plist(program: &str, args: &[String], env: &[(String, String)]) -> String {
    let mut prog_args = String::new();
    for a in std::iter::once(&program.to_string()).chain(args.iter()) {
        prog_args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let mut env_block = String::new();
    if !env.is_empty() {
        env_block.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in env {
            env_block.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        env_block.push_str("  </dict>\n");
    }
    let log = crate::share::gateway_dir().join("service.log");
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{prog_args}  </array>
{env_block}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// A `systemd --user` unit running `program args…` with `env`, enabled for the default user target.
pub fn systemd_unit(program: &str, args: &[String], env: &[(String, String)]) -> String {
    let exec = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut env_lines = String::new();
    for (k, v) in env {
        env_lines.push_str(&format!("Environment={k}={v}\n"));
    }
    format!(
        "[Unit]\n\
         Description=rozum local LLM gateway\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         {env_lines}Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

// ── Windows: a per-user Scheduled Task, plus the launcher it runs ───────────────
//
// TWO FILES, and the reason is the environment. Task Scheduler XML has no element for environment
// variables — `<Exec>` carries a command and arguments and nothing else — while both other arms
// pass `env` through (`ROZUM_CASCADE`, `ROZUM_CONFIG`, and the meeting daemon's web secret). So the
// task runs a generated `.cmd` launcher that sets them and then execs the program. The launcher
// also redirects both streams to the service log, which is the `StandardOutPath` half of the plist
// that a bare task would otherwise drop.

/// Scheduled-task name for the gateway. Backslashes are Task Scheduler folders; this is a leaf in
/// the `\rozum` folder, which keeps both of ours together and out of the root namespace.
pub const WINDOWS_TASK: &str = r"\rozum\rozum-gateway";
/// Scheduled-task name for the meeting daemon.
pub const MEETINGS_WINDOWS_TASK: &str = r"\rozum\rozum-meetings";

/// Where the generated task XML and its launcher live: the per-user config dir.
///
/// Config and not state: these are the "what to run" description an operator may open and read,
/// alongside `rozum.toml` — the log the service writes stays under the state dir.
fn windows_service_dir() -> PathBuf {
    rozum_paths::config_dir().unwrap_or_else(|| home().join(".rozum"))
}

/// `%APPDATA%\rozum\rozum-gateway.task.xml`.
pub fn windows_task_xml_path() -> PathBuf {
    windows_service_dir().join("rozum-gateway.task.xml")
}

/// `%APPDATA%\rozum\rozum-gateway.cmd`.
pub fn windows_launcher_path() -> PathBuf {
    windows_service_dir().join("rozum-gateway.cmd")
}

/// `%APPDATA%\rozum\rozum-meetings.task.xml`.
pub fn meetings_windows_task_xml_path() -> PathBuf {
    windows_service_dir().join("rozum-meetings.task.xml")
}

/// `%APPDATA%\rozum\rozum-meetings.cmd`.
pub fn meetings_windows_launcher_path() -> PathBuf {
    windows_service_dir().join("rozum-meetings.cmd")
}

/// Anything that cannot be put into a `.cmd` file without changing what it means.
///
/// A REFUSAL AND NOT AN ESCAPE. `cmd.exe` quoting has no total escape for a double quote inside a
/// quoted string, and the failure mode of guessing is a service that starts with silently different
/// arguments — a gateway serving a model nobody asked for, or a web secret that is not the secret
/// the console prints. Every value this actually carries (an exe path, a model spec, a port, a hex
/// secret) is quote-free, so refusing costs nothing real and keeps the generator honest about what
/// it can encode.
#[derive(Debug, PartialEq, Eq)]
pub struct CmdQuotingRefusal {
    pub what: String,
    pub value: String,
}

impl std::fmt::Display for CmdQuotingRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} contains a double quote, which cmd.exe cannot carry through a .cmd launcher \
             without changing it: {}",
            self.what, self.value
        )
    }
}

/// `%` is the one character a `.cmd` file eats: `%FOO%` expands, and a literal one is written `%%`.
/// Applied to every value that lands in the launcher.
fn cmd_escape_percent(s: &str) -> String {
    s.replace('%', "%%")
}

fn refuse_if_quoted(what: &str, value: &str) -> Result<(), CmdQuotingRefusal> {
    if value.contains('"') {
        return Err(CmdQuotingRefusal {
            what: what.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

/// The `.cmd` a scheduled task runs: set `env`, then `program args…` with both streams appended to
/// `log`.
///
/// `set "K=V"` — the quotes go around the whole assignment, not the value, which is the form that
/// keeps a trailing space or a `&` out of trouble and does not leave the quotes in the value.
pub fn windows_launcher_cmd(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    log: &std::path::Path,
) -> Result<String, CmdQuotingRefusal> {
    refuse_if_quoted("the program path", program)?;
    for a in args {
        refuse_if_quoted("an argument", a)?;
    }
    for (k, v) in env {
        refuse_if_quoted(&format!("environment value {k}"), v)?;
    }
    let log_s = log.to_string_lossy();
    refuse_if_quoted("the log path", &log_s)?;

    let mut s = String::from("@echo off\r\nsetlocal\r\n");
    for (k, v) in env {
        s.push_str(&format!(
            "set \"{}={}\"\r\n",
            cmd_escape_percent(k),
            cmd_escape_percent(v)
        ));
    }
    let mut line = format!("\"{}\"", cmd_escape_percent(program));
    for a in args {
        line.push_str(&format!(" \"{}\"", cmd_escape_percent(a)));
    }
    // `>>` and not `>`: the task restarts on failure, and truncating on every restart would throw
    // away the log of the failure that caused it.
    s.push_str(&format!(
        "{line} >> \"{}\" 2>&1\r\n",
        cmd_escape_percent(&log_s)
    ));
    Ok(s)
}

/// The file the gateway's Windows launcher appends both streams to — the same
/// `$XDG_STATE_HOME/rozum/gateway/service.log` the launchd plist names.
pub fn windows_log_path() -> PathBuf {
    crate::share::gateway_dir().join("service.log")
}

/// The meeting daemon's equivalent.
pub fn meetings_windows_log_path() -> PathBuf {
    meetings_log_path()
}

/// UTF-16LE with a BOM — what `schtasks /create /xml` reads.
///
/// The generators return `String` so the tests can assert on text, but the file on disk cannot be
/// UTF-8: `schtasks /query /xml` emits UTF-16 and the importer is documented against that, and the
/// declaration this XML carries says `UTF-16` — a UTF-8 file claiming UTF-16 is the one combination
/// that is wrong under every reading. Encoding at the write, once, keeps the mismatch impossible
/// rather than merely unlikely.
pub fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// A Task Scheduler definition running `launcher` at logon, restarted on failure.
///
/// `<LogonTrigger>` with no `<UserId>` means "whichever user this task is registered for", which is
/// what `schtasks /create` without `/ru` gives: the invoking user. `ExecutionTimeLimit=PT0S` is the
/// one that matters and is easy to miss — the default is 72 hours, after which Task Scheduler stops
/// a task that is behaving perfectly, and a gateway that vanishes every three days is worse than
/// one that was never installed.
pub fn windows_task_xml(description: &str, launcher: &std::path::Path) -> String {
    let cmd = xml_escape(&launcher.to_string_lossy());
    let description = xml_escape(description);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowHardTerminate>true</AllowHardTerminate>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
    </Exec>
  </Actions>
</Task>
"#
    )
}

// ── Meeting daemon service (parallel to the gateway service above) ──────────────

/// launchd label / systemd unit base name for the meeting daemon.
pub const MEETINGS_LABEL: &str = "com.rozum.meetings";
pub const MEETINGS_SYSTEMD_UNIT: &str = "rozum-meetings.service";

/// `~/Library/LaunchAgents/com.rozum.meetings.plist`.
pub fn meetings_launchd_plist_path() -> PathBuf {
    home()
        .join("Library/LaunchAgents")
        .join(format!("{MEETINGS_LABEL}.plist"))
}

/// `$XDG_CONFIG_HOME/systemd/user/rozum-meetings.service` (or `~/.config/...`).
pub fn meetings_systemd_unit_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("systemd/user").join(MEETINGS_SYSTEMD_UNIT)
}

/// `$XDG_STATE_HOME/rozum/meetings/service.log`.
pub fn meetings_log_path() -> PathBuf {
    crate::meeting::store::rozum_state_dir()
        .join("meetings")
        .join("service.log")
}

/// A user LaunchAgent plist running the meeting daemon, started at load + kept alive.
pub fn meetings_launchd_plist(program: &str, args: &[String], env: &[(String, String)]) -> String {
    let mut prog_args = String::new();
    for a in std::iter::once(&program.to_string()).chain(args.iter()) {
        prog_args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let mut env_block = String::new();
    if !env.is_empty() {
        env_block.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in env {
            env_block.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        env_block.push_str("  </dict>\n");
    }
    let log = xml_escape(&meetings_log_path().to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{MEETINGS_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{prog_args}  </array>
{env_block}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// A `systemd --user` unit running the meeting daemon, enabled for the default target.
pub fn meetings_systemd_unit(program: &str, args: &[String], env: &[(String, String)]) -> String {
    let exec = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut env_lines = String::new();
    for (k, v) in env {
        env_lines.push_str(&format!("Environment={k}={v}\n"));
    }
    format!(
        "[Unit]\n\
         Description=rozum meeting daemon\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         {env_lines}Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec![
            "gateway".into(),
            "--model".into(),
            "qwen3-4b".into(),
            "--model".into(),
            "claude-haiku-4-5".into(),
        ]
    }

    #[test]
    fn launchd_plist_has_program_args_and_keepalive() {
        let p = launchd_plist(
            "/usr/local/bin/rozum",
            &args(),
            &[("ROZUM_MULTISLOT".into(), "1".into())],
        );
        assert!(p.contains("<string>com.rozum.gateway</string>"));
        assert!(p.contains("<string>/usr/local/bin/rozum</string>"));
        assert!(p.contains("<string>gateway</string>"));
        assert!(p.contains("<string>qwen3-4b</string>"));
        assert!(p.contains("<string>claude-haiku-4-5</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>ROZUM_MULTISLOT</key>\n    <string>1</string>"));
    }

    #[test]
    fn launchd_plist_xml_escapes_values() {
        let p = launchd_plist("/bin/rozum", &["--model".into(), "a&b<c".into()], &[]);
        assert!(p.contains("a&amp;b&lt;c"));
        assert!(!p.contains("a&b<c"));
    }

    #[test]
    fn systemd_unit_has_execstart_and_install() {
        let u = systemd_unit(
            "/usr/local/bin/rozum",
            &args(),
            &[("ROZUM_OFFLINE".into(), "1".into())],
        );
        assert!(u.contains(
            "ExecStart=/usr/local/bin/rozum gateway --model qwen3-4b --model claude-haiku-4-5"
        ));
        assert!(u.contains("Environment=ROZUM_OFFLINE=1"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }

    #[test]
    fn paths_land_in_the_right_dirs() {
        assert!(
            launchd_plist_path()
                .to_string_lossy()
                .ends_with("LaunchAgents/com.rozum.gateway.plist")
        );
        assert!(
            systemd_unit_path()
                .to_string_lossy()
                .ends_with("systemd/user/rozum-gateway.service")
        );
    }

    fn meetings_args() -> Vec<String> {
        vec!["meetings".into(), "start".into(), "--foreground".into()]
    }

    #[test]
    fn meetings_launchd_plist_runs_the_daemon_kept_alive() {
        let p = meetings_launchd_plist("/usr/local/bin/rozum", &meetings_args(), &[]);
        assert!(p.contains("<string>com.rozum.meetings</string>"));
        assert!(p.contains("<string>/usr/local/bin/rozum</string>"));
        assert!(p.contains("<string>meetings</string>"));
        assert!(p.contains("<string>--foreground</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        // The gateway label must NOT appear in the meetings plist.
        assert!(!p.contains("com.rozum.gateway"));
    }

    #[test]
    fn meetings_systemd_unit_execs_the_daemon() {
        let u = meetings_systemd_unit("/usr/local/bin/rozum", &meetings_args(), &[]);
        assert!(u.contains("ExecStart=/usr/local/bin/rozum meetings start --foreground"));
        assert!(u.contains("Description=rozum meeting daemon"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }

    // ── Windows ────────────────────────────────────────────────────────────────

    #[test]
    fn windows_launcher_sets_env_then_runs_the_program_appending_to_the_log() {
        let cmd = windows_launcher_cmd(
            r"C:\Users\a\rozum.exe",
            &args(),
            &[("ROZUM_CASCADE".into(), "fast".into())],
            std::path::Path::new(r"C:\Users\a\service.log"),
        )
        .expect("no quotes anywhere");
        assert!(cmd.starts_with("@echo off\r\n"), "cmd files are CRLF: {cmd:?}");
        assert!(cmd.contains("set \"ROZUM_CASCADE=fast\"\r\n"));
        // The program and every argument quoted individually — a path with a space is the normal
        // case on Windows (`C:\Program Files\…`), not the exotic one.
        assert!(cmd.contains(r#""C:\Users\a\rozum.exe" "gateway" "--model" "qwen3-4b""#));
        assert!(cmd.contains(r#">> "C:\Users\a\service.log" 2>&1"#));
        // The env must be set BEFORE the program runs, or it is not the program's env at all.
        assert!(cmd.find("set \"ROZUM_CASCADE").unwrap() < cmd.find("rozum.exe").unwrap());
    }

    #[test]
    fn windows_launcher_doubles_percent_so_it_is_not_expanded() {
        let cmd = windows_launcher_cmd(
            "rozum.exe",
            &["--model".into(), "a%PATH%b".into()],
            &[("K".into(), "100%".into())],
            std::path::Path::new("log"),
        )
        .unwrap();
        assert!(cmd.contains("a%%PATH%%b"), "{cmd}");
        assert!(cmd.contains("set \"K=100%%\""), "{cmd}");
    }

    #[test]
    fn windows_launcher_refuses_a_double_quote_rather_than_mangling_it() {
        let bad = windows_launcher_cmd(
            "rozum.exe",
            &[r#"--model=a"b"#.into()],
            &[],
            std::path::Path::new("log"),
        );
        let err = bad.expect_err("a quote must not be silently re-encoded");
        assert_eq!(err.what, "an argument");
        assert!(err.to_string().contains("cmd.exe cannot carry"));

        // …and the same for an environment VALUE, which is where the meeting daemon's secret goes.
        let bad_env = windows_launcher_cmd(
            "rozum.exe",
            &[],
            &[("ROZUM_WEB_SECRET".into(), r#"a"b"#.into())],
            std::path::Path::new("log"),
        );
        assert_eq!(
            bad_env.expect_err("env values are not exempt").what,
            "environment value ROZUM_WEB_SECRET"
        );
    }

    #[test]
    fn windows_task_runs_the_launcher_at_logon_with_no_execution_time_limit() {
        let xml = windows_task_xml(
            "rozum local LLM gateway",
            std::path::Path::new(r"C:\Users\a\rozum-gateway.cmd"),
        );
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains(r"<Command>C:\Users\a\rozum-gateway.cmd</Command>"));
        assert!(xml.contains("<Description>rozum local LLM gateway</Description>"));
        assert!(xml.contains("<RestartOnFailure>"));
        // The default is 72 hours. A gateway that stops every three days on a healthy machine is
        // the failure this line prevents, and nothing else in the file hints at it.
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
    }

    #[test]
    fn windows_task_xml_escapes_the_description() {
        let xml = windows_task_xml("a&b<c", std::path::Path::new("x.cmd"));
        assert!(xml.contains("a&amp;b&lt;c"));
        assert!(!xml.contains("a&b<c"));
    }

    #[test]
    fn utf16le_bom_is_what_schtasks_reads() {
        let b = utf16le_with_bom("A<");
        assert_eq!(b, vec![0xFF, 0xFE, b'A', 0x00, b'<', 0x00]);
        // The declaration in the generated file and the bytes on disk must agree.
        let xml = windows_task_xml("d", std::path::Path::new("x.cmd"));
        assert!(xml.contains(r#"encoding="UTF-16""#));
    }

    #[test]
    fn the_two_windows_tasks_do_not_share_a_name_or_a_file() {
        assert_ne!(WINDOWS_TASK, MEETINGS_WINDOWS_TASK);
        assert!(WINDOWS_TASK.starts_with(r"\rozum\"));
        assert!(MEETINGS_WINDOWS_TASK.starts_with(r"\rozum\"));
        assert_ne!(windows_task_xml_path(), meetings_windows_task_xml_path());
        assert_ne!(windows_launcher_path(), meetings_windows_launcher_path());
        for p in [
            windows_task_xml_path(),
            windows_launcher_path(),
            meetings_windows_task_xml_path(),
            meetings_windows_launcher_path(),
        ] {
            assert!(p.is_absolute() || p.starts_with("."), "{}", p.display());
        }
    }

    #[test]
    fn meetings_paths_land_in_the_right_dirs() {
        assert!(
            meetings_launchd_plist_path()
                .to_string_lossy()
                .ends_with("LaunchAgents/com.rozum.meetings.plist")
        );
        assert!(
            meetings_systemd_unit_path()
                .to_string_lossy()
                .ends_with("systemd/user/rozum-meetings.service")
        );
    }
}
