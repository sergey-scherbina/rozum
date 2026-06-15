//! Install the shared gateway as a **user service** — a launchd LaunchAgent on macOS or a
//! `systemd --user` unit on Linux — so it starts at login and stays warm, instead of the
//! lazy-spawn + idle-exit default (`shared-gateway-service`). The CLI (`rozum service …`) writes the
//! generated file and invokes `launchctl` / `systemctl`; this module is the **pure generation +
//! path** layer (no side effects), so it's fully unit-testable.

use std::path::PathBuf;

/// launchd label / systemd unit base name.
pub const SERVICE_LABEL: &str = "com.rozum.gateway";
pub const SYSTEMD_UNIT: &str = "rozum-gateway.service";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `~/Library/LaunchAgents/com.rozum.gateway.plist`.
pub fn launchd_plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist"))
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec!["gateway".into(), "--model".into(), "qwen3-4b".into(), "--model".into(), "claude-haiku-4-5".into()]
    }

    #[test]
    fn launchd_plist_has_program_args_and_keepalive() {
        let p = launchd_plist("/usr/local/bin/rozum", &args(), &[("ROZUM_MULTISLOT".into(), "1".into())]);
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
        let u = systemd_unit("/usr/local/bin/rozum", &args(), &[("ROZUM_OFFLINE".into(), "1".into())]);
        assert!(u.contains("ExecStart=/usr/local/bin/rozum gateway --model qwen3-4b --model claude-haiku-4-5"));
        assert!(u.contains("Environment=ROZUM_OFFLINE=1"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }

    #[test]
    fn paths_land_in_the_right_dirs() {
        assert!(launchd_plist_path().to_string_lossy().ends_with("LaunchAgents/com.rozum.gateway.plist"));
        assert!(systemd_unit_path().to_string_lossy().ends_with("systemd/user/rozum-gateway.service"));
    }
}
