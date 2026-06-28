//! Disk-backed transcript store for daemon-hosted meeting rooms.
//!
//! Canonical storage is an append-only log split into **daily files**
//! (`YYYY-MM-DD.jsonl`) under a room directory — for a project room,
//! `<project>/.rozum/room/`. A message's address is `(date, n)` where `n` is the
//! 0-based index within that day's file, reset to 0 each day. The daemon is the
//! single writer (`TranscriptWriter`); local clients tail the day files directly
//! (`TranscriptReader`). See `docs/specs/agent-meetings-daemon.md`.
//!
//! This module is pure storage: no async, no MCP, no model code. It is wired
//! into the room model in a later phase (P2); on its own it is additive.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};

// ── Dates ────────────────────────────────────────────────────────────────────

/// Today's local calendar date as `YYYY-MM-DD`.
pub fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Local calendar date of a unix timestamp as `YYYY-MM-DD` (falls back to
/// today on an out-of-range timestamp).
pub fn date_of_ts(ts: u64) -> String {
    match Local.timestamp_opt(ts as i64, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => today(),
    }
}

// ── Records ──────────────────────────────────────────────────────────────────

/// One stored message. Self-describing: `date` is its day file and `n` is its
/// line within that file (both redundant with location, kept so a line stands
/// alone for grep and the future REST read).
/// Message kind (P1, spec `meetings-incident-platform.md`). Drives rendering + filtering. `Note` is the
/// default so old plain lines (no `kind`) read as notes and serialize WITHOUT the field (byte-identical).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MsgKind {
    #[default]
    Note,
    Question,
    Event,
    Alert,
    Resolution,
}
impl MsgKind {
    fn is_note(&self) -> bool {
        matches!(self, MsgKind::Note)
    }
    /// Parse a kind name (case-insensitive); `None` if unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "note" => Self::Note,
            "question" => Self::Question,
            "event" => Self::Event,
            "alert" => Self::Alert,
            "resolution" => Self::Resolution,
            _ => return None,
        })
    }
    /// Short uppercase badge label for display; `None` for a plain note (no badge).
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::Note => None,
            Self::Question => Some("ASK"),
            Self::Event => Some("EVENT"),
            Self::Alert => Some("ALERT"),
            Self::Resolution => Some("RESOLVED"),
        }
    }
}

/// Incident severity (the jetsam ladder of support).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}
impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "info" => Self::Info,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "critical" => Self::Critical,
            _ => return None,
        })
    }
    /// Short label for display badges.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "med",
            Self::High => "HIGH",
            Self::Critical => "CRIT",
        }
    }
    /// Ordinal for `>=` filtering (info=0 … critical=4) — "show high and above".
    pub fn rank(&self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Critical => 4,
        }
    }
}

/// Per-message support status.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MsgStatus {
    Open,
    Acknowledged,
    Resolved,
    Closed,
}

/// Structured support metadata on a message — all optional; absent when empty so plain messages stay
/// byte-identical.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MsgMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MsgStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}
impl MsgMeta {
    fn is_empty(&self) -> bool {
        self.severity.is_none()
            && self.status.is_none()
            && self.assignee.is_none()
            && self.tags.is_empty()
            && self.links.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StoredTurn {
    pub date: String,
    pub n: u64,
    pub participant_id: String,
    pub display_name: String,
    pub content: String,
    pub ts: u64,
    // P1 message metadata — all `#[serde(default, skip_serializing_if=…)]`, so a plain message (no
    // metadata) serializes to EXACTLY the v1 JSON (no new keys) and an old line reads with these defaults.
    #[serde(default, skip_serializing_if = "MsgKind::is_note")]
    pub kind: MsgKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "MsgMeta::is_empty")]
    pub meta: MsgMeta,
}
impl StoredTurn {
    /// The stable message id — derived `<date>/<n>` (already unique per room). Not stored.
    pub fn id(&self) -> String {
        format!("{}/{}", self.date, self.n)
    }

    /// A compact one-line support badge (e.g. `[ALERT CRIT ⤷2026-06-28/3 #db]`), or `None` for a
    /// plain note carrying no metadata/thread. Shared by the CLI read and the TUI render so both
    /// surfaces show the same incident signal.
    pub fn badge(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(k) = self.kind.label() {
            parts.push(k.to_string());
        }
        if let Some(sev) = self.meta.severity {
            parts.push(sev.label().to_string());
        }
        if let Some(t) = &self.thread_id {
            parts.push(format!("⤷{t}"));
        }
        for tag in &self.meta.tags {
            parts.push(format!("#{tag}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("[{}]", parts.join(" ")))
        }
    }
}

/// Support metadata supplied when posting a message (P1b write API). `Default` = a plain message.
#[derive(Clone, Debug, Default)]
pub struct PostMeta {
    pub kind: MsgKind,
    pub thread_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub meta: MsgMeta,
}

/// A thread's role (P2): a topic discussion, or a tracked INCIDENT with a lifecycle.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    #[default]
    Topic,
    Incident,
}
impl ThreadKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "topic" => Self::Topic,
            "incident" => Self::Incident,
            _ => return None,
        })
    }
}

/// Incident lifecycle — the resolving state machine (spec § resolving).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    #[default]
    Open,
    Triaging,
    Escalated,
    Resolved,
    Closed,
}
impl ThreadState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Triaging => "triaging",
            Self::Escalated => "escalated",
            Self::Resolved => "resolved",
            Self::Closed => "closed",
        }
    }
    /// A resolved/closed thread is terminal (for metrics: time-to-resolve).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Resolved | Self::Closed)
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "open" => Self::Open,
            "triaging" => Self::Triaging,
            "escalated" => Self::Escalated,
            "resolved" => Self::Resolved,
            "closed" => Self::Closed,
            _ => return None,
        })
    }
}

/// A thread = an incident/topic (P2). Membership (which messages) is DERIVED from messages' `thread_id`;
/// this record holds the thread's own metadata (state/owner/severity — the part not derivable). Stored in
/// `threads.json` (a `{id → Thread}` map), rebuildable in the membership sense from the daily lines.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: ThreadKind,
    #[serde(default)]
    pub state: ThreadState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// Pinned message ids (`<date>/<n>`) — the incident's key messages (current status / root cause) a
    /// responder should see first. Back-compat: absent/empty on old threads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<String>,
    pub created_ts: u64,
    pub updated_ts: u64,
}

/// Per-day counts, the body of `index.json`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayStat {
    pub count: u64,
    pub bytes: u64,
}

/// `index.json` — a rebuildable accelerator: the days list + per-day totals.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Index {
    pub days: BTreeMap<String, DayStat>,
}

/// Room role (P3): a plain chat (today), a support intake queue, or a room scoped to one incident.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomKind {
    #[default]
    Chat,
    Queue,
    Incident,
}
impl RoomKind {
    fn is_chat(&self) -> bool {
        matches!(self, RoomKind::Chat)
    }
}

/// A room member's role.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    #[default]
    Observer,
    Reporter,
    Assignee,
    Oncall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub handle: String,
    #[serde(default)]
    pub role: MemberRole,
}

/// `meta.json` — small room metadata. `budget_chars` is the running total so a
/// reopen restores the budget without re-reading every day file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub topic: String,
    pub project: Option<PathBuf>,
    pub phase: String,
    pub created_at: u64,
    pub budget_chars: u64,
    // P3 room kind + members (serde-default + skip → plain `chat` rooms' meta.json is unchanged).
    #[serde(default, skip_serializing_if = "RoomKind::is_chat")]
    pub kind: RoomKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Member>,
}

/// What the writer publishes on each append; clients read up to `end_offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighWater {
    pub date: String,
    /// Number of messages in `date` so far (the next `n` to be assigned).
    pub n: u64,
    /// Byte length of the `date` day file.
    pub end_offset: u64,
}

// ── Paths ────────────────────────────────────────────────────────────────────

/// On-disk location of one room.
#[derive(Clone, Debug)]
pub struct RoomPaths {
    /// The `room/` directory holding the day files + meta/index/roster.
    pub root: PathBuf,
    /// For a project room, the `.rozum/` dir whose `.gitignore` we own; `None`
    /// for an ad-hoc room living under the state dir (nothing to gitignore).
    gitignore_dir: Option<PathBuf>,
}

impl RoomPaths {
    /// `<project>/.rozum/room/`.
    pub fn for_project(project_dir: &Path) -> Self {
        let rozum = project_dir.join(".rozum");
        Self {
            root: rozum.join("room"),
            gitignore_dir: Some(rozum),
        }
    }

    /// Ad-hoc room under an explicit state dir: `<state_dir>/rooms/<name>/`.
    pub fn ad_hoc_in(state_dir: &Path, name: &str) -> Self {
        Self {
            root: state_dir.join("rooms").join(name),
            gitignore_dir: None,
        }
    }

    /// Ad-hoc room under the real rozum state dir.
    pub fn ad_hoc(name: &str) -> Self {
        Self::ad_hoc_in(&rozum_state_dir(), name)
    }

    /// Reconstruct paths from a known `root` (e.g. a registry entry) without a
    /// gitignore parent — used when reopening a room by its recorded location.
    pub fn raw(root: PathBuf) -> Self {
        Self {
            root,
            gitignore_dir: None,
        }
    }

    pub fn day_file(&self, date: &str) -> PathBuf {
        self.root.join(format!("{date}.jsonl"))
    }
    pub fn meta_path(&self) -> PathBuf {
        self.root.join("meta.json")
    }
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    /// `threads.json` — the per-room thread metadata map (P2). Membership is derived from messages.
    pub fn threads_path(&self) -> PathBuf {
        self.root.join("threads.json")
    }
    pub fn roster_path(&self) -> PathBuf {
        self.root.join("roster.json")
    }

    /// Day dates present on disk, sorted ascending.
    fn day_dates_on_disk(&self) -> Vec<String> {
        let mut dates = vec![];
        if let Ok(rd) = std::fs::read_dir(&self.root) {
            for ent in rd.flatten() {
                if let Some(name) = ent.file_name().to_str() {
                    if let Some(date) = name.strip_suffix(".jsonl") {
                        dates.push(date.to_owned());
                    }
                }
            }
        }
        dates.sort();
        dates
    }
}

// ── Writer ───────────────────────────────────────────────────────────────────

/// The single-writer side. Owns append, per-day `n`, `index.json`, `meta.json`,
/// and lazy materialization (the room dir is created on the first append).
pub struct TranscriptWriter {
    paths: RoomPaths,
    active_date: String,
    next_n: u64,
    end_offset: u64,
    index: Index,
    threads: BTreeMap<String, Thread>,
    room_kind: RoomKind,
    members: Vec<Member>,
    budget_chars: u64,
    materialized: bool,
    name: String,
    topic: String,
    project: Option<PathBuf>,
    created_at: u64,
    phase: String,
    /// State dir for the room registry; `register_room` is called on first
    /// append so a scattered project room is still discoverable.
    registry_state_dir: PathBuf,
}

impl TranscriptWriter {
    /// A fresh, **un-materialized** writer — no disk footprint until the first
    /// `append`.
    pub fn new(
        paths: RoomPaths,
        name: impl Into<String>,
        topic: impl Into<String>,
        project: Option<PathBuf>,
        registry_state_dir: PathBuf,
    ) -> Self {
        Self {
            paths,
            active_date: today(),
            next_n: 0,
            end_offset: 0,
            index: Index::default(),
            threads: BTreeMap::new(),
            room_kind: RoomKind::default(),
            members: Vec::new(),
            budget_chars: 0,
            materialized: false,
            name: name.into(),
            topic: topic.into(),
            project,
            created_at: 0,
            phase: "Active".into(),
            registry_state_dir,
        }
    }

    /// Reopen an existing room from disk: rebuild the index from the day files,
    /// restore budget/meta, and position the high-water at the newest day.
    pub fn open(paths: RoomPaths, registry_state_dir: PathBuf) -> std::io::Result<Self> {
        let dates = paths.day_dates_on_disk();
        let mut index = Index::default();
        for date in &dates {
            let p = paths.day_file(date);
            let (count, bytes) = scan_day(&p);
            index.days.insert(date.clone(), DayStat { count, bytes });
        }
        let meta = load_meta(&paths);
        let materialized = !dates.is_empty();
        let active_date = dates.last().cloned().unwrap_or_else(today);
        let stat = index.days.get(&active_date).copied().unwrap_or_default();
        Ok(Self {
            active_date,
            next_n: stat.count,
            end_offset: stat.bytes,
            budget_chars: meta.as_ref().map(|m| m.budget_chars).unwrap_or(0),
            materialized,
            name: meta.as_ref().map(|m| m.name.clone()).unwrap_or_default(),
            topic: meta.as_ref().map(|m| m.topic.clone()).unwrap_or_default(),
            project: meta.as_ref().and_then(|m| m.project.clone()),
            created_at: meta.as_ref().map(|m| m.created_at).unwrap_or(0),
            room_kind: meta.as_ref().map(|m| m.kind).unwrap_or_default(),
            members: meta.as_ref().map(|m| m.members.clone()).unwrap_or_default(),
            phase: meta.map(|m| m.phase).unwrap_or_else(|| "Active".into()),
            index,
            threads: load_threads_map(&paths.threads_path()),
            paths,
            registry_state_dir,
        })
    }

    pub fn high_water(&self) -> HighWater {
        HighWater {
            date: self.active_date.clone(),
            n: self.next_n,
            end_offset: self.end_offset,
        }
    }

    pub fn budget_chars(&self) -> u64 {
        self.budget_chars
    }
    pub fn is_materialized(&self) -> bool {
        self.materialized
    }
    pub fn index(&self) -> &Index {
        &self.index
    }
    pub fn paths(&self) -> &RoomPaths {
        &self.paths
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Append one message, assigning `(date, n)` from `ts`'s local date. Creates
    /// the room dir + `.gitignore` and registers the room on the first call.
    pub fn append(
        &mut self,
        participant_id: impl Into<String>,
        display_name: impl Into<String>,
        content: impl Into<String>,
        ts: u64,
    ) -> std::io::Result<StoredTurn> {
        self.append_with_meta(participant_id, display_name, content, ts, PostMeta::default())
    }

    /// Append one message WITH support metadata (P1b). `append` is the plain (no-metadata) wrapper.
    pub fn append_with_meta(
        &mut self,
        participant_id: impl Into<String>,
        display_name: impl Into<String>,
        content: impl Into<String>,
        ts: u64,
        pm: PostMeta,
    ) -> std::io::Result<StoredTurn> {
        let content = content.into();
        let date = date_of_ts(ts);

        if !self.materialized {
            self.materialize(ts)?;
        }
        if date != self.active_date {
            // Roll over to (or resume) the target day.
            let stat = self.index.days.get(&date).copied().unwrap_or_default();
            self.active_date = date.clone();
            self.next_n = stat.count;
            self.end_offset = stat.bytes;
        }

        let turn = StoredTurn {
            date: date.clone(),
            n: self.next_n,
            participant_id: participant_id.into(),
            display_name: display_name.into(),
            content: content.clone(),
            ts,
            kind: pm.kind,
            thread_id: pm.thread_id,
            in_reply_to: pm.in_reply_to,
            meta: pm.meta,
        };
        let mut line = serde_json::to_string(&turn).map_err(std::io::Error::other)?;
        line.push('\n');

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.day_file(&date))?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        // Durability: get the message bytes onto disk BEFORE `persist_meta_index` records the new
        // count — otherwise a crash could leave the index claiming a message the JSONL never durably
        // got. (No-op under ROZUM_MEETINGS_FSYNC=0 / tmpfs.)
        if fsync_enabled() {
            f.sync_all()?;
        }

        self.end_offset += line.len() as u64;
        self.next_n += 1;
        self.index.days.insert(
            date,
            DayStat {
                count: self.next_n,
                bytes: self.end_offset,
            },
        );
        self.budget_chars += content.chars().count() as u64;
        self.persist_meta_index()?;
        Ok(turn)
    }

    fn materialize(&mut self, ts: u64) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.paths.root)?;
        if let Some(rozum_dir) = &self.paths.gitignore_dir {
            let gi = rozum_dir.join(".gitignore");
            if !gi.exists() {
                let _ = std::fs::write(&gi, "*\n");
            }
        }
        self.created_at = ts;
        // Load existing thread metadata (P2). Membership is derived from messages; this is the
        // not-derivable part (state/owner/severity/pinned). Absent file → no threads yet; a corrupt
        // primary falls back to the `.bak` (see load_threads_map). Only overwrite if we found some.
        let loaded = load_threads_map(&self.paths.threads_path());
        if !loaded.is_empty() {
            self.threads = loaded;
        }
        let _ = register_room(
            &self.registry_state_dir,
            &RoomLocation {
                name: self.name.clone(),
                root: self.paths.root.clone(),
                project: self.project.clone(),
            },
        );
        self.materialized = true;
        Ok(())
    }

    fn persist_meta_index(&self) -> std::io::Result<()> {
        let meta = Meta {
            name: self.name.clone(),
            topic: self.topic.clone(),
            project: self.project.clone(),
            phase: self.phase.clone(),
            created_at: self.created_at,
            budget_chars: self.budget_chars,
            kind: self.room_kind,
            members: self.members.clone(),
        };
        write_json_atomic(&self.paths.meta_path(), &meta)?;
        write_json_atomic(&self.paths.index_path(), &self.index)?;
        Ok(())
    }

    fn persist_threads(&self) -> std::io::Result<()> {
        let path = self.paths.threads_path();
        // Keep the last-good version as a `.bak` before overwriting — `threads.json` holds the
        // incident state (state/owner/severity/pinned), which is NOT rebuildable from the message log,
        // so a recent backup is the recovery path if the live file is ever lost/corrupted.
        if path.exists() {
            let _ = std::fs::copy(&path, threads_bak_path(&path));
        }
        write_json_atomic(&path, &self.threads)
    }

    // ── P2: thread / incident operations ───────────────────────────────────────────────────────
    /// Open (or return) a thread anchored on `anchor_id` (a message id — the message that started it).
    /// Idempotent on the id: re-opening returns the existing thread unchanged. Messages join it by
    /// setting `thread_id = anchor_id` when posted (`append_with_meta`).
    pub fn open_thread(
        &mut self,
        anchor_id: impl Into<String>,
        title: impl Into<String>,
        kind: ThreadKind,
        ts: u64,
    ) -> std::io::Result<Thread> {
        if !self.materialized {
            self.materialize(ts)?;
        }
        let id = anchor_id.into();
        let title = title.into();
        // Inherit the anchor message's severity — an incident opened on a `critical` alert IS critical,
        // so its SLA/staleness window is meaningful instead of the lax no-severity default.
        let anchor_sev = read_since(&self.paths.root, None, 0)
            .into_iter()
            .find(|m| m.id() == id)
            .and_then(|m| m.meta.severity);
        let t = self
            .threads
            .entry(id.clone())
            .or_insert_with(|| Thread {
                id: id.clone(),
                title,
                kind,
                state: ThreadState::Open,
                owner: None,
                severity: anchor_sev,
                pinned: vec![],
                created_ts: ts,
                updated_ts: ts,
            })
            .clone();
        self.persist_threads()?;
        Ok(t)
    }

    /// Move a thread through the resolving state machine. `None` if the thread id is unknown.
    pub fn set_thread_state(
        &mut self,
        id: &str,
        state: ThreadState,
        ts: u64,
    ) -> std::io::Result<Option<Thread>> {
        let Some(t) = self.threads.get_mut(id) else {
            return Ok(None);
        };
        t.state = state;
        t.updated_ts = ts;
        let updated = t.clone();
        self.persist_threads()?;
        Ok(Some(updated))
    }

    /// Set a thread's owner/assignee + severity (best-effort; `None` if the id is unknown).
    pub fn set_thread_owner_severity(
        &mut self,
        id: &str,
        owner: Option<String>,
        severity: Option<Severity>,
        ts: u64,
    ) -> std::io::Result<Option<Thread>> {
        let Some(t) = self.threads.get_mut(id) else {
            return Ok(None);
        };
        if owner.is_some() {
            t.owner = owner;
        }
        if severity.is_some() {
            t.severity = severity;
        }
        t.updated_ts = ts;
        let updated = t.clone();
        self.persist_threads()?;
        Ok(Some(updated))
    }

    /// Pin (`pin=true`) or unpin a message id within a thread — the incident's key messages. Idempotent;
    /// `None` if the thread id is unknown. Newest-pin-last ordering is preserved.
    pub fn set_pinned(
        &mut self,
        thread_id: &str,
        msg_id: &str,
        pin: bool,
        ts: u64,
    ) -> std::io::Result<Option<Thread>> {
        let Some(t) = self.threads.get_mut(thread_id) else {
            return Ok(None);
        };
        if pin {
            if !t.pinned.iter().any(|m| m == msg_id) {
                t.pinned.push(msg_id.to_string());
            }
        } else {
            t.pinned.retain(|m| m != msg_id);
        }
        t.updated_ts = ts;
        let updated = t.clone();
        self.persist_threads()?;
        Ok(Some(updated))
    }

    pub fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.get(id)
    }

    pub fn threads(&self) -> &BTreeMap<String, Thread> {
        &self.threads
    }

    /// The room's root dir (for reading messages back — incident-context gather).
    pub fn root(&self) -> &Path {
        self.paths.root.as_path()
    }

    // ── P3: room kind + members ────────────────────────────────────────────────────────────────
    /// Set the room kind (chat|queue|incident); persisted to meta.json.
    pub fn set_room_kind(&mut self, kind: RoomKind, ts: u64) -> std::io::Result<()> {
        if !self.materialized {
            self.materialize(ts)?;
        }
        self.room_kind = kind;
        self.persist_meta_index()
    }

    /// Replace the room members; persisted to meta.json.
    pub fn set_members(&mut self, members: Vec<Member>, ts: u64) -> std::io::Result<()> {
        if !self.materialized {
            self.materialize(ts)?;
        }
        self.members = members;
        self.persist_meta_index()
    }

    pub fn room_kind(&self) -> RoomKind {
        self.room_kind
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }
}

// ── Reader ───────────────────────────────────────────────────────────────────

/// The client side: tails one day file, advancing a byte offset. Rolls to the
/// next day with `roll_to`. Parses only complete lines (never a torn final
/// line), so reading concurrently with the writer is safe.
pub struct TranscriptReader {
    root: PathBuf,
    date: String,
    offset: u64,
}

impl TranscriptReader {
    /// Begin tailing `date` from byte 0.
    pub fn open_day(root: impl Into<PathBuf>, date: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            date: date.into(),
            offset: 0,
        }
    }

    pub fn date(&self) -> &str {
        &self.date
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Switch to a new day file, reading from its start.
    pub fn roll_to(&mut self, date: impl Into<String>) {
        self.date = date.into();
        self.offset = 0;
    }

    /// Read every complete line from `offset` to EOF of the current day file,
    /// advancing `offset` past them. A trailing partial line (no `\n`) is left
    /// unconsumed.
    pub fn read_to_eof(&mut self) -> std::io::Result<Vec<StoredTurn>> {
        let path = self.root.join(format!("{}.jsonl", self.date));
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i,
            None => return Ok(vec![]), // no complete line yet
        };
        let complete = &buf[..=last_nl];
        self.offset += (last_nl + 1) as u64;
        Ok(parse_lines(complete))
    }
}

/// Day dates present in a room dir, ascending (`YYYY-MM-DD`). Used by clients for
/// day-scoped rendering / scrollback.
pub fn day_dates(root: &Path) -> Vec<String> {
    let mut dates = vec![];
    if let Ok(rd) = std::fs::read_dir(root) {
        for ent in rd.flatten() {
            if let Some(name) = ent.file_name().to_str() {
                if let Some(date) = name.strip_suffix(".jsonl") {
                    dates.push(date.to_owned());
                }
            }
        }
    }
    dates.sort();
    dates
}

/// Best-effort reconstruction of the thread map from the message log ALONE — the last-resort recovery
/// path when both `threads.json` and its `.bak` are gone. Recovers membership + the anchor's severity +
/// an approximate state (a `resolution` message → resolved; an `escalated …` event → escalated) + a
/// best-effort owner (parsed from `escalated to X` / `assigned to X`). Title is the anchor's content;
/// pinned + exact state aren't recoverable. Exposed via `rozum meetings repair-threads`.
pub fn rebuild_threads(root: &Path) -> BTreeMap<String, Thread> {
    let msgs = read_since(root, None, 0);
    let by_id: BTreeMap<String, &StoredTurn> = msgs.iter().map(|m| (m.id(), m)).collect();
    let tids: std::collections::BTreeSet<String> =
        msgs.iter().filter_map(|m| m.thread_id.clone()).collect();
    let mut out = BTreeMap::new();
    for tid in tids {
        let anchor = by_id.get(&tid).copied();
        let members: Vec<&StoredTurn> = msgs
            .iter()
            .filter(|m| m.id() == tid || m.thread_id.as_deref() == Some(tid.as_str()))
            .collect();
        if members.is_empty() {
            continue;
        }
        let title = anchor
            .map(|a| a.content.chars().take(60).collect::<String>())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| tid.clone());
        let severity = anchor.and_then(|a| a.meta.severity);
        let mut state = ThreadState::Open;
        let mut owner = None;
        for m in &members {
            if m.kind == MsgKind::Resolution {
                state = ThreadState::Resolved;
            }
            if m.kind == MsgKind::Event {
                if let Some(rest) = m.content.strip_prefix("escalated to ") {
                    state = ThreadState::Escalated;
                    let who = rest.split([':', ' ']).next().unwrap_or("").to_string();
                    owner = Some(who).filter(|s| !s.is_empty());
                } else if let Some(rest) = m.content.strip_prefix("assigned to ") {
                    let who = rest.split([':', ' ']).next().unwrap_or("").to_string();
                    owner = Some(who).filter(|s| !s.is_empty());
                }
            }
        }
        let created = anchor.map(|a| a.ts).or_else(|| members.first().map(|m| m.ts)).unwrap_or(0);
        let updated = members.last().map(|m| m.ts).unwrap_or(created);
        out.insert(
            tid.clone(),
            Thread {
                id: tid,
                title,
                kind: ThreadKind::Incident,
                state,
                owner,
                severity,
                pinned: vec![],
                created_ts: created,
                updated_ts: updated,
            },
        );
    }
    out
}

/// Rebuild `threads.json` from the message log and persist it durably (keeping a `.bak` of any current
/// file). Returns the recovered incident count. The recovery action behind `rozum meetings repair-threads`
/// — run it (then restart the daemon) when the incident state was lost.
pub fn repair_threads(root: &Path) -> std::io::Result<usize> {
    let rebuilt = rebuild_threads(root);
    let path = root.join("threads.json");
    if path.exists() {
        let _ = std::fs::copy(&path, threads_bak_path(&path));
    }
    write_json_atomic(&path, &rebuilt)?;
    Ok(rebuilt.len())
}

/// The `.bak` sibling of a `threads.json` (last-good copy, the recovery source).
fn threads_bak_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Load a `threads.json` map, falling back to its `.bak` if the primary is missing/empty/unparseable.
/// The incident state isn't rebuildable from the message log, so this two-file recovery is the safety
/// net behind the durable (fsync'd, atomic) write.
fn load_threads_map(path: &Path) -> BTreeMap<String, Thread> {
    let try_one = |p: &Path| -> Option<BTreeMap<String, Thread>> {
        let bytes = std::fs::read(p).ok()?;
        if bytes.is_empty() {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    };
    try_one(path)
        .or_else(|| try_one(&threads_bak_path(path)))
        .unwrap_or_default()
}

/// Read the per-room thread map from `threads.json` (P2) given just a room root, with `.bak` fallback.
/// Empty if neither the file nor its backup is present/parseable — threads are an additive overlay,
/// so a room with no incidents simply has no map. Used by direct-read surfaces (the REST console).
pub fn read_threads(root: &Path) -> BTreeMap<String, Thread> {
    load_threads_map(&root.join("threads.json"))
}

/// Assemble an incident's whole picture from disk — the `Thread` record + every
/// message in it (the anchor `id == thread_id` plus members by `thread_id`) +
/// the distinct participants + the timespan. The read-only twin of
/// `Room::thread_context`, for surfaces that only have a room root.
pub fn thread_context(root: &Path, thread_id: &str) -> serde_json::Value {
    let thread = read_threads(root).remove(thread_id);
    let all = read_since(root, None, 0);
    let in_thread =
        |m: &StoredTurn| m.id() == thread_id || m.thread_id.as_deref() == Some(thread_id);
    let msgs: Vec<StoredTurn> = all.iter().filter(|m| in_thread(m)).cloned().collect();
    let participants: std::collections::BTreeSet<&str> =
        msgs.iter().map(|m| m.display_name.as_str()).collect();
    // Auto-gathered context: relevant messages NOT formally in the thread (the lead-up before the
    // anchor + same-tag messages elsewhere) — what a responder would otherwise dig for by hand.
    let related = msgs
        .iter()
        .find(|m| m.id() == thread_id)
        .map(|anchor| gather_related(&all, anchor, thread_id))
        .unwrap_or_default();
    serde_json::json!({
        "thread": thread,
        "message_count": msgs.len(),
        "participants": participants,
        "first_ts": msgs.first().map(|m| m.ts),
        "last_ts": msgs.last().map(|m| m.ts),
        "messages": msgs,
        "related": related,
    })
}

/// Auto-gather context related to an incident anchor that isn't formally in the thread: the lead-up
/// (the few messages immediately before the anchor) plus messages elsewhere sharing any of the
/// anchor's tags. Deduped by id, excludes thread members, oldest-first, capped. `all` must be the
/// room's whole history in chronological order (as `read_since(.., None, 0)` returns it).
fn gather_related(all: &[StoredTurn], anchor: &StoredTurn, thread_id: &str) -> Vec<StoredTurn> {
    const LEAD_WINDOW: usize = 5;
    const CAP: usize = 20;
    let in_thread =
        |m: &StoredTurn| m.id() == thread_id || m.thread_id.as_deref() == Some(thread_id);
    let tags = &anchor.meta.tags;

    // Lead-up: the last LEAD_WINDOW non-thread messages strictly before the anchor (chronological).
    let mut lead: Vec<&StoredTurn> = all
        .iter()
        .filter(|m| !in_thread(m) && (m.ts, m.n) < (anchor.ts, anchor.n))
        .collect();
    let lead = lead.split_off(lead.len().saturating_sub(LEAD_WINDOW));

    // Same-tag messages anywhere else (only if the anchor carries tags).
    let same_tag: Vec<&StoredTurn> = if tags.is_empty() {
        vec![]
    } else {
        all.iter()
            .filter(|m| !in_thread(m) && m.meta.tags.iter().any(|t| tags.contains(t)))
            .collect()
    };

    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<StoredTurn> = Vec::new();
    for m in lead.into_iter().chain(same_tag) {
        if seen.insert(m.id()) {
            out.push(m.clone());
        }
    }
    out.sort_by_key(|m| (m.ts, m.n));
    out.truncate(CAP);
    out
}

/// Default SLA window per severity — how long an ACTIVE incident may sit without an update before it
/// "needs attention" (goes stale). Conservative defaults; an unset severity is treated as low-priority.
pub fn sla_secs(sev: Option<Severity>) -> u64 {
    match sev {
        Some(Severity::Critical) => 15 * 60,
        Some(Severity::High) => 60 * 60,
        Some(Severity::Medium) => 4 * 3600,
        Some(Severity::Low) => 8 * 3600,
        _ => 24 * 3600,
    }
}

/// Is this incident stale as of `now` — active (not resolved/closed) AND no update within its
/// severity's SLA window? The signal that an incident is rotting and needs a human.
pub fn thread_is_stale(t: &Thread, now: u64) -> bool {
    !t.state.is_terminal() && now.saturating_sub(t.updated_ts) > sla_secs(t.severity)
}

/// Resolving metrics over a room's threads: totals, a per-state histogram, and the
/// mean time-to-resolve (created→updated) across terminal (resolved/closed) threads.
pub fn thread_metrics(root: &Path) -> serde_json::Value {
    let threads = read_threads(root);
    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    let mut resolve_secs: Vec<u64> = Vec::new();
    for t in threads.values() {
        *by_state.entry(t.state.as_str().to_string()).or_default() += 1;
        if t.state.is_terminal() {
            resolve_secs.push(t.updated_ts.saturating_sub(t.created_ts));
        }
    }
    let resolved = resolve_secs.len() as u64;
    let avg = if resolved == 0 {
        None
    } else {
        Some(resolve_secs.iter().sum::<u64>() / resolved)
    };
    serde_json::json!({
        "total": threads.len(),
        "by_state": by_state,
        "resolved": resolved,
        "avg_time_to_resolve_secs": avg,
    })
}

/// A message-search filter over a room's whole history (`mtg-message-ops`). All fields are AND-ed;
/// `None`/empty means "don't filter on this". `severity` matches that level AND ABOVE (min-severity).
#[derive(Debug, Default, Clone)]
pub struct MsgFilter<'a> {
    /// Case-insensitive substring of the message content.
    pub text: Option<&'a str>,
    /// Exact message kind (note|question|event|alert|resolution).
    pub kind: Option<MsgKind>,
    /// Minimum severity (this level and above).
    pub min_severity: Option<Severity>,
    /// A tag the message must carry.
    pub tag: Option<&'a str>,
    /// Restrict to one thread/incident (anchor id or members).
    pub thread_id: Option<&'a str>,
    /// Only messages on or after this date (`YYYY-MM-DD`).
    pub since_date: Option<&'a str>,
}
impl MsgFilter<'_> {
    fn matches(&self, m: &StoredTurn) -> bool {
        if let Some(t) = self.text {
            if !m.content.to_lowercase().contains(&t.to_lowercase()) {
                return false;
            }
        }
        if let Some(k) = self.kind {
            if m.kind != k {
                return false;
            }
        }
        if let Some(min) = self.min_severity {
            match m.meta.severity {
                Some(s) if s.rank() >= min.rank() => {}
                _ => return false,
            }
        }
        if let Some(tag) = self.tag {
            if !m.meta.tags.iter().any(|t| t == tag) {
                return false;
            }
        }
        if let Some(tid) = self.thread_id {
            if m.id() != tid && m.thread_id.as_deref() != Some(tid) {
                return false;
            }
        }
        true
    }
}

/// Search a room's whole on-disk history for messages matching `filter`, newest-last, capped at
/// `limit` (the most-recent `limit` matches). The read-side of `mtg-message-ops` — spans every day
/// file + thread, unlike the console's client-side filter over just today's feed.
pub fn search_messages(root: &Path, filter: &MsgFilter, limit: usize) -> Vec<StoredTurn> {
    let mut hits: Vec<StoredTurn> = read_since(root, filter.since_date, 0)
        .into_iter()
        .filter(|m| filter.matches(m))
        .collect();
    if hits.len() > limit {
        hits.drain(0..hits.len() - limit);
    }
    hits
}

/// Prune day files older than `retain_days` from `root` (opt-in retention; `retain_days == 0` keeps
/// everything). A day is NEVER pruned if it holds a message belonging to a non-terminal (open) incident
/// — incident context must survive. Rewrites `index.json` to drop the pruned days. Returns the pruned
/// dates. `now_ts` is the current unix time (the cutoff is `now − retain_days`).
pub fn prune_old_days(root: &Path, retain_days: u64, now_ts: u64) -> Vec<String> {
    if retain_days == 0 {
        return vec![];
    }
    let cutoff = date_of_ts(now_ts.saturating_sub(retain_days.saturating_mul(86_400)));
    // Anchor/thread ids of OPEN incidents — their days are protected.
    let open_ids: BTreeMap<String, ()> = read_threads(root)
        .values()
        .filter(|t| !t.state.is_terminal())
        .map(|t| (t.id.clone(), ()))
        .collect();
    let mut pruned = vec![];
    for date in day_dates(root) {
        if date.as_str() >= cutoff.as_str() {
            continue; // within the retention window
        }
        // Protect a day that carries any open-incident anchor or member (or that we can't read).
        let protected = !open_ids.is_empty()
            && read_day(root, &date, 0, None)
                .map(|msgs| {
                    msgs.iter().any(|m| {
                        open_ids.contains_key(&m.id())
                            || m.thread_id.as_deref().is_some_and(|t| open_ids.contains_key(t))
                    })
                })
                .unwrap_or(true);
        if protected {
            continue;
        }
        if std::fs::remove_file(root.join(format!("{date}.jsonl"))).is_ok() {
            pruned.push(date);
        }
    }
    if !pruned.is_empty() {
        let idx_path = root.join("index.json");
        if let Ok(bytes) = std::fs::read(&idx_path) {
            if let Ok(mut index) = serde_json::from_slice::<Index>(&bytes) {
                for d in &pruned {
                    index.days.remove(d);
                }
                let _ = write_json_atomic(&idx_path, &index);
            }
        }
    }
    pruned
}

/// Read every message at or after the cursor `(since_date, since_n)` from disk.
/// `since_date = None` returns the whole history. This is how direct-read clients
/// fetch a `wait` delta — content never transits the daemon.
pub fn read_since(root: &Path, since_date: Option<&str>, since_n: u64) -> Vec<StoredTurn> {
    let mut out = vec![];
    for date in day_dates(root) {
        let from = match since_date {
            Some(sd) if date.as_str() < sd => continue,
            Some(sd) if date.as_str() == sd => since_n,
            _ => 0,
        };
        if let Ok(turns) = read_day(root, &date, from, None) {
            out.extend(turns);
        }
    }
    out
}

/// Read one whole day file, optionally slicing by `n` (`from`, `count`). Used by
/// scrollback and the future REST read.
pub fn read_day(
    root: &Path,
    date: &str,
    from: u64,
    count: Option<u64>,
) -> std::io::Result<Vec<StoredTurn>> {
    let path = root.join(format!("{date}.jsonl"));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut turns = parse_lines(&bytes);
    turns.retain(|t| t.n >= from);
    if let Some(c) = count {
        turns.truncate(c as usize);
    }
    Ok(turns)
}

// ── Registry (`rooms.json`) ──────────────────────────────────────────────────

/// One entry in the room-location registry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomLocation {
    pub name: String,
    pub root: PathBuf,
    pub project: Option<PathBuf>,
}

fn registry_path(state_dir: &Path) -> PathBuf {
    state_dir.join("rooms.json")
}

/// Upsert a room (keyed by `root`) into `<state_dir>/rooms.json`. Also prunes any *other* entry that
/// shares this room's `name` but points at a root that no longer exists on disk — so a stale duplicate
/// (e.g. a deleted/moved project that once held a same-named room) can't shadow the live one when a
/// surface resolves a room by name. See `mtg-registry-dup-name`.
pub fn register_room(state_dir: &Path, loc: &RoomLocation) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let mut rooms = list_registered(state_dir);
    // Drop stale same-name dupes (different root that's gone). Keep this room's own root regardless.
    rooms.retain(|r| r.root == loc.root || r.name != loc.name || r.root.exists());
    if let Some(existing) = rooms.iter_mut().find(|r| r.root == loc.root) {
        *existing = loc.clone();
    } else {
        rooms.push(loc.clone());
    }
    write_json_atomic(&registry_path(state_dir), &rooms)
}

/// All registered room locations (empty / missing file → empty vec).
pub fn list_registered(state_dir: &Path) -> Vec<RoomLocation> {
    match std::fs::read(registry_path(state_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => vec![],
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// `$XDG_STATE_HOME/rozum` (fallback `~/.local/state/rozum`).
pub fn rozum_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            home.join(".local").join("state")
        });
    base.join("rozum")
}

fn parse_lines(bytes: &[u8]) -> Vec<StoredTurn> {
    let mut out = vec![];
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<StoredTurn>(line) {
            Ok(t) => out.push(t),
            Err(e) => tracing::warn!(error = ?e, "skipping malformed transcript line"),
        }
    }
    out
}

/// Count complete lines and byte length of a day file.
fn scan_day(path: &Path) -> (u64, u64) {
    match std::fs::read(path) {
        Ok(bytes) => {
            let count = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
            (count, bytes.len() as u64)
        }
        Err(_) => (0, 0),
    }
}

fn load_meta(paths: &RoomPaths) -> Option<Meta> {
    let bytes = std::fs::read(paths.meta_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read a room's `meta.json` given its `root` (for discovery/summaries).
pub fn read_meta(root: &Path) -> Option<Meta> {
    let bytes = std::fs::read(root.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write `value` as JSON to `path` atomically AND durably: serialize to a sibling `.tmp`, fsync the
/// data, rename over `path`, then fsync the directory so the rename entry itself survives a power loss /
/// kernel panic. Without the fsyncs the rename can be reordered ahead of the data write and expose an
/// empty/stale file after a crash — and `threads.json` (the incident state) is not rebuildable. This is
/// the durability backstop for the support platform's persisted state on a box that has panicked before.
/// `ROZUM_MEETINGS_FSYNC=0` drops the fsyncs (faster, but not crash-durable) — for tmpfs/tests.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let fsync = fsync_enabled();
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        if fsync {
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    if fsync {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

/// Whether persisted writes are fsync'd to physical disk (default on). Off = page-cache only (faster,
/// not crash-durable) — appropriate for tmpfs-backed runtime dirs and tests.
fn fsync_enabled() -> bool {
    std::env::var("ROZUM_MEETINGS_FSYNC")
        .map(|v| v != "0")
        .unwrap_or(true)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A writer rooted at a tempdir project, with its registry in the same dir.
    fn writer_in(dir: &Path) -> TranscriptWriter {
        let paths = RoomPaths::for_project(dir);
        TranscriptWriter::new(
            paths,
            "proj",
            "topic",
            Some(dir.to_path_buf()),
            dir.join("state"),
        )
    }

    // 2026-06-16 12:00 and 2026-06-17 12:00 local-ish anchors. We pass explicit
    // ts so date derivation is exercised but deterministic within a run.
    fn ts_for(date_marker: u64) -> u64 {
        // Two timestamps a full day apart; their *local* dates differ.
        1_718_000_000 + date_marker * 86_400
    }

    // P1 message-metadata: a plain message must serialize BYTE-IDENTICALLY to the v1 JSON (no new keys)
    // and an old v1 line must read back with default metadata — so existing rooms are untouched.
    #[test]
    fn stored_turn_metadata_is_backward_compatible() {
        let v1 = r#"{"date":"2026-06-28","n":3,"participant_id":"p","display_name":"P","content":"hi","ts":42}"#;
        // Old line parses → defaults; derived id.
        let t: StoredTurn = serde_json::from_str(v1).unwrap();
        assert_eq!(t.kind, MsgKind::Note);
        assert!(t.thread_id.is_none() && t.in_reply_to.is_none() && t.meta.is_empty());
        assert_eq!(t.id(), "2026-06-28/3");
        // A plain turn serializes to EXACTLY the v1 string (no metadata keys) → byte-identical rooms.
        let plain = StoredTurn {
            date: "2026-06-28".into(),
            n: 3,
            participant_id: "p".into(),
            display_name: "P".into(),
            content: "hi".into(),
            ts: 42,
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&plain).unwrap(), v1);
        // A metadata-rich turn round-trips (kind + severity + tags emit + parse back).
        let rich = StoredTurn {
            kind: MsgKind::Alert,
            thread_id: Some("2026-06-28/0".into()),
            meta: MsgMeta {
                severity: Some(Severity::High),
                status: Some(MsgStatus::Open),
                tags: vec!["db".into()],
                ..Default::default()
            },
            ..plain.clone()
        };
        let s = serde_json::to_string(&rich).unwrap();
        assert!(s.contains(r#""kind":"alert""#) && s.contains(r#""severity":"high""#) && s.contains(r#""tags":["db"]"#));
        assert_eq!(serde_json::from_str::<StoredTurn>(&s).unwrap(), rich);
    }

    #[test]
    fn badge_renders_metadata_and_is_empty_for_plain_notes() {
        // A plain note carries no badge (the common case stays uncluttered).
        let plain = StoredTurn {
            date: "2026-06-28".into(),
            n: 3,
            participant_id: "p".into(),
            display_name: "P".into(),
            content: "hi".into(),
            ts: 42,
            ..Default::default()
        };
        assert_eq!(plain.badge(), None);
        // An alert with severity + thread + tag renders a compact, ordered badge.
        let alert = StoredTurn {
            kind: MsgKind::Alert,
            thread_id: Some("2026-06-28/0".into()),
            meta: MsgMeta {
                severity: Some(Severity::Critical),
                tags: vec!["db".into(), "prod".into()],
                ..Default::default()
            },
            ..plain.clone()
        };
        assert_eq!(alert.badge().as_deref(), Some("[ALERT CRIT ⤷2026-06-28/0 #db #prod]"));
        // A bare question (kind only) still badges.
        let q = StoredTurn { kind: MsgKind::Question, ..plain };
        assert_eq!(q.badge().as_deref(), Some("[ASK]"));
    }

    #[test]
    fn search_messages_filters_by_text_and_metadata() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        let mut alert = PostMeta::default();
        alert.kind = MsgKind::Alert;
        alert.meta.severity = Some(Severity::Critical);
        alert.meta.tags = vec!["db".into()];
        w.append_with_meta("p", "A", "DB connection timeout", 1_718_000_000, alert).unwrap();
        let mut low = PostMeta::default();
        low.meta.severity = Some(Severity::Low);
        w.append_with_meta("p", "A", "cosmetic timeout in UI", 1_718_000_010, low).unwrap();
        w.append("p", "A", "unrelated chatter", 1_718_000_020).unwrap();

        // Text substring (case-insensitive) matches both "timeout" lines.
        let f = MsgFilter { text: Some("TIMEOUT"), ..Default::default() };
        assert_eq!(search_messages(&root, &f, 100).len(), 2);
        // Min-severity high → only the critical alert (low is below).
        let f = MsgFilter { min_severity: Some(Severity::High), ..Default::default() };
        let hits = search_messages(&root, &f, 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "DB connection timeout");
        // Tag + kind narrow to the same single alert.
        let f = MsgFilter { tag: Some("db"), kind: Some(MsgKind::Alert), ..Default::default() };
        assert_eq!(search_messages(&root, &f, 100).len(), 1);
        // limit keeps the most-recent N.
        let f = MsgFilter { text: Some("timeout"), ..Default::default() };
        let hits = search_messages(&root, &f, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "cosmetic timeout in UI");
    }

    #[test]
    fn thread_staleness_respects_severity_sla_and_terminal_state() {
        let mk = |state: ThreadState, sev: Option<Severity>, updated: u64| Thread {
            id: "2026-06-28/0".into(),
            title: "x".into(),
            kind: ThreadKind::Incident,
            state,
            owner: None,
            severity: sev,
            pinned: vec![],
            created_ts: 0,
            updated_ts: updated,
        };
        let now = 100_000u64;
        // Critical SLA = 15m: updated 20m ago → stale; updated 10m ago → fresh.
        assert!(thread_is_stale(&mk(ThreadState::Open, Some(Severity::Critical), now - 20 * 60), now));
        assert!(!thread_is_stale(&mk(ThreadState::Open, Some(Severity::Critical), now - 10 * 60), now));
        // High SLA = 1h: 20m old critical-vs-high differ — 20m high is still fresh.
        assert!(!thread_is_stale(&mk(ThreadState::Escalated, Some(Severity::High), now - 20 * 60), now));
        // A resolved incident is never stale, however old.
        assert!(!thread_is_stale(&mk(ThreadState::Resolved, Some(Severity::Critical), 0), now));
        // No severity → low-priority 24h window.
        assert!(!thread_is_stale(&mk(ThreadState::Open, None, now - 3600), now));
        assert!(thread_is_stale(&mk(ThreadState::Open, None, now - 25 * 3600), now));
    }

    #[test]
    fn pinning_messages_persists_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        let anchor = w.append("p", "A", "DB down", 1_718_000_000).unwrap();
        let id = anchor.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, 1_718_000_001).unwrap();
        let m = w.append("p", "A", "root cause: bad deploy", 1_718_000_100).unwrap();
        // Pin is idempotent.
        w.set_pinned(&id, &m.id(), true, 1_718_000_200).unwrap();
        let t = w.set_pinned(&id, &m.id(), true, 1_718_000_201).unwrap().unwrap();
        assert_eq!(t.pinned, vec![m.id()]);
        // Persists across reload.
        assert_eq!(read_threads(&root).get(&id).unwrap().pinned, vec![m.id()]);
        // Unpin removes it.
        let t = w.set_pinned(&id, &m.id(), false, 1_718_000_300).unwrap().unwrap();
        assert!(t.pinned.is_empty());
        // Unknown thread → None.
        assert!(w.set_pinned("nope/0", &m.id(), true, 1_718_000_400).unwrap().is_none());
    }

    #[test]
    fn thread_context_auto_gathers_related() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        // Lead-up chatter (not in the thread).
        w.append("p", "A", "deploy 1234 going out", 1_718_000_000).unwrap(); // 0
        w.append("p", "A", "metrics look fine", 1_718_000_010).unwrap(); // 1
        // The incident anchor (tag db) → opened as a thread.
        let mut alert = PostMeta::default();
        alert.kind = MsgKind::Alert;
        alert.meta.tags = vec!["db".into()];
        let anchor = w.append_with_meta("p", "A", "DB is down", 1_718_000_100, alert).unwrap(); // 2
        let id = anchor.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, 1_718_000_101).unwrap();
        // A reply IN the thread.
        let mut reply = PostMeta::default();
        reply.thread_id = Some(id.clone());
        w.append_with_meta("p", "B", "on it", 1_718_000_200, reply).unwrap(); // 3
        // A same-tag message elsewhere (NOT in the thread) — should be auto-gathered.
        let mut tagged = PostMeta::default();
        tagged.meta.tags = vec!["db".into()];
        w.append_with_meta("p", "C", "db replica also flaky last week", 1_718_000_300, tagged).unwrap(); // 4

        let ctx = thread_context(&root, &id);
        // The thread itself = anchor + the in-thread reply.
        assert_eq!(ctx["message_count"], 2);
        let related = ctx["related"].as_array().unwrap();
        let contents: Vec<&str> = related.iter().map(|m| m["content"].as_str().unwrap()).collect();
        // Lead-up (the 2 messages before the anchor) + the same-tag message elsewhere are gathered;
        // the in-thread reply is NOT (it's already in `messages`).
        assert!(contents.contains(&"deploy 1234 going out"), "lead-up gathered: {contents:?}");
        assert!(contents.contains(&"metrics look fine"));
        assert!(contents.contains(&"db replica also flaky last week"), "same-tag gathered");
        assert!(!contents.contains(&"on it"), "in-thread reply excluded from related");
    }

    // P1b: append_with_meta persists metadata; plain append stays plain.
    #[test]
    fn append_with_meta_persists_metadata() {
        let dir = tempdir().unwrap();
        let mut w = writer_in(dir.path());
        let pm = PostMeta {
            kind: MsgKind::Alert,
            meta: MsgMeta {
                severity: Some(Severity::High),
                tags: vec!["db".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let turn = w.append_with_meta("p", "P", "DB is down", ts_for(0), pm).unwrap();
        assert_eq!(turn.kind, MsgKind::Alert);
        assert_eq!(turn.meta.severity, Some(Severity::High));
        assert_eq!(turn.meta.tags, vec!["db".to_string()]);
        let plain = w.append("p2", "P2", "hi", ts_for(0)).unwrap();
        assert_eq!(plain.kind, MsgKind::Note);
        assert!(plain.meta.is_empty());
    }

    // P2: open a thread, escalate it, and confirm it persists across a writer reload (threads.json).
    #[test]
    fn threads_open_escalate_and_persist() {
        let dir = tempdir().unwrap();
        let anchor;
        {
            let mut w = writer_in(dir.path());
            let m = w
                .append_with_meta(
                    "p",
                    "P",
                    "DB is down",
                    ts_for(0),
                    PostMeta { kind: MsgKind::Alert, ..Default::default() },
                )
                .unwrap();
            anchor = m.id();
            let th = w.open_thread(anchor.clone(), "DB outage", ThreadKind::Incident, ts_for(0)).unwrap();
            assert_eq!(th.state, ThreadState::Open);
            // A reply joins the thread.
            w.append_with_meta(
                "p2",
                "P2",
                "looking",
                ts_for(0),
                PostMeta { thread_id: Some(anchor.clone()), in_reply_to: Some(anchor.clone()), ..Default::default() },
            )
            .unwrap();
            let esc = w.set_thread_state(&anchor, ThreadState::Escalated, ts_for(0)).unwrap().unwrap();
            assert_eq!(esc.state, ThreadState::Escalated);
            assert_eq!(w.threads().len(), 1);
        }
        // Fresh writer → threads.json reloaded with the escalated state.
        let mut w = writer_in(dir.path());
        w.append("p3", "P3", "x", ts_for(0)).unwrap(); // triggers materialize → loads threads.json
        let th = w.thread(&anchor).expect("thread persisted");
        assert_eq!(th.state, ThreadState::Escalated);
        assert_eq!(th.kind, ThreadKind::Incident);
        assert_eq!(th.title, "DB outage");
    }

    // P3: room kind + members persist to meta.json across a reopen; plain rooms stay `chat`.
    #[test]
    fn room_kind_and_members_persist() {
        let dir = tempdir().unwrap();
        {
            let mut w = writer_in(dir.path());
            w.set_room_kind(RoomKind::Incident, ts_for(0)).unwrap();
            w.set_members(
                vec![Member { handle: "alice".into(), role: MemberRole::Assignee }],
                ts_for(0),
            )
            .unwrap();
        }
        let w =
            TranscriptWriter::open(RoomPaths::for_project(dir.path()), dir.path().join("state")).unwrap();
        assert_eq!(w.room_kind(), RoomKind::Incident);
        assert_eq!(w.members().len(), 1);
        assert_eq!(w.members()[0].handle, "alice");
        assert_eq!(w.members()[0].role, MemberRole::Assignee);
    }

    #[test]
    fn append_assigns_date_and_zero_based_n() {
        let dir = tempdir().unwrap();
        let mut w = writer_in(dir.path());
        assert!(!w.is_materialized());

        let t0 = w.append("p", "P", "hello", ts_for(0)).unwrap();
        let t1 = w.append("p", "P", "world", ts_for(0)).unwrap();
        assert_eq!(t0.n, 0);
        assert_eq!(t1.n, 1);
        assert_eq!(t0.date, t1.date);
        assert!(w.is_materialized());

        // Day file holds both lines under that date.
        let turns = read_day(&w.paths().root, &t0.date, 0, None).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].content, "hello");
        assert_eq!(turns[1].content, "world");
    }

    #[test]
    fn rollover_resets_n_and_opens_new_file() {
        let dir = tempdir().unwrap();
        let mut w = writer_in(dir.path());
        let day0 = w.append("p", "P", "a", ts_for(0)).unwrap();
        let _ = w.append("p", "P", "b", ts_for(0)).unwrap();
        let day1 = w.append("p", "P", "c", ts_for(1)).unwrap();

        assert_ne!(day0.date, day1.date, "ts a day apart must differ in date");
        assert_eq!(day1.n, 0, "new day resets n to 0");

        // Two separate day files, each self-contained.
        assert_eq!(
            read_day(&w.paths().root, &day0.date, 0, None)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            read_day(&w.paths().root, &day1.date, 0, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn reader_tails_new_lines_only() {
        let dir = tempdir().unwrap();
        let mut w = writer_in(dir.path());
        let t = w.append("p", "P", "one", ts_for(0)).unwrap();
        let mut r = TranscriptReader::open_day(w.paths().root.clone(), &t.date);

        let first = r.read_to_eof().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].content, "one");

        // Nothing new yet.
        assert!(r.read_to_eof().unwrap().is_empty());

        // Append more; the reader picks up only the delta.
        w.append("p", "P", "two", ts_for(0)).unwrap();
        let delta = r.read_to_eof().unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].content, "two");
    }

    #[test]
    fn reopen_recovers_high_water_and_budget() {
        let dir = tempdir().unwrap();
        let (date0, date1);
        {
            let mut w = writer_in(dir.path());
            date0 = w.append("p", "P", "aaaa", ts_for(0)).unwrap().date; // 4 chars
            let _ = w.append("p", "P", "bb", ts_for(0)).unwrap(); // 2 chars
            date1 = w.append("p", "P", "ccc", ts_for(1)).unwrap().date; // 3 chars
            assert_eq!(w.budget_chars(), 9);
        }
        let w2 =
            TranscriptWriter::open(RoomPaths::for_project(dir.path()), dir.path().join("state"))
                .unwrap();
        assert!(w2.is_materialized());
        assert_eq!(w2.budget_chars(), 9, "budget restored from meta.json");
        let hw = w2.high_water();
        assert_eq!(hw.date, date1, "high-water sits on the newest day");
        assert_eq!(hw.n, 1, "newest day had 1 message");
        // Index has both days with correct counts.
        assert_eq!(w2.index().days.get(&date0).unwrap().count, 2);
        assert_eq!(w2.index().days.get(&date1).unwrap().count, 1);
    }

    #[test]
    fn next_append_after_reopen_continues_n() {
        let dir = tempdir().unwrap();
        let date;
        {
            let mut w = writer_in(dir.path());
            date = w.append("p", "P", "x", ts_for(0)).unwrap().date;
        }
        let mut w2 =
            TranscriptWriter::open(RoomPaths::for_project(dir.path()), dir.path().join("state"))
                .unwrap();
        // Same day → n continues at 1.
        let t = w2.append("p", "P", "y", ts_for(0)).unwrap();
        assert_eq!(t.date, date);
        assert_eq!(t.n, 1);
    }

    #[test]
    fn gitignore_and_registry_written_on_first_message() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        let mut w = writer_in(dir.path());
        // Nothing on disk before the first message.
        assert!(!dir.path().join(".rozum").exists());
        assert!(list_registered(&state).is_empty());

        w.append("p", "P", "hi", ts_for(0)).unwrap();

        let gi = dir.path().join(".rozum").join(".gitignore");
        assert_eq!(std::fs::read_to_string(&gi).unwrap().trim(), "*");
        let rooms = list_registered(&state);
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "proj");
        assert_eq!(rooms[0].root, w.paths().root);
    }

    #[test]
    fn read_day_slices_by_n() {
        let dir = tempdir().unwrap();
        let mut w = writer_in(dir.path());
        let mut date = String::new();
        for i in 0..5 {
            date = w.append("p", "P", format!("m{i}"), ts_for(0)).unwrap().date;
        }
        let slice = read_day(&w.paths().root, &date, 2, Some(2)).unwrap();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].n, 2);
        assert_eq!(slice[0].content, "m2");
        assert_eq!(slice[1].n, 3);
    }

    #[test]
    fn registry_upserts_by_root() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        let loc = RoomLocation {
            name: "a".into(),
            root: dir.path().join("r"),
            project: None,
        };
        register_room(&state, &loc).unwrap();
        // Re-register the same root with a new name → upsert, not duplicate.
        register_room(
            &state,
            &RoomLocation {
                name: "renamed".into(),
                ..loc.clone()
            },
        )
        .unwrap();
        let rooms = list_registered(&state);
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "renamed");
    }

    #[test]
    fn retention_prunes_old_days_but_protects_open_incidents() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        // Day 1 (old, plain chatter), day 2 (old, holds an OPEN incident), day "now" (recent).
        let d1 = 1_700_000_000u64; // ~2023-11
        let d2 = d1 + 86_400 * 2;
        let now = d1 + 86_400 * 30; // 30 days later
        w.append("p", "A", "old chatter", d1).unwrap();
        let anchor = w.append("p", "A", "DB down", d2).unwrap();
        let id = anchor.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, d2).unwrap(); // stays OPEN
        w.append("p", "A", "recent", now).unwrap();
        let dates_before = day_dates(&root);
        assert_eq!(dates_before.len(), 3, "three day files: {dates_before:?}");

        // Retain 7 days → day 1 + day 2 are both "old", but day 2 holds the open incident → protected.
        let pruned = prune_old_days(&root, 7, now);
        assert_eq!(pruned, vec![date_of_ts(d1)], "only the plain old day pruned: {pruned:?}");
        let after = day_dates(&root);
        assert!(!after.contains(&date_of_ts(d1)), "old chatter day removed");
        assert!(after.contains(&date_of_ts(d2)), "open-incident day protected");
        assert!(after.contains(&date_of_ts(now)), "recent day kept");
        // retain_days = 0 is a no-op.
        assert!(prune_old_days(&root, 0, now).is_empty());
    }

    #[test]
    fn rebuild_threads_reconstructs_incidents_from_the_log() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        // An alert anchor (critical), opened, escalated, resolved — the messages escalate/resolve post.
        let mut alert = PostMeta::default();
        alert.kind = MsgKind::Alert;
        alert.meta.severity = Some(Severity::Critical);
        let anchor = w.append_with_meta("p", "A", "DB down", 1_718_000_000, alert).unwrap();
        let id = anchor.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, 1_718_000_001).unwrap();
        let mut ev = PostMeta::default();
        ev.kind = MsgKind::Event;
        ev.thread_id = Some(id.clone());
        w.append_with_meta("p", "A", "escalated to oncall: paging", 1_718_000_100, ev).unwrap();
        let mut res = PostMeta::default();
        res.kind = MsgKind::Resolution;
        res.thread_id = Some(id.clone());
        w.append_with_meta("p", "A", "failover done", 1_718_000_200, res).unwrap();

        // Wipe BOTH threads.json and its .bak — simulate total incident-state loss.
        let tp = root.join("threads.json");
        let _ = std::fs::remove_file(&tp);
        let _ = std::fs::remove_file(threads_bak_path(&tp));
        assert!(read_threads(&root).is_empty(), "state truly gone");

        let rebuilt = rebuild_threads(&root);
        let t = rebuilt.get(&id).expect("incident reconstructed from the log");
        assert_eq!(t.severity, Some(Severity::Critical), "severity from anchor");
        assert_eq!(t.state, ThreadState::Resolved, "resolution → resolved");
        assert_eq!(t.owner.as_deref(), Some("oncall"), "owner from 'escalated to oncall'");
        assert_eq!(t.title, "DB down", "title from anchor content");
        // repair_threads persists it.
        assert_eq!(repair_threads(&root).unwrap(), 1);
        assert!(read_threads(&root).contains_key(&id));
    }

    #[test]
    fn threads_recover_from_bak_when_primary_is_corrupt() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::ad_hoc_in(dir.path(), "ops");
        let root = paths.root.clone();
        let mut w = TranscriptWriter::new(paths, "ops", "topic", None, dir.path().to_path_buf());
        let a = w.append("p", "A", "DB down", 1_718_000_000).unwrap();
        let id = a.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, 1_718_000_001).unwrap();
        // A second write makes the previous good version land in `.bak`.
        w.set_thread_state(&id, ThreadState::Escalated, 1_718_000_002).unwrap();
        let tp = root.join("threads.json");
        assert!(threads_bak_path(&tp).exists(), "a .bak should exist after the 2nd write");
        // Simulate a corrupt/half-written primary (what a crash-without-fsync could leave).
        std::fs::write(&tp, b"{ this is not json").unwrap();
        // read_threads recovers the incident from the .bak instead of losing it.
        let recovered = read_threads(&root);
        assert!(recovered.contains_key(&id), "incident recovered from .bak: {recovered:?}");
        // An empty primary also falls back.
        std::fs::write(&tp, b"").unwrap();
        assert!(read_threads(&root).contains_key(&id));
    }

    #[test]
    fn write_json_atomic_round_trips_and_leaves_no_tmp() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub").join("threads.json");
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), 42u64);
        write_json_atomic(&path, &m).unwrap();
        // Content is correct and the sibling .tmp is gone (renamed, not left behind).
        let back: BTreeMap<String, u64> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.get("k"), Some(&42));
        assert!(!path.with_extension("tmp").exists(), "tmp not cleaned up");
        // Overwrite is also clean (atomic replace).
        m.insert("k".to_string(), 7);
        write_json_atomic(&path, &m).unwrap();
        let back: BTreeMap<String, u64> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.get("k"), Some(&7));
    }

    #[test]
    fn registry_prunes_stale_same_name_dupes() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        // A stale registration: a same-named room whose root no longer exists (deleted project).
        let stale_root = dir.path().join("gone");
        register_room(
            &state,
            &RoomLocation { name: "proj".into(), root: stale_root.clone(), project: None },
        )
        .unwrap();
        // A live room of the same name registers (its root exists).
        let live_root = dir.path().join("live");
        std::fs::create_dir_all(&live_root).unwrap();
        register_room(
            &state,
            &RoomLocation { name: "proj".into(), root: live_root.clone(), project: None },
        )
        .unwrap();
        // The stale same-name entry is pruned; only the live one remains.
        let rooms = list_registered(&state);
        assert_eq!(rooms.len(), 1, "stale same-name dupe pruned: {rooms:?}");
        assert_eq!(rooms[0].root, live_root);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root).unwrap();
        let day = "2026-06-16";
        let good = serde_json::to_string(&StoredTurn {
            date: day.into(),
            n: 0,
            participant_id: "p".into(),
            display_name: "P".into(),
            content: "ok".into(),
            ts: 1,
            ..Default::default()
        })
        .unwrap();
        std::fs::write(
            root.join(format!("{day}.jsonl")),
            format!("{good}\nnot json\n"),
        )
        .unwrap();
        let turns = read_day(root, day, 0, None).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "ok");
    }
}
