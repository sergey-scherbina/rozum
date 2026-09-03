//! WHERE the allocations come from: a sampling allocator that captures a backtrace every Nth
//! allocation and aggregates by the innermost generated-code frame.
//!
//! Written because three plausible hypotheses in a row (countOpens' whole-state spread clone,
//! listAwareKeep's read-only by-value param, track's per-token arms) each shaved 5-7% and left
//! the doubling ratio at 3.8x untouched. Constant factors are not the quadratic, and reading
//! the code cannot tell you which site RUNS n^2 times on a given document. This can.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering::Relaxed};

static COUNT: AtomicU64 = AtomicU64::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static SITES: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const EVERY: u64 = 2048;

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

struct Sampling;
unsafe impl GlobalAlloc for Sampling {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if ARMED.load(Relaxed) && COUNT.fetch_add(1, Relaxed) % EVERY == 0 {
            // The capture itself allocates; without this guard it recurses forever.
            IN_HOOK.with(|g| {
                if !g.get() {
                    g.set(true);
                    record();
                    g.set(false);
                }
            });
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

fn record() {
    let bt = std::backtrace::Backtrace::force_capture().to_string();
    // Innermost frame naming a generated-code function: that is the site to attribute to.
    // Attribute to the innermost generated FUNCTION, not to the Clone impl that allocated:
    // "Vec<VmToken> as Clone" is the symptom every time and never says which caller ran n^2.
    let frame = bt
        .lines()
        .filter(|l| l.contains("ssc_program::"))
        .filter(|l| !l.contains("as core::clone::Clone") && !l.contains("drop_in_place"))
        .map(|l| {
            let r = l.rsplit("ssc_program::").next().unwrap_or(l);
            r.split(&[' ', '('][..]).next().unwrap_or(r).trim_end_matches(&[','][..]).to_string()
        })
        .find(|f| !f.is_empty())
        .unwrap_or_else(|| "<no generated frame>".into());
    if let Ok(mut g) = SITES.lock() {
        *g.get_or_insert_with(HashMap::new).entry(frame).or_insert(0) += 1;
    }
}

#[global_allocator]
static A: Sampling = Sampling;

fn main() {
    for p in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&p).unwrap();
        if let Ok(mut g) = SITES.lock() {
            *g = Some(HashMap::new());
        }
        COUNT.store(0, Relaxed);
        ARMED.store(true, Relaxed);
        let n = rozum_agent::rag_chunk::chunk_markdown(&p, &text).len();
        ARMED.store(false, Relaxed);
        std::hint::black_box(n);
        let total = COUNT.load(Relaxed);
        let mut rows: Vec<(String, u64)> = SITES
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default()
            .into_iter()
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        println!("\n=== {} — {} allocations, sampled 1/{} ===", p.rsplit('/').next().unwrap_or(&p), total, EVERY);
        for (site, n) in rows.into_iter().take(12) {
            println!("  {:>6.2}%  {n:>7}  {site}", n as f64 * 100.0 / (total as f64 / EVERY as f64));
        }
    }
}
