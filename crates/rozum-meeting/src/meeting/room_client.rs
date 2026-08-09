use std::{path::Path, time::Duration};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use super::ipc::{ReadHalf, WriteHalf};

type RoomClientResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Make one MCP tool call against a room unix socket.
pub async fn call_room_tool(
    socket_path: &Path,
    tool_name: &str,
    arguments: Value,
    client_name: &str,
    timeout: Duration,
) -> RoomClientResult<Value> {
    let mut connection = RoomConnection::connect(socket_path, client_name, timeout).await?;
    connection.call_tool(tool_name, arguments, timeout).await
}

pub struct RoomConnection {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    next_id: u64,
}

impl RoomConnection {
    pub async fn connect(
        socket_path: &Path,
        client_name: &str,
        timeout: Duration,
    ) -> RoomClientResult<Self> {
        let stream = connect_socket(socket_path).await?;
        let (read_half, mut write_half) = super::ipc::split(stream);
        let mut reader = BufReader::new(read_half);

        write_json(
            &mut write_half,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": client_name,
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
        )
        .await?;
        let initialize = read_response(&mut reader, 1, timeout).await?;
        if let Some(error) = initialize.get("error") {
            return Err(format!("initialize error: {error}").into());
        }

        write_json(
            &mut write_half,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }),
        )
        .await?;

        Ok(Self {
            reader,
            writer: write_half,
            next_id: 2,
        })
    }

    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> RoomClientResult<Value> {
        let id = self.next_id;
        self.next_id += 1;

        write_json(
            &mut self.writer,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": arguments
                }
            }),
        )
        .await?;

        let response = read_response(&mut self.reader, id, timeout).await?;
        if let Some(error) = response.get("error") {
            return Err(format!("tool call error: {error}").into());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "missing tool result".into())
    }
}

async fn connect_socket(socket_path: &Path) -> RoomClientResult<super::ipc::Stream> {
    let stream = tokio::time::timeout(Duration::from_secs(5), super::ipc::connect(socket_path))
        .await
        .map_err(|_| "connection timeout")?
        .map_err(|e| format!("connect error: {e}"))?;
    Ok(stream)
}

pub fn tool_result_text_json(result: &Value) -> Option<Value> {
    let text = result
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|content| {
            if content.get("type")?.as_str()? == "text" {
                content.get("text")?.as_str()
            } else {
                None
            }
        })?;
    serde_json::from_str(text).ok()
}

async fn write_json<W>(writer: &mut W, value: Value) -> RoomClientResult<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut msg = serde_json::to_vec(&value)?;
    msg.push(b'\n');
    writer.write_all(&msg).await?;
    Ok(())
}

async fn read_response<R>(reader: &mut R, id: u64, timeout: Duration) -> RoomClientResult<Value>
where
    R: AsyncBufReadExt + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut line = String::new();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("read timeout".into());
        }

        let bytes_read = tokio::time::timeout(remaining, reader.read_line(&mut line))
            .await
            .map_err(|_| "read timeout")?
            .map_err(|e| format!("read error: {e}"))?;
        if bytes_read == 0 {
            return Err("connection closed before response".into());
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response: Value = serde_json::from_str(trimmed)?;
        if response.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(response);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::ServiceExt;
    use tempfile::tempdir;
    use tokio::net::UnixListener;
    use tokio::sync::{Mutex, broadcast};

    use super::*;
    use crate::meeting::budget::BudgetGuard;
    use crate::meeting::mcp_server::{ConnParticipant, RoomServer};
    use crate::meeting::state::{Meeting, MeetingEvent};

    #[tokio::test]
    async fn calls_room_tool_after_mcp_initialize() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (events_tx, _) = broadcast::channel::<MeetingEvent>(8);
        let meeting = Arc::new(Mutex::new(Meeting::new(
            "test-room",
            "debug proxy",
            "sergiy",
            BudgetGuard::default(),
            events_tx,
        )));
        let meeting_for_accept = Arc::clone(&meeting);

        let accept_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let conn_participant: ConnParticipant = Arc::new(Mutex::new(None));
                let peer_registry = Arc::new(Mutex::new(std::collections::HashMap::new()));
                let handler = RoomServer::new(
                    Arc::clone(&meeting_for_accept),
                    conn_participant,
                    peer_registry,
                );
                tokio::spawn(async move {
                    if let Ok(service) = handler.serve(stream).await {
                        service.waiting().await.ok();
                    }
                });
            }
        });

        let info = call_room_tool(
            &socket_path,
            "_room_info",
            serde_json::json!({}),
            "rozum-test",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let payload = tool_result_text_json(&info).unwrap();
        assert_eq!(payload["name"], "test-room");
        assert_eq!(payload["topic"], "debug proxy");
        assert_eq!(payload["participants"], serde_json::json!(["sergiy"]));

        let mut connection = RoomConnection::connect(&socket_path, "codex", Duration::from_secs(2))
            .await
            .unwrap();
        let join = connection
            .call_tool(
                "_join_internal",
                serde_json::json!({ "client_info_name": "codex" }),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let payload = tool_result_text_json(&join).unwrap();
        assert_eq!(payload["participant_id"], "codex");
        assert_eq!(
            payload["participants"],
            serde_json::json!(["sergiy", "codex"])
        );

        let status = connection
            .call_tool(
                "meeting.status",
                serde_json::json!({}),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        let payload = tool_result_text_json(&status).unwrap();
        let participants = payload["participants"].as_array().unwrap();
        assert_eq!(participants.len(), 2);
        assert_eq!(participants[1]["display_name"], "codex");

        accept_task.abort();
    }
}
