use std::time::Instant;
fn main() {
    for p in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&p).unwrap();
        let na = text.chars().filter(|c| !c.is_ascii()).count() as f64
            / text.chars().count().max(1) as f64 * 100.0;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            let n = rozum_agent::rag_chunk::chunk_markdown(&p, &text).len();
            let e = t.elapsed().as_secs_f64();
            if e < best { best = e; }
            std::hint::black_box(n);
        }
        println!("{:>52}  {:>7} KB  {:>5.1}% non-ASCII  {:>8.3} s",
            p, text.len() / 1024, na, best);
    }
}
