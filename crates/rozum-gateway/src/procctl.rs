//! Starting and stopping child processes: on a platform that has signals, and on one that does not.
//!
//! The gateway spawns three kinds of child — a meeting participant, a coder run, a matrix bench —
//! and then has to answer the same four questions about each: is it still alive, stop it, freeze
//! it, and keep it from dying when the service that started it restarts. Every one of those was
//! written directly against `libc` and `std::os::unix`, in five files, which is why
//! `cargo check --target x86_64-pc-windows-gnu` stopped at 26 errors with `ucc` off
//! (`docs/specs/windows-spawn-seams.md`). Nothing about the questions is unix-specific; only the
//! answers are, so the answers live here and the callers stopped knowing which platform they run on.
//!
//! **`Unsupported` is a third outcome, not a dressed-up failure.** SIGSTOP has no supported Win32
//! equivalent, and a seam that reported "pause failed" for it would send an operator looking for a
//! broken process group that was never asked to freeze. The platforms differ; the difference is
//! reported.
//!
//! **UNVERIFIED ON WINDOWS.** This compiles for the target and nothing more — there is no Windows
//! machine here. The one part that IS proven is the extension logic (`has_runnable_extension` /
//! `program_candidates`), written as pure functions over a `PATHEXT` string precisely so it could
//! be tested on the machine that exists; everything touching Win32 is code that has never run.

use std::path::Path;
use std::process::Command;

/// What a running child — or a whole run's process group — is being asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    /// Stop, with a chance to clean up first. SIGTERM; Ctrl+Break on Windows.
    Terminate,
    /// Freeze in place, to be continued later. SIGSTOP.
    Suspend,
    /// Continue after a `Suspend`. SIGCONT.
    Resume,
}

/// What became of an `Ask`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The platform accepted it. (Not "the child obeyed" — SIGTERM can be caught.)
    Delivered,
    /// The platform rejected it: no such process, not ours to signal, or the call failed.
    Failed,
    /// This platform has no such operation, and saying so is the honest answer.
    Unsupported(&'static str),
}

impl Outcome {
    pub(crate) fn ok(self) -> bool {
        matches!(self, Outcome::Delivered)
    }

    /// The reason, for the one case where the caller has something to tell the operator that
    /// "false" does not say.
    pub(crate) fn why(self) -> Option<&'static str> {
        match self {
            Outcome::Unsupported(w) => Some(w),
            _ => None,
        }
    }
}

/// Put the child in its own process group.
///
/// Two things depend on this and both are load-bearing: the child survives the control server
/// restarting (it is no longer in the group launchd signals), and one `Ask` reaches the whole tree
/// the child starts — a bench run is a bash script that starts an agent that starts a gateway.
///
/// On Windows `CREATE_NEW_PROCESS_GROUP` is the same idea with one consequence worth knowing: it
/// also detaches the child from Ctrl-C, which is exactly why the graceful stop below sends
/// Ctrl+Break instead.
pub(crate) fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
}

/// Is `pid` still a live process?
///
/// One implementation, in `rozum_core::share` — it already had both arms for reaping dead residency
/// leases, and a second copy here is the defect this repo keeps finding in its own boards.
pub(crate) fn pid_alive(pid: u32) -> bool {
    rozum_core::share::pid_alive(pid)
}

/// Ask one child to stop, gracefully.
///
/// Unix sends SIGTERM to that pid alone. Windows has no per-process signal at all: the only
/// graceful stop it offers is a console control event, which addresses a process GROUP — so for a
/// child spawned through `own_process_group` (where the group id is the child's own pid) the ask
/// reaches the child and its descendants. That is wider than the unix arm, and it is the platform's
/// choice, not a decision made here.
///
/// It is deliberately NOT `TerminateProcess`. A hard kill of a process that may be mid-GPU-eval is
/// what rebooted this Mac once (BUG-001), and while the Windows failure mode is unproven, the
/// callers here mean "stop when you can" — a caller that wanted a kill would have to say so.
pub(crate) fn terminate(pid: u32) -> Outcome {
    if pid == 0 {
        return Outcome::Failed; // the "not spawned yet" placeholder; kill(0, …) hits our own group
    }
    #[cfg(unix)]
    {
        let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0;
        return if sent {
            Outcome::Delivered
        } else {
            Outcome::Failed
        };
    }
    #[cfg(windows)]
    {
        return ctrl_break(pid);
    }
}

/// Is the process group led by `leader` still running?
///
/// Unix asks the group directly (`kill(-pgid, 0)` succeeds while ANY member is alive). Windows has
/// no such query, so this narrows to "is the leader alive" — a run whose leader has exited but
/// whose children are still going reads as finished there. Recorded rather than hidden.
///
/// `leader == 0` is never alive: on unix `kill(-0, …)` addresses OUR OWN process group, so the
/// un-guarded version answered "alive" for a record that was never spawned and kept its stale live
/// state forever.
pub(crate) fn group_alive(leader: i32) -> bool {
    if leader <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        return unsafe { libc::kill(-leader, 0) } == 0;
    }
    #[cfg(windows)]
    {
        return pid_alive(leader as u32);
    }
}

/// Ask a whole process group to stop, freeze or continue.
pub(crate) fn signal_group(leader: i32, ask: Ask) -> Outcome {
    if leader <= 0 {
        return Outcome::Failed;
    }
    #[cfg(unix)]
    {
        let sig = match ask {
            Ask::Terminate => libc::SIGTERM,
            Ask::Suspend => libc::SIGSTOP,
            Ask::Resume => libc::SIGCONT,
        };
        let sent = unsafe { libc::killpg(leader, sig) } == 0;
        return if sent {
            Outcome::Delivered
        } else {
            Outcome::Failed
        };
    }
    #[cfg(windows)]
    {
        return match ask {
            Ask::Terminate => ctrl_break(leader as u32),
            // NtSuspendProcess exists and is undocumented; a debugger-style thread walk is neither
            // atomic nor safe against a process that spawns while being walked. There is no
            // supported answer, so this reports that instead of inventing one.
            Ask::Suspend | Ask::Resume => Outcome::Unsupported(
                "Windows has no process-group suspend: SIGSTOP/SIGCONT have no supported Win32 equivalent",
            ),
        };
    }
}

/// The graceful stop Windows actually has: a Ctrl+Break to a process group.
///
/// Only reaches a group created with `CREATE_NEW_PROCESS_GROUP` that shares this process's console
/// — a service with no console cannot deliver it, and that shows up as `Failed` rather than as a
/// silent no-op.
#[cfg(windows)]
fn ctrl_break(group: u32) -> Outcome {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) } != 0;
    if sent {
        Outcome::Delivered
    } else {
        Outcome::Failed
    }
}

/// The pid that started this process, when the platform will say.
///
/// It is in the gateway's shutdown event to name whoever sent the signal (`8103296`). Windows keeps
/// no parent field on a process: recovering it means a Toolhelp32 snapshot walk, whose result is
/// also a lie the moment the parent exits and its pid is reused. `None` — "not known here" — is
/// what the event carries there, rather than a number written blind.
pub(crate) fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        return Some(std::os::unix::process::parent_id());
    }
    #[cfg(windows)]
    {
        return None;
    }
}

/// Replace this process with `cmd`. Returns only when the replacement did NOT happen.
///
/// Unix `exec` keeps the pid, the open files and the service's supervision of it. Windows has no
/// exec: the successor is a NEW process and this one then exits, which means the pid changes, a
/// listening socket is handed over rather than inherited, and a Windows service manager sees its
/// process exit. That is a real behavioural difference on a path (gateway self-reload) that has
/// never run there; it is written down here rather than discovered by whoever runs it first.
pub(crate) fn replace_self(cmd: &mut Command) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        return Err(cmd.exec());
    }
    #[cfg(windows)]
    {
        return cmd.spawn().map(|_| ());
    }
}

/// Can this file be run as a program?
///
/// Unix reads the permission bits. Windows has none: it decides by EXTENSION, against `PATHEXT`.
/// Without that arm the gateway's "is this agent installed?" check answers no for every agent on a
/// Windows host and the UCC refuses to launch anything, naming PATH as the reason — a wrong answer
/// delivered confidently, which is worse than a compile error.
pub(crate) fn is_executable_file(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        return m.permissions().mode() & 0o111 != 0;
    }
    #[cfg(windows)]
    {
        return has_runnable_extension(&p.to_string_lossy(), &pathext());
    }
}

/// Is a program of this bare name runnable from `dir`?
///
/// Unix looks for exactly that file. Windows tries the name with each `PATHEXT` suffix, which is
/// what the shell does — `claude` on a Windows box is `claude.exe` or `claude.cmd` on disk, and a
/// literal lookup finds neither.
pub(crate) fn runnable_in_dir(dir: &Path, name: &str) -> bool {
    #[cfg(unix)]
    {
        return is_executable_file(&dir.join(name));
    }
    #[cfg(windows)]
    {
        return program_candidates(name, &pathext())
            .into_iter()
            .any(|c| is_executable_file(&dir.join(c)));
    }
}

/// What `PATHEXT` says, or what Windows uses when it is unset.
#[cfg(windows)]
fn pathext() -> String {
    std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_PATHEXT.to_string())
}

/// Windows' own default when `PATHEXT` is unset. (The real list is longer — `.VBS`, `.JS`, `.WSF` —
/// but those are script-host types no agent CLI ships as, and every one added is another name this
/// searches for in every PATH directory.)
///
/// Compiled on unix only under `cfg(test)`: the logic below is Windows' alone, and building it into
/// the mac binary would be dead code that a warning then asks someone to delete — deleting the very
/// thing that makes the Windows arm provable here.
#[cfg(any(windows, test))]
pub(crate) const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Does this file name already carry an extension Windows would run?
///
/// A pure function over the `PATHEXT` string so it is testable on the machine that exists — the
/// only part of the Windows arm that is proven rather than merely compiled.
#[cfg(any(windows, test))]
pub(crate) fn has_runnable_extension(name: &str, pathext: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != ".")
        .any(|e| lower.ends_with(&e.to_ascii_lowercase()))
}

/// The file names Windows would actually try for a bare program name.
#[cfg(any(windows, test))]
pub(crate) fn program_candidates(name: &str, pathext: &str) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    if has_runnable_extension(name, pathext) {
        return vec![PathBuf::from(name)];
    }
    pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != ".")
        .map(|e| PathBuf::from(format!("{name}{e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_bare_agent_name_becomes_the_names_windows_would_try() {
        // The reason this matters: `agent_on_path("claude")` on Windows must find `claude.exe`.
        // A literal lookup finds nothing and the UCC then reports every agent as not installed.
        let got = program_candidates("claude", DEFAULT_PATHEXT);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["claude.COM", "claude.EXE", "claude.BAT", "claude.CMD"]
        );
    }

    #[test]
    fn a_name_that_already_carries_a_runnable_extension_is_tried_as_written() {
        // Otherwise an operator who typed `claude.exe` gets a search for `claude.exe.EXE`.
        let got = program_candidates("claude.exe", DEFAULT_PATHEXT);
        assert_eq!(got, vec![PathBuf::from("claude.exe")]);
        // Case is Windows' business, not ours: PATHEXT is upper-case, real files are not.
        assert!(has_runnable_extension(
            "C:\\tools\\Claude.Exe",
            DEFAULT_PATHEXT
        ));
        assert!(has_runnable_extension("nadia.cmd", DEFAULT_PATHEXT));
    }

    #[test]
    fn an_extensionless_or_unknown_name_is_not_runnable_by_itself() {
        // The unix shape (`/usr/local/bin/claude`, no extension) is exactly what Windows will NOT
        // run, so the check must say no rather than assume the unix answer.
        assert!(!has_runnable_extension("claude", DEFAULT_PATHEXT));
        assert!(!has_runnable_extension("notes.txt", DEFAULT_PATHEXT));
        assert!(!has_runnable_extension("archive.exe.gz", DEFAULT_PATHEXT));
    }

    #[test]
    fn a_pathext_with_junk_in_it_does_not_produce_junk_candidates() {
        // A real PATHEXT often ends with a stray separator; a bare "." would make every file
        // runnable, which is how "is this installed?" turns into "does this exist?".
        assert_eq!(
            program_candidates("nadia", ".EXE; .CMD;;.")
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["nadia.EXE", "nadia.CMD"],
        );
        assert!(!has_runnable_extension("anything", ";;."));
    }

    #[test]
    fn a_group_leader_of_zero_is_never_alive_and_never_signalled() {
        // `kill(-0, sig)` addresses the CALLER's process group: the guard is what keeps a
        // never-spawned record from reading as alive, and a stop from hitting this gateway.
        assert!(!group_alive(0));
        assert!(!group_alive(-1));
        assert_eq!(signal_group(0, Ask::Terminate), Outcome::Failed);
        assert_eq!(terminate(0), Outcome::Failed);
    }

    #[test]
    fn an_unsupported_outcome_carries_its_reason_and_is_not_ok() {
        let u = Outcome::Unsupported("no such thing here");
        assert!(!u.ok());
        assert_eq!(u.why(), Some("no such thing here"));
        assert!(Outcome::Delivered.ok());
        assert_eq!(Outcome::Failed.why(), None);
    }

    #[test]
    fn this_process_knows_who_started_it_where_the_platform_says() {
        // On unix the shutdown event's `ppid` must be a real pid; on Windows the field is null.
        #[cfg(unix)]
        assert!(parent_pid().is_some_and(|p| p > 0));
        #[cfg(windows)]
        assert_eq!(parent_pid(), None);
    }
}
