use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::env;
use anyhow::Result;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    pub role: String,

    #[serde(skip_serializing_if="Option::is_none")]
    pub content: Option<String>,

    #[serde(skip_serializing_if="Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    #[serde(skip_serializing_if="Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct GrokRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    max_completion_tokens: u16,

    #[serde(skip_serializing_if="Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Deserialize, Debug)]
struct GrokResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    message: Message,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Function {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Tool {
    pub r#type: String,
    pub function: Function,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: FunctionCall,
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

    fn get_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                r#type: "function".to_string(),
                function: Function {
                    name: "list_files".to_string(),
                    description: "Lists all files and directory in the given path. Use this explore project structure.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "The directory whose content to list. Use '.' for current directory."
                            }
                        },
                        "required": ["path"]
                    })
                }
            },
            Tool {
                r#type: "function".to_string(),
                function: Function {
                    name: "read_file".to_string(),
                    description: "Read the contents of the given file.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Path of the file to read."
                            }
                        },
                        "required": ["path"]
                    })
                }
            }
        ]
    }

    pub async fn send_chat(&self, messages: Vec<Message>) -> Result<Message>{
        let request = GrokRequest {
            model: "grok-4-1-fast-non-reasoning".to_string(),
            messages,
            stream: false,
            max_completion_tokens: 1000,
            tools: Some(self.get_tools())
        };

        let response = self.fetch_client.post("https://api.x.ai/v1/chat/completions")
                        .header("Authorization", format!("Bearer {}", self.api_key))
                        .json(&request)
                        .send().await?;
        
        let status = response.status();
        let text = response.text().await?;
        
        if !status.is_success() {
            anyhow::bail!("API Error ({}): {}", status, text);
        }

        let grok_response: GrokResponse = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("Deserialization Failed: {}. Raw JSON: {}", e, text))?;
        
        Ok(grok_response.choices[0].message.clone())
    }
}