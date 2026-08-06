//! The 0600 JSON store for sensitive control state — users, roles, invites, view tokens.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Four functions with one job between
//! them: read a JSON list, write one back atomically with the directory at 0700 and the file at
//! 0600, and mint a token. They came out first so the modules that follow depend on this rather
//! than on the 3600-line file they all used to share.
//!
//! The atomicity is not decoration: these files hold the console's authorisation state, and a
//! half-written roles file is a half-open door.

use std::path::PathBuf;

use serde::Serialize;
use uuid::Uuid;

/// Atomically write `bytes` to `path` (tmp + rename) with 0600 perms so on-disk secrets (session
/// tokens, WebAuthn credentials, RBAC state, view tokens) are not world-readable. Best-effort like the
/// callers it replaces.
pub(crate) fn atomic_write_private(path: &std::path::Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, path);
    }
}

pub(crate) fn json_load<T: serde::de::DeserializeOwned>(path: Option<PathBuf>) -> Vec<T> {
    path.and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub(crate) fn json_save_rbac<T: Serialize + ?Sized>(path: Option<PathBuf>, val: &T) {
    let Some(p) = path else { return };
    if let Ok(b) = serde_json::to_vec_pretty(val) {
        atomic_write_private(&p, &b); // 0600 — users/roles/invites/view-tokens are sensitive
    }
}

pub(crate) fn rand_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
