//! The local human's **stable identity** — the local-default `Principal` for one operator.
//!
//! The daemon binds a `session_token` to one participant (handle). The TUI and `meetings post`
//! used to mint a fresh random token each launch, so the human showed up as a new
//! adjective-animal every time. This persists a stable token + display in
//! `$XDG_CONFIG_HOME/rozum/identity.json` (`~/.config/rozum/identity.json`), so all of this
//! machine's human clients map to **one** participant across launches — the first, zero-config
//! rung of the `Principal` model in `docs/specs/agent-meeting-coordination.md`. (Auth, multiple
//! humans, and remote are later resolvers on top of this seam.)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A persisted local identity: a stable reconnect token + a friendly display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalIdentity {
    pub token: String,
    pub display: String,
}

/// `$XDG_CONFIG_HOME/rozum` (or `~/.config/rozum`).
fn config_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("rozum"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/rozum"))
}

/// Path to `identity.json`.
pub fn identity_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("identity.json"))
}

/// Load the stable identity, minting + persisting one on first use (token = a fresh uuid,
/// display = `$USER`). Never fails — a write error just means a non-persisted (still stable
/// for this process) identity.
pub fn load_or_create() -> LocalIdentity {
    from_file(identity_path().as_deref())
}

/// The path-injectable core of [`load_or_create`] (so it's testable without mutating the
/// process-global `$XDG_CONFIG_HOME`).
fn from_file(path: Option<&std::path::Path>) -> LocalIdentity {
    if let Some(p) = path {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(id) = serde_json::from_str::<LocalIdentity>(&s) {
                if !id.token.trim().is_empty() {
                    return id;
                }
            }
        }
    }
    let id = LocalIdentity {
        token: uuid::Uuid::new_v4().simple().to_string(),
        display: std::env::var("USER")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "human".into()),
    };
    if let Some(p) = path {
        let _ = write_to(p, &id);
    }
    id
}

/// Persist the identity (creating the config dir).
pub fn save(id: &LocalIdentity) -> Result<(), String> {
    let path = identity_path().ok_or("no config dir ($XDG_CONFIG_HOME / $HOME unset)")?;
    write_to(&path, id)
}

fn write_to(path: &std::path::Path, id: &LocalIdentity) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }
    let mut body = serde_json::to_string_pretty(id).map_err(|e| e.to_string())?;
    body.push('\n');
    std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Set the display name (preserving the stable token), persist, and return the updated identity.
pub fn set_display(name: &str) -> Result<LocalIdentity, String> {
    let mut id = load_or_create();
    id.display = name.trim().to_string();
    save(&id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_rename_keeps_the_token() {
        // Path-injected (no global env mutation → safe under parallel tests).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");

        let a = from_file(Some(&path));
        let b = from_file(Some(&path)); // re-read the persisted file
        assert_eq!(a.token, b.token, "token is stable across reads (persisted)");
        assert!(!a.token.is_empty());

        // Rename keeps the stable token.
        let renamed = LocalIdentity { token: a.token.clone(), display: "Sergiy".into() };
        write_to(&path, &renamed).unwrap();
        let c = from_file(Some(&path));
        assert_eq!(c.display, "Sergiy");
        assert_eq!(c.token, a.token, "rename preserves the token");
    }
}
