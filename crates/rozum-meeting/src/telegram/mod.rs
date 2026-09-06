mod bot;

// The admin console (CLI + UCC) needs to ask a token "who are you?" without owning a bridge,
// so the bot handle and its `getMe` result are part of this module's public surface.
pub use bot::{BotIdentity, TelegramBot as Bot};

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
use crate::messenger_groups::{Registry, default_room};

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
    // A second bot serving different chats uses its OWN group registry so the two don't clash.
    let registry_name = registry_name();
    let mut channels = vec![(chat_id, room.to_string())];
    if let Ok(extra) = std::env::var("TELEGRAM_EXTRA_CHATS") {
        channels.extend(parse_extra_chats(&extra)?);
    }
    // Dynamic groups the operator connected from inside the bot (`/addgroup`).
    channels.extend(Registry::load(&Registry::path(&registry_name)).routes());
    // Dedup by chat_id — primary wins, then env extras, then the registry.
    let mut seen = std::collections::HashSet::new();
    channels.retain(|(id, _)| seen.insert(*id));
    let allowlist = std::env::var("TELEGRAM_ALLOWED_USER_IDS").ok();
    run_bridge_multi(display_name, token, channels, allowlist.as_deref(), &registry_name).await
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
        "telegram",
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
    registry_name: &str,
) -> BridgeResult<()> {
    if channels.is_empty() {
        return Err("no Telegram chats configured".into());
    }
    let primary_chat = channels[0].0;
    let bot = Arc::new(TelegramBot::new(token, primary_chat));

    // Validate the PRIMARY chat fatally (its failure means the bot itself is misconfigured).
    // Extra/group chats validate LENIENTLY: a group where the bot isn't admin is skipped with a
    // warning — a bad group must never take the whole bridge (and the private chat) down.
    let (bot_user_id, primary_validated) = bot.validate_multi(&[primary_chat]).await?;
    let mut good: Vec<(i64, String)> = vec![channels[0].clone()];
    for (chat_id, room) in &channels[1..] {
        match bot.validate_multi(&[*chat_id]).await {
            Ok(_) => good.push((*chat_id, room.clone())),
            Err(e) => eprintln!(
                "[telegram-bridge] skipping chat {chat_id} (room '{room}'): {e}"
            ),
        }
    }
    let channels = good;

    // Global owner: TELEGRAM_OWNER_ID, else the primary (private) chat's peer.
    let owner = std::env::var("TELEGRAM_OWNER_ID")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .or_else(|| primary_validated.first().and_then(|v| v.private_user_id));

    // Env allowlist stays as an additional accept path; owner is its fallback so a group-only
    // bot (no private chat) still starts with the owner authorized.
    let policy = SenderPolicy::resolve(
        configured_allowlist,
        owner.map(|id| id.to_string()),
        "TELEGRAM_ALLOWED_USER_IDS",
    )?;

    // Register the command menu once (non-fatal). nadia's verbs are appended rather than
    // written out again here: one list, in the module that implements them.
    let menu: Vec<(&str, &str)> =
        BOT_COMMANDS.iter().copied().chain(nadia::MENU.iter().copied()).collect();
    if let Err(e) = bot.set_my_commands(&menu).await {
        eprintln!("[telegram-bridge] setMyCommands failed (menu unavailable): {e}");
    }

    // Deliver finished agents' results to the chat that started them, instead of making the
    // operator poll `/status` from a phone. One task for the whole bridge: the watch list is
    // on disk and keyed by agent id, so it does not care which chat task is running.
    // A `nadia serve` from before this deploy is still running (that is deliberate — it keeps
    // the agents alive across a bridge restart), but it serves the old code. Say so, or restart
    // it when nobody is working in it.
    if let Some(note) = nadia::refresh_if_stale() {
        eprintln!("[telegram-bridge] {note}");
    }

    tokio::spawn(nadia::watch_results(Arc::clone(&bot)));

    // One room task per chat, each with its OWN per-room ACL roster (a grant in one chat does not
    // apply in another). The owner is bootstrapped into every room's roster.
    let mut routes: HashMap<i64, mpsc::Sender<PendingIncoming>> = HashMap::new();
    let mut tasks: Vec<tokio::task::JoinHandle<BridgeResult<()>>> = Vec::new();
    for (chat_id, room) in &channels {
        let room_bridge =
            DaemonBridge::connect(room, display_name, "telegram", &chat_id.to_string()).await?;
        let acl_path = Acl::path_for(room);
        let mut acl0 = Acl::load(&acl_path);
        if let Some(o) = owner {
            if acl0.ensure_owner(o) {
                let _ = acl0.save(&acl_path);
            }
        }
        eprintln!(
            "[telegram-bridge] bot {bot_user_id} chat {chat_id} <-> room '{room}' as '{}'; owner {owner:?}, acl {}",
            room_bridge.participant_id(),
            acl_path.display(),
        );
        let (tx, rx) = mpsc::channel::<PendingIncoming>(64);
        routes.insert(*chat_id, tx);
        let bot_c = Arc::clone(&bot);
        let acl = Arc::new(Mutex::new(acl0));
        let policy_c = policy.clone();
        let chat = *chat_id;
        tasks.push(tokio::spawn(async move {
            run_channel(chat, room_bridge, rx, bot_c, acl, acl_path, policy_c).await
        }));
    }

    // Watchdog: if the poller makes no progress for ~90s (a hung getUpdates after sleep/network
    // change, or a blocked channel), exit so launchd restarts a fresh bridge — the durable cursor
    // means no messages are lost. Fixes the observed multi-hour hang.
    let progress = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let wd = Arc::clone(&progress);
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            // 180s tolerates the worst normal iteration (getUpdates timeout + up to 60s error
            // backoff); only a real hang keeps the flag unset across a full interval.
            loop {
                tokio::time::sleep(Duration::from_secs(180)).await;
                if !wd.swap(false, Ordering::Relaxed) {
                    eprintln!(
                        "[telegram-bridge] watchdog: no poll progress in ~180s — exiting to restart"
                    );
                    std::process::exit(1);
                }
            }
        });
    }

    // One shared poller — also handles the owner-only group-topology commands.
    let poller_bot = Arc::clone(&bot);
    let registry_path = Registry::path(registry_name);
    tasks.push(tokio::spawn(async move {
        multi_poller(poller_bot, bot_user_id, routes, owner, primary_chat, registry_path, progress)
            .await
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

                // GRANTING ACCESS IN ADVANCE. `/grant` needs a numeric id, and Telegram shows
                // one nowhere in the UI — so until now the only way to learn someone's id was to
                // have them write to the bot first, which is exactly what you cannot do for a
                // person you want to admit BEFORE they arrive. A forwarded message carries its
                // original author's id, and that is the one identity a bot can learn about
                // someone who has never contacted it.
                //
                // It only OFFERS the command; it does not grant. Forwarding is also how you hand
                // the model something to read — an article, a screenshot's caption — and in a
                // room with `--reply-policy always` that is an ordinary daily act. Granting on
                // any forward would turn sharing content into handing out access, silently. So
                // the act stays explicit: the bot prints a ready line, the operator sends it.
                //
                // Owner-only, and once per author, for the same reason the not-admitted notice
                // is: nobody else can act on it, and a repeat is noise.
                if let Some((fid, fname)) = incoming.message.forwarded_from.clone() {
                    let is_owner = { acl.lock().await.is_owner(id) };
                    if is_owner && fid != id && notified.insert(fid) {
                        send_hint_and_command(
                            &bot,
                            chat_id,
                            &format!(
                                "↪️ Переслано от {fname} (id {fid}). Скопируй команду ниже, \
                                 чтобы дать доступ (можно дописать read write shell):"
                            ),
                            &format!("/grant {fid} chat"),
                        )
                        .await;
                    }
                }

                // Management/utility commands are handled here and NEVER relayed to the room.
                if text.starts_with('/') {
                    let reply = {
                        let mut a = acl.lock().await;
                        handle_command(&text, id, &name, &mut a, &acl_path, chat_id)
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
                        send_hint_and_command(
                            &bot,
                            chat_id,
                            &format!(
                                "🔒 {name} (id {id}) написал(а) боту, но доступа нет. Скопируй \
                                 команду ниже, чтобы добавить (можно дописать read write shell):"
                            ),
                            &format!("/grant {id} chat"),
                        )
                        .await;
                    }
                    let _ = incoming.committed.send(Ok(()));
                    continue;
                }

                // Dialog mode (`/nadia on`): plain text drives the coding agent instead of the
                // chat model. Intercepted BEFORE the room, so the message is not also answered
                // by the assistant — two replies to one message is worse than either alone.
                // The grant is re-checked inside (`handle_text`): `chat` gets you the assistant,
                // and driving an agent needs write+shell.
                if nadia::dialog_on(chat_id) {
                    let caps = { acl.lock().await.caps_for(id) };
                    let text_c = text.clone();
                    let reply = tokio::task::spawn_blocking(move || {
                        nadia::handle_text(chat_id, &text_c, caps)
                    })
                    .await
                    .unwrap_or_else(|e| format!("nadia: {e}"));
                    if let Err(error) = bot.send_message_to(chat_id, &reply).await {
                        eprintln!("[telegram-bridge] sendMessage (nadia) error: {error}");
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

/// Modification stamp of the group registry, or `None` when it does not exist yet. `None` is a
/// legitimate state (a bot with no groups), and the transition `None -> Some` is exactly the
/// "first group connected from outside" case we must notice, so absence is compared like any
/// other value rather than treated as an error.
/// Which bot this bridge IS — `TELEGRAM_REGISTRY`, default `telegram`.
///
/// It names the bot, not the chat, and that distinction is the whole reason this is a function
/// rather than a local. Two bridges (`com.rozum.telegram`, `com.rozum.telegram-groups`) run this
/// same binary against ONE state file, and in a private chat the chat id is the operator's user
/// id — which both bots can post to. Anything stored per chat and read by both therefore has to
/// say which bot it belongs to, or the operator gets their answer from the bot they did not
/// write to (reported live 2026-08-04, BUG-020).
pub fn registry_name() -> String {
    std::env::var("TELEGRAM_REGISTRY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "telegram".to_string())
}

fn registry_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// The single `getUpdates` poller for the bot. Each update is routed to the room task
/// of its chat (`routes`); updates from chats we don't serve are consumed and skipped.
/// The durable per-bot cursor advances only after the routed room append is acknowledged.
#[allow(clippy::too_many_arguments)]
async fn multi_poller(
    bot: Arc<TelegramBot>,
    bot_user_id: i64,
    routes: HashMap<i64, mpsc::Sender<PendingIncoming>>,
    owner: Option<i64>,
    primary_chat: i64,
    registry_path: PathBuf,
    progress: Arc<std::sync::atomic::AtomicBool>,
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
    // Log each not-yet-served chat once — this is how the operator discovers a new group's id.
    let mut logged_unknown: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // Routing topology can also change from OUTSIDE this process — `rozum-gateway messenger
    // groups add/remove`, the UCC console, or a hand edit. The participant pool already
    // reconciles the registry every 5s; the bridge used to read it only at startup, so such an
    // edit applied to half the system and nothing said so. Watch the file and take the same
    // restart path an in-chat `/addgroup` takes.
    let registry_stamp = registry_mtime(&registry_path);

    'poll: loop {
        // Liveness heartbeat for the watchdog — set each iteration; a hang here (getUpdates or a
        // blocked channel ack) stops updating it and the watchdog restarts the process.
        progress.store(true, std::sync::atomic::Ordering::Relaxed);
        if registry_mtime(&registry_path) != registry_stamp {
            eprintln!(
                "[telegram-bridge] group registry changed on disk ({}) — restarting to apply",
                registry_path.display()
            );
            return Ok(());
        }
        match bot.get_updates(offset, 30).await {
            Ok(updates) => {
                poll_error_delay_secs = MIN_POLL_ERROR_SECS;
                for update in &updates {
                    let next_offset = next_update_offset(update.update_id)?;
                    // Parse chat-agnostically, then route to that chat's room task. A non-text/bot
                    // update, or a message from a chat we don't serve, is consumed and skipped.
                    let Some((chat_id, message)) = TelegramBot::extract_any(update) else {
                        save_telegram_offset(&cursor_path, bot.chat_id, next_offset)?;
                        offset = next_offset;
                        continue;
                    };
                    // Owner-only group-topology commands change routing itself, so they are handled
                    // here — this works even from a not-yet-served group. add/remove re-exec the bridge.
                    if owner == Some(message.sender_id) {
                        if let Some(action) = parse_topology_command(&message.text) {
                            let restart =
                                handle_topology(action, chat_id, primary_chat, &registry_path, &bot)
                                    .await;
                            save_telegram_offset(&cursor_path, bot.chat_id, next_offset)?;
                            offset = next_offset;
                            if restart {
                                eprintln!(
                                    "[telegram-bridge] group topology changed — restarting to apply"
                                );
                                return Ok(());
                            }
                            continue;
                        }
                    }
                    let Some(tx) = routes.get(&chat_id) else {
                        if logged_unknown.insert(chat_id) {
                            eprintln!(
                                "[telegram-bridge] update from chat {chat_id} (not served); to route it set TELEGRAM_EXTRA_CHATS={chat_id}=<room>"
                            );
                        }
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
/// Send a prose line and then the command ALONE, as two messages.
///
/// Telegram copies a WHOLE message at a tap, so a hint carrying its explanation and its command
/// together cannot be copied without the prose around it — which defeats the point of printing a
/// command at all. The command gets a message of its own, ready to copy and send unedited.
async fn send_hint_and_command(bot: &TelegramBot, chat_id: i64, prose: &str, command: &str) {
    for part in [prose, command] {
        if let Err(error) = bot.send_message_to(chat_id, part).await {
            eprintln!("[telegram-bridge] sendMessage (hint) error: {error}");
        }
    }
}

/// the commands `handle_command` accepts.
const BOT_COMMANDS: &[(&str, &str)] = &[
    ("help", "Справка и список команд"),
    ("whoami", "Показать мой Telegram id"),
    ("members", "Кто имеет доступ (для владельца)"),
    ("grant", "Дать доступ: /grant <id> chat read write shell"),
    ("revoke", "Убрать доступ: /revoke <id>"),
    ("groups", "Список подключённых групп (владелец)"),
    ("addgroup", "Подключить эту группу (владелец)"),
    ("removegroup", "Отключить группу: /removegroup <id> (владелец)"),
];

const HELP_TEXT: &str = "Команды бота:\n\
/whoami — показать твой Telegram id\n\
/members — кто имеет доступ в этом чате (владелец)\n\
/grant <id> [chat read write shell | all] — дать/изменить доступ (владелец)\n\
/revoke <id> — убрать доступ (владелец)\n\
/groups — список подключённых групп (владелец)\n\
/addgroup — подключить эту группу, свой ростер прав (владелец, в группе)\n\
/removegroup <id> — отключить группу (владелец)\n\
/help — эта справка\n\
Права: chat=писать в чате, read=читать файлы, write=писать файлы, shell=команды в песочнице. \
Ростер прав СВОЙ у каждого чата/группы.";

const NOT_OWNER: &str = "Управлять доступом может только владелец бота.";

pub mod nadia;

/// Handle a `/command` from a Telegram user. Utility commands (`/help`, `/whoami`)
/// are open to everyone; management commands (`/members`, `/grant`, `/revoke`) are
/// owner-only. Mutates + persists the ACL on grant/revoke. Returns the reply text.
fn handle_command(
    text: &str,
    sender_id: i64,
    sender_name: &str,
    acl: &mut Acl,
    acl_path: &Path,
    chat_id: i64,
) -> String {
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
        "/help" | "/start" => format!("{HELP_TEXT}{}", nadia::HELP),
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
        // Subagent control lives behind the SAME roster as the assistant's sandbox:
        // `nadia::handle` re-checks caps_for(sender) itself. Placed in the fallback so a
        // nadia verb can never shadow a command the bot already answers.
        _ => match nadia::parse(text) {
            Some(Ok(c)) => nadia::handle(c, acl.caps_for(sender_id), chat_id),
            Some(Err(usage)) => usage,
            None => "Неизвестная команда. /help — список команд.".to_string(),
        },
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

/// Owner-only commands that change WHICH chats the bot serves (routing topology).
enum TopologyCmd {
    List,
    AddCurrent,
    Remove(Option<i64>),
}

/// Parse a group-topology command (`/groups`, `/addgroup`, `/removegroup [id]`), tolerating a
/// `@BotName` suffix. Returns None for anything else (handled per-room as a normal command).
fn parse_topology_command(text: &str) -> Option<TopologyCmd> {
    let mut parts = text.split_whitespace();
    let cmd = parts.next()?.split('@').next()?.to_ascii_lowercase();
    match cmd.as_str() {
        "/groups" | "/listgroups" => Some(TopologyCmd::List),
        "/addgroup" | "/connect" => Some(TopologyCmd::AddCurrent),
        "/removegroup" | "/disconnect" | "/leavegroup" => {
            Some(TopologyCmd::Remove(parts.next().and_then(|s| s.parse::<i64>().ok())))
        }
        _ => None,
    }
}

/// Apply a topology command against the registry and reply to `chat_id`. Returns true when the
/// bridge must restart to apply a route change (add/remove); false for a pure query (list).
async fn handle_topology(
    action: TopologyCmd,
    chat_id: i64,
    primary_chat: i64,
    registry_path: &Path,
    bot: &TelegramBot,
) -> bool {
    let mut reg = Registry::load(registry_path);
    let (reply, restart) = match action {
        TopologyCmd::List => (render_groups(&reg), false),
        TopologyCmd::AddCurrent => {
            if chat_id == primary_chat {
                ("«/addgroup» отправь В ГРУППЕ (не в личном чате), чтобы её подключить.".to_string(), false)
            } else if let Some(room) = reg.room_for(chat_id) {
                (format!("Эта группа уже подключена (комната «{room}»)."), false)
            } else {
                let room = default_room(chat_id);
                reg.add(chat_id, &room, "");
                match reg.save(registry_path) {
                    Ok(()) => (
                        format!("✅ Группа подключена → комната «{room}», свой отдельный ростер прав. Применяю…"),
                        true,
                    ),
                    Err(e) => (format!("Не удалось сохранить реестр групп: {e}"), false),
                }
            }
        }
        TopologyCmd::Remove(id) => {
            let target = id.unwrap_or(chat_id);
            if target == primary_chat {
                ("Личный чат нельзя отключить.".to_string(), false)
            } else {
                match reg.remove(target) {
                    Some(g) => match reg.save(registry_path) {
                        Ok(()) => (format!("🚫 Группа {target} отключена (комната «{}»). Применяю…", g.room), true),
                        Err(e) => (format!("Не удалось сохранить реестр групп: {e}"), false),
                    },
                    None => (format!("Группа {target} не найдена среди подключённых. /groups — список."), false),
                }
            }
        }
    };
    if let Err(e) = bot.send_message_to(chat_id, &reply).await {
        eprintln!("[telegram-bridge] sendMessage (topology) error: {e}");
    }
    restart
}

fn render_groups(reg: &Registry) -> String {
    if reg.groups.is_empty() {
        return "Подключённых групп нет. Отправь /addgroup В ГРУППЕ, чтобы её подключить.".to_string();
    }
    let mut lines = vec!["👥 Подключённые группы:".to_string()];
    for g in &reg.groups {
        let title = if g.title.trim().is_empty() {
            String::new()
        } else {
            format!(" «{}»", g.title)
        };
        lines.push(format!("• id {}{} → комната «{}»", g.chat_id, title, g.room));
    }
    lines.push(String::new());
    lines.push("/addgroup (в группе) · /removegroup <id> · /groups".to_string());
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
    fn registry_mtime_is_the_signal_the_poller_restarts_on() {
        // The poll loop restarts the bridge when this value MOVES. Three transitions matter, and
        // all three are things an external editor (CLI, console, hand edit) actually does.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");

        // (1) absent is a legitimate steady state — a bot with no groups — not an error.
        assert!(registry_mtime(&path).is_none());

        // (2) absent -> present: the FIRST group connected from outside. Missed today.
        std::fs::write(&path, r#"{"groups":[]}"#).unwrap();
        let first = registry_mtime(&path);
        assert!(first.is_some());
        assert_ne!(first, None, "appearing must read as a change");

        // (3) present -> modified. Filesystem mtime can be coarse, so assert on the CONTENT
        // transition via an explicitly newer timestamp rather than racing the clock.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::open(&path)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(later)))
            .unwrap();
        assert_ne!(registry_mtime(&path), first, "a rewrite must read as a change");

        // (4) present -> absent (a registry deleted wholesale) is a change too.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(registry_mtime(&path), None);
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
    fn parse_topology_command_recognizes_group_verbs() {
        assert!(matches!(parse_topology_command("/groups"), Some(TopologyCmd::List)));
        assert!(matches!(parse_topology_command("/addgroup@MyBot"), Some(TopologyCmd::AddCurrent)));
        assert!(matches!(
            parse_topology_command("/removegroup -100"),
            Some(TopologyCmd::Remove(Some(-100)))
        ));
        assert!(matches!(parse_topology_command("/removegroup"), Some(TopologyCmd::Remove(None))));
        // per-user + non-commands are NOT topology commands
        assert!(parse_topology_command("/grant 5 chat").is_none());
        assert!(parse_topology_command("hello").is_none());
    }

    #[test]
    fn owner_can_grant_revoke_and_list_members() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        let mut acl = Acl::default();
        acl.ensure_owner(1);

        // whoami is open to anyone
        assert!(handle_command("/whoami", 999, "Guest", &mut acl, &path, 42).contains("999"));

        // a non-owner cannot manage
        assert_eq!(handle_command("/grant 5 chat", 999, "Guest", &mut acl, &path, 42), NOT_OWNER);
        assert!(acl.members.is_empty());

        // owner grants with explicit caps, persisted
        let reply = handle_command("/grant 5 chat read write", 1, "Owner", &mut acl, &path, 42);
        assert!(reply.contains("chat+read+write"), "got: {reply}");
        let reloaded = Acl::load(&path);
        let caps = reloaded.caps_for(5);
        assert!(caps.chat && caps.read && caps.write && !caps.shell);

        // default caps when none given = chat only
        handle_command("/grant 6", 1, "Owner", &mut acl, &path, 42);
        assert!(acl.caps_for(6).chat && !acl.caps_for(6).read);

        // /members lists them (owner only)
        let members = handle_command("/members", 1, "Owner", &mut acl, &path, 42);
        assert!(members.contains("id 5") && members.contains("id 6"));
        assert_eq!(handle_command("/members", 999, "Guest", &mut acl, &path, 42), NOT_OWNER);

        // revoke
        assert!(handle_command("/revoke 5", 1, "Owner", &mut acl, &path, 42).contains("удалён"));
        assert_eq!(Acl::load(&path).caps_for(5), Caps::default());

        // bad caps token is reported, group-suffixed command still parses
        assert!(handle_command("/grant@MyBot 7 bogus", 1, "Owner", &mut acl, &path, 42).contains("bogus"));
    }
}
