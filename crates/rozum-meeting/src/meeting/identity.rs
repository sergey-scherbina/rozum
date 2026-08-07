//! Stable, session-lifetime participant identity for daemon-hosted rooms.
//!
//! A participant gets an **opaque** `ParticipantId` (a UUID, decoupled from the
//! display name) and a friendly **handle** (`eager-otter`) minted once and unique
//! within the room. Continuity across reconnects is keyed on a `session_token`
//! the proxy holds for its lifetime — re-presenting the token rebinds the same
//! id and handle (no `#N` reshuffle). The binding is persisted in the room's
//! `roster.json`, so a daemon restart rebinds a live proxy's token.
//!
//! Additive: this module provides the primitives; wiring them into the room
//! model (replacing the old name+staleness reclaim) happens in a later phase.
//! See `docs/specs/agent-meetings-daemon.md`.

use std::collections::HashSet;
use std::path::Path;

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::participant::ParticipantId;

/// What a participant is here to DO, as opposed to what kind of client they are.
///
/// `RosterEntry.kind` is `mcp | human | bridge` — a transport fact. It cannot answer "who is
/// on-call", which is exactly what escalation needs to route an incident, so roles are a separate
/// axis rather than a widening of `kind`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Files incidents; the default stance of a human in a support room.
    Reporter,
    /// Carries work assigned to them.
    Assignee,
    /// The routing target when an incident escalates and no assignee is named.
    OnCall,
    /// Present and reading; never routed to.
    Observer,
    /// May change other participants' roles.
    Admin,
}

impl Role {
    /// Parse the wire spelling. Returns `None` rather than defaulting, because a typo silently
    /// becoming `Observer` is how someone stops being paged without anyone noticing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reporter" => Some(Self::Reporter),
            "assignee" => Some(Self::Assignee),
            "on_call" | "on-call" | "oncall" => Some(Self::OnCall),
            "observer" => Some(Self::Observer),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reporter => "reporter",
            Self::Assignee => "assignee",
            Self::OnCall => "on_call",
            Self::Observer => "observer",
            Self::Admin => "admin",
        }
    }

    /// Every spelling the CLI and REST surfaces accept, for help text and error messages.
    pub const ALL: [Role; 5] = [
        Role::Reporter,
        Role::Assignee,
        Role::OnCall,
        Role::Observer,
        Role::Admin,
    ];
}

/// A participant's durable record within a room's roster.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterEntry {
    /// Opaque `ParticipantId` value (a UUID).
    pub id: String,
    /// Friendly, unique-within-room handle, e.g. `eager-otter`.
    pub handle: String,
    /// The client's base name (e.g. `claude`), shown decorated with the handle.
    pub base_name: String,
    /// `"mcp" | "human" | "bridge"`.
    pub kind: String,
    pub project: Option<String>,
    /// The proxy's session token; `None` for the human/operator. The reconnect
    /// key.
    pub session_token: Option<String>,
    /// What this participant is here to do. A VECTOR because the states overlap in practice: the
    /// operator is on-call and the assignee of two incidents at the same time, and a single-valued
    /// field forces a lie the first time that happens.
    ///
    /// `default` is load-bearing — every `roster.json` written before this field existed must keep
    /// loading, and it reads as "no declared role", which is the status quo, rather than as a
    /// wrong one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
}

/// A room's participant roster, persisted as `roster.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Roster {
    pub participants: Vec<RosterEntry>,
}

impl Roster {
    /// Load from `path`; a missing or unreadable file yields an empty roster.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to `path` (atomic via tmp + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)
    }

    fn taken_handles(&self) -> HashSet<String> {
        self.participants.iter().map(|e| e.handle.clone()).collect()
    }

    /// Look up an entry by its session token.
    /// Give `handle` a role. Idempotent: granting a role twice is not an error, because the
    /// caller is usually a human typing the same command again after a restart.
    ///
    /// Returns `false` when no such handle is in the room — a silent no-op here would let a typo
    /// look like a successful grant, and nobody checks a roster they believe they just changed.
    pub fn grant(&mut self, handle: &str, role: Role) -> bool {
        match self.participants.iter_mut().find(|e| e.handle == handle) {
            Some(e) => {
                if !e.roles.contains(&role) {
                    e.roles.push(role);
                    e.roles.sort();
                }
                true
            }
            None => false,
        }
    }

    /// Take a role away. Also idempotent, and also `false` for an unknown handle.
    pub fn revoke(&mut self, handle: &str, role: Role) -> bool {
        match self.participants.iter_mut().find(|e| e.handle == handle) {
            Some(e) => {
                e.roles.retain(|r| *r != role);
                true
            }
            None => false,
        }
    }

    /// Everyone holding `role`, in roster order.
    ///
    /// This is what escalation needs: `meeting.escalate` takes a free-text `to` today and cannot
    /// answer "who is on-call". Returning a LIST rather than one participant is deliberate — a room
    /// with two people on call is a normal state, and picking one of them silently is a policy
    /// decision that belongs to the escalation code, not to the roster.
    pub fn with_role(&self, role: Role) -> Vec<&RosterEntry> {
        self.participants
            .iter()
            .filter(|e| e.roles.contains(&role))
            .collect()
    }

    pub fn by_token(&self, token: &str) -> Option<&RosterEntry> {
        self.participants
            .iter()
            .find(|e| e.session_token.as_deref() == Some(token))
    }

    /// Resolve a participant by `session_token` — rebinding the same id+handle on
    /// reconnect — or mint a fresh opaque id + unique handle and record it.
    /// Returns `(id, handle, is_new)`.
    pub fn resolve_or_mint(
        &mut self,
        session_token: Option<&str>,
        base_name: &str,
        kind: &str,
        project: Option<&str>,
    ) -> (ParticipantId, String, bool) {
        if let Some(tok) = session_token {
            if let Some(e) = self.by_token(tok) {
                return (ParticipantId::new(e.id.clone()), e.handle.clone(), false);
            }
        }
        let id = Uuid::new_v4().simple().to_string();
        let handle = mint_handle(&self.taken_handles());
        self.participants.push(RosterEntry {
            id: id.clone(),
            handle: handle.clone(),
            base_name: base_name.to_owned(),
            kind: kind.to_owned(),
            project: project.map(|p| p.to_owned()),
            session_token: session_token.map(|t| t.to_owned()),
            // A new participant declares no role. Anything else would be guessing on their behalf,
            // and guessing `Observer` is how someone silently stops being paged.
            roles: Vec::new(),
        });
        (ParticipantId::new(id), handle, true)
    }
}

/// How a participant is shown: by its **identity name** (the human's account / the agent's own
/// name). The minted `handle` stays internal (uniqueness, plus a distinct label for an un-named
/// client) but is no longer mashed into the display — so a human never looks like an agent
/// (`Sergiy`, not `Sergiy · plucky-fox`) and a named agent shows its own name. Falls back to the
/// handle only when there is no base name. See `docs/specs/meeting-identity-roster.md`.
pub fn display_name(base_name: &str, handle: &str) -> String {
    if base_name.trim().is_empty() {
        handle.to_string()
    } else {
        base_name.to_string()
    }
}

const ADJECTIVES: &[&str] = &[
    "eager", "calm", "brave", "bright", "swift", "keen", "wise", "bold", "merry", "nimble",
    "quiet", "lucky", "sunny", "clever", "gentle", "jolly", "spry", "witty", "mellow", "plucky",
];

const ANIMALS: &[&str] = &[
    "otter", "lynx", "heron", "fox", "wren", "marten", "ibex", "tapir", "civet", "gecko", "raven",
    "stoat", "shrew", "vole", "newt", "quail", "finch", "crane", "sable", "perch",
];

/// Mint a friendly handle (`adjective-animal`) not already in `taken`. Falls
/// back to a numeric suffix only if the (400-combo) space is exhausted.
pub fn mint_handle(taken: &HashSet<String>) -> String {
    let mut rng = rand::thread_rng();
    for _ in 0..64 {
        let adj = ADJECTIVES.choose(&mut rng).unwrap_or(&"eager");
        let animal = ANIMALS.choose(&mut rng).unwrap_or(&"otter");
        let candidate = format!("{adj}-{animal}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    // Space effectively exhausted — deterministic fallback.
    for adj in ADJECTIVES {
        for animal in ANIMALS {
            for i in 2.. {
                let candidate = format!("{adj}-{animal}-{i}");
                if !taken.contains(&candidate) {
                    return candidate;
                }
                if i > taken.len() + 2 {
                    break;
                }
            }
        }
    }
    format!("agent-{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn same_token_rebinds_same_id_and_handle() {
        let mut r = Roster::default();
        let (id1, h1, new1) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        assert!(new1);
        // Reconnect with the same token → same identity, not new.
        let (id2, h2, new2) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        assert!(!new2);
        assert_eq!(id1, id2);
        assert_eq!(h1, h2);
        assert_eq!(r.participants.len(), 1, "no duplicate entry on reconnect");
    }

    #[test]
    fn different_tokens_get_different_handles_and_ids() {
        let mut r = Roster::default();
        let (id1, h1, _) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", None);
        let (id2, h2, _) = r.resolve_or_mint(Some("tok-B"), "claude", "mcp", None);
        assert_ne!(id1, id2);
        assert_ne!(h1, h2, "two same-named agents must not collide on handle");
        assert_eq!(r.participants.len(), 2);
    }

    #[test]
    fn ids_are_opaque_uuids_no_hash_suffix() {
        let mut r = Roster::default();
        let (id, handle, _) = r.resolve_or_mint(Some("t"), "claude", "mcp", None);
        assert!(!id.0.contains('#'), "no positional #N suffix");
        assert!(!handle.contains('#'));
        assert_eq!(id.0.len(), 32, "uuid simple form");
        // Display is the identity name; the minted handle stays internal (not mashed in).
        assert_eq!(display_name("claude", &handle), "claude");
        // An un-named client still gets a distinct label from its handle.
        assert_eq!(display_name("", &handle), handle);
    }

    #[test]
    fn roster_round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.json");
        let mut r = Roster::default();
        r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        r.resolve_or_mint(None, "operator", "human", None);
        r.save(&path).unwrap();

        let loaded = Roster::load(&path);
        assert_eq!(loaded.participants.len(), 2);
        // The token binding survives the reload (daemon-restart rebind).
        let e = loaded.by_token("tok-A").unwrap();
        assert_eq!(e.base_name, "claude");
        assert_eq!(e.kind, "mcp");
    }

    #[test]
    fn minted_handle_avoids_taken() {
        // Pre-fill many handles; minted one must still avoid them all.
        let mut taken = HashSet::new();
        for adj in &ADJECTIVES[..10] {
            for animal in &ANIMALS[..10] {
                taken.insert(format!("{adj}-{animal}"));
            }
        }
        let h = mint_handle(&taken);
        assert!(!taken.contains(&h));
    }

    #[test]
    fn human_without_token_mints_each_time() {
        let mut r = Roster::default();
        // No token → no reconnect key → a fresh identity each call.
        let (id1, _, new1) = r.resolve_or_mint(None, "operator", "human", None);
        let (id2, _, new2) = r.resolve_or_mint(None, "operator", "human", None);
        assert!(new1 && new2);
        assert_ne!(id1, id2);
    }
}

#[cfg(test)]
mod role_tests {
    use super::*;

    const PRE_ROLES_ROSTER: &str = include_str!("../../tests/fixtures/roster.pre-roles.json");
    const PRE_ROLES_META: &str = include_str!("../../tests/fixtures/meta.pre-roles.json");

    /// THE migration assertion, and the reason the fixtures exist: a binary that knows about roles
    /// must still read a roster written by one that did not. The bytes are the shape the operator's
    /// live daemon was writing, redacted — see `tests/fixtures/README.md`.
    #[test]
    fn a_roster_written_before_roles_existed_still_loads() {
        let roster: Roster = serde_json::from_str(PRE_ROLES_ROSTER).expect("pre-roles roster loads");
        assert_eq!(roster.participants.len(), 3);
        for e in &roster.participants {
            // Absent means "no declared role" — the status quo — never a guessed one.
            assert!(e.roles.is_empty(), "{} got a role from nowhere", e.handle);
        }
        // And the fields that were there before must survive the round trip unchanged.
        let human = &roster.participants[0];
        assert_eq!(human.kind, "human");
        assert!(human.session_token.is_some());
    }

    /// The other direction: an OLD binary reading what a new one writes. `skip_serializing_if`
    /// means a role-less entry serialises byte-identically to before, so the common case cannot
    /// break a reader that has never heard of the field.
    #[test]
    fn a_role_less_entry_serialises_without_the_field() {
        let roster: Roster = serde_json::from_str(PRE_ROLES_ROSTER).unwrap();
        let out = serde_json::to_string(&roster).unwrap();
        assert!(!out.contains("roles"), "{out}");
    }

    /// `meta.json` is untouched by this change; the fixture is here so that if someone later edits
    /// `Phase` (R2 in the spec) the failure lands in a test rather than on the operator's daemon.
    #[test]
    fn the_pre_change_room_meta_still_parses() {
        let v: serde_json::Value = serde_json::from_str(PRE_ROLES_META).unwrap();
        assert_eq!(v["phase"], "Active");
        assert!(v["created_at"].is_number());
    }

    #[test]
    fn granting_is_idempotent_and_an_unknown_handle_is_reported() {
        let mut roster: Roster = serde_json::from_str(PRE_ROLES_ROSTER).unwrap();
        let who = roster.participants[0].handle.clone();

        assert!(roster.grant(&who, Role::OnCall));
        assert!(roster.grant(&who, Role::OnCall), "a repeat grant is not an error");
        assert_eq!(roster.participants[0].roles, vec![Role::OnCall]);

        // A typo must NOT read as success: nobody re-checks a roster they believe they just changed.
        assert!(!roster.grant("no-such-handle", Role::OnCall));
        assert!(!roster.revoke("no-such-handle", Role::OnCall));
    }

    /// The case that made `roles` a vector: on-call AND assignee at once.
    #[test]
    fn a_participant_holds_several_roles_at_once() {
        let mut roster: Roster = serde_json::from_str(PRE_ROLES_ROSTER).unwrap();
        let who = roster.participants[0].handle.clone();
        roster.grant(&who, Role::OnCall);
        roster.grant(&who, Role::Assignee);

        assert_eq!(roster.with_role(Role::OnCall).len(), 1);
        assert_eq!(roster.with_role(Role::Assignee).len(), 1);

        roster.revoke(&who, Role::OnCall);
        assert!(roster.with_role(Role::OnCall).is_empty());
        assert_eq!(roster.with_role(Role::Assignee).len(), 1, "revoke took the wrong one");
    }

    /// Two people on call is a normal state, and the roster must not pick one.
    #[test]
    fn with_role_returns_everyone_not_a_winner() {
        let mut roster: Roster = serde_json::from_str(PRE_ROLES_ROSTER).unwrap();
        let (a, b) = (roster.participants[0].handle.clone(), roster.participants[1].handle.clone());
        roster.grant(&a, Role::OnCall);
        roster.grant(&b, Role::OnCall);
        assert_eq!(roster.with_role(Role::OnCall).len(), 2);
    }

    /// A typo must not become a role. `Observer` is the dangerous default: it looks harmless and
    /// silently stops someone being paged.
    #[test]
    fn an_unknown_spelling_is_rejected_rather_than_defaulted() {
        assert_eq!(Role::parse("on-call"), Some(Role::OnCall));
        assert_eq!(Role::parse("  OnCall "), Some(Role::OnCall));
        assert_eq!(Role::parse("on_call"), Some(Role::OnCall));
        assert_eq!(Role::parse("observor"), None);
        assert_eq!(Role::parse(""), None);
        for r in Role::ALL {
            assert_eq!(Role::parse(r.as_str()), Some(r), "{} does not round-trip", r.as_str());
        }
    }
}
