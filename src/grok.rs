use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::env;
use anyhow::Result;

#[derive(Serialize, Deserialize, Debug)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Serialize)]
struct GrokRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    max_completion_tokens: u16,
}

#[derive(Deserialize, Debug)]
struct GrokResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    message: Message,
}

#[derive(Clone)]
pub struct GrokClient {
    fetch_client: Client,
    api_key: String,
}

impl GrokClient {
    pub fn new() -> Self {
        let api_key = env::var("XAI_API_KEY").expect("XAI_API_KEY must be set");
        Self {
            fetch_client: Client::new(),
            api_key: api_key
        }
    }

    pub async fn send_chat(&self, messages: Vec<Message>) -> Result<String>{
        let request = GrokRequest {
            model: "grok-4-1-fast-non-reasoning".to_string(),
            messages,
            stream: false,
            max_completion_tokens: 1000,
        };

        let response = self.fetch_client.post("https://api.x.ai/v1/chat/completions")
                        .header("Authorization", format!("Bearer {}", self.api_key))
                        .json(&request)
                        .send().await?;
        
        let status = response.status();
        let text = response.text().await?;
        
        if !status.is_success() {
            return Ok(format!("API Error ({}): {}", status, text));
        }

        match serde_json::from_str::<GrokResponse>(&text) {
            Ok(decoded) => Ok(decoded.choices[0].message.content.clone()),
            Err(e) => {
                Ok(format!("Deserialization Failed: {}. Raw JSON: {}", e, text))
            }
        }     
    }
}