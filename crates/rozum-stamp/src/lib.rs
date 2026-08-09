//! What commit a binary was built from — readable from the FILE, without running it.
//!
//! `doctor --services` can say a service is healthy and be right, while the binary serving it is
//! days behind `master`. Health and freshness are different questions and this answers the second
//! (`docs/specs/deployment-drift.md`).

/// The prefix a reader scans for. Deliberately unusual, and deliberately NOT written as one literal
/// anywhere else: a scanner searching for a string it also contains would find ITSELF in the doctor
/// binary and report whatever it was compiled from as every service's commit.
pub const MARK_PREFIX: &str = "ROZUM+BUILD+MARK=";

/// The marker itself, baked in by `build.rs`.
///
/// A byte ARRAY, not a `&str`, and this took three attempts — each one shipped and then measured:
///
/// 1. `#[used] static … : &str` — the compiler kept the static, the LINKER dropped it, and the scan
///    found nothing in the binary that declared it. Caught by the test below.
/// 2. `#[unsafe(no_mangle)] static … : &str` — passed the test in DEBUG and failed in RELEASE, on
///    the operator's machine, after being deployed. A `&str` static is a POINTER: `#[used]` and
///    `no_mangle` keep the pointer, while the string bytes live in another section that
///    `-dead_strip` is free to remove once nothing references them.
/// 3. This: the text itself IS the static. `#[used]` on a data array marks it `no_dead_strip`, and
///    there is no second object for the linker to separate from it.
///
/// The lesson is in the failure, not the fix: a stamp check that passes in debug and vanishes in
/// release reports every deployed binary as unstamped, which is precisely the "unknown reported as
/// silence" this module exists to remove.
const MARK_TEXT: &str = concat!("ROZUM+BUILD+MARK=", env!("ROZUM_BUILD_COMMIT"));

#[used]
static BUILD_MARK: [u8; MARK_TEXT.len()] = {
    let src = MARK_TEXT.as_bytes();
    let mut out = [0u8; MARK_TEXT.len()];
    let mut i = 0;
    while i < out.len() {
        out[i] = src[i];
        i += 1;
    }
    out
};

/// The commit this binary was built from, or `"unknown"` when it was built outside a git checkout.
pub fn commit() -> &'static str {
    env!("ROZUM_BUILD_COMMIT")
}

/// The repository root as it existed at build time. May not exist any more — a build in a worktree
/// bakes the worktree's path, and worktrees are deleted when their branch lands. Callers must treat
/// a missing path as "cannot compare", never as "up to date".
pub fn repo() -> &'static str {
    env!("ROZUM_BUILD_REPO")
}

/// Find the commit a built binary carries, by reading the file.
///
/// `None` for a binary that carries no marker — the ScalaScript-emitted `rozum-meeting-ssc` links
/// none of our crates, and a shell script obviously does not either. Unstamped is reported as
/// unstamped; it is not the same as up to date, and conflating them is the failure this exists for.
pub fn commit_of_file(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let needle = MARK_PREFIX.as_bytes();
    // EVERY occurrence, not the first. A binary that scans for this prefix also CONTAINS it — as
    // the `MARK_PREFIX` constant — and the compiler is free to place that bare copy before the
    // stamped one. The first version took the first hit, found the constant, read zero hex digits
    // after it and reported "unstamped" for a binary that was stamped. Found by the test on the
    // very first run; it is the same self-collision the doc comment above warns about, authored by
    // the person who wrote the warning.
    let mut from = 0usize;
    while let Some(rel) = bytes[from..].windows(needle.len()).position(|w| w == needle) {
        let at = from + rel + needle.len();
        let rest = &bytes[at..];
        let end = rest.iter().position(|b| !b.is_ascii_hexdigit()).unwrap_or(rest.len());
        if end >= 7 {
            if let Ok(sha) = std::str::from_utf8(&rest[..end]) {
                return Some(sha.to_string());
            }
        }
        from = at;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp has to survive being LINKED, which is the whole mechanism: a `static` nothing calls
    /// is exactly what a linker is entitled to drop.
    #[test]
    fn this_binary_carries_its_own_marker() {
        assert!(MARK_TEXT.starts_with(MARK_PREFIX));
        assert_eq!(BUILD_MARK.len(), MARK_TEXT.len());
        let exe = std::env::current_exe().expect("test binary path");
        let found = commit_of_file(&exe);
        // A test binary is built by the same cargo invocation, so it carries the same commit.
        assert_eq!(found.as_deref(), Some(commit()), "marker not findable in {}", exe.display());
    }

    #[test]
    fn a_file_without_a_marker_is_none_not_a_guess() {
        let d = std::env::temp_dir().join(format!("rozum-stamp-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("plain");
        std::fs::write(&p, b"#!/bin/sh\necho hello\n").unwrap();
        assert_eq!(commit_of_file(&p), None);
        assert_eq!(commit_of_file(&d.join("missing")), None);
        std::fs::remove_dir_all(&d).ok();
    }
}
