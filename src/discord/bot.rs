use serde::Deserialize;

pub struct DiscordBot {
    pub token: String,
    pub channel_id: String,
    http: reqwest::Client,
}

pub struct IncomingMessage {
    pub sender_name: String,
    pub text: String,
}

#[derive(Deserialize)]
struct GatewayResponse {
    url: String,
}

impl DiscordBot {
    pub fn new(token: String, channel_id: String) -> Self {
        Self {
            token,
            channel_id,
            http: reqwest::Client::new(),
        }
    }

    fn auth(&self) -> String {
        format!("Bot {}", self.token)
    }

    pub async fn get_gateway_url(
        &self,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let resp: GatewayResponse = self
            .http
            .get("https://discord.com/api/v10/gateway")
            .header("Authorization", self.auth())
            .send()
            .await?
            .json()
            .await?;
        Ok(format!("{}?v=10&encoding=json", resp.url))
    }

    pub async fn send_message(
        &self,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .http
            .post(format!(
                "https://discord.com/api/v10/channels/{}/messages",
                self.channel_id
            ))
            .header("Authorization", self.auth())
            .json(&serde_json::json!({ "content": text }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord send_message failed: {body}").into());
        }
        Ok(())
    }
}
