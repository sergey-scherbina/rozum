mod bot;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use bot::{DiscordBot, IncomingMessage};

use crate::messenger::{BridgeResult, DaemonBridge, SenderPolicy};

// GUILD_MESSAGES + MESSAGE_CONTENT (privileged — must be enabled in Discord Dev Portal).
const INTENTS: u64 = 512 | 32768;
const GATEWAY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTHY_GATEWAY_SESSION: Duration = Duration::from_secs(60);
const MIN_RECONNECT_DELAY_SECS: u64 = 5;
const MAX_RECONNECT_DELAY_SECS: u64 = 60;

/// Public CLI entry shared by the compatibility gateway command and the thin
/// `rozum-meet discord` frontend.
pub async fn run_from_env(room: &str, display_name: &str) -> BridgeResult<()> {
    let token = std::env::var("DISCORD_BOT_TOKEN").map_err(|_| "DISCORD_BOT_TOKEN is not set")?;
    let channel_id =
        std::env::var("DISCORD_CHANNEL_ID").map_err(|_| "DISCORD_CHANNEL_ID is not set")?;
    run_bridge(room, display_name, token, channel_id).await
}

pub async fn run_bridge(
    room: &str,
    display_name: &str,
    token: String,
    channel_id: String,
) -> BridgeResult<()> {
    let configured_allowlist = std::env::var("DISCORD_ALLOWED_USER_IDS").ok();
    let policy = SenderPolicy::resolve(
        configured_allowlist.as_deref(),
        None,
        "DISCORD_ALLOWED_USER_IDS",
    )?;

    let bot = Arc::new(DiscordBot::new(token, channel_id.clone()));
    let startup = bot.validate_startup().await?;

    // Validate the external trust boundary before joining the internal room.
    let mut room_bridge = DaemonBridge::connect(room, display_name, "discord", &channel_id).await?;
    eprintln!(
        "[discord-bridge] bot '{}' joined daemon room '{}' as '{}' (channel {})",
        startup.bot_username,
        room,
        room_bridge.participant_id(),
        channel_id
    );

    // Gateway heartbeats and ACK reads must never wait behind room/HTTP work.
    // Allowed events are queued losslessly for this process; the explicit
    // sender policy is the trust boundary for queue growth.
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<IncomingMessage>();
    let gateway_bot = Arc::clone(&bot);
    let mut gateway = tokio::spawn(async move {
        discord_gateway_poller(
            gateway_bot,
            startup.gateway_url,
            startup.bot_user_id,
            policy,
            incoming_tx,
        )
        .await
    });

    let result = loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(message) = incoming else {
                    break Err("Discord Gateway receive loop ended".into());
                };
                if let Err(error) = room_bridge
                    .submit(&message.sender_name, &message.sender_id, &message.text)
                    .await
                {
                    break Err(error);
                }
            }
            outbound = room_bridge.next_outbound() => {
                let messages = match outbound {
                    Ok(messages) => messages,
                    Err(error) => break Err(error),
                };
                for message in messages {
                    if let Err(error) = bot.send_message(&message).await {
                        eprintln!("[discord-bridge] send message error: {error}");
                    }
                }
            }
            finished = &mut gateway => {
                break match finished {
                    Ok(Ok(())) => Err("Discord Gateway receive loop ended".into()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(format!("Discord Gateway task failed: {error}").into()),
                };
            }
        }
    };

    gateway.abort();
    result
}

async fn discord_gateway_poller(
    bot: Arc<DiscordBot>,
    gateway_url: String,
    bot_user_id: String,
    policy: SenderPolicy,
    incoming_tx: mpsc::UnboundedSender<IncomingMessage>,
) -> BridgeResult<()> {
    // A fresh IDENTIFY is rate-limited by Discord. This bridge does not Resume,
    // so every reconnect waits at least one 5-second identify window.
    let mut reconnect_delay_secs = MIN_RECONNECT_DELAY_SECS;

    loop {
        let mut ready_at = None;
        let outcome = gateway_session(
            &bot.token,
            &bot.channel_id,
            &gateway_url,
            &bot_user_id,
            &policy,
            &incoming_tx,
            &mut ready_at,
        )
        .await;
        let healthy_session =
            ready_at.is_some_and(|ready_at: Instant| ready_at.elapsed() >= HEALTHY_GATEWAY_SESSION);
        if healthy_session {
            reconnect_delay_secs = MIN_RECONNECT_DELAY_SECS;
        }
        match outcome {
            Ok(GatewayEnd::Reconnect(reason)) => {
                eprintln!("[discord-bridge] Gateway reconnect: {reason}");
            }
            Ok(GatewayEnd::Fatal(reason)) => return Err(reason.into()),
            Err(error) => {
                eprintln!("[discord-bridge] Gateway error: {error}");
            }
        }
        if incoming_tx.is_closed() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
        reconnect_delay_secs = reconnect_delay_after_session(reconnect_delay_secs, healthy_session);
    }
}

fn reconnect_delay_after_session(current_secs: u64, healthy_session: bool) -> u64 {
    if healthy_session {
        MIN_RECONNECT_DELAY_SECS
    } else {
        current_secs.saturating_mul(2).min(MAX_RECONNECT_DELAY_SECS)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum GatewayEnd {
    Reconnect(&'static str),
    Fatal(String),
}

async fn gateway_session(
    token: &str,
    channel_id: &str,
    gateway_url: &str,
    bot_user_id: &str,
    policy: &SenderPolicy,
    incoming_tx: &mpsc::UnboundedSender<IncomingMessage>,
    ready_at: &mut Option<Instant>,
) -> BridgeResult<GatewayEnd> {
    let (ws, _) = tokio::time::timeout(GATEWAY_HANDSHAKE_TIMEOUT, connect_async(gateway_url))
        .await
        .map_err(|_| "Discord Gateway connection timed out")??;
    let (mut write, mut read) = ws.split();

    // Receive HELLO (op 10) and extract the server-owned heartbeat interval.
    let hello_msg = tokio::time::timeout(GATEWAY_HANDSHAKE_TIMEOUT, read.next())
        .await
        .map_err(|_| "Discord Gateway HELLO timed out")?
        .ok_or("Gateway closed before HELLO")??;
    let Message::Text(hello_text) = hello_msg else {
        return Err("expected text HELLO frame".into());
    };
    let hello: serde_json::Value = serde_json::from_str(hello_text.as_str())?;
    if hello["op"].as_u64() != Some(10) {
        return Err(format!("expected HELLO (op 10), got op={}", hello["op"]).into());
    }
    let heartbeat_ms = hello["d"]["heartbeat_interval"]
        .as_u64()
        .ok_or("HELLO omitted heartbeat_interval")?
        .max(1);

    // IDENTIFY starts a fresh session. Correct Resume state is intentionally a
    // future optimization; reconnects remain bounded and single-shard.
    let identify = serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": INTENTS,
            "properties": { "os": std::env::consts::OS, "browser": "rozum", "device": "rozum" }
        }
    });
    write.send(Message::Text(identify.to_string())).await?;

    // A deterministic half-interval is valid jitter and avoids a reconnect
    // herd without adding nondeterminism to offline tests.
    let period = Duration::from_millis(heartbeat_ms);
    let mut heartbeat = interval_at(Instant::now() + period / 2, period);
    let mut last_sequence: Option<u64> = None;
    let mut awaiting_ack = false;

    loop {
        tokio::select! {
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(GatewayEnd::Reconnect("connection closed")),
                };
                match msg {
                    Message::Text(text) => {
                        let payload: serde_json::Value = match serde_json::from_str(text.as_str()) {
                            Ok(payload) => payload,
                            Err(_) => continue,
                        };
                        if let Some(sequence) = payload["s"].as_u64() {
                            last_sequence = Some(sequence);
                        }
                        match payload["op"].as_u64() {
                            Some(0) => {
                                if payload["t"].as_str() == Some("READY") {
                                    ready_at.get_or_insert_with(Instant::now);
                                }
                                if let Some(message) = parse_message_create(
                                    &payload,
                                    channel_id,
                                    bot_user_id,
                                    policy,
                                ) {
                                    if incoming_tx.send(message).is_err() {
                                        return Ok(GatewayEnd::Fatal("bridge input closed".to_owned()));
                                    }
                                }
                            }
                            Some(1) => {
                                send_heartbeat(&mut write, last_sequence).await?;
                                awaiting_ack = true;
                                heartbeat.reset_after(period);
                            }
                            Some(7) => return Ok(GatewayEnd::Reconnect("server requested reconnect")),
                            Some(9) => return Ok(GatewayEnd::Reconnect("invalid session; re-identifying")),
                            Some(11) => awaiting_ack = false,
                            _ => {}
                        }
                    }
                    Message::Close(frame) => {
                        let code = frame.as_ref().map(|frame| u16::from(frame.code));
                        return Ok(classify_gateway_close(code));
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_ack {
                    return Ok(GatewayEnd::Reconnect("heartbeat ACK missing"));
                }
                send_heartbeat(&mut write, last_sequence).await?;
                awaiting_ack = true;
            }
        }
    }
}

fn classify_gateway_close(code: Option<u16>) -> GatewayEnd {
    match code {
        Some(4004) => GatewayEnd::Fatal("Discord Gateway rejected the bot token".to_owned()),
        Some(4010) => {
            GatewayEnd::Fatal("Discord Gateway rejected the shard configuration".to_owned())
        }
        Some(4011) => GatewayEnd::Fatal(
            "Discord requires sharding; the single-shard bridge cannot connect".to_owned(),
        ),
        Some(4012) => GatewayEnd::Fatal("Discord Gateway rejected API version 10".to_owned()),
        Some(4013) => {
            GatewayEnd::Fatal("Discord Gateway rejected the requested intents".to_owned())
        }
        Some(4014) => GatewayEnd::Fatal(
            "Discord Message Content intent is not enabled/approved in the Developer Portal"
                .to_owned(),
        ),
        _ => GatewayEnd::Reconnect("Gateway close frame"),
    }
}

async fn send_heartbeat<S>(write: &mut S, last_sequence: Option<u64>) -> BridgeResult<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let heartbeat = serde_json::json!({ "op": 1, "d": last_sequence });
    write
        .send(Message::Text(heartbeat.to_string()))
        .await
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
}

fn parse_message_create(
    payload: &serde_json::Value,
    channel_id: &str,
    bot_user_id: &str,
    policy: &SenderPolicy,
) -> Option<IncomingMessage> {
    if payload["op"].as_u64() != Some(0) || payload["t"].as_str() != Some("MESSAGE_CREATE") {
        return None;
    }
    let data = &payload["d"];
    if data["channel_id"].as_str() != Some(channel_id) || data.get("webhook_id").is_some() {
        return None;
    }
    let author = &data["author"];
    let sender_id = author["id"].as_str()?;
    if !bot::is_snowflake(sender_id)
        || sender_id == bot_user_id
        || author["bot"].as_bool() == Some(true)
        || !policy.allows(sender_id)
    {
        return None;
    }
    let content = data["content"].as_str()?.trim();
    if content.is_empty() {
        return None;
    }
    let sender_name = author["global_name"]
        .as_str()
        .or_else(|| author["username"].as_str())
        .unwrap_or("user")
        .to_owned();
    Some(IncomingMessage {
        sender_id: sender_id.to_owned(),
        sender_name,
        text: content.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(author: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "op": 0,
            "t": "MESSAGE_CREATE",
            "s": 17,
            "d": {
                "channel_id": "100",
                "content": "hello",
                "author": author,
            }
        })
    }

    #[test]
    fn message_parser_enforces_channel_sender_and_non_bot_boundary() {
        let policy = SenderPolicy::resolve(Some("42"), None, "IDS").unwrap();
        let allowed = message(serde_json::json!({
            "id": "42", "username": "sergiy", "global_name": "Sergiy", "bot": false
        }));
        let parsed = parse_message_create(&allowed, "100", "999", &policy).unwrap();
        assert_eq!(parsed.sender_id, "42");
        assert_eq!(parsed.sender_name, "Sergiy");
        assert_eq!(parsed.text, "hello");

        assert!(parse_message_create(&allowed, "other", "999", &policy).is_none());
        assert!(parse_message_create(&allowed, "100", "42", &policy).is_none());

        let bot = message(serde_json::json!({
            "id": "42", "username": "relay", "bot": true
        }));
        assert!(parse_message_create(&bot, "100", "999", &policy).is_none());

        let unauthorized = message(serde_json::json!({
            "id": "7", "username": "intruder", "bot": false
        }));
        assert!(parse_message_create(&unauthorized, "100", "999", &policy).is_none());
    }

    #[test]
    fn message_parser_rejects_webhooks_empty_and_non_dispatch_payloads() {
        let policy = SenderPolicy::All;
        let mut webhook = message(serde_json::json!({
            "id": "42", "username": "hook", "bot": false
        }));
        webhook["d"]["webhook_id"] = serde_json::json!("55");
        assert!(parse_message_create(&webhook, "100", "999", &policy).is_none());

        let mut empty = message(serde_json::json!({
            "id": "42", "username": "user", "bot": false
        }));
        empty["d"]["content"] = serde_json::json!("   ");
        assert!(parse_message_create(&empty, "100", "999", &policy).is_none());

        let mut other = empty;
        other["t"] = serde_json::json!("READY");
        assert!(parse_message_create(&other, "100", "999", &policy).is_none());
    }

    #[test]
    fn heartbeat_payload_carries_latest_sequence() {
        assert_eq!(
            serde_json::json!({ "op": 1, "d": Some(73_u64) }),
            serde_json::json!({ "op": 1, "d": 73 })
        );
        assert_eq!(
            serde_json::json!({ "op": 1, "d": Option::<u64>::None }),
            serde_json::json!({ "op": 1, "d": null })
        );
    }

    #[test]
    fn non_reconnectable_gateway_close_codes_are_fatal() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert!(matches!(
                classify_gateway_close(Some(code)),
                GatewayEnd::Fatal(_)
            ));
        }
        assert_eq!(
            classify_gateway_close(Some(4009)),
            GatewayEnd::Reconnect("Gateway close frame")
        );
    }

    #[test]
    fn reconnect_backoff_resets_only_after_a_healthy_session() {
        assert_eq!(reconnect_delay_after_session(5, false), 10);
        assert_eq!(reconnect_delay_after_session(10, false), 20);
        assert_eq!(reconnect_delay_after_session(60, false), 60);
        assert_eq!(reconnect_delay_after_session(60, true), 5);
    }
}
