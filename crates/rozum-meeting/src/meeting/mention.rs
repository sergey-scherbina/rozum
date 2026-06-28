//! Mention detection — who does a room turn address?
//!
//! Addressing a sibling in the room (`-> plucky-fox`, `@nimble-raven`) is matched ONLY against
//! KNOWN participant handles, because the transcript is full of `-> undeclared` / `-> opt-in` /
//! `-> runtime` (technical prose) that must not count as a mention. The set of known handles is the
//! universe of valid targets — a `-> X` is a mention iff `X` has spoken in the room.
//!
//! See `docs/specs/meeting-mention-inbox.md`.

use super::store::StoredTurn;
use std::collections::BTreeSet;

/// The handle part of a room display name: `"Sergiy · plucky-fox"` → `"plucky-fox"`.
/// Falls back to the whole trimmed name when there is no `" · "` separator.
pub fn handle_of(display_name: &str) -> &str {
    match display_name.rsplit_once(" · ") {
        Some((_, h)) => h.trim(),
        None => display_name.trim(),
    }
}

/// The distinct set of handles that have spoken in the transcript — the universe of valid mention
/// targets. (A `-> X` only counts if `X` appears here.)
pub fn known_handles(turns: &[StoredTurn]) -> BTreeSet<String> {
    turns
        .iter()
        .map(|t| handle_of(&t.display_name).to_string())
        .filter(|h| !h.is_empty())
        .collect()
}

/// A boundary char ends a handle token: end-of-string, or anything outside `[a-z0-9-]` (handles are
/// kebab-case, so `-` is part of the token, not a boundary).
fn is_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '-'),
    }
}

/// Does the text just before a handle occurrence introduce a mention — `@` (at a word boundary) or
/// `->` (with optional spaces/tabs before the handle)?
fn has_marker(pre: &str) -> bool {
    // `->` arrow, allowing spaces/tabs between it and the handle.
    let trimmed = pre.trim_end_matches([' ', '\t']);
    if trimmed.ends_with("->") {
        return true;
    }
    // `@` at-mention, itself at a word boundary (so `mail@plucky-fox` does not count).
    if let Some(rest) = pre.strip_suffix('@') {
        return is_boundary(rest.chars().next_back());
    }
    false
}

/// Does `content` address `handle`? True iff it contains `@handle` or `-> handle`
/// (case-insensitive) with a trailing boundary, so `-> plucky-foxtrot` does not match `plucky-fox`
/// and `sunny-civet` is not matched by the bare handle `civet`.
pub fn addresses(content: &str, handle: &str) -> bool {
    if handle.is_empty() {
        return false;
    }
    let hay = content.to_ascii_lowercase();
    let needle = handle.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&needle) {
        let i = from + rel;
        let after = hay[i + needle.len()..].chars().next();
        if is_boundary(after) && has_marker(&hay[..i]) {
            return true;
        }
        from = i + 1; // keep scanning; a later occurrence may carry the marker
    }
    false
}

/// Every known handle that `content` addresses (a turn may address several).
pub fn mentions(content: &str, known: &BTreeSet<String>) -> Vec<String> {
    known.iter().filter(|h| addresses(content, h)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handles(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn handle_of_splits_on_separator() {
        assert_eq!(handle_of("Sergiy · plucky-fox"), "plucky-fox");
        assert_eq!(handle_of("plucky-fox"), "plucky-fox"); // no separator → whole name
        assert_eq!(handle_of("  bare  "), "bare");
    }

    #[test]
    fn arrow_and_at_address_a_known_handle() {
        assert!(addresses("nimble-raven -> sunny-civet: take it", "sunny-civet"));
        assert!(addresses("handoff -> plucky-fox: draft ready", "plucky-fox"));
        assert!(addresses("@plucky-fox can you build this", "plucky-fox"));
        assert!(addresses("->plucky-fox (no space)", "plucky-fox"));
        assert!(addresses("ping @PLUCKY-FOX uppercase", "plucky-fox")); // case-insensitive
    }

    #[test]
    fn the_false_positive_corpus_is_NOT_a_mention() {
        // Real technical prose from the room that must never count as addressing someone.
        let known = handles(&["sunny-civet", "plucky-fox", "nimble-raven"]);
        assert!(mentions("RustCodeWalk .map(roomLineName) -> undeclared Either", &known).is_empty());
        assert!(mentions("flipped to opt-in -> opt-in/default-OFF", &known).is_empty());
        assert!(mentions("gateway->mlx telemetry -> runtime cost", &known).is_empty());
        // `addresses` against a NON-handle token does match the arrow — the known-handle filter in
        // `mentions` is what makes these inert (the spec's load-bearing decision).
        assert!(addresses("-> undeclared Either", "undeclared"));
    }

    #[test]
    fn trailing_boundary_prevents_prefix_matches() {
        assert!(!addresses("-> plucky-foxtrot dancing", "plucky-fox")); // longer token
        assert!(!addresses("-> sunny-civet", "civet")); // not the bare suffix of a handle
        assert!(addresses("-> sunny-civet", "sunny-civet")); // the full handle still matches
    }

    #[test]
    fn at_must_be_at_a_word_boundary() {
        assert!(!addresses("mail-to-x@plucky-fox.example", "plucky-fox")); // email-ish, not a mention
        assert!(addresses("(@plucky-fox)", "plucky-fox")); // punctuation before @ is fine
    }

    #[test]
    fn mentions_returns_all_addressed_known_handles() {
        let known = handles(&["sunny-civet", "plucky-fox", "nimble-raven"]);
        let mut got = mentions("-> sunny-civet and @plucky-fox both look", &known);
        got.sort();
        assert_eq!(got, vec!["plucky-fox".to_string(), "sunny-civet".to_string()]);
        // a plain mention of the name without a marker is not addressing
        assert!(mentions("sunny-civet shipped it", &known).is_empty());
    }

    #[test]
    fn known_handles_dedups_from_display_names() {
        let turn = |dn: &str| StoredTurn {
            date: "2026-06-23".into(),
            n: 1,
            participant_id: "p".into(),
            display_name: dn.into(),
            content: "x".into(),
            ts: 0,
            ..Default::default()
        };
        let turns =
            vec![turn("Sergiy · plucky-fox"), turn("Sergiy · plucky-fox"), turn("Sergiy · nimble-raven")];
        assert_eq!(known_handles(&turns), handles(&["plucky-fox", "nimble-raven"]));
    }
}
