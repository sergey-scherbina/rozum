//! A content digest of the chunker's output, so two builds of the generated crate can be
//! compared for EQUALITY rather than for "it still runs".
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
fn main() {
    for p in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&p).unwrap();
        let chunks = rozum_agent::rag_chunk::chunk_markdown(&p, &text);
        let mut h = DefaultHasher::new();
        for c in &chunks {
            c.id.hash(&mut h);
            c.text.hash(&mut h);
        }
        println!("{:>16}  {:>5} chunks  digest {:016x}", p.rsplit('/').next().unwrap_or(&p), chunks.len(), h.finish());
    }
}
