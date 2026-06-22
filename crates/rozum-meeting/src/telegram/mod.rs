mod bot;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use bot::{IncomingMessage, TelegramBot};

use crate::meeting::room_client::{RoomConnection, tool_result_text_json};
use crate::meeting::room_path::room_socket;

type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub async fn run_bridge(
    room: &str,
    display_name: &str,
    token: String,
    chat_id: i64,
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
    eprintln!("[telegram-bridge] joined room '{room}' as '{my_id}'");

    let bot = Arc::new(TelegramBot::new(token, chat_id));
    let conn = Arc::new(Mutex::new(conn));

    // Telegram → room: forward each incoming Telegram message as a submit.
    let poller_bot = Arc::clone(&bot);
    let poller_conn = Arc::clone(&conn);
    tokio::spawn(async move {
        telegram_poller(poller_bot, poller_conn).await;
    });

    // Room → Telegram: forward each new transcript entry from non-self speakers.
    room_loop(conn, bot, my_id).await
}

async fn telegram_poller(bot: Arc<TelegramBot>, conn: Arc<Mutex<RoomConnection>>) {
    const MIN_POLL_ERROR_SECS: u64 = 5;
    const MAX_POLL_ERROR_SECS: u64 = 60;
    let mut poll_error_delay_secs = MIN_POLL_ERROR_SECS;

    let mut offset: i64 = 0;
    loop {
        match bot.get_updates(offset, 30).await {
            Ok(updates) => {
                poll_error_delay_secs = MIN_POLL_ERROR_SECS;
                for update in &updates {
                    offset = update.update_id + 1;
                    if let Some(msg) = TelegramBot::extract_message(update) {
                        let from_target_chat = update
                            .message
                            .as_ref()
                            .map_or(false, |m| m.chat.id == bot.chat_id);
                        if from_target_chat {
                            submit_to_room(&conn, &msg).await;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[telegram-bridge] getUpdates error: {e}");
                tokio::time::sleep(Duration::from_secs(poll_error_delay_secs)).await;
                poll_error_delay_secs =
                    (poll_error_delay_secs.saturating_mul(2)).min(MAX_POLL_ERROR_SECS);
            }
        }
    }
}

async fn submit_to_room(conn: &Arc<Mutex<RoomConnection>>, msg: &IncomingMessage) {
    let content = format!("[{}]: {}", msg.sender_name, msg.text);
    let mut c = conn.lock().await;
    let _ = c
        .call_tool(
            "meeting.submit",
            serde_json::json!({ "content": content }),
            Duration::from_secs(5),
        )
        .await;
}

async fn room_loop(
    conn: Arc<Mutex<RoomConnection>>,
    bot: Arc<TelegramBot>,
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
            eprintln!("[telegram-bridge] room ended");
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
                        eprintln!("[telegram-bridge] send_message error: {e}");
                    }
                }
            }
        }
    }
}
