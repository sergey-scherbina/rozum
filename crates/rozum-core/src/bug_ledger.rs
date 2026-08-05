//! A guard over `BUGS.md`, the bug ledger.
//!
//! **Why this lives in a library crate rather than in a script or a bin test.** CI runs
//! `cargo build --workspace --bins` and `cargo test --workspace --lib` (`.github/workflows/ci.yml`),
//! so a check that is not in a LIB does not run on push — and a guard nobody runs is worse than no
//! guard, because it reads as coverage. A shell script would have the same problem twice over: this
//! repo has no hooks configured (`core.hooksPath` is unset) and no pre-push suite.
//!
//! **Why it exists at all.** Two different bugs were both filed as `BUG-017` — nadia's jail escape
//! (2026-08-04) and the meeting daemon's missing REST secret (2026-08-05). Nothing noticed. By the
//! time it surfaced, the wrong number had reached commit messages, a spec, a meeting room and an
//! agent's notes, and each of those now pointed at somebody else's bug. Renumbering after the fact
//! is the expensive kind of fix; this makes it a build failure instead.

use std::path::{Path, PathBuf};

/// Everything this guard can find wrong, one variant per rule, so a failure names itself.
#[derive(Debug, PartialEq, Eq)]
pub enum LedgerFault {
    /// The same id heads two entries.
    DuplicateId(u32),
    /// Entries are not newest-first by id. Carries the pair that breaks the order.
    OutOfOrder { after: u32, before: u32 },
    /// A heading that is not `## BUG-NNN — <title>`.
    Malformed(String),
    /// A gap in the numbering: ids should be contiguous.
    Gap { missing: u32 },
}

impl std::fmt::Display for LedgerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(
                f,
                "BUG-{id:03} heads two entries. Give the LATER one the next free number and say in \
                 it that it was filed under the old one — the wrong number has usually already \
                 reached commits and rooms."
            ),
            Self::OutOfOrder { after, before } => write!(
                f,
                "BUG-{after:03} appears after BUG-{before:03}. The file is newest-first; a new \
                 entry goes at the TOP."
            ),
            Self::Malformed(line) => write!(
                f,
                "heading is not `## BUG-NNN — <title>`: {line:?}. The id is parsed from it, so a \
                 heading nobody can parse is an entry nobody can reference."
            ),
            Self::Gap { missing } => write!(
                f,
                "BUG-{missing:03} is missing. Ids are contiguous — a gap means an entry was deleted \
                 rather than marked resolved, and its number must never be reused."
            ),
        }
    }
}

/// Ids in the order the file lists them.
pub fn ids_in_file_order(markdown: &str) -> Result<Vec<u32>, LedgerFault> {
    let mut ids = Vec::new();
    for line in markdown.lines() {
        let Some(rest) = line.strip_prefix("## BUG-") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        // A heading must carry a number AND a title after it; `## BUG-7` or `## BUG-` is a typo
        // that would otherwise silently drop an entry out of every check below.
        if digits.is_empty() || digits.len() != 3 || rest[digits.len()..].trim().is_empty() {
            return Err(LedgerFault::Malformed(line.to_string()));
        }
        ids.push(digits.parse().expect("three ascii digits"));
    }
    Ok(ids)
}

/// Check the ledger. `Ok(count)` is how many entries passed.
pub fn check(markdown: &str) -> Result<usize, LedgerFault> {
    let ids = ids_in_file_order(markdown)?;

    // Order is read from the FILE, never from a sorted copy: the whole convention is newest-first,
    // and sorting first would happily pass a file somebody had silently reordered.
    for pair in ids.windows(2) {
        if pair[0] == pair[1] {
            return Err(LedgerFault::DuplicateId(pair[0]));
        }
        if pair[0] < pair[1] {
            return Err(LedgerFault::OutOfOrder {
                after: pair[0],
                before: pair[1],
            });
        }
    }
    // Duplicates that are not adjacent (an entry filed in the middle under an existing number)
    // would pass the window check above, so look at the set too.
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0] == pair[1] {
            return Err(LedgerFault::DuplicateId(pair[0]));
        }
        if pair[1] != pair[0] + 1 {
            return Err(LedgerFault::Gap {
                missing: pair[0] + 1,
            });
        }
    }
    Ok(ids.len())
}

/// `BUGS.md` at the repository root, found by walking up from this crate.
pub fn ledger_path() -> PathBuf {
    let mut dir: &Path = Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("BUGS.md");
        if candidate.is_file() {
            return candidate;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return PathBuf::from("BUGS.md"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repositorys_own_ledger_passes() {
        let path = ledger_path();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        match check(&text) {
            Ok(n) => assert!(n > 0, "no entries found in {} — the parser is wrong, not the file, \
                                     because a ledger with zero bugs would be news", path.display()),
            Err(fault) => panic!("{}: {fault}", path.display()),
        }
    }

    /// The case that actually happened, in the shape it happened: BUG-017 twice, not adjacent.
    #[test]
    fn the_collision_that_prompted_this_is_caught() {
        let doc = "\
## BUG-019 — c
## BUG-018 — b
## BUG-017 — the jail let the agent delete its own workspace
## BUG-017 — a client-triggered auto-start brings the daemon up without its secret
## BUG-016 — a
";
        assert_eq!(check(doc), Err(LedgerFault::DuplicateId(17)));
    }

    #[test]
    fn newest_first_is_read_from_the_file_not_from_a_sorted_copy() {
        let doc = "## BUG-001 — a\n## BUG-002 — b\n";
        assert_eq!(
            check(doc),
            Err(LedgerFault::OutOfOrder {
                after: 1,
                before: 2
            })
        );
    }

    #[test]
    fn a_deleted_entry_leaves_a_gap_and_a_gap_is_a_fault() {
        // BUG-002 is gone: its number must never be reused, so the file must keep the entry
        // (resolved is a status, not a deletion).
        let doc = "## BUG-003 — c\n## BUG-001 — a\n";
        assert_eq!(check(doc), Err(LedgerFault::Gap { missing: 2 }));
    }

    #[test]
    fn a_heading_the_parser_cannot_read_is_a_fault_not_a_skip() {
        // Silently skipping these is how an entry drops out of every other check in this file.
        for bad in ["## BUG-7 — short", "## BUG- — empty", "## BUG-021"] {
            assert!(
                matches!(check(bad), Err(LedgerFault::Malformed(_))),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn prose_that_merely_mentions_a_bug_id_is_not_an_entry() {
        // Only a heading files a bug. Body text referring to one must not be counted, or every
        // cross-reference would read as a duplicate.
        let doc = "## BUG-002 — b\n\nSame family as BUG-001, see above.\n## BUG-001 — a\n";
        assert_eq!(check(doc), Ok(2));
    }
}
