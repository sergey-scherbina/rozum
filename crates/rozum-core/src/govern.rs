//! Memory-pressure governor — the **measured** safety backstop
//! (`docs/specs/safe-multi-model-program.md` §1).
//!
//! Admission (`share::acquire_residency`) is *open-loop*: it reserves a *predicted*
//! footprint and refuses if the sum exceeds a budget. Necessary, but an estimate can be
//! wrong (a pathological prompt, a backend with no cache bound, a model that genuinely
//! needs more). A *guarantee* needs a **closed loop**: watch the host's ACTUAL free RAM
//! and act before the danger threshold — so reality, not a prediction, is the safety
//! authority, and the host can never cross into the vm-compressor-exhaustion / watchdog
//! kernel-panic zone ([[project-reboot-watchdog-oom]]).
//!
//! This module is the **pure decision core** — no sampling, no I/O — so it is fully
//! unit-testable. The live loop (a gateway task) samples `vm_stat` free RAM + per-model
//! `mlx_rs::memory::get_active/get_cache` on a short interval and feeds [`classify`];
//! [`action_for`] says what to do; the Switchboard carries it out (evict lowest-utility
//! idle model). The bands are deliberately conservative (act on *free* RAM, the quantity
//! whose exhaustion reboots the box — not on a derived "budget").

/// Host memory-pressure band, from actual free RAM vs total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    /// Plenty of headroom — admit new loads freely.
    Green,
    /// Getting tight — stop admitting, start prewarming swap candidates.
    Yellow,
    /// Danger — shed load now (evict an idle model) before the OS jetsam/panics.
    Red,
}

/// Free-RAM thresholds as a fraction of total host RAM. Acting on *free* (not a budget)
/// is what makes this a real backstop: it catches over-spend from any source (a bad
/// estimate, an un-bounded backend, another process) that the admission budget can't see.
#[derive(Clone, Copy, Debug)]
pub struct PressureThresholds {
    /// Enter Yellow when free RAM drops to/below this fraction of total (default 0.20).
    pub yellow_free_frac: f64,
    /// Enter Red when free RAM drops to/below this fraction of total (default 0.10).
    pub red_free_frac: f64,
}

impl Default for PressureThresholds {
    fn default() -> Self {
        Self { yellow_free_frac: 0.20, red_free_frac: 0.10 }
    }
}

impl PressureThresholds {
    /// Read `ROZUM_GOV_YELLOW_FRAC` / `ROZUM_GOV_RED_FRAC` (each a fraction in (0,1)),
    /// falling back to the conservative defaults. `red` is clamped ≤ `yellow`.
    pub fn from_env() -> Self {
        let frac = |k: &str, d: f64| -> f64 {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|x| x.is_finite() && *x > 0.0 && *x < 1.0)
                .unwrap_or(d)
        };
        let yellow = frac("ROZUM_GOV_YELLOW_FRAC", 0.20);
        let red = frac("ROZUM_GOV_RED_FRAC", 0.10).min(yellow);
        Self { yellow_free_frac: yellow, red_free_frac: red }
    }
}

/// Classify host memory pressure from `free`/`total` bytes. Unknown total (`0`) ⇒ `Red`
/// (assume the worst — never optimistic about safety). `free ≤ red_frac` ⇒ Red;
/// `free ≤ yellow_frac` ⇒ Yellow; else Green.
pub fn classify(free_bytes: u64, total_bytes: u64, t: &PressureThresholds) -> Pressure {
    if total_bytes == 0 {
        return Pressure::Red;
    }
    let free_frac = free_bytes as f64 / total_bytes as f64;
    if free_frac <= t.red_free_frac {
        Pressure::Red
    } else if free_frac <= t.yellow_free_frac {
        Pressure::Yellow
    } else {
        Pressure::Green
    }
}

/// What the governor prescribes for a pressure band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Green — new model loads may be admitted.
    Admit,
    /// Yellow — refuse new admissions; prewarm swap candidates; do not yet evict.
    HoldAdmissions,
    /// Red — evict the lowest-utility *idle* resident model immediately.
    EvictOne,
}

/// Map a pressure band to the governor's action.
pub fn action_for(p: Pressure) -> Action {
    match p {
        Pressure::Green => Action::Admit,
        Pressure::Yellow => Action::HoldAdmissions,
        Pressure::Red => Action::EvictOne,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;

    #[test]
    fn classify_bands_on_free_fraction() {
        let t = PressureThresholds::default(); // yellow 0.20, red 0.10
        let total = 36 * GB;
        // 50% free → green.
        assert_eq!(classify(18 * GB, total, &t), Pressure::Green);
        // 15% free → yellow (≤0.20, >0.10).
        assert_eq!(classify(5 * GB + GB / 2, total, &t), Pressure::Yellow);
        // exactly 20% free → yellow (boundary is inclusive).
        assert_eq!(classify((total as f64 * 0.20) as u64, total, &t), Pressure::Yellow);
        // 8% free → red.
        assert_eq!(classify(3 * GB, total, &t), Pressure::Red);
        // exactly 10% free → red (boundary inclusive).
        assert_eq!(classify((total as f64 * 0.10) as u64, total, &t), Pressure::Red);
    }

    #[test]
    fn unknown_total_is_red() {
        // Never optimistic when we can't measure.
        assert_eq!(classify(99 * GB, 0, &PressureThresholds::default()), Pressure::Red);
    }

    #[test]
    fn action_mapping() {
        assert_eq!(action_for(Pressure::Green), Action::Admit);
        assert_eq!(action_for(Pressure::Yellow), Action::HoldAdmissions);
        assert_eq!(action_for(Pressure::Red), Action::EvictOne);
    }

    #[test]
    fn env_thresholds_clamp_red_below_yellow() {
        // red is clamped ≤ yellow even if env sets it higher (defaults here: 0.10 ≤ 0.20).
        let t = PressureThresholds::from_env();
        assert!(t.red_free_frac <= t.yellow_free_frac);
    }
}
