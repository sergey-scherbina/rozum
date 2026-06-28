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
            threads: std::fs::read(paths.threads_path())
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
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
        // not-derivable part (state/owner/severity). Absent file → no threads yet.
        if let Ok(bytes) = std::fs::read(self.paths.threads_path()) {
            if let Ok(t) = serde_json::from_slice::<BTreeMap<String, Thread>>(&bytes) {
                self.threads = t;
            }
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
        write_json_atomic(&self.paths.threads_path(), &self.threads)
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
        let t = self
            .threads
            .entry(id.clone())
            .or_insert_with(|| Thread {
                id: id.clone(),
                title,
                kind,
                state: ThreadState::Open,
                owner: None,
                severity: None,
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

    pub fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.get(id)
    }

    pub fn threads(&self) -> &BTreeMap<String, Thread> {
        &self.threads
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

/// Upsert a room (keyed by `root`) into `<state_dir>/rooms.json`.
pub fn register_room(state_dir: &Path, loc: &RoomLocation) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let mut rooms = list_registered(state_dir);
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)
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
