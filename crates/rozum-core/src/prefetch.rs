//! Page-cache prefetch for fast model swap (smmr-C, `docs/specs/safe-multi-model-residency.md`).
//!
//! When two models can't co-reside (the residency ledger says *oversubscribed*), rozum
//! **swaps** rather than co-loads: unload the old model → let the GPU settle → load the
//! new one — *never both resident at once* (that simultaneous footprint is the OOM that
//! reboots the host). The slow part of a cold load is reading the weights off disk. This
//! module warms a model's files into the **OS page cache** so the subsequent load reads
//! from RAM, letting the swap overlap "fetch the next model" with the *old* model's drain.
//!
//! Why this is budget-free: page cache is **reclaimable** and is **not** GPU residency, so
//! it does not count against the RAM budget — the OS evicts it under pressure. Warming a
//! model therefore never contributes to the overcommit the residency gate guards against.
//!
//! v1 uses a portable sequential `read()` (no `unsafe`, no platform `madvise`/`fadvise`):
//! the read populates the page cache as a side effect. A future optimization can switch to
//! `posix_fadvise(WILLNEED)` / `madvise(MADV_WILLNEED)` to skip the userspace copy.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Bytes per read — large enough to amortize syscalls, small enough to bound memory and
/// to check `cancel` often.
const CHUNK: usize = 4 * 1024 * 1024;

/// Warm every regular file directly under `dir` (non-recursive — model dirs are flat:
/// `*.safetensors` + tokenizer/config) into the OS page cache, returning the total bytes
/// read. **Best-effort:** per-file/dir errors are skipped — a warm is an optimization,
/// never required for correctness, so it must never fail a swap. Checks `cancel` between
/// chunks so the caller can abort the moment the old model finishes draining (the warm is
/// only worth doing *while* there's idle time to overlap).
pub fn warm_dir_page_cache(dir: &Path, cancel: &AtomicBool) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    for entry in entries.flatten() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            total += warm_file(&entry.path(), &mut buf, cancel);
        }
    }
    total
}

/// Sequentially read one file so its pages enter the OS cache; bytes are discarded.
fn warm_file(path: &Path, buf: &mut [u8], cancel: &AtomicBool) -> u64 {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return 0;
    };
    let mut warmed = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match f.read(buf) {
            Ok(0) => break,
            Ok(n) => warmed += n as u64,
            Err(_) => break,
        }
    }
    warmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warms_all_regular_files_and_reports_bytes() {
        let dir = std::env::temp_dir().join(format!("rozum-prefetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.safetensors"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("b.safetensors"), vec![2u8; 2000]).unwrap();
        std::fs::write(dir.join("config.json"), vec![3u8; 50]).unwrap();
        // A subdirectory is NOT recursed into.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/ignored"), vec![4u8; 9999]).unwrap();

        let cancel = AtomicBool::new(false);
        let warmed = warm_dir_page_cache(&dir, &cancel);
        assert_eq!(warmed, 1000 + 2000 + 50, "sums regular files, skips subdirs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_aborts_warming() {
        let dir = std::env::temp_dir().join(format!("rozum-prefetch-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.safetensors"), vec![1u8; 4096]).unwrap();

        let cancel = AtomicBool::new(true); // already cancelled
        let warmed = warm_dir_page_cache(&dir, &cancel);
        assert_eq!(warmed, 0, "a pre-cancelled warm reads nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_noop() {
        let cancel = AtomicBool::new(false);
        let warmed = warm_dir_page_cache(Path::new("/no/such/rozum/dir"), &cancel);
        assert_eq!(warmed, 0);
    }
}
