//! Engine-agnostic token sampler (`extract-shared-sampler`, L2).
//!
//! The sampler (repeat-penalty → temperature → top-k → top-p → categorical) defined over a
//! plain logit slice `&[f32]` + an RNG, so every leaf materializes its final-position logits
//! and calls this instead of re-implementing it. The per-token GPU→CPU copy of one vocab
//! vector is negligible for our op-launch-bound decode. Mirrors the MLX fork's `sample_with`
//! semantics on the CPU. (The MLX hot path keeps its on-device version for the byte-exact
//! oracle tests; this is the canonical definition + what the CPU leaves, e.g. GGUF, call.)

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::backend::SamplingParams;

/// Window of recent tokens the repeat penalty applies over.
const REPEAT_WINDOW: usize = 64;

/// Concrete sampler configuration, resolved from the optional [`SamplingParams`].
#[derive(Debug, Clone, Copy)]
pub struct SamplerConfig {
    /// `0.0` → greedy (argmax).
    pub temperature: f32,
    /// `0` → off.
    pub top_k: usize,
    /// `1.0` → off.
    pub top_p: f32,
    /// `1.0` → off (HF convention: divide positive logits / multiply negative ones).
    pub repeat_penalty: f32,
}

impl SamplerConfig {
    pub fn from_params(p: &SamplingParams) -> Self {
        Self {
            temperature: p.temperature.unwrap_or(0.0),
            top_k: p.top_k.unwrap_or(0) as usize,
            top_p: p.top_p.unwrap_or(1.0),
            repeat_penalty: p.repeat_penalty.unwrap_or(1.0),
        }
    }
}

/// A deterministic RNG when `seed` is set, else seeded from entropy.
pub fn seeded_rng(seed: Option<u64>) -> StdRng {
    match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_entropy(),
    }
}

/// The last [`REPEAT_WINDOW`] token ids — pass as `recent` for the repeat penalty.
pub fn repeat_window(recent: &[u32]) -> &[u32] {
    &recent[recent.len().saturating_sub(REPEAT_WINDOW)..]
}

/// Sample one token id from `logits` (`[vocab]`). Repeat penalty over `recent`, then
/// `temperature == 0` → argmax, else temperature scale → top-k → top-p (nucleus) → categorical.
pub fn sample(logits: &[f32], cfg: &SamplerConfig, recent: &[u32], rng: &mut impl Rng) -> u32 {
    let mut l = logits.to_vec();

    // 1. Repeat penalty (HF convention).
    if cfg.repeat_penalty != 1.0 {
        for &t in recent {
            if let Some(v) = l.get_mut(t as usize) {
                *v = if *v > 0.0 { *v / cfg.repeat_penalty } else { *v * cfg.repeat_penalty };
            }
        }
    }

    // 2. Greedy.
    if cfg.temperature <= 0.0 {
        return argmax(&l);
    }

    // 3. Temperature.
    for v in &mut l {
        *v /= cfg.temperature;
    }

    // 4. Top-k: keep the k largest (mask the rest).
    if cfg.top_k > 0 && cfg.top_k < l.len() {
        let kth = kth_largest(&l, cfg.top_k);
        for v in &mut l {
            if *v < kth {
                *v = f32::NEG_INFINITY;
            }
        }
    }

    // 5. Softmax → (top-p) → categorical.
    let probs = softmax(&l);
    if cfg.top_p < 1.0 {
        sample_top_p(&probs, cfg.top_p, rng)
    } else {
        sample_categorical(&probs, rng)
    }
}

fn argmax(l: &[f32]) -> u32 {
    l.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i as u32)
        .unwrap_or(0)
}

/// The value of the k-th largest element (the top-k keep threshold).
fn kth_largest(l: &[f32], k: usize) -> f32 {
    let mut v = l.to_vec();
    // Ascending select at index len-k → the k-th largest sits there.
    let idx = v.len() - k;
    v.select_nth_unstable_by(idx, f32::total_cmp);
    v[idx]
}

/// Numerically-stable softmax (ignores `-inf` entries).
fn softmax(l: &[f32]) -> Vec<f32> {
    let max = l.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut exps: Vec<f32> = l.iter().map(|&x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum > 0.0 {
        for e in &mut exps {
            *e /= sum;
        }
    }
    exps
}

/// Sample an index from a probability distribution (sums to ~1).
fn sample_categorical(probs: &[f32], rng: &mut impl Rng) -> u32 {
    let r: f32 = rng.r#gen::<f32>();
    let mut cum = 0.0f32;
    for (i, &p) in probs.iter().enumerate() {
        cum += p;
        if r < cum {
            return i as u32;
        }
    }
    // Floating-point slack → the last non-zero.
    probs
        .iter()
        .enumerate()
        .rev()
        .find(|&(_, &p)| p > 0.0)
        .map(|(i, _)| i as u32)
        .unwrap_or(0)
}

/// Nucleus sampling: keep the smallest set of highest-prob tokens summing to ≥ `top_p`,
/// renormalize, and sample within it.
fn sample_top_p(probs: &[f32], top_p: f32, rng: &mut impl Rng) -> u32 {
    let mut idx: Vec<usize> = (0..probs.len()).collect();
    idx.sort_unstable_by(|&a, &b| probs[b].total_cmp(&probs[a])); // descending
    let mut cum = 0.0f32;
    let mut keep = 0usize;
    for (rank, &i) in idx.iter().enumerate() {
        cum += probs[i];
        keep = rank + 1;
        if cum >= top_p {
            break;
        }
    }
    let kept = &idx[..keep];
    let mass: f32 = kept.iter().map(|&i| probs[i]).sum();
    let r = rng.r#gen::<f32>() * mass;
    let mut c = 0.0f32;
    for &i in kept {
        c += probs[i];
        if r < c {
            return i as u32;
        }
    }
    kept[keep - 1] as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(temp: f32, top_k: usize, top_p: f32, rp: f32) -> SamplerConfig {
        SamplerConfig { temperature: temp, top_k, top_p, repeat_penalty: rp }
    }

    #[test]
    fn greedy_is_argmax() {
        let mut rng = seeded_rng(Some(1));
        let l = [0.1, 3.0, 0.5, 2.9];
        assert_eq!(sample(&l, &cfg(0.0, 0, 1.0, 1.0), &[], &mut rng), 1);
    }

    #[test]
    fn repeat_penalty_demotes_recent_token() {
        let mut rng = seeded_rng(Some(1));
        // idx 0 is the max, but penalizing it (recent=[0]) drops it below idx 1.
        let l = [1.0, 0.9, 0.0];
        let out = sample(&l, &cfg(0.0, 0, 1.0, 2.0), &[0], &mut rng);
        assert_eq!(out, 1, "penalized token 0 → argmax becomes 1");
    }

    #[test]
    fn top_k_1_collapses_to_argmax() {
        let l = [0.1, 3.0, 2.5, 2.9];
        // Only the single largest survives, so any temperature/seed yields the argmax.
        for seed in [1u64, 2, 99] {
            let mut rng = seeded_rng(Some(seed));
            assert_eq!(sample(&l, &cfg(1.0, 1, 1.0, 1.0), &[], &mut rng), 1);
        }
    }

    #[test]
    fn top_p_keeps_only_the_dominant_token() {
        // A peaked distribution: token 0 already exceeds p=0.5, so nucleus keeps only it.
        let l = [5.0, 0.0, 0.0, 0.0];
        for seed in [1u64, 7, 42] {
            let mut rng = seeded_rng(Some(seed));
            assert_eq!(sample(&l, &cfg(1.0, 0, 0.5, 1.0), &[], &mut rng), 0);
        }
    }

    #[test]
    fn seeded_sampling_is_deterministic() {
        let l = [1.0, 1.0, 1.0, 1.0]; // uniform → the seed decides
        let pick = |s| {
            let mut rng = seeded_rng(Some(s));
            sample(&l, &cfg(1.0, 0, 1.0, 1.0), &[], &mut rng)
        };
        assert_eq!(pick(123), pick(123), "same seed → same token");
        // and a valid index
        assert!(pick(123) < 4);
    }

    #[test]
    fn repeat_window_bounds() {
        let recent: Vec<u32> = (0..100).collect();
        assert_eq!(repeat_window(&recent).len(), REPEAT_WINDOW);
        assert_eq!(repeat_window(&[1, 2, 3]).len(), 3);
    }
}
