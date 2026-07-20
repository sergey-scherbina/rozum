//! Shared daemon-side machinery for external messenger bridges.
//!
//! Platform modules own their HTTP/Gateway protocols. This module owns the
//! trust policy, safe text splitting, and the one room contract they share:
//! join the multi-room daemon as `kind=bridge`, submit on the action socket,
//! and tail new canonical store entries on a dedicated poll socket.

use std::collections::HashSet;
use std::path::Path;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::meeting::daemon::daemon_alive;
use crate::meeting::daemon_proxy::spawn_daemon;
use crate::meeting::room_path::meeting_sock;
use crate::meeting::store::StoredTurn;
use crate::meeting::tui_client::MeetingClient;

pub type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SenderPolicy {
    All,
    Only(HashSet<String>),
}

impl SenderPolicy {
    /// Resolve a comma-separated allowlist. `*` is accepted only by itself so
    /// broad trust is always conspicuous. `fallback` is used for Telegram's
    /// one-user private-chat case; group/Discord callers pass `None` and an
    /// omitted variable becomes a startup error.
    pub fn resolve(
        raw: Option<&str>,
        fallback: Option<String>,
        variable: &str,
    ) -> BridgeResult<Self> {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            Some("*") => Ok(Self::All),
            Some(value) => {
                let mut ids = HashSet::new();
                for item in value.split(',') {
                    let id = item.trim();
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(format!(
                            "{variable} must be comma-separated numeric user IDs or '*'"
                        )
                        .into());
                    }
                    ids.insert(id.to_owned());
                }
                if ids.is_empty() {
                    return Err(format!("{variable} contains no user IDs").into());
                }
                Ok(Self::Only(ids))
            }
            None => match fallback {
                Some(id) => Ok(Self::Only(HashSet::from([id]))),
                None => Err(format!(
                    "{variable} is required; set numeric user IDs or '*' to trust the whole target chat"
                )
                .into()),
            },
        }
    }

    pub fn allows(&self, sender_id: impl AsRef<str>) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => ids.contains(sender_id.as_ref()),
        }
    }
}

/// Split text without breaking UTF-8, bounded by UTF-16 code units (the
/// conservative common denominator for messenger character accounting).
/// Prefer a newline, then whitespace, near the end of each chunk.
pub fn split_text(text: &str, max_utf16_units: usize) -> Vec<String> {
    assert!(
        max_utf16_units >= 2,
        "a Unicode scalar may need two UTF-16 units"
    );
    let mut rest = text.trim();
    let mut chunks = Vec::new();

    while !rest.is_empty() {
        let mut used = 0usize;
        let mut hard_cut = rest.len();
        for (idx, ch) in rest.char_indices() {
            let units = ch.len_utf16();
            if used + units > max_utf16_units {
                hard_cut = idx;
                break;
            }
            used += units;
        }

        if hard_cut == rest.len() {
            chunks.push(rest.to_owned());
            break;
        }

        let window = &rest[..hard_cut];
        let preferred = window
            .rfind('\n')
            .map(|idx| idx + 1)
            .filter(|idx| *idx > 0)
            .or_else(|| {
                window
                    .char_indices()
                    .rev()
                    .find(|(_, ch)| ch.is_whitespace())
                    .map(|(idx, ch)| idx + ch.len_utf8())
                    .filter(|idx| *idx > 0)
            })
            .unwrap_or(hard_cut);

        let chunk = rest[..preferred].trim_end();
        if !chunk.is_empty() {
            chunks.push(chunk.to_owned());
        }
        rest = rest[preferred..].trim_start();
    }

    chunks
}

/// One messenger's joined room session. The action client never long-polls;
/// `MeetingClient::spawn_poll` owns a second connection with the same stable
/// session token.
pub struct DaemonBridge {
    client: MeetingClient,
    participant_id: String,
    room_rx: mpsc::Receiver<Vec<StoredTurn>>,
    poll_task: JoinHandle<()>,
}

impl DaemonBridge {
    pub async fn connect(
        room: &str,
        display_name: &str,
        platform: &str,
        target: &str,
    ) -> BridgeResult<Self> {
        let sock = meeting_sock();
        if !daemon_alive(&sock).await {
            spawn_daemon().await;
        }
        if !daemon_alive(&sock).await {
            return Err("meeting daemon did not start".into());
        }
        Self::connect_at(&sock, room, display_name, platform, target).await
    }

    async fn connect_at(
        sock: &Path,
        room: &str,
        display_name: &str,
        platform: &str,
        target: &str,
    ) -> BridgeResult<Self> {
        let token = stable_session_token(platform, room, target);
        let mut client = MeetingClient::connect_bridge_as(sock, display_name, &token).await?;
        let info = client
            .list_rooms()
            .await?
            .into_iter()
            .find(|candidate| candidate.name == room)
            .ok_or_else(|| format!("room not found in meeting daemon: {room}"))?;
        client.enter_named(&info).await?;
        let participant_id = client
            .participant_id()
            .ok_or("daemon join returned no participant_id")?
            .to_owned();
        let (room_rx, poll_task) = client.spawn_poll();
        Ok(Self {
            client,
            participant_id,
            room_rx,
            poll_task,
        })
    }

    pub fn participant_id(&self) -> &str {
        &self.participant_id
    }

    pub async fn submit(
        &mut self,
        sender_name: &str,
        sender_id: &str,
        text: &str,
    ) -> BridgeResult<()> {
        let sender = sender_name.trim().replace(['\r', '\n', '[', ']'], " ");
        let sender_id = sender_id.trim();
        let text = text.trim();
        if sender.is_empty() || sender_id.is_empty() || text.is_empty() {
            return Ok(());
        }
        if !sender_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("messenger sender ID must be numeric".into());
        }
        self.client
            .submit(&format!("[{sender} #{sender_id}]: {text}"))
            .await
            .map_err(|e| format!("submit to meeting daemon: {e}").into())
    }

    /// Wait for the next daemon delta and render only non-self, non-empty room
    /// entries for the messenger. An ended poll stream is an error so a process
    /// supervisor can restart the bridge after a daemon restart.
    pub async fn next_outbound(&mut self) -> BridgeResult<Vec<String>> {
        let turns = self
            .room_rx
            .recv()
            .await
            .ok_or("meeting daemon poll ended")?;
        Ok(render_outbound(&turns, &self.participant_id))
    }
}

impl Drop for DaemonBridge {
    fn drop(&mut self) {
        self.poll_task.abort();
    }
}

fn stable_session_token(platform: &str, room: &str, target: &str) -> String {
    let identity = format!("{platform}\0{room}\0{target}");
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, identity.as_bytes());
    format!("messenger-bridge-v1-{id}")
}

fn render_outbound(turns: &[StoredTurn], participant_id: &str) -> Vec<String> {
    turns
        .iter()
        .filter(|turn| turn.participant_id != participant_id && !turn.content.trim().is_empty())
        .map(|turn| {
            let speaker = if turn.display_name.trim().is_empty() {
                turn.participant_id.as_str()
            } else {
                turn.display_name.as_str()
            };
            format!("[{speaker}]: {}", turn.content.trim())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::daemon::serve_daemon;
    use crate::meeting::registry::RoomRegistry;
    use crate::meeting::tui_client::MeetingClient;
    use std::sync::Arc;
    use std::time::Duration;
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

    #[test]
    fn sender_policy_is_explicit_and_deny_by_default() {
        let only = SenderPolicy::resolve(Some(" 12,34 "), None, "IDS").unwrap();
        assert!(only.allows("12"));
        assert!(!only.allows("56"));
        assert_eq!(
            SenderPolicy::resolve(Some("*"), None, "IDS").unwrap(),
            SenderPolicy::All
        );
        assert!(SenderPolicy::resolve(None, None, "IDS").is_err());
        assert!(SenderPolicy::resolve(Some("*,12"), None, "IDS").is_err());
        assert!(SenderPolicy::resolve(Some("not-an-id"), None, "IDS").is_err());
    }

    #[test]
    fn split_text_is_utf8_safe_and_utf16_bounded() {
        let text = "alpha βeta\n".repeat(8) + &"😀".repeat(9);
        let chunks = split_text(&text, 18);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= 18)
        );
        assert_eq!(
            chunks.join(" ").split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn bridge_identity_token_is_stable_and_target_scoped() {
        let token = stable_session_token("telegram", "ops", "42");
        assert_eq!(token, stable_session_token("telegram", "ops", "42"));
        assert_ne!(token, stable_session_token("telegram", "ops", "43"));
        assert_ne!(token, stable_session_token("discord", "ops", "42"));
    }

    #[tokio::test]
    async fn daemon_bridge_skips_history_and_self_but_relays_new_turns() {
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

        let mut human = MeetingClient::connect(&sock, "alice").await.unwrap();
        human.enter_or_create("bridge-test").await.unwrap();
        human.submit("old history must not replay").await.unwrap();

        let mut bridge =
            DaemonBridge::connect_at(&sock, "bridge-test", "telegram", "telegram", "42")
                .await
                .unwrap();
        let status = bridge
            .client
            .call("meeting.status", serde_json::json!({}))
            .await
            .unwrap();
        assert!(status["participants"].as_array().unwrap().iter().any(|p| {
            p["id"].as_str() == Some(bridge.participant_id())
                && p["kind"].as_str() == Some("bridge")
        }));

        human.submit("new outbound").await.unwrap();
        let outbound = tokio::time::timeout(Duration::from_secs(3), bridge.next_outbound())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outbound, vec!["[alice]: new outbound"]);
        assert!(!outbound.iter().any(|line| line.contains("old history")));

        let mut discord =
            DaemonBridge::connect_at(&sock, "bridge-test", "discord", "discord", "99")
                .await
                .unwrap();
        bridge.submit("Sergiy", "123", "incoming").await.unwrap();
        let self_batch = tokio::time::timeout(Duration::from_secs(3), bridge.next_outbound())
            .await
            .unwrap()
            .unwrap();
        assert!(self_batch.is_empty(), "bridge must not echo its own submit");
        let cross_bridge = tokio::time::timeout(Duration::from_secs(3), discord.next_outbound())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cross_bridge,
            vec!["[telegram]: [Sergiy #123]: incoming"],
            "independently attached bridges share the room's new transcript"
        );

        let seen = human.poll().await.unwrap();
        assert!(
            seen.iter()
                .any(|turn| turn.content == "[Sergiy #123]: incoming")
        );
    }
}
