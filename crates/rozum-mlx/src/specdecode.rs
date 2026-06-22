//! Engine-agnostic speculative decoding orchestrator.
//!
//! A [`Draft`] proposes `k` tokens; a [`Target`] verifies them in one forward and
//! returns the tokens to emit — the longest prefix that equals the target's own
//! greedy choice, plus the target's corrected/bonus token. The orchestrator only
//! ever emits the target's greedy tokens, so the output is **byte-identical to
//! plain greedy decode of the target** regardless of draft quality — a latency
//! win, not a quality tradeoff. The draft only changes how many tokens the target
//! commits per forward.
//!
//! This is the durable core: it knows nothing about MLX/GGUF/devices. An engine
//! opts in by implementing [`Target`] (`verify`) over its KV state, and a small
//! model implements [`Draft`]; the draft and target are two residents that
//! device-aware placement can pin to different devices (a small draft on a cheap
//! device, the big target on the fast one). See
//! `docs/specs/speculative-decoding.md`.

pub type TokenId = u32;

/// One target forward's result: the tokens to emit (accepted greedy prefix +
/// the corrected/bonus token) and whether the target stopped (EOS).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Verify {
    pub emit: Vec<TokenId>,
    pub eos: bool,
}

/// The target model's speculative-verify capability (engine-implemented).
pub trait Target {
    /// In **one forward**, score the `draft` tokens against the target's own
    /// greedy continuation of `ctx`: emit the longest prefix of `draft` equal to
    /// that continuation, plus the target's next greedy token (the correction at
    /// the first divergence, or the bonus token if all `draft` matched). The
    /// emitted tokens are therefore always the target's greedy tokens.
    fn verify(&mut self, ctx: &[TokenId], draft: &[TokenId]) -> Verify;
}

/// A small draft model: greedily propose the next `k` tokens given `ctx`.
pub trait Draft {
    fn propose(&mut self, ctx: &[TokenId], k: usize) -> Vec<TokenId>;
}

/// Run speculative decoding to at most `max_new` tokens, invoking `on_token` for
/// each emitted token as it is produced. If `on_token` returns `false` the loop
/// stops immediately (the caller hit its own stop condition — client gone, EOS it
/// tracks, max-tokens, a loop guard). Returns the number of tokens emitted. Every
/// emitted token is one of the target's greedy tokens, so the streamed sequence is
/// a valid greedy decode of the target regardless of draft quality.
pub fn decode_streaming(
    prompt: &[TokenId],
    draft: &mut dyn Draft,
    target: &mut dyn Target,
    k: usize,
    max_new: usize,
    on_token: &mut dyn FnMut(TokenId) -> bool,
) -> usize {
    let k = k.max(1);
    let mut emitted = 0usize;
    let mut ctx: Vec<TokenId> = prompt.to_vec();
    while emitted < max_new {
        let proposed = draft.propose(&ctx, k);
        let v = target.verify(&ctx, &proposed);
        if v.emit.is_empty() {
            break;
        }
        for t in &v.emit {
            if emitted >= max_new {
                break;
            }
            ctx.push(*t);
            emitted += 1;
            if !on_token(*t) {
                return emitted;
            }
        }
        if v.eos {
            break;
        }
    }
    emitted
}

/// Run speculative decoding to at most `max_new` tokens, collecting the result.
/// The result is exactly what greedy decoding the target alone would produce (in
/// exact arithmetic). A thin wrapper over [`decode_streaming`].
pub fn decode(
    prompt: &[TokenId],
    draft: &mut dyn Draft,
    target: &mut dyn Target,
    k: usize,
    max_new: usize,
) -> Vec<TokenId> {
    let mut out: Vec<TokenId> = Vec::new();
    decode_streaming(prompt, draft, target, k, max_new, &mut |t| {
        out.push(t);
        true
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic greedy target: its output for `prompt` is the fixed
    /// sequence `seq`. `verify` implements accept-longest-greedy-prefix off that
    /// canonical stream and counts the forwards it took.
    struct MockTarget {
        full: Vec<TokenId>, // prompt ++ seq
        forwards: usize,
    }
    impl MockTarget {
        fn new(prompt: &[TokenId], seq: &[TokenId]) -> Self {
            let mut full = prompt.to_vec();
            full.extend_from_slice(seq);
            Self { full, forwards: 0 }
        }
    }
    impl Target for MockTarget {
        fn verify(&mut self, ctx: &[TokenId], draft: &[TokenId]) -> Verify {
            self.forwards += 1;
            let m = ctx.len();
            // longest draft prefix equal to the canonical continuation
            let mut acc = 0;
            while acc < draft.len() && m + acc < self.full.len() && draft[acc] == self.full[m + acc]
            {
                acc += 1;
            }
            // emit accepted prefix + one corrected/bonus target token
            let upto = (m + acc + 1).min(self.full.len());
            let emit = self.full[m.min(self.full.len())..upto].to_vec();
            let eos = upto == self.full.len();
            Verify { emit, eos }
        }
    }

    struct Oracle {
        full: Vec<TokenId>,
    } // always proposes the true continuation
    impl Draft for Oracle {
        fn propose(&mut self, ctx: &[TokenId], k: usize) -> Vec<TokenId> {
            let m = ctx.len();
            (0..k)
                .filter_map(|i| self.full.get(m + i).copied())
                .collect()
        }
    }

    struct AlwaysWrong;
    impl Draft for AlwaysWrong {
        fn propose(&mut self, _ctx: &[TokenId], k: usize) -> Vec<TokenId> {
            vec![999_999; k] // never matches a real token id
        }
    }

    /// Right on even ctx-lengths, wrong on odd — exercises partial acceptance.
    struct Flaky {
        full: Vec<TokenId>,
    }
    impl Draft for Flaky {
        fn propose(&mut self, ctx: &[TokenId], k: usize) -> Vec<TokenId> {
            let m = ctx.len();
            (0..k)
                .map(|i| {
                    let t = self.full.get(m + i).copied().unwrap_or(0);
                    if (m + i) % 2 == 0 {
                        t
                    } else {
                        t.wrapping_add(7)
                    }
                })
                .collect()
        }
    }

    fn run(
        prompt: &[TokenId],
        seq: &[TokenId],
        draft: &mut dyn Draft,
        k: usize,
    ) -> (Vec<TokenId>, usize) {
        let mut target = MockTarget::new(prompt, seq);
        let out = decode(prompt, draft, &mut target, k, 10_000);
        (out, target.forwards)
    }

    #[test]
    fn byte_identical_regardless_of_draft_quality() {
        let prompt = [1, 2, 3];
        let seq = [10, 11, 12, 13, 14, 15, 16, 17];
        let full: Vec<_> = prompt.iter().chain(seq.iter()).copied().collect();

        let (oracle_out, _) = run(&prompt, &seq, &mut Oracle { full: full.clone() }, 4);
        let (wrong_out, _) = run(&prompt, &seq, &mut AlwaysWrong, 4);
        let (flaky_out, _) = run(&prompt, &seq, &mut Flaky { full: full.clone() }, 4);

        // Every draft yields exactly the target's greedy sequence.
        assert_eq!(oracle_out, seq);
        assert_eq!(wrong_out, seq);
        assert_eq!(flaky_out, seq);
    }

    #[test]
    fn oracle_draft_needs_far_fewer_forwards_than_wrong() {
        let prompt = [1];
        let seq: Vec<TokenId> = (100..132).collect(); // 32 tokens
        let full: Vec<_> = prompt.iter().chain(seq.iter()).copied().collect();
        let k = 4;

        let (oracle_out, oracle_fwd) = run(&prompt, &seq, &mut Oracle { full }, k);
        let (wrong_out, wrong_fwd) = run(&prompt, &seq, &mut AlwaysWrong, k);

        assert_eq!(oracle_out, seq);
        assert_eq!(wrong_out, seq);
        // All-wrong draft commits 1 token/forward == plain greedy baseline.
        assert_eq!(wrong_fwd, seq.len());
        // Oracle commits k+1 per forward → ~len/(k+1) forwards (the speedup).
        assert!(
            oracle_fwd <= seq.len().div_ceil(k + 1) + 1,
            "oracle forwards {oracle_fwd} not near len/(k+1) ({})",
            seq.len().div_ceil(k + 1)
        );
        assert!(oracle_fwd * 3 < wrong_fwd, "expected a clear speedup");
    }

    #[test]
    fn stops_at_eos() {
        let prompt = [1, 2];
        let seq = [5, 6, 7];
        let full: Vec<_> = prompt.iter().chain(seq.iter()).copied().collect();
        let (out, _) = run(&prompt, &seq, &mut Oracle { full }, 8);
        assert_eq!(out, seq, "emits the whole sequence then stops at EOS");
    }
}
