mod bot;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use bot::{IncomingMessage, TelegramBot};

use crate::meeting::store::rozum_state_dir;
use crate::messenger::{BridgeResult, DaemonBridge, SenderPolicy};

/// Public CLI entry shared by the compatibility gateway command and the thin
/// `rozum-meet telegram` frontend.
pub async fn run_from_env(room: &str, display_name: &str) -> BridgeResult<()> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| "TELEGRAM_BOT_TOKEN is not set")?;
    let chat_id = std::env::var("TELEGRAM_CHAT_ID")
        .map_err(|_| "TELEGRAM_CHAT_ID is not set")?
        .parse::<i64>()
        .map_err(|_| "TELEGRAM_CHAT_ID must be a numeric chat ID")?;
    run_bridge(room, display_name, token, chat_id).await
}

pub async fn run_bridge(
    room: &str,
    display_name: &str,
    token: String,
    chat_id: i64,
) -> BridgeResult<()> {
    let configured_allowlist = std::env::var("TELEGRAM_ALLOWED_USER_IDS").ok();
    run_bridge_with_allowlist(
        room,
        display_name,
        token,
        chat_id,
        configured_allowlist.as_deref(),
    )
    .await
}

async fn run_bridge_with_allowlist(
    room: &str,
    display_name: &str,
    token: String,
    chat_id: i64,
    configured_allowlist: Option<&str>,
) -> BridgeResult<()> {
    let bot = Arc::new(TelegramBot::new(token, chat_id));
    let target = bot.validate().await?;
    let policy = SenderPolicy::resolve(
        configured_allowlist,
        target.private_user_id.map(|id| id.to_string()),
        "TELEGRAM_ALLOWED_USER_IDS",
    )?;

    // Validate the external trust boundary before joining the internal room.
    let mut room_bridge =
        DaemonBridge::connect(room, display_name, "telegram", &chat_id.to_string()).await?;
    eprintln!(
        "[telegram-bridge] bot {} joined daemon room '{}' as '{}' (chat {}, {:?})",
        target.bot_user_id,
        room,
        room_bridge.participant_id(),
        target.chat_id,
        target.kind
    );

    let (incoming_tx, mut incoming_rx) = mpsc::channel::<PendingIncoming>(64);
    let poller_bot = Arc::clone(&bot);
    let bot_user_id = target.bot_user_id;
    let mut poller =
        tokio::spawn(
            async move { telegram_poller(poller_bot, bot_user_id, policy, incoming_tx).await },
        );

    let result = loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(incoming) = incoming else {
                    break Err("Telegram receive loop ended".into());
                };
                let submit = room_bridge
                    .submit(
                        &incoming.message.sender_name,
                        &incoming.message.sender_id.to_string(),
                        &incoming.message.text,
                    )
                    .await;
                let acknowledgement = submit
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = incoming.committed.send(acknowledgement);
                if let Err(error) = submit {
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
                        // TelegramBot rebuilds transport errors without the URL,
                        // so this cannot print the token embedded in that URL.
                        eprintln!("[telegram-bridge] sendMessage error: {error}");
                    }
                }
            }
            finished = &mut poller => {
                break match finished {
                    Ok(Ok(())) => Err("Telegram receive loop ended".into()),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(format!("Telegram receive task failed: {error}").into()),
                };
            }
        }
    };

    poller.abort();
    result
}

struct PendingIncoming {
    message: IncomingMessage,
    /// Telegram confirms an update only when the next `getUpdates` call uses a
    /// higher offset. Do not make that call until the daemon append succeeded.
    committed: oneshot::Sender<Result<(), String>>,
}

async fn telegram_poller(
    bot: Arc<TelegramBot>,
    bot_user_id: i64,
    policy: SenderPolicy,
    incoming_tx: mpsc::Sender<PendingIncoming>,
) -> BridgeResult<()> {
    const MIN_POLL_ERROR_SECS: u64 = 2;
    const MAX_POLL_ERROR_SECS: u64 = 60;
    let mut poll_error_delay_secs = MIN_POLL_ERROR_SECS;

    let cursor_path = telegram_cursor_path(bot_user_id);
    let mut offset = match load_telegram_offset(&cursor_path, bot.chat_id)? {
        Some(offset) => offset,
        None => {
            // A negative offset asks Telegram for the last pending update and
            // forgets everything older. This happens only on first attachment;
            // later restarts resume the durable per-bot cursor below.
            let offset = loop {
                match bot.get_updates(-1, 0).await {
                    Ok(updates) => {
                        break match updates.last() {
                            Some(update) => next_update_offset(update.update_id)?,
                            None => 0,
                        };
                    }
                    Err(error) => {
                        eprintln!("[telegram-bridge] initial getUpdates error: {error}");
                        tokio::time::sleep(Duration::from_secs(poll_error_delay_secs)).await;
                        poll_error_delay_secs =
                            (poll_error_delay_secs.saturating_mul(2)).min(MAX_POLL_ERROR_SECS);
                    }
                }
            };
            save_telegram_offset(&cursor_path, bot.chat_id, offset)?;
            offset
        }
    };
    poll_error_delay_secs = MIN_POLL_ERROR_SECS;

    'poll: loop {
        match bot.get_updates(offset, 30).await {
            Ok(updates) => {
                poll_error_delay_secs = MIN_POLL_ERROR_SECS;
                for update in &updates {
                    let next_offset = next_update_offset(update.update_id)?;
                    let Some(message) =
                        TelegramBot::extract_message(update, bot.chat_id, |sender_id| {
                            policy.allows(sender_id.to_string())
                        })
                    else {
                        save_telegram_offset(&cursor_path, bot.chat_id, next_offset)?;
                        offset = next_offset;
                        continue;
                    };
                    let (committed, confirmation) = oneshot::channel();
                    if incoming_tx
                        .send(PendingIncoming { message, committed })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    match confirmation.await {
                        Ok(Ok(())) => {
                            // Persist only after the daemon append. A crash may
                            // duplicate the narrow append-before-rename window,
                            // but cannot acknowledge and lose an unappended turn.
                            save_telegram_offset(&cursor_path, bot.chat_id, next_offset)?;
                            offset = next_offset;
                            // Confirm this append promptly. Any later entries
                            // from the current batch are intentionally fetched
                            // again by the next call at the new offset.
                            continue 'poll;
                        }
                        Ok(Err(error)) => {
                            return Err(format!(
                                "meeting daemon rejected Telegram update {}: {error}",
                                update.update_id
                            )
                            .into());
                        }
                        Err(_) => return Ok(()),
                    }
                }
            }
            Err(error) => {
                eprintln!("[telegram-bridge] getUpdates error: {error}");
                tokio::time::sleep(Duration::from_secs(poll_error_delay_secs)).await;
                poll_error_delay_secs =
                    (poll_error_delay_secs.saturating_mul(2)).min(MAX_POLL_ERROR_SECS);
            }
        }
    }
}

fn telegram_cursor_path(bot_user_id: i64) -> PathBuf {
    rozum_state_dir()
        .join("messenger-cursors")
        .join("telegram")
        .join(format!("{bot_user_id}.offset"))
}

#[derive(Debug, Deserialize, Serialize)]
struct TelegramCursor {
    chat_id: i64,
    next_offset: i64,
}

fn load_telegram_offset(path: &Path, chat_id: i64) -> BridgeResult<Option<i64>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read Telegram update cursor: {error}").into()),
    };
    let cursor: TelegramCursor = serde_json::from_str(&raw).map_err(
        |_| "Telegram update cursor is corrupt; remove it explicitly to attach from now",
    )?;
    if cursor.next_offset < 0 {
        return Err(
            "Telegram update cursor is corrupt; remove it explicitly to attach from now".into(),
        );
    }
    if cursor.chat_id != chat_id {
        // A bot's update stream is global. Re-targeting it must not reuse an
        // acknowledgement cursor from another chat; attach from "now" again.
        return Ok(None);
    }
    Ok(Some(cursor.next_offset))
}

fn save_telegram_offset(path: &Path, chat_id: i64, offset: i64) -> BridgeResult<()> {
    if offset < 0 {
        return Err("Telegram update cursor cannot be negative".into());
    }
    let parent = path.parent().ok_or("Telegram cursor path has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create Telegram cursor directory: {error}"))?;
    let temporary = path.with_extension(format!("offset.{}.tmp", std::process::id()));
    let cursor = TelegramCursor {
        chat_id,
        next_offset: offset,
    };
    let encoded = serde_json::to_vec(&cursor)
        .map_err(|error| format!("encode Telegram update cursor: {error}"))?;
    std::fs::write(&temporary, encoded)
        .map_err(|error| format!("write Telegram update cursor: {error}"))?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("commit Telegram update cursor: {error}").into());
    }
    Ok(())
}

fn next_update_offset(update_id: i64) -> BridgeResult<i64> {
    update_id
        .checked_add(1)
        .ok_or_else(|| "Telegram update ID overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_cursor_round_trips_and_rejects_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let cursor = directory.path().join("telegram.offset");
        assert_eq!(load_telegram_offset(&cursor, -100).unwrap(), None);

        save_telegram_offset(&cursor, -100, 42).unwrap();
        assert_eq!(load_telegram_offset(&cursor, -100).unwrap(), Some(42));
        assert_eq!(
            load_telegram_offset(&cursor, -200).unwrap(),
            None,
            "a different target is a first attachment, not a cursor resume"
        );

        std::fs::write(&cursor, "not-an-offset\n").unwrap();
        assert!(load_telegram_offset(&cursor, -100).is_err());
    }

    #[test]
    fn update_offset_is_checked() {
        assert_eq!(next_update_offset(7).unwrap(), 8);
        assert!(next_update_offset(i64::MAX).is_err());
    }
}
