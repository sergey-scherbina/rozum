//! Allocation accounting for the markdown chunker: COUNT and BYTES, separately.
//!
//! The two grow differently and the difference is the diagnosis. Count growing ~2x per input
//! doubling while bytes grow ~4x means the same number of clones, each one bigger — a whole
//! accumulator copied per line. Count growing ~4x means more clones instead. Guessing between
//! those two sent an earlier round of this campaign at the wrong cause.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        COUNT.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn main() {
    let mut prev: Option<(f64, f64, f64)> = None;
    for p in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&p).unwrap();
        COUNT.store(0, Relaxed);
        BYTES.store(0, Relaxed);
        let t = std::time::Instant::now();
        let n = rozum_agent::rag_chunk::chunk_markdown(&p, &text).len();
        let secs = t.elapsed().as_secs_f64();
        std::hint::black_box(n);
        let (c, b) = (COUNT.load(Relaxed) as f64, BYTES.load(Relaxed) as f64);
        let name = p.rsplit('/').next().unwrap_or(&p).to_string();
        match prev {
            None => println!(
                "{name:>12}  {secs:>8.3} s  allocs {c:>12.0}  bytes {:>10.1} MB",
                b / 1e6
            ),
            Some((pc, pb, ps)) => println!(
                "{name:>12}  {secs:>8.3} s (x{rs:.2})  allocs {c:>12.0} (x{rc:.2})  bytes {mb:>10.1} MB (x{rb:.2})",
                rs = secs / ps,
                rc = c / pc,
                mb = b / 1e6,
                rb = b / pb
            ),
        }
        prev = Some((c, b, secs));
    }
}
