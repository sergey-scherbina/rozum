//! `MeetingClient` — the daemon-client view-model behind the human TUI.
//!
//! The human is just another local daemon client: it connects to `meeting.sock`,
//! lists rooms (for the picker), enters one (joining as `kind="human"`), and
//! renders the transcript **read directly from disk** (day-scoped). New messages
//! arrive via `meeting.wait_my_turn` (the daemon wakeup); content is read from
//! the room's day files. This module is the testable logic; the ratatui shell
//! (`meetings attach`) renders it and forwards keypresses. See
//! `docs/specs/agent-meetings-daemon.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::room_client::{RoomConnection, tool_result_text_json};
use super::store::{
    Index, RoomPaths, StoredTurn, day_dates, read_day, read_index_checked, read_since_checked,
};

const T: Duration = Duration::from_secs(5);
/// How long one `wait_my_turn` may hang before the client gives up on it.
///
/// This is also how long a bridge can stay deaf after the daemon dies: the poll notices only when
/// its request fails. A real process death closes the socket and is noticed at once; a daemon that
/// stops answering without closing is noticed here. `ROZUM_MEETING_WAIT_SECS` exists so a test can
/// shorten the window — production never sets it, and 30 s is the value it has always had.
fn wait_timeout() -> Duration {
    std::env::var("ROZUM_MEETING_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30))
}

type ClientResult<U> = Result<U, Box<dyn std::error::Error + Send + Sync>>;

/// A room as seen in the picker.
#[derive(Clone, Debug)]
pub struct RoomInfo {
    pub name: String,
    pub project: Option<String>,
    pub root: PathBuf,
    pub topic: String,
    pub participants: u64,
    pub last_date: Option<String>,
}

/// How a room was entered — the poll connection rejoins it the same way.
#[derive(Clone, Debug)]
enum JoinSpec {
    Project(String),
    Named(String),
}

/// How this local daemon client appears in the room roster. The public
/// constructors deliberately default to `Human`; transports use
/// [`MeetingClient::connect_bridge_as`] so a messenger relay is never
/// misrepresented as the operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientKind {
    Human,
    Bridge,
}

impl ClientKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Bridge => "bridge",
        }
    }
}

struct PollSession {
    token: String,
    name: String,
    kind: ClientKind,
}

pub struct MeetingClient {
    conn: RoomConnection,
    sock: PathBuf,
    display_name: String,
    session_token: String,
    kind: ClientKind,
    participant_id: Option<String>,
    room_name: Option<String>,
    room_root: Option<PathBuf>,
    join_spec: Option<JoinSpec>,
    /// `(date, n)` high-water the human has seen — the `wait` cursor.
    cursor: Option<(String, u64)>,
    /// Transcript loaded for rendering (current day, plus any scrolled-in days).
    transcript: Vec<StoredTurn>,
    /// Earliest day currently loaded (for scrollback).
    oldest_loaded_date: Option<String>,
}

impl MeetingClient {
    /// Connect to the daemon as `display_name` (a fresh, ephemeral session token).
    pub async fn connect(sock: &Path, display_name: &str) -> ClientResult<Self> {
        Self::connect_with(
            sock,
            display_name,
            uuid::Uuid::new_v4().simple().to_string(),
            ClientKind::Human,
        )
        .await
    }

    /// Connect with a **stable** session token — the local human's persisted identity
    /// ([`super::local_identity`]), so all this machine's human clients map to one participant
    /// (handle) across launches instead of a fresh random one.
    pub async fn connect_as(sock: &Path, display_name: &str, token: &str) -> ClientResult<Self> {
        Self::connect_with(sock, display_name, token.to_owned(), ClientKind::Human).await
    }

    /// Connect a transport bridge with a stable token. The same token is reused
    /// by the dedicated poll connection, so both sockets bind to one roster
    /// participant and self-authored messages can be suppressed reliably.
    pub(crate) async fn connect_bridge_as(
        sock: &Path,
        display_name: &str,
        token: &str,
    ) -> ClientResult<Self> {
        Self::connect_with(sock, display_name, token.to_owned(), ClientKind::Bridge).await
    }

    async fn connect_with(
        sock: &Path,
        display_name: &str,
        token: String,
        kind: ClientKind,
    ) -> ClientResult<Self> {
        let conn = RoomConnection::connect(sock, display_name, T).await?;
        Ok(Self {
            conn,
            sock: sock.to_path_buf(),
            display_name: display_name.to_owned(),
            session_token: token,
            kind,
            participant_id: None,
            room_name: None,
            room_root: None,
            join_spec: None,
            cursor: None,
            transcript: vec![],
            oldest_loaded_date: None,
        })
    }

    pub fn room_name(&self) -> Option<&str> {
        self.room_name.as_deref()
    }
    pub fn participant_id(&self) -> Option<&str> {
        self.participant_id.as_deref()
    }
    /// The joined room's on-disk dir — content is read directly from here (the daemon's
    /// single-writer/direct-read contract). Used by the web client to tail the transcript.
    pub fn room_root(&self) -> Option<&Path> {
        self.room_root.as_deref()
    }
    pub fn transcript(&self) -> &[StoredTurn] {
        &self.transcript
    }

    /// Rooms known to the daemon (for the picker).
    pub async fn list_rooms(&mut self) -> ClientResult<Vec<RoomInfo>> {
        let r = self.conn.call_tool("rooms.list", json!({}), T).await?;
        let v = tool_result_text_json(&r).ok_or("bad rooms.list")?;
        let mut out = vec![];
        if let Some(rooms) = v.get("rooms").and_then(Value::as_array) {
            for room in rooms {
                let Some(name) = room.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let project = room
                    .get("project")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let root = room
                    .get("root")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default();
                out.push(RoomInfo {
                    name: name.to_owned(),
                    project,
                    root,
                    topic: room
                        .get("topic")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    participants: room
                        .get("participants")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    last_date: room
                        .get("last_date")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
        }
        Ok(out)
    }

    /// Enter (and create, lazily) the room for `project`.
    pub async fn enter_project(&mut self, project: &str) -> ClientResult<String> {
        let r = self
            .conn
            .call_tool(
                "_join_internal",
                json!({
                    "client_info_name": self.display_name,
                    "project": project,
                    "session_token": self.session_token,
                    "kind": self.kind.as_str(),
                }),
                T,
            )
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad _join_internal")?;
        let room = v
            .get("room")
            .and_then(Value::as_str)
            .ok_or("no room in result")?
            .to_owned();
        self.participant_id = v
            .get("participant_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.room_name = Some(room.clone());
        self.room_root = Some(RoomPaths::for_project(Path::new(project)).root);
        self.join_spec = Some(JoinSpec::Project(project.to_owned()));
        self.initialize_after_join(&v)?;
        Ok(room)
    }

    /// Enter a room chosen from the picker.
    pub async fn enter_named(&mut self, info: &RoomInfo) -> ClientResult<()> {
        let r = self
            .conn
            .call_tool(
                "rooms.join",
                json!({
                    "name": info.name,
                    "client_info_name": self.display_name,
                    "session_token": self.session_token,
                    "kind": self.kind.as_str(),
                }),
                T,
            )
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad rooms.join")?;
        self.participant_id = v
            .get("participant_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.room_name = Some(info.name.clone());
        self.room_root = Some(info.root.clone());
        self.join_spec = Some(JoinSpec::Named(info.name.clone()));
        self.initialize_after_join(&v)?;
        Ok(())
    }

    /// Create + enter a new ad-hoc room (the picker's "new room").
    pub async fn new_room(&mut self, topic: Option<&str>) -> ClientResult<String> {
        let mut args = json!({
            "client_info_name": self.display_name,
            "session_token": self.session_token,
            "kind": self.kind.as_str(),
        });
        if let Some(t) = topic {
            args["topic"] = json!(t);
        }
        let r = self.conn.call_tool("rooms.new", args, T).await?;
        let v = tool_result_text_json(&r).ok_or("bad rooms.new")?;
        let name = v
            .get("room")
            .and_then(Value::as_str)
            .ok_or("no room in result")?
            .to_owned();
        self.participant_id = v
            .get("participant_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let root = v
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or("no root in result")?;
        self.room_name = Some(name.clone());
        self.room_root = Some(root);
        self.join_spec = Some(JoinSpec::Named(name.clone()));
        self.initialize_after_join(&v)?;
        Ok(name)
    }

    /// Create-or-open a **named** room (idempotent `rooms.new`) and enter it — used for a
    /// shared room like `commons` that may not exist yet (unlike `enter_named`, which opens
    /// an existing room only).
    pub async fn enter_or_create(&mut self, name: &str) -> ClientResult<String> {
        let r = self
            .conn
            .call_tool(
                "rooms.new",
                json!({
                    "name": name,
                    "client_info_name": self.display_name,
                    "session_token": self.session_token,
                    "kind": self.kind.as_str(),
                }),
                T,
            )
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad rooms.new")?;
        let room = v
            .get("room")
            .and_then(Value::as_str)
            .ok_or("no room in result")?
            .to_owned();
        self.participant_id = v
            .get("participant_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let root = v
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or("no root in result")?;
        self.room_name = Some(room.clone());
        self.room_root = Some(root);
        self.join_spec = Some(JoinSpec::Named(room.clone()));
        self.initialize_after_join(&v)?;
        Ok(room)
    }

    /// Spawn a dedicated background connection that long-polls the current room
    /// and streams new messages — so the UI loop never has to cancel an
    /// in-flight `wait_my_turn` (which would leak a daemon long-poll). Returns the
    /// receiver of new-message batches + the task handle (abort it on switch/quit).
    /// Start the next poll from a known high-water instead of from "now".
    ///
    /// A reconnecting caller has already delivered turns up to some `(date, n)`; without this the
    /// fresh poll begins at the room's current head and everything said during the outage is
    /// skipped. The files are on disk either way — this is what lets the delta cover the gap.
    pub fn set_cursor(&mut self, cursor: Option<(String, u64)>) {
        self.cursor = cursor;
    }

    pub fn spawn_poll(&self) -> (mpsc::Receiver<Vec<StoredTurn>>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<Vec<StoredTurn>>(64);
        let (Some(spec), Some(root)) = (self.join_spec.clone(), self.room_root.clone()) else {
            // No room yet → an immediately-finished task (the caller will respawn
            // after entering a room).
            return (rx, tokio::spawn(async {}));
        };
        let sock = self.sock.clone();
        let session = PollSession {
            token: self.session_token.clone(),
            name: self.display_name.clone(),
            kind: self.kind,
        };
        let cursor = self.cursor.clone();
        let handle = tokio::spawn(poll_loop(sock, spec, session, root, cursor, tx));
        (rx, handle)
    }

    /// Post a message as the human.
    pub async fn submit(&mut self, content: &str) -> ClientResult<()> {
        self.submit_with_meta(content, json!({})).await
    }

    /// Post with optional support metadata (kind/thread_id/severity/tags) merged into the submit args.
    pub async fn submit_with_meta(&mut self, content: &str, meta: Value) -> ClientResult<()> {
        let mut args = json!({ "content": content });
        if let (Value::Object(a), Value::Object(m)) = (&mut args, &meta) {
            for (k, v) in m {
                a.insert(k.clone(), v.clone());
            }
        }
        let r = self.conn.call_tool("meeting.submit", args, T).await?;
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("submit rejected: {r}").into());
        }
        Ok(())
    }

    /// Call an arbitrary room MCP tool (e.g. `meeting.thread_open`, `meeting.escalate`) and return its
    /// parsed JSON payload (the text content decoded as JSON, or the raw result if it isn't JSON text).
    /// Errors if the tool reports `isError`. The generic lever behind the `meetings incident` verbs.
    pub async fn call(&mut self, tool: &str, args: Value) -> ClientResult<Value> {
        let r = self.conn.call_tool(tool, args, T).await?;
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("{tool} rejected: {}", result_text(&r)).into());
        }
        Ok(result_json(&r))
    }

    /// Long-poll for new messages; append them to the transcript and return them.
    pub async fn poll(&mut self) -> ClientResult<Vec<StoredTurn>> {
        let since_cursor = self.cursor.clone();
        let since = match &since_cursor {
            Some((d, n)) => json!({ "since_date": d, "since_n": n }),
            None => json!({}),
        };
        let r = self
            .conn
            .call_tool("meeting.wait_my_turn", since, wait_timeout())
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad wait result")?;
        let still_waiting = v
            .get("still_waiting")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let next_cursor = cursor_from_result(&v).ok_or("meeting wait omitted high_water")?;
        // New messages: read the content straight from the room's day files.
        let mut new = vec![];
        if !still_waiting {
            let root = self
                .room_root
                .as_ref()
                .ok_or("joined room has no store root")?;
            let (sd, sn) = match &since_cursor {
                Some((d, n)) => (Some(d.as_str()), *n),
                None => (None, 0),
            };
            new = read_delta_to(root, sd, sn, &next_cursor)?;
            if !delta_is_contiguous(&new, since_cursor.as_ref(), &next_cursor) {
                return Err("meeting store delta did not reach daemon high-water".into());
            }
        }
        self.cursor = Some(next_cursor);
        self.transcript.extend(new.iter().cloned());
        Ok(new)
    }

    /// Fetch the day before the earliest loaded day (advancing the scrollback
    /// marker). Returns its messages for the caller to prepend; empty at the
    /// earliest day.
    pub fn prev_day_turns(&mut self) -> Vec<StoredTurn> {
        let (Some(root), Some(oldest)) = (self.room_root.clone(), self.oldest_loaded_date.clone())
        else {
            return vec![];
        };
        match prev_day(&root, &oldest) {
            Some((prev, turns)) => {
                self.oldest_loaded_date = Some(prev);
                turns
            }
            None => vec![],
        }
    }

    /// (Re)load the current (newest) day from disk and set the cursor.
    fn reload_current_day(&mut self) {
        self.transcript.clear();
        self.oldest_loaded_date = None;
        self.cursor = None;
        let Some(root) = &self.room_root else { return };
        let dates = day_dates(root);
        let Some(last) = dates.last() else { return };
        if let Ok(turns) = read_day(root, last, 0, None) {
            self.cursor = Some((last.clone(), turns.len() as u64));
            self.oldest_loaded_date = Some(last.clone());
            self.transcript = turns;
        }
    }

    fn initialize_after_join(&mut self, result: &Value) -> ClientResult<()> {
        if self.kind == ClientKind::Bridge {
            self.transcript.clear();
            self.oldest_loaded_date = None;
            self.cursor = Some(cursor_from_result(result).ok_or(
                "meeting daemon join omitted high_water; restart the daemon with the current binary",
            )?);
        } else {
            self.reload_current_day();
        }
        Ok(())
    }
}

/// Dedicated poll loop on its own connection (the TUI's second connection):
/// rejoin the room with the shared `token`, then long-poll forever, reading each
/// delta from disk and pushing it down `tx`. Exits when the room ends, the socket
/// dies, or the receiver is dropped.
async fn poll_loop(
    sock: PathBuf,
    spec: JoinSpec,
    session: PollSession,
    root: PathBuf,
    mut cursor: Option<(String, u64)>,
    tx: mpsc::Sender<Vec<StoredTurn>>,
) {
    let Ok(mut conn) = RoomConnection::connect(&sock, &session.name, T).await else {
        return;
    };
    // Rejoin with the same session_token → the same participant identity.
    let join = match &spec {
        JoinSpec::Project(p) => {
            conn.call_tool(
                "_join_internal",
                json!({ "client_info_name": session.name, "project": p, "session_token": session.token, "kind": session.kind.as_str() }),
                T,
            )
            .await
        }
        JoinSpec::Named(n) => {
            conn.call_tool(
                "rooms.join",
                json!({ "name": n, "client_info_name": session.name, "session_token": session.token, "kind": session.kind.as_str() }),
                T,
            )
            .await
        }
    };
    let Ok(join) = join else {
        return;
    };
    if join.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }

    loop {
        let prev = cursor.clone();
        let since = match &prev {
            Some((d, n)) => json!({ "since_date": d, "since_n": n }),
            None => json!({}),
        };
        let Ok(r) = conn.call_tool("meeting.wait_my_turn", since, wait_timeout()).await else {
            return; // socket died → end the stream (UI can respawn)
        };
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(v) = tool_result_text_json(&r) else {
            return;
        };
        if v.get("ended").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let still_waiting = v
            .get("still_waiting")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let Some(next_cursor) = cursor_from_result(&v) else {
            return;
        };
        if !still_waiting {
            let (sd, sn) = match &prev {
                Some((d, n)) => (Some(d.as_str()), *n),
                None => (None, 0),
            };
            let Ok(turns) = read_delta_to(&root, sd, sn, &next_cursor) else {
                return;
            };
            if !delta_is_contiguous(&turns, prev.as_ref(), &next_cursor) {
                return;
            }
            if !turns.is_empty() && tx.send(turns).await.is_err() {
                return; // UI gone
            }
        }
        cursor = Some(next_cursor);
    }
}

fn cursor_from_result(value: &Value) -> Option<(String, u64)> {
    let high_water = value.get("high_water")?;
    Some((
        high_water.get("date")?.as_str()?.to_owned(),
        high_water.get("n")?.as_u64()?,
    ))
}

fn read_delta_to(
    root: &Path,
    since_date: Option<&str>,
    since_n: u64,
    high_water: &(String, u64),
) -> std::io::Result<Vec<StoredTurn>> {
    let index = read_index_checked(root)?;
    let expected = expected_delta_len(&index, since_date.map(|date| (date, since_n)), high_water)?;
    let mut turns = read_since_checked(root, since_date, since_n)?;
    turns.retain(|turn| (turn.date.as_str(), turn.n) < (high_water.0.as_str(), high_water.1));
    if u64::try_from(turns.len()).ok() != Some(expected) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "meeting store delta count does not reach daemon high-water",
        ));
    }
    Ok(turns)
}

fn expected_delta_len(
    index: &Index,
    previous: Option<(&str, u64)>,
    high_water: &(String, u64),
) -> std::io::Result<u64> {
    let invalid =
        |message: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let high_stat = index
        .days
        .get(&high_water.0)
        .ok_or_else(|| invalid("meeting store index omits daemon high-water day"))?;
    if high_water.1 > high_stat.count {
        return Err(invalid("meeting store index is behind daemon high-water"));
    }

    let mut expected = 0_u64;
    let lower_date = match previous {
        Some((date, _)) if date > high_water.0.as_str() => {
            return Err(invalid("meeting cursor is after daemon high-water"));
        }
        Some((date, n)) if date == high_water.0 => {
            return high_water
                .1
                .checked_sub(n)
                .ok_or_else(|| invalid("meeting cursor is after daemon high-water"));
        }
        Some((date, n)) => {
            let previous_stat = index
                .days
                .get(date)
                .ok_or_else(|| invalid("meeting store index omits cursor day"))?;
            expected = previous_stat
                .count
                .checked_sub(n)
                .ok_or_else(|| invalid("meeting cursor exceeds indexed day"))?;
            Some(date)
        }
        None => None,
    };

    for (date, stat) in &index.days {
        if date.as_str() >= high_water.0.as_str()
            || lower_date.is_some_and(|lower| date.as_str() <= lower)
        {
            continue;
        }
        expected = expected
            .checked_add(stat.count)
            .ok_or_else(|| invalid("meeting store delta count overflow"))?;
    }
    expected
        .checked_add(high_water.1)
        .ok_or_else(|| invalid("meeting store delta count overflow"))
}

fn delta_is_contiguous(
    turns: &[StoredTurn],
    previous: Option<&(String, u64)>,
    next: &(String, u64),
) -> bool {
    if previous == Some(next) {
        return turns.is_empty();
    }
    let Some(first) = turns.first() else {
        return false;
    };
    let expected_first_n = previous
        .filter(|(date, _)| date == &first.date)
        .map(|(_, n)| *n)
        .unwrap_or(0);
    if first.n != expected_first_n {
        return false;
    }
    if turns.windows(2).any(|pair| {
        let left = &pair[0];
        let right = &pair[1];
        if left.date == right.date {
            right.n != left.n + 1
        } else {
            right.date <= left.date || right.n != 0
        }
    }) {
        return false;
    }
    turns
        .last()
        .is_some_and(|last| last.date == next.0 && last.n + 1 == next.1)
}

/// The day immediately before `oldest` in `root`, with its messages.
fn prev_day(root: &Path, oldest: &str) -> Option<(String, Vec<StoredTurn>)> {
    let dates = day_dates(root);
    let idx = dates.iter().position(|d| d == oldest)?;
    if idx == 0 {
        return None;
    }
    let prev = dates[idx - 1].clone();
    let turns = read_day(root, &prev, 0, None).ok()?;
    Some((prev, turns))
}

/// Where a one-shot [`post_once`] message goes.
pub enum PostTarget {
    /// The canonical room of this git project (a project path).
    Project(String),
    /// A named room from `rooms.list` — must already exist.
    Named(String),
    /// A shared room by name, created-or-opened (`ROZUM_MEETING_ROOM`, e.g. `commons`).
    Shared(String),
}

/// One-shot post: connect as `display`, join `target`, submit `text`, return the room name.
/// The shared transport for `rozum meetings post` and the coordination hooks (SessionStart/Stop).
/// The daemon must already be reachable at `sock` — the caller ensures it's up.
pub async fn post_once(
    sock: &Path,
    target: PostTarget,
    display: &str,
    token: Option<&str>,
    text: &str,
    meta: Value,
) -> ClientResult<String> {
    let mut client = match token {
        Some(t) => MeetingClient::connect_as(sock, display, t).await?,
        None => MeetingClient::connect(sock, display).await?,
    };
    let room = match target {
        PostTarget::Project(p) => client.enter_project(&p).await?,
        PostTarget::Shared(name) => client.enter_or_create(&name).await?,
        PostTarget::Named(name) => {
            let info = client
                .list_rooms()
                .await?
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| format!("no room named '{name}'"))?;
            client.enter_named(&info).await?;
            name
        }
    };
    client.submit_with_meta(text, meta).await?;
    Ok(room)
}

/// Extract the first text content of an MCP `CallToolResult` value as a string (empty if absent).
fn result_text(r: &Value) -> String {
    r.get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Decode an MCP `CallToolResult`'s text content as JSON; falls back to the raw value if it isn't JSON.
fn result_json(r: &Value) -> Value {
    let text = result_text(r);
    serde_json::from_str(&text).unwrap_or_else(|_| r.clone())
}

/// One-shot room tool call: connect as `display`, join `target`, call `tool(args)`, return its JSON.
/// The shared transport for the `rozum meetings incident …` verbs (thread open/escalate/resolve/…),
/// mirroring `post_once` but for the thread MCP tools. The daemon must already be reachable at `sock`.
pub async fn call_once(
    sock: &Path,
    target: PostTarget,
    display: &str,
    token: Option<&str>,
    tool: &str,
    args: Value,
) -> ClientResult<Value> {
    let mut client = match token {
        Some(t) => MeetingClient::connect_as(sock, display, t).await?,
        None => MeetingClient::connect(sock, display).await?,
    };
    match target {
        PostTarget::Project(p) => {
            client.enter_project(&p).await?;
        }
        PostTarget::Shared(name) => {
            client.enter_or_create(&name).await?;
        }
        PostTarget::Named(name) => {
            let info = client
                .list_rooms()
                .await?
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| format!("no room named '{name}'"))?;
            client.enter_named(&info).await?;
        }
    };
    client.call(tool, args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::daemon::serve_daemon;
    use crate::meeting::registry::RoomRegistry;
    use crate::meeting::store::{DayStat, TranscriptWriter};
    use std::sync::Arc;
    use tempfile::tempdir;

    async fn wait_for_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("socket never appeared");
    }

    fn turn(date: &str, n: u64) -> StoredTurn {
        StoredTurn {
            date: date.to_owned(),
            n,
            ..StoredTurn::default()
        }
    }

    #[test]
    fn store_delta_must_be_contiguous_through_daemon_high_water() {
        let previous = ("2026-07-20".to_owned(), 2);
        let next = ("2026-07-20".to_owned(), 4);
        assert!(delta_is_contiguous(
            &[turn("2026-07-20", 2), turn("2026-07-20", 3)],
            Some(&previous),
            &next,
        ));
        assert!(!delta_is_contiguous(
            &[turn("2026-07-20", 3)],
            Some(&previous),
            &next,
        ));
        assert!(!delta_is_contiguous(&[], Some(&previous), &next));
    }

    #[test]
    fn indexed_delta_count_proves_rollover_tail() {
        let mut index = Index::default();
        index
            .days
            .insert("2026-07-20".to_owned(), DayStat { count: 4, bytes: 0 });
        index
            .days
            .insert("2026-07-21".to_owned(), DayStat { count: 3, bytes: 0 });
        index
            .days
            .insert("2026-07-22".to_owned(), DayStat { count: 2, bytes: 0 });

        assert_eq!(
            expected_delta_len(
                &index,
                Some(("2026-07-20", 2)),
                &("2026-07-22".to_owned(), 1),
            )
            .unwrap(),
            6,
            "old-day tail + complete middle day + high-water prefix"
        );
        assert_eq!(
            expected_delta_len(
                &index,
                Some(("2026-07-22", 0)),
                &("2026-07-22".to_owned(), 2),
            )
            .unwrap(),
            2
        );
        assert!(
            expected_delta_len(
                &index,
                Some(("2026-07-19", 0)),
                &("2026-07-22".to_owned(), 1),
            )
            .is_err(),
            "an omitted cursor day cannot silently lose its rollover tail"
        );
    }

    #[tokio::test]
    async fn human_enters_submits_loads_and_tails() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("meeting.sock");
        let registry = Arc::new(RoomRegistry::new(dir.path().join("state")));
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let _ = serve_daemon(&sock, registry).await;
            });
        }
        wait_for_socket(&sock).await;

        let project = tempdir().unwrap();
        let mut client = MeetingClient::connect(&sock, "alice").await.unwrap();

        let room = client
            .enter_project(&project.path().to_string_lossy())
            .await
            .unwrap();
        assert_eq!(room, project.path().file_name().unwrap().to_string_lossy());
        assert!(client.transcript().is_empty(), "unspoken room is empty");

        client.submit("hello from alice").await.unwrap();
        let new = client.poll().await.unwrap();
        assert!(new.iter().any(|t| t.content == "hello from alice"));
        assert!(
            client
                .transcript()
                .iter()
                .any(|t| t.content == "hello from alice")
        );

        // The room is now discoverable in the picker.
        let rooms = client.list_rooms().await.unwrap();
        assert!(rooms.iter().any(|r| r.name == room));
    }

    #[tokio::test]
    async fn poll_stream_delivers_new_messages() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("meeting.sock");
        let registry = Arc::new(RoomRegistry::new(dir.path().join("state")));
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let _ = serve_daemon(&sock, registry).await;
            });
        }
        wait_for_socket(&sock).await;

        let project = tempdir().unwrap();
        let mut client = MeetingClient::connect(&sock, "alice").await.unwrap();
        client
            .enter_project(&project.path().to_string_lossy())
            .await
            .unwrap();

        // The poll loop runs on its own connection (same session_token → same
        // identity), so submitting on the action connection is picked up.
        let (mut rx, handle) = client.spawn_poll();
        client.submit("hi from poll test").await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("poll stream delivered within 3s")
            .expect("a batch");
        assert!(got.iter().any(|t| t.content == "hi from poll test"));
        handle.abort();
    }

    #[test]
    fn prev_day_scrollback_walks_back() {
        // Two day files written directly to a room root.
        let dir = tempdir().unwrap();
        let paths = RoomPaths::for_project(dir.path());
        let mut w = TranscriptWriter::new(paths.clone(), "r", "", None, dir.path().join("state"));
        let d0 = w.append("p", "P", "day0-a", 1_718_000_000).unwrap().date;
        let d1 = w
            .append("p", "P", "day1-a", 1_718_000_000 + 86_400)
            .unwrap()
            .date;
        assert_ne!(d0, d1);

        // Newest day first; scrollback finds the previous day.
        let (prev, turns) = prev_day(&paths.root, &d1).expect("a previous day exists");
        assert_eq!(prev, d0);
        assert_eq!(turns[0].content, "day0-a");
        // At the earliest day there is nothing older.
        assert!(prev_day(&paths.root, &d0).is_none());
    }

    #[tokio::test]
    async fn post_once_lands_in_the_project_room() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("meeting.sock");
        let registry = Arc::new(RoomRegistry::new(dir.path().join("state")));
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let _ = serve_daemon(&sock, registry).await;
            });
        }
        wait_for_socket(&sock).await;

        let project = tempdir().unwrap();
        let proj = project.path().to_string_lossy().into_owned();
        let room = post_once(
            &sock,
            PostTarget::Project(proj.clone()),
            "tester",
            None,
            "joined: working on X",
            json!({}),
        )
        .await
        .expect("post_once succeeds");
        assert!(!room.is_empty());

        // A fresh client sees the posted message in the room transcript.
        let mut reader = MeetingClient::connect(&sock, "reader").await.unwrap();
        reader.enter_project(&proj).await.unwrap();
        assert!(
            reader.transcript().iter().any(|t| t.content == "joined: working on X"),
            "the one-shot post is in the room transcript"
        );

        // An unknown named room is a clean error, not a panic.
        assert!(
            post_once(&sock, PostTarget::Named("nope".into()), "tester", None, "x", json!({}))
                .await
                .is_err()
        );

        // Shared(name) create-or-opens the room (no prior existence needed), and a second
        // post reuses it.
        let r1 = post_once(&sock, PostTarget::Shared("commons".into()), "a", None, "hi commons", json!({}))
            .await
            .expect("shared post creates + posts");
        assert_eq!(r1, "commons");
        post_once(&sock, PostTarget::Shared("commons".into()), "b", None, "again", json!({}))
            .await
            .expect("second shared post reuses the room");
        let mut reader = MeetingClient::connect(&sock, "reader2").await.unwrap();
        reader.enter_or_create("commons").await.unwrap();
        let contents: Vec<_> = reader.transcript().iter().map(|t| t.content.clone()).collect();
        assert!(contents.contains(&"hi commons".to_string()) && contents.contains(&"again".to_string()));
    }
}
