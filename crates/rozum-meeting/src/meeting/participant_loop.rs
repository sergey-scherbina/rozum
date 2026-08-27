//! Shared join → long-poll → reply-policy → submit loop, used by every kind of
//! meeting participant (`model_participant`, `agent_participant`).
//!
//! Extracted out of `model_participant.rs` rather than duplicated when
//! `agent_participant.rs` needed the same shape: this loop is ~150 lines of real
//! behavior (identity/session stability, history dedup, the reply-policy gate),
//! not boilerplate, and this codebase has already been burned once by two copies
//! of one policy quietly drifting apart (BUGS.md BUG-047, two seatbelt profiles
//! that "differ on purpose" until someone read them side by side). One loop, two
//! implementations of [`ReplyGenerator`].

use std::future::Future;
use std::pin::Pin;

use super::room_path::meeting_sock;
use super::store::StoredTurn;
use super::tui_client::MeetingClient;

/// When a participant contributes. A politeness policy of ONE participant — never a
/// room-level moderator or turn-scheduler (global meeting invariant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyPolicy {
    /// Reply only when the participant's handle is `@mentioned` (safe multi-participant default).
    Mention,
    /// Reply to any new human message (never its own or a peer participant's — loop guard).
    Always,
    /// Never reply on its own (driven externally).
    Manual,
}

impl std::str::FromStr for ReplyPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mention" => Ok(Self::Mention),
            "always" => Ok(Self::Always),
            "manual" => Ok(Self::Manual),
            other => Err(format!("unknown reply-policy '{other}' (mention|always|manual)")),
        }
    }
}

/// Derive a clean roster handle from a model spec: the last path/spec segment up to
/// the size/version suffix. `mlx-community:gpt-oss-20b-MXFP4-Q4` → `gpt-oss`;
/// `mlx-community:Qwen3.6-35B-A3B-4bit` → `qwen3.6`.
pub fn derive_handle(model: &str) -> String {
    let seg = model.rsplit([':', '/']).next().unwrap_or(model);
    let mut out = String::new();
    let chars: Vec<char> = seg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Stop at a `-<digit>` (size/quant suffix like `-20b`, `-35B`).
        if chars[i] == '-' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
        out.push(chars[i]);
        i += 1;
    }
    let h = if out.is_empty() { seg.to_string() } else { out };
    h.to_ascii_lowercase()
}

/// The stable base of a roster display name — the daemon appends a session suffix
/// (`gpt-oss · jolly-marten` → `gpt-oss`), so compare on the base, not the full name.
pub fn base_handle(display: &str) -> &str {
    display.split('·').next().map(str::trim).unwrap_or(display)
}

/// Parse the numeric messenger sender id from a bridge-submitted turn. Bridges
/// submit `[<display> #<id>]: <text>`; extract `<id>` so a participant can look up
/// that user's capabilities. Returns None for non-bridge turns (no prefix).
pub fn sender_id_from_content(content: &str) -> Option<i64> {
    let content = content.trim_start();
    let close = content.strip_prefix('[')?.find(']')?;
    let head = &content[1..=close]; // "<display> #<id>"
    let hash = head.rfind('#')?;
    head[hash + 1..].trim_end_matches(']').trim().parse::<i64>().ok()
}

/// Should the participant reply to `turn`? `handle` is its own base handle (callers
/// have already excluded its own turns). `peers` are other participant handles in the
/// room, so `Always` never triggers a participant↔participant runaway. Under
/// `Mention`, it replies when the message @mentions its own handle OR `mention_alias`
/// (the bot's messenger username, e.g. `@Rozum_chat_bot`) — so a group can address it
/// by name.
pub fn should_reply(
    policy: ReplyPolicy,
    turn: &StoredTurn,
    handle: &str,
    peers: &[String],
    mention_alias: Option<&str>,
) -> bool {
    match policy {
        ReplyPolicy::Manual => false,
        ReplyPolicy::Mention => {
            let c = turn.content.to_ascii_lowercase();
            c.contains(&format!("@{}", handle.to_ascii_lowercase()))
                || mention_alias
                    .map(|a| a.trim().to_ascii_lowercase())
                    .is_some_and(|a| !a.is_empty() && c.contains(&a))
        }
        ReplyPolicy::Always => {
            let from = base_handle(&turn.display_name);
            !peers.iter().any(|p| p.eq_ignore_ascii_case(from))
        }
    }
}

/// Remove the bot's own @mention (`mention_alias`, e.g. `@Rozum_chat_bot`) from a message so
/// the model sees a clean question. Case-insensitive; collapses the leftover spaces.
pub fn strip_mention(text: &str, mention_alias: Option<&str>) -> String {
    let Some(alias) = mention_alias.map(str::trim).filter(|a| !a.is_empty()) else {
        return text.to_string();
    };
    let lower = text.to_ascii_lowercase();
    let alias_lower = alias.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if lower[i..].starts_with(&alias_lower) {
            i += alias.len();
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turns a conversation into a reply. `&'a mut self` (not `&self`) so an implementor
/// MAY hold per-call mutable state, though neither current implementor needs it.
/// `None` means "stay silent" (a transient failure, or nothing worth saying) — the
/// loop never treats silence as fatal, matching the old model-participant behavior
/// of swallowing a generation error rather than crashing the room.
pub trait ReplyGenerator: Send {
    fn reply<'a>(
        &'a mut self,
        history: &'a [StoredTurn],
        turn: &'a StoredTurn,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// Static join/session parameters for one participant run.
pub struct ParticipantConfig {
    pub room: String,
    pub handle: String,
    /// Freeform description used in the "joined: I'm X (…)" room announcement and in
    /// logs — e.g. `format!("model {model}")` or `format!("agent {agent} via {model}")`.
    pub label: String,
    pub policy: ReplyPolicy,
    /// Other participant handles in the room, so `Always` never loops participant↔participant.
    pub peers: Vec<String>,
    pub mention_alias: Option<String>,
    /// Prefix for the stable roster identity token (`"{prefix}-{handle}"`) — keep
    /// distinct per participant KIND so a model-participant and an agent-participant
    /// using the same handle don't fight over one roster seat across restarts.
    pub identity_prefix: &'static str,
}

/// Join `cfg.room` as `cfg.handle`, then reply via `replier` per `cfg.policy` until the
/// connection closes. A `replier.reply` failure is the generator's own concern to log —
/// this loop only ever sees `None` (stay silent) or `Some(text)` (submit it); a
/// gateway/subprocess hiccup must never crash the room.
pub async fn run_participant_loop(
    cfg: ParticipantConfig,
    mut replier: impl ReplyGenerator,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use super::daemon::daemon_alive;
    use super::daemon_proxy::spawn_daemon;

    let ParticipantConfig { room, handle, label, policy, peers, mention_alias, identity_prefix } =
        cfg;

    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        spawn_daemon().await;
    }
    // Stable identity per (kind, handle) → ONE roster participant across restarts.
    let token = format!("{identity_prefix}-{}", handle.to_ascii_lowercase());
    let mut client = MeetingClient::connect_as(&sock, &handle, &token).await?;
    let joined = client.enter_or_create(&room).await?;
    eprintln!(
        "[{identity_prefix}] '{handle}' joined room '{joined}' (requested '{room}', {label}, policy={policy:?})"
    );
    // Announce presence — this also MATERIALIZES a freshly-created room on disk so it is
    // discoverable to `meetings status` / human posts (a direct client, unlike the MCP proxy,
    // posts no automatic `joined:` line).
    let hint = match policy {
        ReplyPolicy::Mention => format!("joined: I'm {handle} ({label}). @mention me to chat."),
        ReplyPolicy::Always => format!("joined: I'm {handle} ({label}). I'll chime in here."),
        ReplyPolicy::Manual => format!("joined: I'm {handle} ({label})."),
    };
    let _ = client.submit(&hint).await;

    // Maintain our OWN running transcript: `spawn_poll` streams new turns on a separate
    // connection, so `client.transcript()` (the main connection's view) does NOT include them —
    // building context from it would omit the very message we're replying to. Seed with the
    // backlog, extend per batch.
    let mut history = client.transcript().to_vec();
    let (mut rx, _poll) = client.spawn_poll();
    while let Some(batch) = rx.recv().await {
        history.extend(batch.iter().cloned());
        if history.len() > 200 {
            history.drain(0..history.len() - 200);
        }
        for turn in batch {
            if base_handle(&turn.display_name).eq_ignore_ascii_case(&handle) {
                continue; // never reply to itself (display carries a session suffix)
            }
            if !should_reply(policy, &turn, &handle, &peers, mention_alias.as_deref()) {
                continue;
            }
            eprintln!("[{identity_prefix}] replying to {} …", base_handle(&turn.display_name));
            if let Some(text) = replier.reply(&history, &turn).await {
                if !text.trim().is_empty() {
                    if let Err(e) = client.submit(text.trim()).await {
                        eprintln!("[{identity_prefix}] submit failed: {e}");
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(name: &str, content: &str) -> StoredTurn {
        StoredTurn {
            date: "2026-06-20".into(),
            n: 1,
            participant_id: "x".into(),
            display_name: name.into(),
            content: content.into(),
            ts: 0,
            ..Default::default()
        }
    }

    #[test]
    fn mention_policy_only_on_mention() {
        assert!(should_reply(
            ReplyPolicy::Mention,
            &turn("Alice", "hey @gpt-oss what do you think?"),
            "gpt-oss",
            &[],
            None
        ));
        assert!(!should_reply(
            ReplyPolicy::Mention,
            &turn("Alice", "just chatting"),
            "gpt-oss",
            &[],
            None
        ));
    }

    #[test]
    fn always_skips_peer_models_no_loop() {
        let peers = vec!["qwen3.6".to_string()];
        assert!(should_reply(ReplyPolicy::Always, &turn("Alice", "hi"), "gpt-oss", &peers, None));
        // a peer's message must NOT trigger a reply (no participant↔participant loop)
        assert!(!should_reply(
            ReplyPolicy::Always,
            &turn("qwen3.6", "hi back"),
            "gpt-oss",
            &peers,
            None
        ));
        // Mention policy matches the bot's messenger alias, not just the handle.
        assert!(should_reply(
            ReplyPolicy::Mention,
            &turn("Alice", "hey @Rozum_chat_bot help"),
            "qwen",
            &[],
            Some("@Rozum_chat_bot")
        ));
        assert!(!should_reply(
            ReplyPolicy::Mention,
            &turn("Alice", "just chatting"),
            "qwen",
            &[],
            Some("@Rozum_chat_bot")
        ));
        assert_eq!(strip_mention("@Rozum_chat_bot что такое rozum?", Some("@Rozum_chat_bot")), "что такое rozum?");
    }

    #[test]
    fn manual_never_replies() {
        assert!(!should_reply(ReplyPolicy::Manual, &turn("Alice", "@gpt-oss hi"), "gpt-oss", &[], None));
    }

    #[test]
    fn base_handle_strips_session_suffix() {
        assert_eq!(base_handle("gpt-oss · jolly-marten"), "gpt-oss");
        assert_eq!(base_handle("Sergiy · calm-tapir"), "Sergiy");
        assert_eq!(base_handle("plain"), "plain");
        // self-skip works despite the suffix
        assert!(!should_reply(
            ReplyPolicy::Always,
            &turn("gpt-oss · jolly-marten", "hi"),
            "gpt-oss",
            &["gpt-oss".into()],
            None
        ));
    }

    #[test]
    fn parses_messenger_sender_id_from_turn() {
        assert_eq!(sender_id_from_content("[Bob #42]: привет"), Some(42));
        assert_eq!(sender_id_from_content("[Сергій #1711036782]: hi"), Some(1711036782));
        // no bridge prefix → no id (local TUI turn)
        assert_eq!(sender_id_from_content("just a plain message"), None);
        assert_eq!(sender_id_from_content("[no id here]: x"), None);
    }

    #[test]
    fn reply_policy_parse_and_handle_derivation() {
        assert_eq!("mention".parse::<ReplyPolicy>().unwrap(), ReplyPolicy::Mention);
        assert!("bogus".parse::<ReplyPolicy>().is_err());
        assert_eq!(derive_handle("mlx-community:gpt-oss-20b-MXFP4-Q4"), "gpt-oss");
        assert_eq!(derive_handle("mlx-community:Qwen3.6-35B-A3B-4bit"), "qwen3.6");
    }
}
