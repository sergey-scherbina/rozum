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

/// One resolution order for the whole workspace, in `rozum_core::userpaths`: `HOME` is not a
/// Windows variable, and this had its own copy of the rule that assumed it was.
pub(crate) fn state_dir() -> Option<PathBuf> {
    rozum_paths::state_dir()
}

pub(crate) fn ucc_site_dir() -> PathBuf {
    rozum_paths::home_dir()
        .unwrap_or_else(rozum_paths::temp_dir)
        .join(".rozum")
        .join("ucc")
        .join("site")
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
