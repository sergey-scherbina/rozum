//! Confine a child process's writes on Linux, by Landlock — the MECHANISM, not the policy.
//!
//! Two consumers, and the split between them is the point. nadia's `exec` and the meeting
//! assistant's in-chat shell both need "this child may write here and nowhere else", and both were
//! macOS-only: nadia ran UNCONFINED off macOS (BUG-044) and the assistant's shell REFUSED to run
//! there at all. Neither behaviour was a decision anyone made.
//!
//! **Their policies genuinely differ and are NOT unified here.** nadia's seatbelt profile is
//! `(allow default)` with writes denied and re-allowed under the workspace, `CARGO_HOME` and
//! `TMPDIR`, because a coding agent runs `cargo`. The assistant's is `(deny default)` with writes
//! only under its root, because an in-chat shell should reach less. Folding those into one list
//! would be a silent policy change for whichever one lost — so each caller passes its own paths and
//! this crate applies them.
//!
//! | | macOS | Linux | elsewhere |
//! |---|---|---|---|
//! | mechanism | seatbelt (`sandbox-exec -p`, caller-owned) | Landlock (here) | none |
//! | the root itself | `(deny file-write-unlink (literal root))` | falls out: rights are granted BENEATH a path | — |
//! | unavailable | `sandbox-exec` missing → caller decides | kernel < 5.13 → [`Outcome::Unavailable`] | [`Outcome::NoMechanism`] |

use std::path::Path;

/// What happened when confinement was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The child will run confined.
    Applied,
    /// This platform HAS a mechanism but it is not usable here — an old kernel, a restricted
    /// container. The caller decides whether to run anyway, and must say so if it does.
    Unavailable(String),
    /// This platform has no mechanism at all.
    NoMechanism,
}

impl Outcome {
    pub fn applied(&self) -> bool {
        matches!(self, Outcome::Applied)
    }
}

/// Apply Landlock to a child, building the ruleset in the PARENT and applying it in `pre_exec`.
///
/// The split is deliberate: between fork and exec, allocating can deadlock on another thread's
/// malloc lock, so everything that allocates happens before the fork and the child performs only
/// the syscall.
///
/// **Fails closed in the child.** If a ruleset was built — so the kernel claimed support — and then
/// enforces nothing, the spawn fails rather than running an agent's shell unsandboxed. A sandbox
/// that silently is not one is worse than no sandbox, because the caller stops watching for the
/// failure it is supposed to prevent.
///
/// A `writable` path that does not exist is skipped rather than fatal: `/dev/stdout` is absent in
/// some containers, and refusing to confine at all over one missing sink trades the whole sandbox
/// for it.
#[cfg(target_os = "linux")]
pub fn confine_child(cmd: &mut std::process::Command, writable: &[&Path]) -> Outcome {
    use landlock::{
        ABI, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    };
    use std::os::unix::process::CommandExt;

    let abi = ABI::V1;
    // WRITES ONLY — `from_write`, not `from_all`.
    //
    // A Landlock ruleset denies every right it HANDLES except where granted, so handling all of
    // them confines reads and EXECUTE as well: the child then cannot exec `/bin/sh` and dies with
    // `EACCES` before running a byte. That is not a theory — it is what CI reported the first time
    // this ran on Linux, on the very test written to catch it, one commit after these docs said
    // "writes only, reads free". Handling only the write rights leaves reads and exec untouched,
    // which is what the seatbelt profile means by `(allow default) (deny file-write*)`.
    let rights = AccessFs::from_write(abi);

    let built = Ruleset::default()
        .handle_access(rights)
        .and_then(|r| r.create())
        .and_then(|r| {
            let mut r = r.no_new_privs(true);
            for p in writable {
                if let Ok(fd) = PathFd::new(p) {
                    r = r.add_rule(PathBeneath::new(fd, rights))?;
                }
            }
            Ok(r)
        });

    let ruleset = match built {
        Ok(r) => r,
        Err(e) => return Outcome::Unavailable(format!("ruleset: {e}")),
    };

    unsafe {
        cmd.pre_exec(move || match ruleset.try_clone()?.restrict_self() {
            Ok(s) if s.ruleset == RulesetStatus::NotEnforced => {
                Err(std::io::Error::other("landlock: kernel enforced nothing"))
            }
            Ok(_) => Ok(()),
            Err(e) => Err(std::io::Error::other(format!("landlock: {e}"))),
        });
    }
    Outcome::Applied
}

/// Not Linux: nothing to apply. macOS confines by WRAPPING the command in `sandbox-exec`, which
/// changes the argv and is therefore the caller's job; anywhere else there is no mechanism.
#[cfg(not(target_os = "linux"))]
pub fn confine_child(_cmd: &mut std::process::Command, _writable: &[&Path]) -> Outcome {
    if cfg!(target_os = "macos") {
        Outcome::Applied
    } else {
        Outcome::NoMechanism
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_platform_reports_what_it_can_actually_do() {
        // The three outcomes are not decoration: a caller has to tell "confined" from "this kernel
        // cannot" from "this OS has nothing", because only the first one lets it stay quiet.
        let mut cmd = std::process::Command::new("/bin/echo");
        let root = std::env::temp_dir();
        let paths: Vec<&Path> = vec![root.as_path()];
        let outcome = confine_child(&mut cmd, &paths);

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(
            outcome.applied() || matches!(outcome, Outcome::Unavailable(_)),
            "a platform with a mechanism must not report NoMechanism: {outcome:?}"
        );
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(outcome, Outcome::NoMechanism);
    }

    #[test]
    fn reads_stay_free_and_writes_outside_the_grant_are_refused() {
        // The two halves of "writes only, reads free", pinned together because getting the first
        // one wrong is silent on macOS and fatal on Linux: handling every access right instead of
        // the write ones denied EXECUTE, and the child died with EACCES before running.
        let dir = std::env::temp_dir().join(format!("rozum-confine-halves-{}", std::process::id()));
        let inside = dir.join("work");
        std::fs::create_dir_all(&inside).unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        // Reads something well outside the grant, writes inside it, then tries to write outside.
        cmd.arg("-c")
            .arg("cat /etc/hostname >/dev/null 2>&1; echo in > ok.txt; echo out > ../escaped.txt")
            .current_dir(&inside);
        let paths: Vec<&Path> = vec![inside.as_path()];
        if !confine_child(&mut cmd, &paths).applied() {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no mechanism here; the platform's own test covers that
        }
        let out = cmd.output().expect("a confined child must still exec");
        assert!(inside.join("ok.txt").exists(), "a write INSIDE the grant must succeed");
        #[cfg(target_os = "linux")]
        assert!(
            !dir.join("escaped.txt").exists(),
            "a write OUTSIDE the grant must be refused; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = out;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_confined_child_still_runs_and_can_write_where_it_was_allowed() {
        // End to end where the mechanism exists. On macOS this is a no-op by design (the caller
        // wraps in `sandbox-exec`), so it asserts the plumbing rather than the confinement; on
        // Linux CI it asserts both, which is the only machine here that can.
        let dir = std::env::temp_dir().join(format!("rozum-confine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("echo ok > inside.txt && cat inside.txt").current_dir(&dir);
        let paths: Vec<&Path> = vec![dir.as_path()];
        let outcome = confine_child(&mut cmd, &paths);
        if !outcome.applied() {
            eprintln!("skipped: confinement unavailable here ({outcome:?})");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let out = cmd.output().expect("the confined child must still spawn");
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
