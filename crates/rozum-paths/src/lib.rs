//! Where this user's files live, on a platform that has `$HOME` and on one that does not.
//!
//! `HOME` is not a Windows variable. Every path in this workspace was resolved from it, so on
//! Windows every one of them fell through to the same fallback — and for the residency ledger that
//! is not a cosmetic problem: the ledger is what stops a second model load from exhausting host RAM
//! (BUG-003, a kernel-watchdog reboot), and a ledger at a machine-wide path is one every account on
//! the box would share. A per-user safety mechanism must resolve to a per-user directory.
//!
//! **Unix behaviour is unchanged wherever `HOME` is set**, which under launchd and in every shell
//! here it is. The only paths that move are the ones that had nowhere to go.
//!
//! **UNVERIFIED ON WINDOWS**, like the rest of the port: the variables and their order are what the
//! platform documents, not what was observed on a machine that does not exist here.

use std::path::PathBuf;

/// This user's home directory, or `None` when the platform will not say.
///
/// `HOME` first on both platforms — it is what unix uses, and on Windows a Git-Bash/MSYS shell sets
/// it too, so honouring it keeps one machine's tools agreeing with each other. `USERPROFILE` is
/// Windows' own answer and the fallback there.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    #[cfg(windows)]
    {
        return std::env::var_os("USERPROFILE").map(PathBuf::from);
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// rozum's durable per-user state directory: `$XDG_STATE_HOME/rozum`, `%LOCALAPPDATA%\rozum`, or
/// `~/.local/state/rozum`. `None` when there is no home to put it under.
///
/// `XDG_STATE_HOME` wins on BOTH platforms and that is deliberate: every isolated test in this
/// workspace redirects it, and a Windows arm that ignored it would quietly point those tests at the
/// real state directory.
///
/// Callers keep deciding what `None` means. A log file may fall back to [`temp_dir`]; a ledger that
/// exists to prevent an OOM reboot should refuse instead, because a shared fallback path is exactly
/// the failure it guards against.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(x).join("rozum"));
    }
    #[cfg(windows)]
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(l).join("rozum"));
    }
    // `.join(".local").join("state")`, not `.join(".local/state")`: a literal separator inside a
    // join is the one place a path stops being a `PathBuf` and starts being a string that happens
    // to work.
    home_dir().map(|h| h.join(".local").join("state").join("rozum"))
}

/// rozum's per-user CONFIG directory: `$XDG_CONFIG_HOME/rozum`, `%APPDATA%\rozum`, or
/// `~/.config/rozum`.
///
/// Separate from [`state_dir`] because the two hold different things — a config file the operator
/// edits, versus state the program writes — and because Windows keeps them in different places
/// (`%APPDATA%` roams between machines, `%LOCALAPPDATA%` does not). Collapsing them would put a
/// residency ledger on a network share.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x).join("rozum"));
    }
    #[cfg(windows)]
    if let Some(a) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(a).join("rozum"));
    }
    home_dir().map(|h| h.join(".config").join("rozum"))
}

/// The system temp directory — the last resort when there is no home.
///
/// `std::env::temp_dir()`, not `/tmp`: on Windows `PathBuf::from("/tmp")` names `\tmp` on the
/// current drive, a directory nobody created and every account shares.
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests mutate process-wide environment variables, so they must not run beside each
    /// other. (`set_var` is `unsafe` in edition 2024 precisely because of this.)
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Restore(&'static str, Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.1.take() {
                Some(v) => unsafe { std::env::set_var(self.0, v) },
                None => unsafe { std::env::remove_var(self.0) },
            }
        }
    }
    fn stash(k: &'static str) -> Restore {
        Restore(k, std::env::var_os(k))
    }

    #[test]
    fn an_explicit_state_home_wins_on_every_platform() {
        // Every isolated test in this workspace redirects XDG_STATE_HOME. If the Windows arm
        // ignored it, those tests would write into the operator's real state directory.
        let _g = env_lock();
        let _x = stash("XDG_STATE_HOME");
        unsafe { std::env::set_var("XDG_STATE_HOME", "/somewhere/else") };
        assert_eq!(state_dir(), Some(PathBuf::from("/somewhere/else").join("rozum")));
    }

    #[test]
    fn without_a_home_there_is_no_state_dir_to_return() {
        // The caller decides what that means — a log file falls back to a temp dir, the residency
        // ledger must not. Returning a shared path here would take that decision away from both.
        let _g = env_lock();
        let _x = stash("XDG_STATE_HOME");
        let _h = stash("HOME");
        let _u = stash("USERPROFILE");
        let _l = stash("LOCALAPPDATA");
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("LOCALAPPDATA");
        }
        assert_eq!(home_dir(), None);
        assert_eq!(state_dir(), None);
    }

    #[test]
    fn the_home_based_layout_is_the_one_this_machine_already_uses() {
        // The regression that matters on the platform that IS running: `~/.local/state/rozum`,
        // unchanged, or every path in the process moves at once.
        let _g = env_lock();
        let _x = stash("XDG_STATE_HOME");
        let _h = stash("HOME");
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::set_var("HOME", "/Users/someone");
        }
        assert_eq!(home_dir(), Some(PathBuf::from("/Users/someone")));
        // Joins, not a literal: `/` is not the separator everywhere, and this crate exists BECAUSE
        // of the platform that spells it differently. Windows takes `%LOCALAPPDATA%` ahead of
        // `HOME`, so the home-based layout is asserted where it is the rule.
        #[cfg(not(windows))]
        assert_eq!(
            state_dir(),
            Some(
                PathBuf::from("/Users/someone")
                    .join(".local")
                    .join("state")
                    .join("rozum")
            ),
        );
    }

    #[test]
    fn the_last_resort_is_the_systems_temp_dir_not_a_literal_slash_tmp() {
        // On Windows `PathBuf::from("/tmp")` is `\tmp` on the current drive: a directory nobody
        // created, shared by every account. `temp_dir()` is per-user where the platform says so.
        let t = temp_dir();
        assert!(t.is_absolute(), "temp dir must be absolute, got {}", t.display());
        #[cfg(unix)]
        assert!(t.exists(), "temp dir must exist on unix, got {}", t.display());
    }
}
