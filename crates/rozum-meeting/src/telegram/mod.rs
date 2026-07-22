mod bot;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc, oneshot};

use bot::{IncomingMessage, TelegramBot};

use crate::meeting::store::rozum_state_dir;
use crate::messenger::{BridgeResult, DaemonBridge, SenderPolicy};
use crate::messenger_acl::{Acl, Caps};

/// Public CLI entry shared by the compatibility gateway command and the thin
/// `rozum-meet telegram` frontend. One bot can serve several chats (its `getUpdates`
/// stream is global): `--room`/`TELEGRAM_CHAT_ID` is the primary chat→room; extra
/// chats are `TELEGRAM_EXTRA_CHATS="<chat_id>=<room>[,<chat_id>=<room>…]"`.
pub async fn run_from_env(room: &str, display_name: &str) -> BridgeResult<()> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| "TELEGRAM_BOT_TOKEN is not set")?;
    let chat_id = std::env::var("TELEGRAM_CHAT_ID")
        .map_err(|_| "TELEGRAM_CHAT_ID is not set")?
        .parse::<i64>()
        .map_err(|_| "TELEGRAM_CHAT_ID must be a numeric chat ID")?;
    let mut channels = vec![(chat_id, room.to_string())];
    if let Ok(extra) = std::env::var("TELEGRAM_EXTRA_CHATS") {
        channels.extend(parse_extra_chats(&extra)?);
    }
    let allowlist = std::env::var("TELEGRAM_ALLOWED_USER_IDS").ok();
    run_bridge_multi(display_name, token, channels, allowlist.as_deref()).await
}

/// Single-chat entry retained for direct callers. Delegates to the multi-chat runner.
pub async fn run_bridge(
    room: &str,
    display_name: &str,
    token: String,
    chat_id: i64,
) -> BridgeResult<()> {
    let allowlist = std::env::var("TELEGRAM_ALLOWED_USER_IDS").ok();
    run_bridge_multi(
        display_name,
        token,
        vec![(chat_id, room.to_string())],
        allowlist.as_deref(),
    )
    .await
}

/// Parse `TELEGRAM_EXTRA_CHATS` = `<chat_id>=<room>[,<chat_id>=<room>…]` into pairs.
fn parse_extra_chats(raw: &str) -> BridgeResult<Vec<(i64, String)>> {
    let mut out = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (id_s, room) = item.split_once('=').ok_or_else(|| {
            format!("TELEGRAM_EXTRA_CHATS entry '{item}' must be <chat_id>=<room>")
        })?;
        let id: i64 = id_s
            .trim()
            .parse()
            .map_err(|_| format!("TELEGRAM_EXTRA_CHATS chat id '{id_s}' is not numeric"))?;
        let room = room.trim();
        if room.is_empty() {
            return Err(format!("TELEGRAM_EXTRA_CHATS entry '{item}' has an empty room").into());
        }
        out.push((id, room.to_string()));
    }
    Ok(out)
}

/// Run one bot over `channels` (each `(chat_id, room)`), sharing a single `getUpdates`
/// poller that routes each update to the matching chat's room task. All chats share one
/// ACL (capabilities are per user id, independent of which chat they speak in).
async fn run_bridge_multi(
    display_name: &str,
    token: String,
    channels: Vec<(i64, String)>,
    configured_allowlist: Option<&str>,
) -> BridgeResult<()> {
    if channels.is_empty() {
        return Err("no Telegram chats configured".into());
    }
    let primary_chat = channels[0].0;
    let bot = Arc::new(TelegramBot::new(token, primary_chat));
    let chat_ids: Vec<i64> = channels.iter().map(|(c, _)| *c).collect();
    let (bot_user_id, validated) = bot.validate_multi(&chat_ids).await?;

    // Owner = TELEGRAM_OWNER_ID, else the private-chat peer among the chats (personal bot
    // needs no config). Shared ACL across all chats; the owner has every capability.
    let acl_path = Acl::path("telegram");
    let mut acl0 = Acl::load(&acl_path);
    let owner = std::env::var("TELEGRAM_OWNER_ID")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .or_else(|| validated.iter().find_map(|v| v.private_user_id));
    if let Some(o) = owner {
        if acl0.ensure_owner(o) {
            let _ = acl0.save(&acl_path);
        }
    }
    let acl = Arc::new(Mutex::new(acl0));

    // Env allowlist stays as an additional accept path; owner is its fallback so a group-only
    // bot (no private chat) still starts with the owner authorized.
    let policy = SenderPolicy::resolve(
        configured_allowlist,
        owner.map(|id| id.to_string()),
        "TELEGRAM_ALLOWED_USER_IDS",
    )?;

    // Register the command menu once (non-fatal).
    if let Err(e) = bot.set_my_commands(BOT_COMMANDS).await {
        eprintln!("[telegram-bridge] setMyCommands failed (menu unavailable): {e}");
    }

    // Connect one room task per chat; the poller routes updates to them by chat_id.
    let mut routes: HashMap<i64, mpsc::Sender<PendingIncoming>> = HashMap::new();
    let mut tasks: Vec<tokio::task::JoinHandle<BridgeResult<()>>> = Vec::new();
    for (chat_id, room) in &channels {
        let room_bridge =
            DaemonBridge::connect(room, display_name, "telegram", &chat_id.to_string()).await?;
        eprintln!(
            "[telegram-bridge] bot {bot_user_id} chat {chat_id} <-> room '{room}' as '{}'; owner {owner:?}",
            room_bridge.participant_id(),
        );
        let (tx, rx) = mpsc::channel::<PendingIncoming>(64);
        routes.insert(*chat_id, tx);
        let bot_c = Arc::clone(&bot);
        let acl_c = Arc::clone(&acl);
        let acl_path_c = acl_path.clone();
        let policy_c = policy.clone();
        let chat = *chat_id;
        tasks.push(tokio::spawn(async move {
            run_channel(chat, room_bridge, rx, bot_c, acl_c, acl_path_c, policy_c).await
        }));
    }

    // One shared poller.
    let poller_bot = Arc::clone(&bot);
    tasks.push(tokio::spawn(async move {
        multi_poller(poller_bot, bot_user_id, routes).await
    }));

    // First task to finish (error, or an ended stream) tears the bridge down so the
    // supervisor restarts it.
    let (res, _idx, remaining) = futures::future::select_all(tasks).await;
    for h in remaining {
        h.abort();
    }
    match res {
        Ok(inner) => inner,
        Err(join) => Err(format!("Telegram bridge task failed: {join}").into()),
    }
}

/// One chat's room task: submit that chat's incoming messages (authorized, non-command) to
/// its room, run owner commands, and relay the room's new turns back to that chat.
#[allow(clippy::too_many_arguments)]
async fn run_channel(
    chat_id: i64,
    mut room_bridge: DaemonBridge,
    mut incoming_rx: mpsc::Receiver<PendingIncoming>,
    bot: Arc<TelegramBot>,
    acl: Arc<Mutex<Acl>>,
    acl_path: PathBuf,
    policy: SenderPolicy,
) -> BridgeResult<()> {
    let mut notified: std::collections::HashSet<i64> = std::collections::HashSet::new();
    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(incoming) = incoming else {
                    return Ok(()); // poller gone → this chat ends
                };
                let id = incoming.message.sender_id;
                let name = incoming.message.sender_name.clone();
                let text = incoming.message.text.trim().to_string();

                // Management/utility commands are handled here and NEVER relayed to the room.
                if text.starts_with('/') {
                    let reply = {
                        let mut a = acl.lock().await;
                        handle_command(&text, id, &name, &mut a, &acl_path)
                    };
                    if let Err(error) = bot.send_message_to(chat_id, &reply).await {
                        eprintln!("[telegram-bridge] sendMessage (command) error: {error}");
                    }
                    let _ = incoming.committed.send(Ok(()));
                    continue;
                }

                // Chat authorization: owner, an ACL member with `chat`, or the env allowlist.
                let allowed = {
                    let a = acl.lock().await;
                    a.is_owner(id) || a.caps_for(id).chat
                } || policy.allows(id.to_string());
                if !allowed {
                    if notified.insert(id) {
                        let hint = format!(
                            "🔒 {name} (id {id}) написал(а) боту, но доступа нет. Добавить: \
                             /grant {id} chat  (можно + read write shell)."
                        );
                        if let Err(error) = bot.send_message_to(chat_id, &hint).await {
                            eprintln!("[telegram-bridge] sendMessage (notify) error: {error}");
                        }
                    }
                    let _ = incoming.committed.send(Ok(()));
                    continue;
                }

                let submit = room_bridge.submit(&name, &id.to_string(), &text).await;
                let acknowledgement = submit.as_ref().map(|_| ()).map_err(|error| error.to_string());
                let _ = incoming.committed.send(acknowledgement);
                if let Err(error) = submit {
                    return Err(error);
                }
            }
            outbound = room_bridge.next_outbound() => {
                let messages = match outbound {
                    Ok(messages) => messages,
                    Err(error) => return Err(error),
                };
                for message in messages {
                    if let Err(error) = bot.send_message_to(chat_id, &message).await {
                        eprintln!("[telegram-bridge] sendMessage error: {error}");
                    }
                }
            }
        }
    }
}

struct PendingIncoming {
    message: IncomingMessage,
    /// Telegram confirms an update only when the next `getUpdates` call uses a
    /// higher offset. Do not make that call until the daemon append succeeded.
    committed: oneshot::Sender<Result<(), String>>,
}

/// The single `getUpdates` poller for the bot. Each update is routed to the room task
/// of its chat (`routes`); updates from chats we don't serve are consumed and skipped.
/// The durable per-bot cursor advances only after the routed room append is acknowledged.
async fn multi_poller(
    bot: Arc<TelegramBot>,
    bot_user_id: i64,
    routes: HashMap<i64, mpsc::Sender<PendingIncoming>>,
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
                    // Parse chat-agnostically, then route to that chat's room task. A message from
                    // a chat we don't serve (or a non-text/bot update) is consumed and skipped.
                    let route = TelegramBot::extract_any(update)
                        .and_then(|(chat_id, message)| routes.get(&chat_id).map(|tx| (tx, message)));
                    let Some((tx, message)) = route else {
                        save_telegram_offset(&cursor_path, bot.chat_id, next_offset)?;
                        offset = next_offset;
                        continue;
                    };
                    let (committed, confirmation) = oneshot::channel();
                    if tx.send(PendingIncoming { message, committed }).await.is_err() {
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
    fn parse_extra_chats_parses_pairs_and_rejects_garbage() {
        let ok = parse_extra_chats("-1002003=assistant-group, 555=team").unwrap();
        assert_eq!(
            ok,
            vec![(-1002003, "assistant-group".to_string()), (555, "team".to_string())]
        );
        assert!(parse_extra_chats("  ").unwrap().is_empty());
        assert!(parse_extra_chats("noequals").is_err(), "missing '='");
        assert!(parse_extra_chats("abc=room").is_err(), "non-numeric id");
        assert!(parse_extra_chats("123=").is_err(), "empty room");
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
