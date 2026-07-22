use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, de::DeserializeOwned};

type BotResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const TELEGRAM_MAX_MESSAGE_UTF16: usize = 4_000;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const MAX_RETRY_AFTER_SECS: u64 = 60;
const STARTUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct TelegramBot {
    token: String,
    pub chat_id: i64,
    http: reqwest::Client,
    api_base: String,
}

#[derive(Debug, Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TgMessage {
    pub chat: TgChat,
    pub from: Option<TgFrom>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgChat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct TgFrom {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    is_bot: bool,
    #[serde(default)]
    can_read_all_group_messages: bool,
}

#[derive(Debug, Deserialize)]
struct TgChatMember {
    user: TgUser,
    status: String,
    #[serde(default)]
    is_member: bool,
}

#[derive(Debug, Deserialize)]
struct WebhookInfo {
    #[serde(default)]
    url: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<u16>,
    parameters: Option<ResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelegramChatKind {
    Private,
    Group,
    Supergroup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedChat {
    pub bot_user_id: i64,
    pub chat_id: i64,
    pub kind: TelegramChatKind,
    /// Telegram private-chat IDs are the peer user's ID. Group-like targets
    /// deliberately return `None` and require an explicit sender policy.
    pub private_user_id: Option<i64>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct IncomingMessage {
    pub sender_id: i64,
    pub sender_name: String,
    pub text: String,
}

impl TelegramBot {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self::with_api_base(token, chat_id, TELEGRAM_API_BASE)
    }

    fn with_api_base(token: String, chat_id: i64, api_base: &str) -> Self {
        Self {
            token,
            chat_id,
            http: reqwest::Client::new(),
            api_base: api_base.trim_end_matches('/').to_owned(),
        }
    }

    fn api(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.token, method)
    }

    /// Validate the bot credential and target before the bridge joins a room.
    /// Errors are deliberately rebuilt without reqwest's URL-bearing display
    /// text because Telegram embeds the bot token in every API URL.
    /// Single-chat validation wrapper (delegates to `validate_multi`). Retained as the
    /// one-chat API surface + validation-test entry; the bridge itself uses `validate_multi`.
    #[allow(dead_code)]
    pub async fn validate(&self) -> BotResult<ValidatedChat> {
        let (_, mut chats) = self.validate_multi(&[self.chat_id]).await?;
        Ok(chats.remove(0))
    }

    /// Validate the bot once (getMe + no active webhook) and then each target chat.
    /// Returns the bot user id and one `ValidatedChat` per input, in order. Used by
    /// the multi-chat bridge (one bot serving several chats over a single getUpdates).
    pub async fn validate_multi(&self, chat_ids: &[i64]) -> BotResult<(i64, Vec<ValidatedChat>)> {
        let me: TgUser = self.get_api("getMe", &[], STARTUP_REQUEST_TIMEOUT).await?;
        if !me.is_bot || me.id <= 0 {
            return Err("Telegram getMe returned a non-bot identity".into());
        }
        let webhook: WebhookInfo = self
            .get_api("getWebhookInfo", &[], STARTUP_REQUEST_TIMEOUT)
            .await?;
        if !webhook.url.trim().is_empty() {
            return Err(
                "Telegram bot has an active webhook; call deleteWebhook before using getUpdates"
                    .into(),
            );
        }
        let mut out = Vec::with_capacity(chat_ids.len());
        for &cid in chat_ids {
            out.push(self.validate_chat(cid, &me).await?);
        }
        Ok((me.id, out))
    }

    async fn validate_chat(&self, chat_id: i64, me: &TgUser) -> BotResult<ValidatedChat> {
        let chat: TgChat = self
            .get_api(
                "getChat",
                &[("chat_id", chat_id.to_string())],
                STARTUP_REQUEST_TIMEOUT,
            )
            .await?;
        if chat.id != chat_id {
            return Err("Telegram getChat returned a different target chat".into());
        }

        let (kind, private_user_id) = match chat.kind.as_str() {
            "private" => (TelegramChatKind::Private, Some(chat.id)),
            "group" => (TelegramChatKind::Group, None),
            "supergroup" => (TelegramChatKind::Supergroup, None),
            _ => {
                return Err("Telegram target must be a private chat, group, or supergroup".into());
            }
        };

        if kind != TelegramChatKind::Private {
            let member: TgChatMember = self
                .get_api(
                    "getChatMember",
                    &[
                        ("chat_id", chat_id.to_string()),
                        ("user_id", me.id.to_string()),
                    ],
                    STARTUP_REQUEST_TIMEOUT,
                )
                .await?;
            if member.user.id != me.id || !member.user.is_bot {
                return Err("Telegram getChatMember returned a different bot identity".into());
            }

            let is_admin = matches!(member.status.as_str(), "creator" | "administrator");
            let is_present = is_admin
                || member.status == "member"
                || (member.status == "restricted" && member.is_member);
            if !is_present {
                return Err("Telegram bot is not a member of the target group".into());
            }
            if !me.can_read_all_group_messages && !is_admin {
                return Err(
                    "Telegram bot cannot receive ordinary group messages in the target chat; disable privacy mode with BotFather or promote the bot to administrator"
                        .into(),
                );
            }
        }

        Ok(ValidatedChat {
            bot_user_id: me.id,
            chat_id: chat.id,
            kind,
            private_user_id,
        })
    }

    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> BotResult<Vec<TgUpdate>> {
        self.get_api(
            "getUpdates",
            &[
                ("offset", offset.to_string()),
                ("timeout", timeout_secs.to_string()),
                ("allowed_updates", "[\"message\"]".to_string()),
            ],
            Duration::from_secs(timeout_secs.saturating_add(10)),
        )
        .await
    }

    /// Deliver text in conservative Telegram-sized chunks. A rate-limited
    /// chunk is retried at most twice, after exactly the server's `retry_after`
    /// delay when that delay is within the safety bound.
    /// Register the bot's command menu — the list shown behind the Menu button and the
    /// `/` autocomplete. Called once at startup; a failure is non-fatal (the bridge and
    /// its text commands work without a menu). `commands` are `(name, description)` pairs.
    pub async fn set_my_commands(&self, commands: &[(&str, &str)]) -> BotResult<()> {
        let cmds: Vec<serde_json::Value> = commands
            .iter()
            .map(|(c, d)| serde_json::json!({ "command": c, "description": d }))
            .collect();
        let _: bool = self
            .post_api(
                "setMyCommands",
                &serde_json::json!({ "commands": cmds }),
                STARTUP_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    /// Send to the bot's primary chat. The multi-chat bridge uses `send_message_to`;
    /// retained as the single-chat convenience + chunking-test entry.
    #[allow(dead_code)]
    pub async fn send_message(&self, text: &str) -> BotResult<()> {
        self.send_message_to(self.chat_id, text).await
    }

    /// Send text to a SPECIFIC chat (the multi-chat bridge routes each room's turns
    /// back to its own chat). Chunked the same way as `send_message`.
    pub async fn send_message_to(&self, chat_id: i64, text: &str) -> BotResult<()> {
        for chunk in crate::messenger::split_text(text, TELEGRAM_MAX_MESSAGE_UTF16) {
            let _: serde_json::Value = self
                .post_api(
                    "sendMessage",
                    &serde_json::json!({
                        "chat_id": chat_id,
                        "text": chunk,
                    }),
                    STARTUP_REQUEST_TIMEOUT,
                )
                .await?;
        }
        Ok(())
    }

    async fn get_api<T>(
        &self,
        method: &'static str,
        query: &[(&str, String)],
        timeout: Duration,
    ) -> BotResult<T>
    where
        T: DeserializeOwned,
    {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .http
                .get(self.api(method))
                .query(query)
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| sanitized_transport_error(method, &error))?;
            let status = response.status();
            let api: ApiResponse<T> = response.json().await.map_err(|_| {
                format!(
                    "Telegram {method} returned an unreadable response (HTTP {})",
                    status.as_u16()
                )
            })?;

            if api.ok {
                return api
                    .result
                    .ok_or_else(|| format!("Telegram {method} returned no result").into());
            }
            if self
                .wait_for_rate_limit(method, status, &api, attempt)
                .await?
            {
                continue;
            }
            return Err(self.api_error(method, status, &api));
        }
        unreachable!("bounded Telegram retry loop always returns")
    }

    async fn post_api<T>(
        &self,
        method: &'static str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> BotResult<T>
    where
        T: DeserializeOwned,
    {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .http
                .post(self.api(method))
                .json(body)
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| sanitized_transport_error(method, &error))?;
            let status = response.status();
            let api: ApiResponse<T> = response.json().await.map_err(|_| {
                format!(
                    "Telegram {method} returned an unreadable response (HTTP {})",
                    status.as_u16()
                )
            })?;

            if api.ok {
                return api
                    .result
                    .ok_or_else(|| format!("Telegram {method} returned no result").into());
            }
            if self
                .wait_for_rate_limit(method, status, &api, attempt)
                .await?
            {
                continue;
            }
            return Err(self.api_error(method, status, &api));
        }
        unreachable!("bounded Telegram retry loop always returns")
    }

    async fn wait_for_rate_limit<T>(
        &self,
        method: &'static str,
        status: StatusCode,
        api: &ApiResponse<T>,
        attempt: usize,
    ) -> BotResult<bool> {
        if status != StatusCode::TOO_MANY_REQUESTS && api.error_code != Some(429) {
            return Ok(false);
        }
        if attempt >= MAX_RATE_LIMIT_RETRIES {
            return Err(self.api_error(method, status, api));
        }
        let Some(retry_after) = api
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.retry_after)
        else {
            return Err(format!("Telegram {method} was rate limited without retry_after").into());
        };
        if retry_after > MAX_RETRY_AFTER_SECS {
            return Err(format!(
                "Telegram {method} retry_after exceeds the {MAX_RETRY_AFTER_SECS}s safety bound"
            )
            .into());
        }
        tokio::time::sleep(Duration::from_secs(retry_after)).await;
        Ok(true)
    }

    fn api_error<T>(
        &self,
        method: &'static str,
        status: StatusCode,
        api: &ApiResponse<T>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        let detail = api
            .description
            .as_deref()
            .map(|description| self.sanitize(description))
            .unwrap_or_else(|| "Telegram API rejected the request".to_owned());
        let code = api.error_code.unwrap_or(status.as_u16());
        format!("Telegram {method} failed (code {code}): {detail}").into()
    }

    fn sanitize(&self, value: &str) -> String {
        if self.token.is_empty() {
            value.to_owned()
        } else {
            value.replace(&self.token, "<redacted>")
        }
    }

    /// Extract a text message only when both the target chat and caller-supplied
    /// sender policy accept it. Keeping authorization in this pure parser makes
    /// malformed/wrong-chat/unauthorized payload behavior offline-testable.
    #[allow(dead_code)] // superseded by `extract_any`; kept for its single-chat filter tests
    pub fn extract_message<F>(
        update: &TgUpdate,
        target_chat_id: i64,
        is_sender_allowed: F,
    ) -> Option<IncomingMessage>
    where
        F: FnOnce(i64) -> bool,
    {
        let msg = update.message.as_ref()?;
        if msg.chat.id != target_chat_id {
            return None;
        }
        let from = msg.from.as_ref()?;
        if from.is_bot || !is_sender_allowed(from.id) {
            return None;
        }
        let text = msg.text.as_deref()?.trim();
        if text.is_empty() {
            return None;
        }
        let sender_name = if from.first_name.is_empty() {
            from.username.as_deref().unwrap_or("user").to_owned()
        } else {
            from.first_name.clone()
        };
        Some(IncomingMessage {
            sender_id: from.id,
            sender_name,
            text: text.to_owned(),
        })
    }

    /// Chat-agnostic parse for the multi-chat bridge: return `(chat_id, message)` for
    /// any non-bot, non-empty text message. The caller decides whether that chat is one
    /// it serves and applies per-chat authorization (unlike `extract_message`, which
    /// filters to a single target chat and a sender-allow closure).
    pub fn extract_any(update: &TgUpdate) -> Option<(i64, IncomingMessage)> {
        let msg = update.message.as_ref()?;
        let from = msg.from.as_ref()?;
        if from.is_bot {
            return None;
        }
        let text = msg.text.as_deref()?.trim();
        if text.is_empty() {
            return None;
        }
        let sender_name = if from.first_name.is_empty() {
            from.username.as_deref().unwrap_or("user").to_owned()
        } else {
            from.first_name.clone()
        };
        Some((
            msg.chat.id,
            IncomingMessage {
                sender_id: from.id,
                sender_name,
                text: text.to_owned(),
            },
        ))
    }
}

fn sanitized_transport_error(
    method: &'static str,
    error: &reqwest::Error,
) -> Box<dyn std::error::Error + Send + Sync> {
    let reason = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_request() {
        "could not build or send the request"
    } else {
        "transport failed"
    };
    format!("Telegram {method} {reason}").into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::extract::{Query, State};
    use axum::http::StatusCode as AxumStatusCode;
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    use super::*;

    const TEST_TOKEN: &str = "123456:test-secret-token";
    const TEST_CHAT_ID: i64 = 424_242;

    #[derive(Clone)]
    struct MockState {
        reject_get_me: bool,
        reject_get_chat_member: bool,
        can_read_all_group_messages: bool,
        chat_kind: &'static str,
        bot_member_status: &'static str,
        bot_member_is_member: bool,
        webhook_url: &'static str,
        rate_limit_responses: usize,
        send_attempts: Arc<AtomicUsize>,
        sent_payloads: Arc<Mutex<Vec<Value>>>,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                reject_get_me: false,
                reject_get_chat_member: false,
                can_read_all_group_messages: false,
                chat_kind: "private",
                bot_member_status: "member",
                bot_member_is_member: true,
                webhook_url: "",
                rate_limit_responses: 0,
                send_attempts: Arc::new(AtomicUsize::new(0)),
                sent_payloads: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    async fn get_me(State(state): State<MockState>) -> (AxumStatusCode, Json<Value>) {
        if state.reject_get_me {
            return (
                AxumStatusCode::UNAUTHORIZED,
                Json(json!({
                    "ok": false,
                    "error_code": 401,
                    "description": format!(
                        "invalid token {TEST_TOKEN} in /bot{TEST_TOKEN}/getMe"
                    )
                })),
            );
        }
        (
            AxumStatusCode::OK,
            Json(json!({
                "ok": true,
                "result": {
                    "id": 999_999_i64,
                    "is_bot": true,
                    "can_read_all_group_messages": state.can_read_all_group_messages
                }
            })),
        )
    }

    #[derive(Deserialize)]
    struct ChatQuery {
        chat_id: i64,
    }

    #[derive(Deserialize)]
    struct ChatMemberQuery {
        chat_id: i64,
        user_id: i64,
    }

    async fn get_chat(
        State(state): State<MockState>,
        Query(query): Query<ChatQuery>,
    ) -> (AxumStatusCode, Json<Value>) {
        (
            AxumStatusCode::OK,
            Json(json!({
                "ok": true,
                "result": { "id": query.chat_id, "type": state.chat_kind }
            })),
        )
    }

    async fn get_webhook_info(State(state): State<MockState>) -> Json<Value> {
        Json(json!({
            "ok": true,
            "result": {
                "url": state.webhook_url,
                "has_custom_certificate": false,
                "pending_update_count": 0
            }
        }))
    }

    async fn get_chat_member(
        State(state): State<MockState>,
        Query(query): Query<ChatMemberQuery>,
    ) -> (AxumStatusCode, Json<Value>) {
        if state.reject_get_chat_member {
            return (
                AxumStatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error_code": 403,
                    "description": format!(
                        "cannot inspect /bot{TEST_TOKEN}/getChatMember for {TEST_TOKEN}"
                    )
                })),
            );
        }
        if query.chat_id != TEST_CHAT_ID {
            return (
                AxumStatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error_code": 400,
                    "description": "unexpected target chat"
                })),
            );
        }
        (
            AxumStatusCode::OK,
            Json(json!({
                "ok": true,
                "result": {
                    "user": { "id": query.user_id, "is_bot": true },
                    "status": state.bot_member_status,
                    "is_member": state.bot_member_is_member
                }
            })),
        )
    }

    async fn send_message(
        State(state): State<MockState>,
        Json(payload): Json<Value>,
    ) -> (AxumStatusCode, Json<Value>) {
        state.sent_payloads.lock().await.push(payload);
        let attempt = state.send_attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < state.rate_limit_responses {
            (
                AxumStatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "ok": false,
                    "error_code": 429,
                    "description": "Too Many Requests",
                    "parameters": { "retry_after": 0 }
                })),
            )
        } else {
            (
                AxumStatusCode::OK,
                Json(json!({ "ok": true, "result": { "message_id": attempt } })),
            )
        }
    }

    fn mock_router(state: MockState) -> Router {
        Router::new()
            .route("/{bot_token}/getMe", get(get_me))
            .route("/{bot_token}/getWebhookInfo", get(get_webhook_info))
            .route("/{bot_token}/getChat", get(get_chat))
            .route("/{bot_token}/getChatMember", get(get_chat_member))
            .route("/{bot_token}/sendMessage", post(send_message))
            .with_state(state)
    }

    async fn start_mock(state: MockState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_router(state)).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn validates_bot_and_private_chat_without_live_credentials() {
        let base = start_mock(MockState::default()).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let validated = bot.validate().await.unwrap();

        assert_eq!(validated.bot_user_id, 999_999);
        assert_eq!(validated.chat_id, TEST_CHAT_ID);
        assert_eq!(validated.kind, TelegramChatKind::Private);
        assert_eq!(validated.private_user_id, Some(TEST_CHAT_ID));
    }

    #[tokio::test]
    async fn validates_group_when_privacy_mode_is_disabled() {
        let state = MockState {
            can_read_all_group_messages: true,
            chat_kind: "supergroup",
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let validated = bot.validate().await.unwrap();

        assert_eq!(validated.kind, TelegramChatKind::Supergroup);
        assert_eq!(validated.private_user_id, None);
    }

    #[tokio::test]
    async fn validates_group_when_bot_is_an_administrator() {
        let state = MockState {
            chat_kind: "group",
            bot_member_status: "administrator",
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let validated = bot.validate().await.unwrap();

        assert_eq!(validated.kind, TelegramChatKind::Group);
        assert_eq!(validated.private_user_id, None);
    }

    #[tokio::test]
    async fn validation_rejects_group_member_with_privacy_mode_enabled() {
        let state = MockState {
            chat_kind: "supergroup",
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let error = bot.validate().await.unwrap_err().to_string();

        assert!(error.contains("cannot receive ordinary group messages"));
        assert!(error.contains("BotFather"));
        assert!(error.contains("administrator"));
        assert!(!error.contains(TEST_TOKEN));
    }

    #[tokio::test]
    async fn group_membership_errors_never_expose_the_token() {
        let state = MockState {
            reject_get_chat_member: true,
            chat_kind: "group",
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let error = bot.validate().await.unwrap_err().to_string();

        assert!(error.contains("Telegram getChatMember"));
        assert!(error.contains("403"));
        assert!(error.contains("<redacted>"));
        assert!(!error.contains(TEST_TOKEN));
        assert!(!error.contains("/bot123456:"));
    }

    #[tokio::test]
    async fn validation_errors_never_expose_token_or_token_url() {
        let state = MockState {
            reject_get_me: true,
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let error = bot.validate().await.unwrap_err().to_string();

        assert!(error.contains("401"));
        assert!(error.contains("<redacted>"));
        assert!(!error.contains(TEST_TOKEN));
        assert!(!error.contains("/bot123456:"));
    }

    #[tokio::test]
    async fn validation_rejects_an_active_webhook() {
        let state = MockState {
            webhook_url: "https://example.invalid/hook",
            ..MockState::default()
        };
        let base = start_mock(state).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let error = bot.validate().await.unwrap_err().to_string();

        assert!(error.contains("active webhook"));
        assert!(error.contains("deleteWebhook"));
        assert!(!error.contains(TEST_TOKEN));
    }

    #[tokio::test]
    async fn transport_errors_do_not_include_the_url_bearing_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let bot = TelegramBot::with_api_base(
            TEST_TOKEN.into(),
            TEST_CHAT_ID,
            &format!("http://{address}"),
        );

        let error = bot.validate().await.unwrap_err().to_string();

        assert!(error.contains("Telegram getMe"));
        assert!(!error.contains(TEST_TOKEN));
        assert!(!error.contains("http://"));
    }

    #[test]
    fn extraction_requires_target_chat_allowed_sender_and_nonempty_text() {
        let update: TgUpdate = serde_json::from_value(json!({
            "update_id": 7,
            "message": {
                "chat": { "id": TEST_CHAT_ID, "type": "supergroup" },
                "from": {
                    "id": 123,
                    "first_name": "Alice",
                    "username": "alice"
                },
                "text": "  hello  "
            }
        }))
        .unwrap();

        let accepted = TelegramBot::extract_message(&update, TEST_CHAT_ID, |id| id == 123).unwrap();
        assert_eq!(
            accepted,
            IncomingMessage {
                sender_id: 123,
                sender_name: "Alice".into(),
                text: "hello".into(),
            }
        );
        assert!(TelegramBot::extract_message(&update, TEST_CHAT_ID + 1, |_| true).is_none());
        assert!(TelegramBot::extract_message(&update, TEST_CHAT_ID, |_| false).is_none());

        let bot_authored: TgUpdate = serde_json::from_value(json!({
            "update_id": 8,
            "message": {
                "chat": { "id": TEST_CHAT_ID, "type": "supergroup" },
                "from": {
                    "id": 555,
                    "is_bot": true,
                    "first_name": "Relay"
                },
                "text": "must not loop"
            }
        }))
        .unwrap();
        assert!(
            TelegramBot::extract_message(&bot_authored, TEST_CHAT_ID, |_| true).is_none(),
            "bot-authored updates are never an inbound bridge message"
        );

        let empty: TgUpdate = serde_json::from_value(json!({
            "update_id": 9,
            "message": {
                "chat": { "id": TEST_CHAT_ID, "type": "private" },
                "from": { "id": 123, "first_name": "Alice" },
                "text": "  \n "
            }
        }))
        .unwrap();
        assert!(TelegramBot::extract_message(&empty, TEST_CHAT_ID, |_| true).is_none());
    }

    #[test]
    fn extract_any_returns_chat_id_and_message_for_any_human_text() {
        let update: TgUpdate = serde_json::from_value(json!({
            "update_id": 10,
            "message": {
                "chat": { "id": -1002003004, "type": "supergroup" },
                "from": { "id": 77, "first_name": "Bob" },
                "text": "  hi group  "
            }
        }))
        .unwrap();
        let (chat_id, msg) = TelegramBot::extract_any(&update).unwrap();
        assert_eq!(chat_id, -1002003004);
        assert_eq!(msg.sender_id, 77);
        assert_eq!(msg.text, "hi group");

        // bot-authored and empty text are still rejected (no chat routing for them)
        let bot: TgUpdate = serde_json::from_value(json!({
            "update_id": 11,
            "message": { "chat": {"id": 1, "type": "private"},
                         "from": {"id": 9, "is_bot": true, "first_name": "R"}, "text": "x" }
        }))
        .unwrap();
        assert!(TelegramBot::extract_any(&bot).is_none());
    }

    #[tokio::test]
    async fn chunks_on_utf16_limit_and_retries_one_429() {
        let state = MockState {
            rate_limit_responses: 1,
            ..MockState::default()
        };
        let base = start_mock(state.clone()).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);
        let text = format!("{}😀tail", "a".repeat(3_999));

        bot.send_message(&text).await.unwrap();

        let payloads = state.sent_payloads.lock().await;
        assert_eq!(payloads.len(), 3, "two chunks plus one rate-limit retry");
        let chunks: Vec<&str> = payloads
            .iter()
            .map(|payload| payload["text"].as_str().unwrap())
            .collect();
        assert_eq!(chunks[0], chunks[1]);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= TELEGRAM_MAX_MESSAGE_UTF16)
        );
        assert_eq!(format!("{}{}", chunks[1], chunks[2]), text);
        assert!(
            payloads
                .iter()
                .all(|payload| payload["chat_id"] == TEST_CHAT_ID)
        );
    }

    #[tokio::test]
    async fn rate_limit_retry_budget_is_bounded() {
        let state = MockState {
            rate_limit_responses: usize::MAX,
            ..MockState::default()
        };
        let base = start_mock(state.clone()).await;
        let bot = TelegramBot::with_api_base(TEST_TOKEN.into(), TEST_CHAT_ID, &base);

        let error = bot.send_message("hello").await.unwrap_err().to_string();

        assert_eq!(
            state.send_attempts.load(Ordering::SeqCst),
            MAX_RATE_LIMIT_RETRIES + 1
        );
        assert!(error.contains("429"));
        assert!(!error.contains(TEST_TOKEN));
    }
}
