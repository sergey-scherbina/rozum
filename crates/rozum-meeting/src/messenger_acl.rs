//! Per-user access control for a messenger bridge: who may participate and with
//! what capabilities. The operator owns it and edits it LIVE from inside Telegram
//! (`/grant`, `/revoke`, `/members`); it is read by both the bridge (chat gating +
//! command handling) and the model participant (per-turn tool gating). Persisted
//! as JSON under the rozum state dir so both processes share one source of truth.
//! Spec: `docs/specs/messenger-access-control.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::meeting::store::rozum_state_dir;

/// What a user is allowed to do. The owner implicitly has all of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// Participate in the chat at all: messages are relayed to the model and it answers.
    #[serde(default)]
    pub chat: bool,
    /// The model may read/list files in the sandbox on this user's behalf.
    #[serde(default)]
    pub read: bool,
    /// The model may write files in the sandbox on this user's behalf.
    #[serde(default)]
    pub write: bool,
    /// The model may run shell commands (confined to the sandbox) on this user's behalf.
    #[serde(default)]
    pub shell: bool,
}

impl Caps {
    pub fn all() -> Self {
        Caps { chat: true, read: true, write: true, shell: true }
    }

    /// Parse capability tokens (`chat read write shell`, or `all` / `none`). Any
    /// unknown token is an error so a typo never silently grants nothing.
    pub fn parse_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Result<Caps, String> {
        let mut c = Caps::default();
        let mut any = false;
        for raw in tokens {
            let t = raw.trim().to_ascii_lowercase();
            if t.is_empty() {
                continue;
            }
            any = true;
            match t.as_str() {
                "all" => c = Caps::all(),
                "none" => c = Caps::default(),
                "chat" => c.chat = true,
                "read" | "r" => c.read = true,
                "write" | "w" => c.write = true,
                "shell" | "sh" | "exec" => c.shell = true,
                other => {
                    return Err(format!(
                        "неизвестное право '{other}' (chat|read|write|shell|all|none)"
                    ));
                }
            }
        }
        if !any {
            return Err("не указаны права (например: chat read write shell)".into());
        }
        Ok(c)
    }

    /// Compact human summary, e.g. `chat+read+write`.
    pub fn summary(&self) -> String {
        let mut v = Vec::new();
        if self.chat {
            v.push("chat");
        }
        if self.read {
            v.push("read");
        }
        if self.write {
            v.push("write");
        }
        if self.shell {
            v.push("shell");
        }
        if v.is_empty() { "—".into() } else { v.join("+") }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub caps: Caps,
}

/// The whole access list for one platform. `owner` has every capability and is the
/// only id allowed to run management commands.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Acl {
    #[serde(default)]
    pub owner: Option<i64>,
    /// Members keyed by numeric user id.
    #[serde(default)]
    pub members: BTreeMap<i64, Member>,
}

impl Acl {
    /// Canonical on-disk path for a platform's ACL (e.g. `telegram`).
    pub fn path(platform: &str) -> PathBuf {
        rozum_state_dir().join("messenger-acl").join(format!("{platform}.json"))
    }

    /// Load the ACL, or a fresh empty one if the file is missing or unreadable.
    /// A corrupt file is treated as empty rather than crashing the bridge — the
    /// operator can re-grant; access defaults to deny.
    pub fn load(path: &Path) -> Acl {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Acl::default(),
        }
    }

    /// Atomically persist (write temp + rename), creating the parent dir.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("ACL path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let encoded = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, encoded)?;
        std::fs::rename(&tmp, path)
    }

    pub fn is_owner(&self, id: i64) -> bool {
        self.owner == Some(id)
    }

    /// Effective capabilities for a user: everything for the owner, the stored
    /// caps for a member, nothing otherwise.
    pub fn caps_for(&self, id: i64) -> Caps {
        if self.is_owner(id) {
            return Caps::all();
        }
        self.members.get(&id).map(|m| m.caps).unwrap_or_default()
    }

    /// Set the owner if none is set yet; returns true if it changed.
    pub fn ensure_owner(&mut self, id: i64) -> bool {
        if self.owner.is_none() {
            self.owner = Some(id);
            true
        } else {
            false
        }
    }

    /// Add or update a member's name/caps. The owner is never demoted to a member.
    pub fn grant(&mut self, id: i64, name: &str, caps: Caps) {
        if self.is_owner(id) {
            return;
        }
        let m = self.members.entry(id).or_default();
        if !name.trim().is_empty() {
            m.name = name.trim().to_string();
        }
        m.caps = caps;
    }

    /// Remove a member. Returns true if one was present.
    pub fn revoke(&mut self, id: i64) -> bool {
        self.members.remove(&id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tokens_sets_bits_and_rejects_unknown() {
        let c = Caps::parse_tokens(["chat", "read", "write"]).unwrap();
        assert!(c.chat && c.read && c.write && !c.shell);
        assert_eq!(Caps::parse_tokens(["all"]).unwrap(), Caps::all());
        assert_eq!(Caps::parse_tokens(["none"]).unwrap(), Caps::default());
        assert!(Caps::parse_tokens(["bogus"]).is_err());
        assert!(Caps::parse_tokens([""]).is_err(), "empty means no caps given");
    }

    #[test]
    fn owner_has_all_caps_members_have_granted() {
        let mut acl = Acl::default();
        acl.ensure_owner(1);
        assert_eq!(acl.caps_for(1), Caps::all());
        acl.grant(2, "Bob", Caps::parse_tokens(["chat", "read"]).unwrap());
        let c = acl.caps_for(2);
        assert!(c.chat && c.read && !c.write && !c.shell);
        // unknown user → nothing
        assert_eq!(acl.caps_for(99), Caps::default());
        // owner cannot be shadowed by a member entry
        acl.grant(1, "x", Caps::default());
        assert_eq!(acl.caps_for(1), Caps::all());
    }

    #[test]
    fn revoke_removes_member() {
        let mut acl = Acl::default();
        acl.grant(5, "Ann", Caps::all());
        assert!(acl.revoke(5));
        assert!(!acl.revoke(5));
        assert_eq!(acl.caps_for(5), Caps::default());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        let mut acl = Acl::default();
        acl.ensure_owner(1711036782);
        acl.grant(42, "Bob", Caps::parse_tokens(["chat", "read", "shell"]).unwrap());
        acl.save(&path).unwrap();

        let back = Acl::load(&path);
        assert_eq!(back.owner, Some(1711036782));
        assert_eq!(back.caps_for(1711036782), Caps::all());
        let c = back.caps_for(42);
        assert!(c.chat && c.read && c.shell && !c.write);
        assert_eq!(back.members.get(&42).unwrap().name, "Bob");
    }

    #[test]
    fn missing_or_corrupt_file_is_empty_deny() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        assert_eq!(Acl::load(&path).owner, None);
        std::fs::write(&path, "not json").unwrap();
        let acl = Acl::load(&path);
        assert_eq!(acl.owner, None);
        assert_eq!(acl.caps_for(1), Caps::default());
    }
}
