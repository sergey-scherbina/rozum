mod bot;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::meeting::room_client::{RoomConnection, tool_result_text_json};
use crate::meeting::room_path::room_socket;
use bot::{DiscordBot, IncomingMessage};

type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// GUILD_MESSAGES + MESSAGE_CONTENT (privileged — must be enabled in Discord Dev Portal)
const INTENTS: u64 = 512 | 32768;

pub async fn run_bridge(
    room: &str,
    display_name: &str,
    token: String,
    channel_id: String,
) -> BridgeResult<()> {
    let socket_path = room_socket(room);
    if !socket_path.exists() {
        return Err(format!("room not found: {room}").into());
    }

    let mut conn = RoomConnection::connect(&socket_path, display_name, Duration::from_secs(5))
        .await
        .map_err(|e| format!("connect to room: {e}"))?;

    let join_result = conn
        .call_tool(
            "_join_internal",
            serde_json::json!({ "client_info_name": display_name, "kind": "bridge" }),
            Duration::from_secs(5),
        )
        .await
        .map_err(|e| format!("join room: {e}"))?;

    let join_payload = tool_result_text_json(&join_result).ok_or("invalid join response")?;
    let my_id = join_payload["participant_id"]
        .as_str()
        .unwrap_or(display_name)
        .to_owned();
    eprintln!("[discord-bridge] joined room '{room}' as '{my_id}'");

    let bot = Arc::new(DiscordBot::new(token, channel_id));
    let conn = Arc::new(Mutex::new(conn));

    let poller_bot = Arc::clone(&bot);
    let poller_conn = Arc::clone(&conn);
    tokio::spawn(async move {
        discord_gateway_poller(poller_bot, poller_conn).await;
    });

    room_loop(conn, bot, my_id).await
}

async fn discord_gateway_poller(bot: Arc<DiscordBot>, conn: Arc<Mutex<RoomConnection>>) {
    const MIN_RECONNECT_DELAY_SECS: u64 = 5;
    const MAX_RECONNECT_DELAY_SECS: u64 = 60;
    let mut reconnect_delay_secs = MIN_RECONNECT_DELAY_SECS;

    loop {
        let gateway_url = match bot.get_gateway_url().await {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[discord-bridge] failed to get gateway URL: {e}");
                tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                reconnect_delay_secs =
                    (reconnect_delay_secs.saturating_mul(2)).min(MAX_RECONNECT_DELAY_SECS);
                continue;
            }
        };
        if let Err(e) = gateway_session(&bot.token, &bot.channel_id, &gateway_url, &conn).await {
            eprintln!("[discord-bridge] gateway error: {e}");
            tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
            reconnect_delay_secs =
                (reconnect_delay_secs.saturating_mul(2)).min(MAX_RECONNECT_DELAY_SECS);
        } else {
            reconnect_delay_secs = MIN_RECONNECT_DELAY_SECS;
            tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
        }
    }
}

async fn gateway_session(
    token: &str,
    channel_id: &str,
    gateway_url: &str,
    conn: &Arc<Mutex<RoomConnection>>,
) -> BridgeResult<()> {
    let (ws, _) = connect_async(gateway_url).await?;
    let (mut write, mut read) = ws.split();

    // Receive HELLO (op 10) and extract heartbeat interval.
    let hello_msg = read.next().await.ok_or("gateway closed before HELLO")??;
    let Message::Text(hello_text) = hello_msg else {
        return Err("expected text HELLO frame".into());
    };
    let hello: serde_json::Value = serde_json::from_str(&hello_text)?;
    if hello["op"].as_u64() != Some(10) {
        return Err(format!("expected HELLO (op 10), got op={}", hello["op"]).into());
    }
    let heartbeat_ms = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41250);

    // Send IDENTIFY (op 2).
    let identify = serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": INTENTS,
            "properties": { "os": "macos", "browser": "rozum", "device": "rozum" }
        }
    });
    write.send(Message::Text(identify.to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_ms));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                };
                match msg {
                    Message::Text(text) => {
                        handle_dispatch(&text, channel_id, conn).await;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                let hb = serde_json::json!({ "op": 1, "d": null });
                if write.send(Message::Text(hb.to_string())).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_dispatch(text: &str, channel_id: &str, conn: &Arc<Mutex<RoomConnection>>) {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    // op 7 = RECONNECT, op 9 = INVALID_SESSION — caller handles reconnect via loop
    match payload["op"].as_u64() {
        Some(0) => {} // DISPATCH — continue below
        _ => return,
    }
    if payload["t"].as_str() != Some("MESSAGE_CREATE") {
        return;
    }
    let d = &payload["d"];
    if d["channel_id"].as_str() != Some(channel_id) {
        return;
    }
    let content = d["content"].as_str().unwrap_or("").trim();
    if content.is_empty() {
        return;
    }
    let sender = d["author"]["global_name"]
        .as_str()
        .or_else(|| d["author"]["username"].as_str())
        .unwrap_or("user")
        .to_owned();
    let msg = IncomingMessage {
        sender_name: sender,
        text: content.to_owned(),
    };
    let payload = format!("[{}]: {}", msg.sender_name, msg.text);
    let mut c = conn.lock().await;
    let _ = c
        .call_tool(
            "meeting.submit",
            serde_json::json!({ "content": payload }),
            Duration::from_secs(5),
        )
        .await;
}

async fn room_loop(
    conn: Arc<Mutex<RoomConnection>>,
    bot: Arc<DiscordBot>,
    my_id: String,
) -> BridgeResult<()> {
    let mut since_seq: usize = 0;

    loop {
        let result = {
            let mut c = conn.lock().await;
            c.call_tool(
                "meeting.wait_my_turn",
                serde_json::json!({ "since_seq": since_seq }),
                Duration::from_secs(35),
            )
            .await
            .map_err(|e| format!("wait_my_turn: {e}"))?
        };

        let payload = tool_result_text_json(&result).ok_or("invalid wait_my_turn response")?;

        if payload["ended"].as_bool() == Some(true) {
            eprintln!("[discord-bridge] room ended");
            return Ok(());
        }

        if payload["still_waiting"].as_bool() == Some(true) {
            if let Some(seq) = payload["seq"].as_u64() {
                since_seq = seq as usize;
            }
            continue;
        }

        let turn = &payload["turn"];
        if let Some(seq) = turn["seq"].as_u64() {
            since_seq = seq as usize;
        }

        if let Some(delta) = turn["transcript_delta"].as_array() {
            for entry in delta {
                let speaker = entry["display_name"].as_str().unwrap_or("?");
                let content = entry["content"].as_str().unwrap_or("").trim();
                if speaker != my_id && !content.is_empty() {
                    let text = format!("[{}]: {}", speaker, content);
                    if let Err(e) = bot.send_message(&text).await {
                        eprintln!("[discord-bridge] send_message error: {e}");
                    }
                }
            }
        }
    }
}
