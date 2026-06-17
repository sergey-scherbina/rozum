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
}

pub struct MeetingClient {
    conn: RoomConnection,
    display_name: String,
    session_token: String,
    room_name: Option<String>,
    room_root: Option<PathBuf>,
    /// `(date, n)` high-water the human has seen — the `wait` cursor.
    cursor: Option<(String, u64)>,
    /// Transcript loaded for rendering (current day, plus any scrolled-in days).
    transcript: Vec<StoredTurn>,
    /// Earliest day currently loaded (for scrollback).
    oldest_loaded_date: Option<String>,
}

impl MeetingClient {
    /// Connect to the daemon as `display_name` (a fresh session token).
    pub async fn connect(sock: &Path, display_name: &str) -> ClientResult<Self> {
        let conn = RoomConnection::connect(sock, display_name, T).await?;
        Ok(Self {
            conn,
            display_name: display_name.to_owned(),
            session_token: uuid::Uuid::new_v4().simple().to_string(),
            room_name: None,
            room_root: None,
            cursor: None,
            transcript: vec![],
            oldest_loaded_date: None,
        })
    }

    pub fn room_name(&self) -> Option<&str> {
        self.room_name.as_deref()
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
        self.reload_current_day();
        Ok(())
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

    /// Scroll back: prepend the previous day's messages. Returns how many were
    /// loaded (0 if already at the earliest day).
    pub fn load_prev_day(&mut self) -> usize {
        let (Some(root), Some(oldest)) = (self.room_root.clone(), self.oldest_loaded_date.clone())
        else {
            return 0;
        };
        match prev_day(&root, &oldest) {
            Some((prev, mut turns)) => {
                let n = turns.len();
                turns.extend(self.transcript.drain(..));
                self.transcript = turns;
                self.oldest_loaded_date = Some(prev);
                n
            }
            None => 0,
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
}
