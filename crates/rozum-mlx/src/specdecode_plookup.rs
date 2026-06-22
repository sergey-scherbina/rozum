//! Prompt-lookup decoding (draft-free speculative decode).
//!
//! A [`Draft`] that proposes the next `k` tokens with **no model** — it finds the most
//! recent earlier occurrence of the last `ngram` tokens of the context and returns the
//! tokens that followed it. The heavy verbatim self-similarity of agentic/code output (the
//! model re-emits the file it just read) becomes speed through the existing verify loop:
//! `Target::verify` accepts only tokens equal to the target's own greedy pick, so a wrong
//! proposal is simply rejected — never a correctness risk. No GPU, no extra resident model,
//! so it composes with the residency gate (BUG-003) and never pushes toward overcommit.
//!
//! Spec: `docs/specs/prompt-lookup-decoding.md`.

use crate::specdecode::{Draft, TokenId};

/// Draft-free proposer over the running context (prompt + generated tokens).
pub struct PromptLookupDraft {
    /// Match the last `ngram` tokens (≥1).
    ngram: usize,
    /// Never propose more than this many tokens per step.
    max_k: usize,
    /// Only scan the last `window` tokens for a match (`0` = whole context), bounding the
    /// search to O(window).
    window: usize,
}

impl PromptLookupDraft {
    pub fn new(ngram: usize, max_k: usize, window: usize) -> Self {
        Self { ngram: ngram.max(1), max_k: max_k.max(1), window }
    }

    /// `ROZUM_PLOOKUP_NGRAM` (2), `_K` (5), `_WINDOW` (8192).
    pub fn from_env() -> Self {
        let u = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        Self::new(u("ROZUM_PLOOKUP_NGRAM", 2), u("ROZUM_PLOOKUP_K", 5), u("ROZUM_PLOOKUP_WINDOW", 8192))
    }
}

impl Draft for PromptLookupDraft {
    fn propose(&mut self, ctx: &[TokenId], k: usize) -> Vec<TokenId> {
        let k = k.min(self.max_k);
        let n = self.ngram;
        let len = ctx.len();
        if k == 0 || len <= n {
            return Vec::new();
        }
        let needle = &ctx[len - n..];
        // Candidate match start positions: `j` such that `ctx[j..j+n] == needle`, with `j`
        // strictly before the current needle at `len - n`. Scan right→left for the most
        // recent match (longest, freshest copy region).
        let hi = len - n; // exclusive upper bound for `j`
        let lo = if self.window > 0 { hi.saturating_sub(self.window) } else { 0 };
        for j in (lo..hi).rev() {
            if &ctx[j..j + n] == needle {
                let start = j + n;
                let end = (start + k).min(len); // clamp to existing tokens
                return if end > start { ctx[start..end].to_vec() } else { Vec::new() };
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposes_continuation_after_most_recent_ngram_match() {
        // ctx ends in [10,11]; the earlier [10,11] at index 0 is followed by [12,20].
        let ctx = [10u32, 11, 12, 20, 99, 10, 11];
        let mut d = PromptLookupDraft::new(2, 5, 0);
        assert_eq!(d.propose(&ctx, 5), vec![12, 20, 99, 10, 11].into_iter().take(5).collect::<Vec<_>>());
    }

    #[test]
    fn picks_the_most_recent_match_not_the_first() {
        // needle [1,2] occurs at j=0 (→[3]) and j=4 (→[7]); the freshest (j=4) wins.
        let ctx = [1u32, 2, 3, 50, 1, 2, 7, 8, 1, 2];
        let mut d = PromptLookupDraft::new(2, 2, 0);
        assert_eq!(d.propose(&ctx, 2), vec![7, 8]);
    }

    #[test]
    fn no_match_proposes_nothing() {
        let ctx = [5u32, 6, 7, 8];
        let mut d = PromptLookupDraft::new(2, 5, 0);
        assert!(d.propose(&ctx, 5).is_empty());
    }

    #[test]
    fn clamps_k_to_max_k_and_available_tokens() {
        let ctx = [1u32, 2, 3, 4, 1, 2];
        // max_k = 1 caps the proposal even if k asks for 5.
        let mut d = PromptLookupDraft::new(2, 1, 0);
        assert_eq!(d.propose(&ctx, 5), vec![3]);
        // The j=0 match `[1,2]` is followed by everything up to the end: `[3,4,1,2]` —
        // the periodic continuation (verify accepts only what the model agrees with).
        // Available tokens cap it below the requested 9.
        let mut d2 = PromptLookupDraft::new(2, 9, 0);
        assert_eq!(d2.propose(&ctx, 9), vec![3, 4, 1, 2]);
    }

    #[test]
    fn window_bounds_the_scan() {
        // The only [1,2] match is far back; a tight window can't see it.
        let ctx = [1u32, 2, 9, 0, 0, 0, 0, 0, 0, 1, 2];
        let mut near = PromptLookupDraft::new(2, 3, 3); // window too small
        assert!(near.propose(&ctx, 3).is_empty());
        let mut far = PromptLookupDraft::new(2, 3, 100); // window large enough
        assert_eq!(far.propose(&ctx, 3), vec![9, 0, 0]);
    }

    #[test]
    fn ngram_longer_than_context_is_safe() {
        let ctx = [1u32];
        let mut d = PromptLookupDraft::new(3, 5, 0);
        assert!(d.propose(&ctx, 5).is_empty());
    }
}
