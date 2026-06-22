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
use super::store::{RoomPaths, StoredTurn, day_dates, read_day, read_since};

const T: Duration = Duration::from_secs(5);
const WAIT_T: Duration = Duration::from_secs(30);

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

pub struct MeetingClient {
    conn: RoomConnection,
    sock: PathBuf,
    display_name: String,
    session_token: String,
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
        Self::connect_with(sock, display_name, uuid::Uuid::new_v4().simple().to_string()).await
    }

    /// Connect with a **stable** session token — the local human's persisted identity
    /// ([`super::local_identity`]), so all this machine's human clients map to one participant
    /// (handle) across launches instead of a fresh random one.
    pub async fn connect_as(sock: &Path, display_name: &str, token: &str) -> ClientResult<Self> {
        Self::connect_with(sock, display_name, token.to_owned()).await
    }

    async fn connect_with(sock: &Path, display_name: &str, token: String) -> ClientResult<Self> {
        let conn = RoomConnection::connect(sock, display_name, T).await?;
        Ok(Self {
            conn,
            sock: sock.to_path_buf(),
            display_name: display_name.to_owned(),
            session_token: token,
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
                    "kind": "human",
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
        self.room_name = Some(room.clone());
        self.room_root = Some(RoomPaths::for_project(Path::new(project)).root);
        self.join_spec = Some(JoinSpec::Project(project.to_owned()));
        self.reload_current_day();
        Ok(room)
    }

    /// Enter a room chosen from the picker.
    pub async fn enter_named(&mut self, info: &RoomInfo) -> ClientResult<()> {
        self.conn
            .call_tool(
                "rooms.join",
                json!({
                    "name": info.name,
                    "client_info_name": self.display_name,
                    "session_token": self.session_token,
                    "kind": "human",
                }),
                T,
            )
            .await?;
        self.room_name = Some(info.name.clone());
        self.room_root = Some(info.root.clone());
        self.join_spec = Some(JoinSpec::Named(info.name.clone()));
        self.reload_current_day();
        Ok(())
    }

    /// Create + enter a new ad-hoc room (the picker's "new room").
    pub async fn new_room(&mut self, topic: Option<&str>) -> ClientResult<String> {
        let mut args = json!({
            "client_info_name": self.display_name,
            "session_token": self.session_token,
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
        let root = v
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or("no root in result")?;
        self.room_name = Some(name.clone());
        self.room_root = Some(root);
        self.join_spec = Some(JoinSpec::Named(name.clone()));
        self.reload_current_day();
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
                }),
                T,
            )
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad rooms.new")?;
        let room = v.get("room").and_then(Value::as_str).ok_or("no room in result")?.to_owned();
        let root = v
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or("no root in result")?;
        self.room_name = Some(room.clone());
        self.room_root = Some(root);
        self.join_spec = Some(JoinSpec::Named(room.clone()));
        self.reload_current_day();
        Ok(room)
    }

    /// Spawn a dedicated background connection that long-polls the current room
    /// and streams new messages — so the UI loop never has to cancel an
    /// in-flight `wait_my_turn` (which would leak a daemon long-poll). Returns the
    /// receiver of new-message batches + the task handle (abort it on switch/quit).
    pub fn spawn_poll(&self) -> (mpsc::Receiver<Vec<StoredTurn>>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<Vec<StoredTurn>>(64);
        let (Some(spec), Some(root)) = (self.join_spec.clone(), self.room_root.clone()) else {
            // No room yet → an immediately-finished task (the caller will respawn
            // after entering a room).
            return (rx, tokio::spawn(async {}));
        };
        let sock = self.sock.clone();
        let token = self.session_token.clone();
        let name = self.display_name.clone();
        let cursor = self.cursor.clone();
        let handle = tokio::spawn(poll_loop(sock, spec, token, name, root, cursor, tx));
        (rx, handle)
    }

    /// Post a message as the human.
    pub async fn submit(&mut self, content: &str) -> ClientResult<()> {
        let r = self
            .conn
            .call_tool("meeting.submit", json!({ "content": content }), T)
            .await?;
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("submit rejected: {r}").into());
        }
        Ok(())
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
            .call_tool("meeting.wait_my_turn", since, WAIT_T)
            .await?;
        let v = tool_result_text_json(&r).ok_or("bad wait result")?;
        let still_waiting = v
            .get("still_waiting")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if let Some(hw) = v.get("high_water") {
            if let (Some(d), Some(n)) = (
                hw.get("date").and_then(Value::as_str),
                hw.get("n").and_then(Value::as_u64),
            ) {
                self.cursor = Some((d.to_owned(), n));
            }
        }
        // New messages: read the content straight from the room's day files.
        let mut new = vec![];
        if !still_waiting {
            if let Some(root) = &self.room_root {
                let (sd, sn) = match &since_cursor {
                    Some((d, n)) => (Some(d.as_str()), *n),
                    None => (None, 0),
                };
                new = read_since(root, sd, sn);
            }
        }
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
}

/// Dedicated poll loop on its own connection (the TUI's second connection):
/// rejoin the room with the shared `token`, then long-poll forever, reading each
/// delta from disk and pushing it down `tx`. Exits when the room ends, the socket
/// dies, or the receiver is dropped.
async fn poll_loop(
    sock: PathBuf,
    spec: JoinSpec,
    token: String,
    name: String,
    root: PathBuf,
    mut cursor: Option<(String, u64)>,
    tx: mpsc::Sender<Vec<StoredTurn>>,
) {
    let Ok(mut conn) = RoomConnection::connect(&sock, &name, T).await else {
        return;
    };
    // Rejoin with the same session_token → the same participant identity.
    let join = match &spec {
        JoinSpec::Project(p) => {
            conn.call_tool(
                "_join_internal",
                json!({ "client_info_name": name, "project": p, "session_token": token, "kind": "human" }),
                T,
            )
            .await
        }
        JoinSpec::Named(n) => {
            conn.call_tool(
                "rooms.join",
                json!({ "name": n, "client_info_name": name, "session_token": token, "kind": "human" }),
                T,
            )
            .await
        }
    };
    if join.is_err() {
        return;
    }

    loop {
        let prev = cursor.clone();
        let since = match &prev {
            Some((d, n)) => json!({ "since_date": d, "since_n": n }),
            None => json!({}),
        };
        let Ok(r) = conn.call_tool("meeting.wait_my_turn", since, WAIT_T).await else {
            return; // socket died → end the stream (UI can respawn)
        };
        let Some(v) = tool_result_text_json(&r) else {
            continue;
        };
        if v.get("ended").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let still_waiting = v
            .get("still_waiting")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if let Some(hw) = v.get("high_water") {
            if let (Some(d), Some(n)) = (
                hw.get("date").and_then(Value::as_str),
                hw.get("n").and_then(Value::as_u64),
            ) {
                cursor = Some((d.to_owned(), n));
            }
        }
        if !still_waiting {
            let (sd, sn) = match &prev {
                Some((d, n)) => (Some(d.as_str()), *n),
                None => (None, 0),
            };
            let turns = read_since(&root, sd, sn);
            if !turns.is_empty() && tx.send(turns).await.is_err() {
                return; // UI gone
            }
        }
    }
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
    client.submit(text).await?;
    Ok(room)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::daemon::serve_daemon;
    use crate::meeting::registry::RoomRegistry;
    use crate::meeting::store::TranscriptWriter;
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
        let room = post_once(&sock, PostTarget::Project(proj.clone()), "tester", None, "joined: working on X")
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
        assert!(post_once(&sock, PostTarget::Named("nope".into()), "tester", None, "x").await.is_err());

        // Shared(name) create-or-opens the room (no prior existence needed), and a second
        // post reuses it.
        let r1 = post_once(&sock, PostTarget::Shared("commons".into()), "a", None, "hi commons")
            .await
            .expect("shared post creates + posts");
        assert_eq!(r1, "commons");
        post_once(&sock, PostTarget::Shared("commons".into()), "b", None, "again")
            .await
            .expect("second shared post reuses the room");
        let mut reader = MeetingClient::connect(&sock, "reader2").await.unwrap();
        reader.enter_or_create("commons").await.unwrap();
        let contents: Vec<_> = reader.transcript().iter().map(|t| t.content.clone()).collect();
        assert!(contents.contains(&"hi commons".to_string()) && contents.contains(&"again".to_string()));
    }
}
