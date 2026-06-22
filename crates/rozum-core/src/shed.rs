//! Memory-pressure watchdog: graceful shedding before the jetsam cascade.
//!
//! The residency gate ([`crate::share::acquire_residency`]) stops overcommit at **load
//! time**. But host memory can still drift toward the OS jetsam ladder *at runtime* — a
//! model's KV cache grows on a long context, and the `gguf`/`mistralrs` paths are not
//! cap-enforced — and that drift is what rebooted the Mac (BUG-003, vm-compressor →
//! jetsam → watchdog panic). This is the runtime-drift half of the fix: each gateway
//! watches the OS's own memory-pressure level and, when the host is under real pressure
//! and this gateway is **idle**, unloads its **own** model (it lazily reloads on the
//! next request). A reboot becomes graceful degradation.
//!
//! It keys on the OS pressure level — the same signal jetsam uses — not a homemade
//! "available bytes" estimate (the kernel computes availability far better than we can).
//! Fail-safe: on any read failure it reports `Normal`, so the watchdog never sheds
//! spuriously.

/// The OS memory-pressure level — the jetsam ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureLevel {
    Normal,
    Warn,
    Critical,
}

impl PressureLevel {
    /// Stable lowercase label for `/stats` / logs / UIs.
    pub fn as_str(self) -> &'static str {
        match self {
            PressureLevel::Normal => "normal",
            PressureLevel::Warn => "warn",
            PressureLevel::Critical => "critical",
        }
    }
}

/// Read the OS memory-pressure level. macOS: `kern.memorystatus_vm_pressure_level`
/// (`1` normal, `2` warn, `4` critical — the value jetsam acts on). Shells `sysctl`
/// for consistency with [`crate::concurrency::total_ram_bytes`] (no `libc` dep).
/// `Normal` on non-macOS or any failure.
pub fn read_host_pressure() -> PressureLevel {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "kern.memorystatus_vm_pressure_level"])
            .output()
        {
            match String::from_utf8_lossy(&out.stdout).trim().parse::<i32>() {
                Ok(4) => return PressureLevel::Critical,
                Ok(2) => return PressureLevel::Warn,
                _ => {}
            }
        }
    }
    PressureLevel::Normal
}

/// What the shed decision sees: the host pressure plus this gateway's own state.
#[derive(Clone, Copy, Debug)]
pub struct ShedInputs {
    pub pressure: PressureLevel,
    /// In-flight requests right now — never shed a model that is actively serving.
    pub inflight: u64,
    /// Seconds since this gateway last served a request.
    pub idle_secs: u64,
}

/// Tunable policy.
#[derive(Clone, Copy, Debug)]
pub struct ShedPolicy {
    /// Don't shed a model that served within this window (avoid thrashing a model that
    /// is between requests). `ROZUM_GATEWAY_SHED_MIN_IDLE_SECS`, default 30.
    pub min_idle_secs: u64,
    /// Shed at `Warn` too (`true`), or only at `Critical` (`false`).
    /// `ROZUM_GATEWAY_SHED_ON_WARN`, default `false` — Critical-only by default so the
    /// watchdog only acts at the OS's pre-jetsam rung and never disrupts a healthy
    /// single-model run; set `1` for earlier, more-aggressive shedding.
    pub shed_on_warn: bool,
    /// Master switch. `ROZUM_GATEWAY_SHED=0` disables the watchdog entirely.
    pub enabled: bool,
}

impl ShedPolicy {
    pub fn from_env() -> Self {
        let u64env = |k: &str, d: u64| {
            std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
        };
        let boolenv = |k: &str, d: bool| {
            std::env::var(k)
                .ok()
                .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
                .unwrap_or(d)
        };
        Self {
            min_idle_secs: u64env("ROZUM_GATEWAY_SHED_MIN_IDLE_SECS", 30),
            shed_on_warn: boolenv("ROZUM_GATEWAY_SHED_ON_WARN", false),
            enabled: boolenv("ROZUM_GATEWAY_SHED", true),
        }
    }
}

/// Should THIS gateway unload its own resident model now, to relieve host memory
/// pressure? Conservative — never interrupts in-flight work, only sheds an idle model
/// under genuine OS pressure.
pub fn should_shed(i: &ShedInputs, p: &ShedPolicy) -> bool {
    if !p.enabled {
        return false;
    }
    if i.inflight > 0 {
        return false; // never interrupt active serving
    }
    if i.idle_secs < p.min_idle_secs {
        return false; // not idle long enough
    }
    match i.pressure {
        PressureLevel::Critical => true,
        PressureLevel::Warn => p.shed_on_warn,
        PressureLevel::Normal => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(pressure: PressureLevel, inflight: u64, idle_secs: u64) -> ShedInputs {
        ShedInputs { pressure, inflight, idle_secs }
    }
    fn policy() -> ShedPolicy {
        ShedPolicy { min_idle_secs: 30, shed_on_warn: true, enabled: true }
    }

    #[test]
    fn never_sheds_while_serving_even_under_critical() {
        assert!(!should_shed(&inputs(PressureLevel::Critical, 1, 999), &policy()));
    }

    #[test]
    fn never_sheds_a_recently_active_model() {
        assert!(!should_shed(&inputs(PressureLevel::Critical, 0, 5), &policy()));
    }

    #[test]
    fn sheds_idle_model_under_critical() {
        assert!(should_shed(&inputs(PressureLevel::Critical, 0, 60), &policy()));
    }

    #[test]
    fn warn_sheds_only_when_policy_allows() {
        let idle = inputs(PressureLevel::Warn, 0, 60);
        assert!(should_shed(&idle, &policy()));
        let no_warn = ShedPolicy { shed_on_warn: false, ..policy() };
        assert!(!should_shed(&idle, &no_warn));
    }

    #[test]
    fn never_sheds_under_normal_pressure() {
        assert!(!should_shed(&inputs(PressureLevel::Normal, 0, 9999), &policy()));
    }

    #[test]
    fn master_switch_disables() {
        let off = ShedPolicy { enabled: false, ..policy() };
        assert!(!should_shed(&inputs(PressureLevel::Critical, 0, 9999), &off));
    }

    #[test]
    fn reader_never_panics_and_is_sane() {
        // On the macOS test host this returns a real level; elsewhere Normal. Either
        // way it must not panic and must be one of the three.
        let p = read_host_pressure();
        assert!(matches!(
            p,
            PressureLevel::Normal | PressureLevel::Warn | PressureLevel::Critical
        ));
    }
}
