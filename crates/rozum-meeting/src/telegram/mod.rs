mod bot;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use bot::{IncomingMessage, TelegramBot};

use crate::meeting::store::rozum_state_dir;
use crate::messenger::{BridgeResult, DaemonBridge, SenderPolicy};
use crate::messenger_acl::{Acl, Caps};

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

    // Access control the operator edits LIVE from inside Telegram. The owner is
    // TELEGRAM_OWNER_ID if set, else the private-chat peer (so a personal bot needs
    // no config). The owner has every capability and is the only id that may manage
    // access; members are added via `/grant`.
    let acl_path = Acl::path("telegram");
    let mut acl = Acl::load(&acl_path);
    let owner = std::env::var("TELEGRAM_OWNER_ID")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .or(target.private_user_id);
    if let Some(o) = owner {
        if acl.ensure_owner(o) {
            let _ = acl.save(&acl_path);
        }
    }

    // The env allowlist stays as an additional accept path; the owner is its fallback so a
    // group bridge with TELEGRAM_OWNER_ID set (and no allowlist) starts with just the owner,
    // who then grants others. Chat access = owner OR ACL `chat` OR env allowlist.
    let policy = SenderPolicy::resolve(
        configured_allowlist,
        owner.map(|id| id.to_string()),
        "TELEGRAM_ALLOWED_USER_IDS",
    )?;

    // Validate the external trust boundary before joining the internal room.
    let mut room_bridge =
        DaemonBridge::connect(room, display_name, "telegram", &chat_id.to_string()).await?;
    eprintln!(
        "[telegram-bridge] bot {} joined daemon room '{}' as '{}' (chat {}, {:?}); owner {:?}, acl {}",
        target.bot_user_id,
        room,
        room_bridge.participant_id(),
        target.chat_id,
        target.kind,
        acl.owner,
        acl_path.display(),
    );

    // Register the command menu (Menu button / `/` list). Non-fatal: text commands work regardless.
    if let Err(e) = bot.set_my_commands(BOT_COMMANDS).await {
        eprintln!("[telegram-bridge] setMyCommands failed (menu unavailable): {e}");
    }

    let (incoming_tx, mut incoming_rx) = mpsc::channel::<PendingIncoming>(64);
    let poller_bot = Arc::clone(&bot);
    let bot_user_id = target.bot_user_id;
    let mut poller =
        tokio::spawn(async move { telegram_poller(poller_bot, bot_user_id, incoming_tx).await });

    // Owner is pinged once per unknown sender so they can /grant that id.
    let mut notified: std::collections::HashSet<i64> = std::collections::HashSet::new();

    let result = loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(incoming) = incoming else {
                    break Err("Telegram receive loop ended".into());
                };
                let id = incoming.message.sender_id;
                let name = incoming.message.sender_name.clone();
                let text = incoming.message.text.trim().to_string();

                // Management/utility commands are handled here and NEVER relayed to the room.
                if text.starts_with('/') {
                    let reply = handle_command(&text, id, &name, &mut acl, &acl_path);
                    if let Err(error) = bot.send_message(&reply).await {
                        eprintln!("[telegram-bridge] sendMessage (command) error: {error}");
                    }
                    let _ = incoming.committed.send(Ok(()));
                    continue;
                }

                // Chat authorization: owner, an ACL member with `chat`, or the env allowlist.
                let allowed =
                    acl.is_owner(id) || acl.caps_for(id).chat || policy.allows(id.to_string());
                if !allowed {
                    if notified.insert(id) {
                        let hint = format!(
                            "🔒 {name} (id {id}) написал(а) боту, но доступа нет. Добавить: \
                             /grant {id} chat  (можно + read write shell)."
                        );
                        if let Err(error) = bot.send_message(&hint).await {
                            eprintln!("[telegram-bridge] sendMessage (notify) error: {error}");
                        }
                    }
                    let _ = incoming.committed.send(Ok(()));
                    continue;
                }

                let submit = room_bridge.submit(&name, &id.to_string(), &text).await;
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
                    // Accept every non-bot text message from the target chat; authorization,
                    // command handling, and per-user gating happen in the bridge's main loop
                    // (which owns the mutable ACL and can reply to the sender).
                    let Some(message) =
                        TelegramBot::extract_message(update, bot.chat_id, |_| true)
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

/// The bot's Telegram command menu (name, description) — registered at startup via
/// `setMyCommands`, so they appear behind the Menu button and the `/` list. Mirrors
/// the commands `handle_command` accepts.
const BOT_COMMANDS: &[(&str, &str)] = &[
    ("help", "Справка и список команд"),
    ("whoami", "Показать мой Telegram id"),
    ("members", "Кто имеет доступ (для владельца)"),
    ("grant", "Дать доступ: /grant <id> chat read write shell"),
    ("revoke", "Убрать доступ: /revoke <id>"),
];

const HELP_TEXT: &str = "Команды бота:\n\
/whoami — показать твой Telegram id\n\
/members — кто имеет доступ (только владелец)\n\
/grant <id> [chat read write shell | all] — дать/изменить доступ (владелец)\n\
/revoke <id> — убрать доступ (владелец)\n\
/help — эта справка\n\n\
Права: chat=писать в чате, read=читать файлы, write=писать файлы, shell=команды в песочнице.";

const NOT_OWNER: &str = "Управлять доступом может только владелец бота.";

/// Handle a `/command` from a Telegram user. Utility commands (`/help`, `/whoami`)
/// are open to everyone; management commands (`/members`, `/grant`, `/revoke`) are
/// owner-only. Mutates + persists the ACL on grant/revoke. Returns the reply text.
fn handle_command(text: &str, sender_id: i64, sender_name: &str, acl: &mut Acl, acl_path: &Path) -> String {
    let mut parts = text.split_whitespace();
    // Telegram group commands may carry a `@BotName` suffix — strip it.
    let cmd = parts
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_owner = acl.is_owner(sender_id);

    match cmd.as_str() {
        "/help" | "/start" => HELP_TEXT.to_string(),
        "/whoami" | "/id" => format!("Твой Telegram id: {sender_id}\nИмя: {sender_name}"),
        "/members" | "/who" => {
            if !is_owner {
                return NOT_OWNER.to_string();
            }
            render_members(acl)
        }
        "/grant" | "/add" => {
            if !is_owner {
                return NOT_OWNER.to_string();
            }
            let Some(id_str) = parts.next() else {
                return "Использование: /grant <id> [chat read write shell | all]".to_string();
            };
            let Ok(id) = id_str.parse::<i64>() else {
                return format!("id должен быть числом, а не '{id_str}'.");
            };
            if acl.is_owner(id) {
                return "Это владелец — у него уже все права.".to_string();
            }
            let tokens: Vec<&str> = parts.collect();
            let caps = if tokens.is_empty() {
                Caps { chat: true, ..Default::default() }
            } else {
                match Caps::parse_tokens(tokens) {
                    Ok(c) => c,
                    Err(e) => return e,
                }
            };
            acl.grant(id, "", caps);
            if let Err(e) = acl.save(acl_path) {
                return format!("Не удалось сохранить ACL: {e}");
            }
            format!("✅ Доступ обновлён: id {id} → {}", caps.summary())
        }
        "/revoke" | "/remove" | "/kick" => {
            if !is_owner {
                return NOT_OWNER.to_string();
            }
            let Some(id_str) = parts.next() else {
                return "Использование: /revoke <id>".to_string();
            };
            let Ok(id) = id_str.parse::<i64>() else {
                return format!("id должен быть числом, а не '{id_str}'.");
            };
            if acl.revoke(id) {
                if let Err(e) = acl.save(acl_path) {
                    return format!("Не удалось сохранить ACL: {e}");
                }
                format!("🚫 Доступ удалён: id {id}")
            } else {
                format!("id {id} не найден среди добавленных.")
            }
        }
        _ => "Неизвестная команда. /help — список команд.".to_string(),
    }
}

fn render_members(acl: &Acl) -> String {
    let mut lines = vec!["👥 Доступ к боту:".to_string()];
    match acl.owner {
        Some(o) => lines.push(format!("• владелец: id {o} — все права")),
        None => lines.push("• владелец не задан".to_string()),
    }
    if acl.members.is_empty() {
        lines.push("• собеседников пока нет".to_string());
    } else {
        for (id, m) in &acl.members {
            let name = if m.name.trim().is_empty() {
                "(без имени)".to_string()
            } else {
                m.name.clone()
            };
            lines.push(format!("• {name} — id {id} — {}", m.caps.summary()));
        }
    }
    lines.push(String::new());
    lines.push("Команды: /grant <id> chat read write shell · /revoke <id> · /whoami".to_string());
    lines.join("\n")
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

    #[test]
    fn owner_can_grant_revoke_and_list_members() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        let mut acl = Acl::default();
        acl.ensure_owner(1);

        // whoami is open to anyone
        assert!(handle_command("/whoami", 999, "Guest", &mut acl, &path).contains("999"));

        // a non-owner cannot manage
        assert_eq!(handle_command("/grant 5 chat", 999, "Guest", &mut acl, &path), NOT_OWNER);
        assert!(acl.members.is_empty());

        // owner grants with explicit caps, persisted
        let reply = handle_command("/grant 5 chat read write", 1, "Owner", &mut acl, &path);
        assert!(reply.contains("chat+read+write"), "got: {reply}");
        let reloaded = Acl::load(&path);
        let caps = reloaded.caps_for(5);
        assert!(caps.chat && caps.read && caps.write && !caps.shell);

        // default caps when none given = chat only
        handle_command("/grant 6", 1, "Owner", &mut acl, &path);
        assert!(acl.caps_for(6).chat && !acl.caps_for(6).read);

        // /members lists them (owner only)
        let members = handle_command("/members", 1, "Owner", &mut acl, &path);
        assert!(members.contains("id 5") && members.contains("id 6"));
        assert_eq!(handle_command("/members", 999, "Guest", &mut acl, &path), NOT_OWNER);

        // revoke
        assert!(handle_command("/revoke 5", 1, "Owner", &mut acl, &path).contains("удалён"));
        assert_eq!(Acl::load(&path).caps_for(5), Caps::default());

        // bad caps token is reported, group-suffixed command still parses
        assert!(handle_command("/grant@MyBot 7 bogus", 1, "Owner", &mut acl, &path).contains("bogus"));
    }
}
