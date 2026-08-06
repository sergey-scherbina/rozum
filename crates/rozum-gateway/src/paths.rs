//! Where the gateway keeps state on disk, and what may be joined onto it.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Two functions, used 17 and 19 times
//! across the control routes, that had to be reachable from the matrix module without it importing
//! the 4300-line file they lived in.

use std::path::PathBuf;

/// True if `s` is safe to use as a SINGLE filesystem path segment (matrix `stamp`/`agent`/`task` etc.)
/// — non-empty, not `.`/`..`, and containing no path separator or NUL. Rejects rather than mangles, so a
/// crafted `../../etc` walk cannot escape the results dir the segment is joined onto (path-traversal fix).
pub(crate) fn safe_path_seg(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.chars().any(|c| matches!(c, '/' | '\\' | '\0'))
}

pub(crate) fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|b| b.join("rozum"))
}

#[cfg(test)]
mod tests {
    use super::*;

        #[test]
        fn safe_path_seg_rejects_traversal() {
            // Regression guard for the matrix-cell path-traversal fix: a single path segment must carry no
            // separator, `..`, `.`, or NUL, so a crafted stamp/agent/task can't walk outside the bench
            // results dir (arbitrary file read + a dir-existence oracle otherwise).
            assert!(safe_path_seg("1783166880"));
            assert!(safe_path_seg("claude"));
            assert!(safe_path_seg("task-01"));
            assert!(!safe_path_seg(".."));
            assert!(!safe_path_seg("."));
            assert!(!safe_path_seg(""));
            assert!(!safe_path_seg("../../etc"));
            assert!(!safe_path_seg("a/b"));
            assert!(!safe_path_seg("a\\b"));
            assert!(!safe_path_seg("a\0b"));
        }
}
