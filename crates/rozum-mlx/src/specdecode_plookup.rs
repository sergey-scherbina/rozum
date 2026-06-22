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

    /// Measure prompt-lookup's accept-rate / speedup on a REAL agentic edit, with a REAL
    /// model tokenizer — the P0 economic gate (spec `docs/specs/prompt-lookup-decoding.md`).
    /// The scenario: a model is given a source file and asked to rewrite it with a few
    /// changes; it re-emits the whole file (≈verbatim) with the edits. No model forward —
    /// the accept-rate is purely how well the n-gram lookup predicts the real output
    /// sequence (in real decode the forward just confirms the greedy token).
    ///   cargo test -p rozum-mlx --no-default-features --release prompt_lookup_acceptrate -- --ignored --nocapture
    #[test]
    #[ignore = "accept-rate measurement; needs a tokenizer.json (ROZUM_PLOOKUP_TOKENIZER or cached Qwen3-0.6B-4bit)"]
    fn prompt_lookup_acceptrate_on_real_edit() {
        use tokenizers::Tokenizer;
        let tok_path = std::env::var("ROZUM_PLOOKUP_TOKENIZER").ok().or_else(|| {
            let base = format!(
                "{}/.cache/huggingface/hub/models--mlx-community--Qwen3-0.6B-4bit/snapshots",
                std::env::var("HOME").unwrap_or_default()
            );
            std::fs::read_dir(&base).ok()?.flatten().find_map(|e| {
                let p = e.path().join("tokenizer.json");
                p.exists().then(|| p.to_string_lossy().into_owned())
            })
        });
        let Some(tok_path) = tok_path else {
            eprintln!("SKIP: no tokenizer.json (set ROZUM_PLOOKUP_TOKENIZER=/path/to/tokenizer.json)");
            return;
        };
        let tok = Tokenizer::from_file(&tok_path).expect("load tokenizer");

        // A real agentic edit: rename a few identifiers + note the revision across a real file.
        let orig = include_str!("specdecode.rs");
        let edited = orig
            .replace("draft", "drafter")
            .replace("target", "tgt")
            .replace("Speculative", "Speculative (revised)");
        let prompt = format!(
            "Here is `specdecode.rs`:\n```rust\n{orig}\n```\nRewrite it: rename `draft`→`drafter`, \
             `target`→`tgt`, and note the revision.\n```rust\n"
        );
        let prompt_ids: Vec<u32> = tok.encode(prompt, false).expect("encode prompt").get_ids().to_vec();
        let output_ids: Vec<u32> = tok.encode(edited, false).expect("encode output").get_ids().to_vec();
        eprintln!(
            "prompt={} tok, output={} tok ({}% of output's tokens also appear in the prompt context)",
            prompt_ids.len(),
            output_ids.len(),
            {
                let set: std::collections::HashSet<u32> = prompt_ids.iter().copied().collect();
                100 * output_ids.iter().filter(|t| set.contains(t)).count() / output_ids.len().max(1)
            }
        );

        for (ngram, k) in [(1usize, 5usize), (2, 5), (3, 8), (2, 10)] {
            let mut d = PromptLookupDraft::new(ngram, k, 8192);
            let mut ctx = prompt_ids.clone();
            let (mut pos, mut forwards, mut accepted) = (0usize, 0usize, 0usize);
            while pos < output_ids.len() {
                let prop = d.propose(&ctx, k);
                let mut acc = 0usize;
                while acc < prop.len() && pos + acc < output_ids.len() && prop[acc] == output_ids[pos + acc] {
                    acc += 1;
                }
                // One forward emits the accepted prefix + 1 bonus (the model's own greedy token).
                let emit = (acc + 1).min(output_ids.len() - pos);
                ctx.extend_from_slice(&output_ids[pos..pos + emit]);
                accepted += emit - 1; // the +1 bonus is the model's token, not a lookup save
                pos += emit;
                forwards += 1;
            }
            let toks = output_ids.len();
            eprintln!(
                "PLOOKUP n={ngram} k={k:>2}: forwards={forwards:>4} / {toks} tok  →  \
                 tokens/forward={:.2}  accept-rate={:.1}%  speedup={:.2}×",
                toks as f64 / forwards as f64,
                100.0 * accepted as f64 / toks as f64,
                toks as f64 / forwards as f64,
            );
        }
    }
}
