//! The door between the console and the `.ssc` server that stands behind it.
//!
//! One shared secret, in one place, because three processes need to agree on it: the Rust console
//! proxying a request (`rozum-gateway`), the ScalaScript server deciding whether to answer, and
//! `doctor` probing that server directly. Two of those are Rust and this is what they share; the
//! third reads the same file, in the same order, from `clients/control/public-matrix.ssc`.
//!
//! It is a DOOR, not an authorisation: carrying it says "this came through the console" and nothing
//! about who is calling. That is sufficient, and the reason it is sufficient is measured rather than
//! assumed — `require_auth` injects a user id that no route handler in `control.rs` reads, so a
//! server behind the gate never needs the principal. `docs/specs/ucc-ssc-session.md`.

use std::path::{Path, PathBuf};

/// Sent by the console's proxy, checked by the `.ssc` server.
pub const HEADER: &str = "x-rozum-ucc-door";

/// `~/.rozum/secrets/ucc-ssc-door` — the same directory, ownership and 600 the messenger tokens and
/// the meeting web secret use.
pub fn secret_path() -> PathBuf {
    rozum_paths::home_dir()
        .unwrap_or_else(rozum_paths::temp_dir)
        .join(".rozum")
        .join("secrets")
        .join("ucc-ssc-door")
}

/// Environment first, file second, so it does not matter who started the process — the same
/// resolution `ROZUM_WEB_SECRET` uses for the meeting REST server.
pub fn secret() -> Option<String> {
    resolve(std::env::var("ROZUM_UCC_SSC_SECRET").ok(), &secret_path())
}

/// Pure core, so the precedence is testable without the process environment or a real home.
///
/// Empty and whitespace-only are NOT a secret. A file created by `touch` must leave the door OPEN
/// rather than close it against every caller including the console itself — the failure that would
/// otherwise arrive as "the console 403s and nothing in the logs says why".
pub fn resolve(from_env: Option<String>, path: &Path) -> Option<String> {
    let clean = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(v) = from_env.and_then(clean) {
        return Some(v);
    }
    std::fs::read_to_string(path).ok().and_then(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_prefers_the_environment_and_ignores_an_empty_one() {
        let dir = std::env::temp_dir().join(format!("rozum-door-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ucc-ssc-door");
        std::fs::write(&file, "  from-file\n").unwrap();

        // Environment wins — that is what lets a launchd job carry it without a file.
        assert_eq!(resolve(Some("from-env".into()), &file).as_deref(), Some("from-env"));
        // File fallback, trimmed: a secret written with `echo` carries a newline.
        assert_eq!(resolve(None, &file).as_deref(), Some("from-file"));
        // An EMPTY variable is not a secret. Treating it as one would close the door on the console
        // itself the first time a job template sets the name with no value.
        assert_eq!(resolve(Some("   ".into()), &file).as_deref(), Some("from-file"));

        // Nothing anywhere → None → the proxy sends no header and the `.ssc` half stays open, which
        // is what keeps a host that was never given a secret serving exactly what it serves now.
        std::fs::write(&file, "\n  \n").unwrap();
        assert_eq!(resolve(None, &file), None);
        assert_eq!(resolve(None, &dir.join("nope")), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
