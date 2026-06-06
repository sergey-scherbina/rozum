use serde::Deserialize;

pub struct TelegramBot {
    token: String,
    pub chat_id: i64,
    http: reqwest::Client,
}

#[derive(Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

#[derive(Deserialize)]
pub struct TgMessage {
    pub chat: TgChat,
    pub from: Option<TgFrom>,
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct TgChat {
    pub id: i64,
}

#[derive(Deserialize)]
pub struct TgFrom {
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

pub struct IncomingMessage {
    pub sender_name: String,
    pub text: String,
}

impl TelegramBot {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            token,
            chat_id,
            http: reqwest::Client::new(),
        }
    }

    fn api(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<TgUpdate>, Box<dyn std::error::Error + Send + Sync>> {
        let url = self.api("getUpdates");
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", timeout_secs.to_string()),
                ("allowed_updates", "[\"message\"]".to_string()),
            ])
            .timeout(std::time::Duration::from_secs(timeout_secs + 10))
            .send()
            .await?;

        let api: ApiResponse<Vec<TgUpdate>> = resp.json().await?;
        if !api.ok {
            return Err(api
                .description
                .unwrap_or_else(|| "Telegram API error".to_owned())
                .into());
        }
        Ok(api.result.unwrap_or_default())
    }

    pub async fn send_message(
        &self,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = self.api("sendMessage");
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
            }))
            .send()
            .await?;

        let api: ApiResponse<serde_json::Value> = resp.json().await?;
        if !api.ok {
            return Err(api
                .description
                .unwrap_or_else(|| "sendMessage failed".to_owned())
                .into());
        }
        Ok(())
    }

    pub fn extract_message(update: &TgUpdate) -> Option<IncomingMessage> {
        let msg = update.message.as_ref()?;
        let text = msg.text.as_deref()?.trim();
        if text.is_empty() {
            return None;
        }
        let sender_name = msg
            .from
            .as_ref()
            .map(|f| {
                if f.first_name.is_empty() {
                    f.username.as_deref().unwrap_or("user").to_owned()
                } else {
                    f.first_name.clone()
                }
            })
            .unwrap_or_else(|| "user".to_owned());
        Some(IncomingMessage {
            sender_name,
            text: text.to_owned(),
        })
    }
}
