use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use reqwest::{StatusCode, header};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_USER_AGENT: &str = concat!(
    "DiscordBot (https://github.com/sergey-scherbina/rozum, ",
    env!("CARGO_PKG_VERSION"),
    ")"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE_UTF16_UNITS: usize = 1900;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_ERROR_DETAIL_CHARS: usize = 300;

type BotResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub struct DiscordBot {
    pub token: String,
    pub channel_id: String,
    http: reqwest::Client,
    api_base: String,
}

pub struct IncomingMessage {
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
}

/// Discord identity and Gateway endpoint resolved during startup validation.
/// The caller uses the id to suppress self-authored Gateway messages and reuses
/// the URL for reconnects without another REST lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordStartup {
    pub bot_user_id: String,
    pub bot_username: String,
    pub gateway_url: String,
}

#[derive(Debug)]
struct DiscordClientError(String);

impl Display for DiscordClientError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for DiscordClientError {}

#[derive(Deserialize)]
struct CurrentUserResponse {
    id: String,
    username: String,
    #[serde(default)]
    bot: bool,
}

#[derive(Deserialize)]
struct ChannelResponse {
    id: String,
    #[serde(rename = "type")]
    kind: u8,
    guild_id: Option<String>,
}

#[derive(Deserialize)]
struct GatewayResponse {
    url: String,
}

#[derive(Deserialize)]
struct DiscordErrorResponse {
    code: Option<i64>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct RateLimitResponse {
    retry_after: f64,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<&'static str>,
}

#[derive(Serialize)]
struct MessagePayload<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
}

impl DiscordBot {
    pub fn new(token: String, channel_id: String) -> Self {
        Self::with_api_base(token, channel_id, DISCORD_API_BASE)
    }

    fn with_api_base(token: String, channel_id: String, api_base: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(DISCORD_USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("static Discord HTTP client configuration must be valid");
        Self {
            token,
            channel_id,
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
        }
    }

    fn auth(&self) -> String {
        format!("Bot {}", self.token)
    }

    /// Validate the bot credential and target before the room is joined, then
    /// return the bot identity and authenticated Gateway endpoint to the caller.
    pub async fn validate_startup(&self) -> BotResult<DiscordStartup> {
        if !is_snowflake(&self.channel_id) {
            return Err(self.error("Discord channel ID must be a positive numeric snowflake"));
        }
        let me: CurrentUserResponse = self.get_json("/users/@me", "validate bot token").await?;
        if !me.bot {
            return Err(self.error("Discord credential belongs to a user, not a bot"));
        }
        if me.id.trim().is_empty() || me.username.trim().is_empty() {
            return Err(self.error("Discord current-user response omitted the bot identity"));
        }

        let channel: ChannelResponse = self
            .get_json(
                &format!("/channels/{}", self.channel_id),
                "validate target channel",
            )
            .await?;
        if channel.id != self.channel_id {
            return Err(self.error("Discord returned a different target channel"));
        }
        if channel.guild_id.is_none() {
            return Err(self.error("Discord target must be a guild channel or thread"));
        }
        if !is_supported_channel_kind(channel.kind) {
            return Err(self.error(format!(
                "Discord target channel type {} cannot carry bridge messages",
                channel.kind
            )));
        }

        let gateway_url = self.get_gateway_url().await?;
        Ok(DiscordStartup {
            bot_user_id: me.id,
            bot_username: me.username,
            gateway_url,
        })
    }

    pub async fn get_gateway_url(&self) -> BotResult<String> {
        let gateway: GatewayResponse = self.get_json("/gateway/bot", "get Gateway URL").await?;
        let url = gateway.url.trim_end_matches('/');
        if !url.starts_with("wss://") {
            return Err(self.error("Discord returned an invalid Gateway URL"));
        }
        Ok(format!("{url}/?v=10&encoding=json"))
    }

    /// Deliver one logical room message. Each Discord request stays below a
    /// conservative 1900 UTF-16-unit limit and cannot resolve mentions.
    pub async fn send_message(&self, text: &str) -> BotResult<()> {
        if !is_snowflake(&self.channel_id) {
            return Err(self.error("Discord channel ID must be a positive numeric snowflake"));
        }
        for chunk in crate::messenger::split_text(text, MAX_MESSAGE_UTF16_UNITS) {
            self.send_chunk(&chunk).await?;
        }
        Ok(())
    }

    async fn send_chunk(&self, chunk: &str) -> BotResult<()> {
        let path = format!("/channels/{}/messages", self.channel_id);
        let payload = MessagePayload {
            content: chunk,
            allowed_mentions: AllowedMentions { parse: vec![] },
        };

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .http
                .post(self.url(&path))
                .header(header::AUTHORIZATION, self.auth())
                .json(&payload)
                .send()
                .await
                .map_err(|error| self.transport_error("send message", &error))?;
            let status = response.status();
            let retry_after_header = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_seconds);
            let body = response
                .bytes()
                .await
                .map_err(|error| self.transport_error("read send-message response", &error))?;

            if status.is_success() {
                return Ok(());
            }

            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES {
                let retry_after = retry_after_from_body(&body)
                    .or(retry_after_header)
                    .ok_or_else(|| self.status_error("send message", status, &body))?;
                if retry_after > MAX_RETRY_AFTER {
                    return Err(self.error(format!(
                        "Discord send message failed with HTTP {status}: retry_after exceeds the {}s safety bound",
                        MAX_RETRY_AFTER.as_secs()
                    )));
                }
                tokio::time::sleep(retry_after).await;
                continue;
            }

            return Err(self.status_error("send message", status, &body));
        }

        Err(self.error("Discord send message exhausted its retry budget"))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str, operation: &str) -> BotResult<T> {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .http
                .get(self.url(path))
                .header(header::AUTHORIZATION, self.auth())
                .send()
                .await
                .map_err(|error| self.transport_error(operation, &error))?;
            let status = response.status();
            let retry_after_header = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_seconds);
            let body = response.bytes().await.map_err(|error| {
                self.transport_error(&format!("read {operation} response"), &error)
            })?;
            if status.is_success() {
                return serde_json::from_slice(&body).map_err(|_| {
                    self.error(format!(
                        "Discord {operation} returned an invalid JSON response"
                    ))
                });
            }
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES {
                let retry_after = retry_after_from_body(&body)
                    .or(retry_after_header)
                    .ok_or_else(|| self.status_error(operation, status, &body))?;
                if retry_after > MAX_RETRY_AFTER {
                    return Err(self.error(format!(
                        "Discord {operation} failed with HTTP {status}: retry_after exceeds the {}s safety bound",
                        MAX_RETRY_AFTER.as_secs()
                    )));
                }
                tokio::time::sleep(retry_after).await;
                continue;
            }
            return Err(self.status_error(operation, status, &body));
        }
        Err(self.error(format!("Discord {operation} exhausted its retry budget")))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    fn transport_error(
        &self,
        operation: &str,
        error: &reqwest::Error,
    ) -> Box<dyn Error + Send + Sync> {
        self.error(format!(
            "Discord {operation} request failed: {}",
            sanitize_detail(&error.to_string(), &self.token)
        ))
    }

    fn status_error(
        &self,
        operation: &str,
        status: StatusCode,
        body: &[u8],
    ) -> Box<dyn Error + Send + Sync> {
        let detail = serde_json::from_slice::<DiscordErrorResponse>(body)
            .ok()
            .and_then(|error| match (error.code, error.message) {
                (Some(code), Some(message)) => Some(format!(
                    "code {code}: {}",
                    sanitize_detail(&message, &self.token)
                )),
                (Some(code), None) => Some(format!("code {code}")),
                (None, Some(message)) => Some(sanitize_detail(&message, &self.token)),
                (None, None) => None,
            });
        let suffix = detail.map(|value| format!(": {value}")).unwrap_or_default();
        self.error(format!(
            "Discord {operation} failed with HTTP {status}{suffix}"
        ))
    }

    fn error(&self, message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
        let message = sanitize_detail(&message.into(), &self.token);
        Box::new(DiscordClientError(message))
    }
}

fn is_supported_channel_kind(kind: u8) -> bool {
    // GUILD_TEXT, GUILD_ANNOUNCEMENT, ANNOUNCEMENT_THREAD, PUBLIC_THREAD,
    // PRIVATE_THREAD. Forum/media parents require thread creation and are out of scope.
    matches!(kind, 0 | 5 | 10 | 11 | 12)
}

pub(super) fn is_snowflake(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id != 0)
}

fn retry_after_from_body(body: &[u8]) -> Option<Duration> {
    let response: RateLimitResponse = serde_json::from_slice(body).ok()?;
    duration_from_seconds(response.retry_after)
}

fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    duration_from_seconds(value.trim().parse().ok()?)
}

fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

fn sanitize_detail(detail: &str, token: &str) -> String {
    let redacted = if token.is_empty() {
        detail.to_owned()
    } else {
        detail.replace(token, "[REDACTED]")
    };
    let mut clean = String::with_capacity(redacted.len().min(MAX_ERROR_DETAIL_CHARS));
    for (index, ch) in redacted.chars().enumerate() {
        if index >= MAX_ERROR_DETAIL_CHARS {
            clean.push('…');
            break;
        }
        if ch.is_control() {
            clean.push(' ');
        } else {
            clean.push(ch);
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, Request, StatusCode as AxumStatusCode};
    use axum::middleware::{self, Next};
    use axum::response::Response;
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    use super::*;

    const TEST_TOKEN: &str = "test-secret-token";
    const TEST_CHANNEL: &str = "123456789012345678";

    #[derive(Clone, Default)]
    struct MockState {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        send_attempts: Arc<AtomicUsize>,
        startup_rate_limits: Arc<AtomicUsize>,
    }

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        path: String,
        authorization: Option<String>,
        user_agent: Option<String>,
        json: Option<Value>,
    }

    async fn capture_request(
        State(state): State<MockState>,
        request: Request<Body>,
        next: Next,
    ) -> Response {
        let path = request.uri().path().to_owned();
        let authorization = header_value(request.headers(), header::AUTHORIZATION);
        let user_agent = header_value(request.headers(), header::USER_AGENT);
        state.requests.lock().await.push(CapturedRequest {
            path,
            authorization,
            user_agent,
            json: None,
        });
        next.run(request).await
    }

    async fn current_user(State(state): State<MockState>) -> (AxumStatusCode, Json<Value>) {
        if state
            .startup_rate_limits
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return (
                AxumStatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "message": "rate limited", "retry_after": 0.0 })),
            );
        }
        (
            AxumStatusCode::OK,
            Json(json!({
                "id": "999999999999999999",
                "username": "rozum-test",
                "bot": true
            })),
        )
    }

    async fn target_channel(Path(channel): Path<String>) -> Json<Value> {
        Json(json!({ "id": channel, "type": 0, "guild_id": "guild-1" }))
    }

    async fn gateway() -> Json<Value> {
        Json(json!({ "url": "wss://gateway.example.invalid" }))
    }

    async fn send_message(
        State(state): State<MockState>,
        Path(_channel): Path<String>,
        Json(payload): Json<Value>,
    ) -> (AxumStatusCode, Json<Value>) {
        let attempt = state.send_attempts.fetch_add(1, Ordering::SeqCst);
        if let Some(last) = state.requests.lock().await.last_mut() {
            last.json = Some(payload);
        }
        if attempt == 0 {
            (
                AxumStatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "message": "rate limited", "retry_after": 0.0 })),
            )
        } else {
            (AxumStatusCode::OK, Json(json!({ "id": attempt })))
        }
    }

    fn mock_router(state: MockState) -> Router {
        Router::new()
            .route("/users/@me", get(current_user))
            .route("/channels/{channel}", get(target_channel))
            .route("/gateway/bot", get(gateway))
            .route("/channels/{channel}/messages", post(send_message))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                capture_request,
            ))
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

    fn header_value(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn validates_bot_channel_and_gateway_with_sanitized_headers() {
        let state = MockState::default();
        let base = start_mock(state.clone()).await;
        let bot = DiscordBot::with_api_base(TEST_TOKEN.into(), TEST_CHANNEL.into(), &base);

        let startup = bot.validate_startup().await.unwrap();

        assert_eq!(startup.bot_user_id, "999999999999999999");
        assert_eq!(startup.bot_username, "rozum-test");
        assert_eq!(
            startup.gateway_url,
            "wss://gateway.example.invalid/?v=10&encoding=json"
        );
        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            ["/users/@me", "/channels/123456789012345678", "/gateway/bot"]
        );
        assert!(requests.iter().all(|request| {
            request.authorization.as_deref() == Some("Bot test-secret-token")
                && request.user_agent.as_deref() == Some(DISCORD_USER_AGENT)
        }));
    }

    #[tokio::test]
    async fn chunks_messages_disables_mentions_and_retries_one_429() {
        let state = MockState::default();
        let base = start_mock(state.clone()).await;
        let bot = DiscordBot::with_api_base(TEST_TOKEN.into(), TEST_CHANNEL.into(), &base);
        let text = format!("{}😀tail", "a".repeat(1899));

        bot.send_message(&text).await.unwrap();

        let requests = state.requests.lock().await;
        let sends: Vec<&CapturedRequest> = requests
            .iter()
            .filter(|request| request.path.ends_with("/messages"))
            .collect();
        assert_eq!(sends.len(), 3, "two chunks plus one rate-limit retry");
        for request in sends {
            let payload = request.json.as_ref().unwrap();
            let content = payload["content"].as_str().unwrap();
            assert!(content.encode_utf16().count() <= MAX_MESSAGE_UTF16_UNITS);
            assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        }
    }

    #[tokio::test]
    async fn startup_validation_retries_a_bounded_rate_limit() {
        let state = MockState {
            startup_rate_limits: Arc::new(AtomicUsize::new(1)),
            ..MockState::default()
        };
        let base = start_mock(state.clone()).await;
        let bot = DiscordBot::with_api_base(TEST_TOKEN.into(), TEST_CHANNEL.into(), &base);

        let startup = bot.validate_startup().await.unwrap();

        assert_eq!(startup.bot_user_id, "999999999999999999");
        let requests = state.requests.lock().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == "/users/@me")
                .count(),
            2
        );
    }

    #[test]
    fn status_errors_include_status_and_never_the_token() {
        let bot = DiscordBot::new(TEST_TOKEN.into(), TEST_CHANNEL.into());
        let body = json!({
            "code": 40001,
            "message": format!("invalid credential {TEST_TOKEN}")
        })
        .to_string();

        let error = bot
            .status_error(
                "validate bot token",
                StatusCode::UNAUTHORIZED,
                body.as_bytes(),
            )
            .to_string();

        assert!(error.contains("401 Unauthorized"));
        assert!(error.contains("code 40001"));
        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains(TEST_TOKEN));
    }

    #[test]
    fn retry_after_parser_accepts_fractional_seconds_and_rejects_invalid_values() {
        assert_eq!(
            retry_after_from_body(br#"{"retry_after":0.25}"#),
            Some(Duration::from_millis(250))
        );
        assert_eq!(parse_retry_after_seconds("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after_seconds("NaN"), None);
        assert_eq!(parse_retry_after_seconds("-1"), None);
        assert_eq!(parse_retry_after_seconds("1e300"), None);
    }

    #[test]
    fn validates_channel_snowflakes_without_accepting_url_fragments() {
        assert!(is_snowflake(TEST_CHANNEL));
        assert!(!is_snowflake(""));
        assert!(!is_snowflake("0"));
        assert!(!is_snowflake("123/messages"));
    }
}
