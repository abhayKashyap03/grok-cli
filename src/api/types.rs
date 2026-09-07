//! Wire types for the xAI chat-completions API.
//!
//! The xAI API is OpenAI-compatible, so these types double as a description of
//! the OpenAI chat-completions schema. Everything that can be absent on the wire
//! is an `Option` and is skipped during serialization, because xAI rejects
//! explicit `null`s for several of these fields.

use serde::{Deserialize, Serialize};

/// The role of a message in the conversation.
///
/// Serialized as a lowercase string to match the wire format.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A single message in a conversation.
///
/// This is the unit that gets sent to the API and stored in the session
/// transcript. Assistant messages may carry `tool_calls`; tool messages must
/// carry the `tool_call_id` they are responding to.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Message {
    pub role: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Reasoning traces emitted by reasoning models. Captured for display but
    /// never sent back to the API (the provider reconstructs it server-side and
    /// echoing it back is rejected).
    #[serde(skip_serializing, default)]
    pub reasoning_content: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System.as_str().into(), content: Some(content.into()), ..Default::default() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User.as_str().into(), content: Some(content.into()), ..Default::default() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant.as_str().into(), content: Some(content.into()), ..Default::default() }
    }

    /// A tool result message. `tool_call_id` must match the id of the
    /// assistant's tool call, or the API rejects the whole request.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool.as_str().into(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    pub fn is_role(&self, role: Role) -> bool {
        self.role == role.as_str()
    }

    /// Rough character count of everything this message contributes to context.
    pub fn char_len(&self) -> usize {
        let mut n = self.content.as_deref().map_or(0, str::len);
        if let Some(calls) = &self.tool_calls {
            for c in calls {
                n += c.function.name.len() + c.function.arguments.len();
            }
        }
        n
    }
}

/// The function payload of a tool call. `arguments` is a JSON *string*, not an
/// object — that is the OpenAI wire format, and models stream it in fragments.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: arguments.into() },
        }
    }

    /// Parse `arguments` into a JSON value, tolerating the empty string that
    /// models emit for zero-argument tools.
    pub fn parsed_arguments(&self) -> Result<serde_json::Value, serde_json::Error> {
        let raw = self.function.arguments.trim();
        if raw.is_empty() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(raw)
    }
}

/// A tool advertised to the model.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn function(name: impl Into<String>, description: impl Into<String>, parameters: serde_json::Value) -> Self {
        Self {
            kind: "function".into(),
            function: FunctionSpec { name: name.into(), description: description.into(), parameters },
        }
    }
}

/// Token accounting returned by the API.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Prompt tokens served from the provider's cache. Cheaper, and worth
    /// surfacing because it tells the user their prefix is stable.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

impl Usage {
    pub fn cached_tokens(&self) -> u64 {
        self.prompt_tokens_details.map_or(0, |d| d.cached_tokens)
    }

    /// Accumulate another usage record into this one.
    pub fn add(&mut self, other: &Self) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        let cached = self.cached_tokens() + other.cached_tokens();
        if cached > 0 {
            self.prompt_tokens_details = Some(PromptTokensDetails { cached_tokens: cached });
        }
    }
}

/// A completed assistant turn, assembled from either a streamed or a
/// non-streamed response.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub message: Message,
    pub finish_reason: Option<String>,
    pub usage: Usage,
}

impl Completion {
    pub fn has_tool_calls(&self) -> bool {
        self.message.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
    }
}

/// Incremental events produced while streaming a completion.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant-visible text.
    Text(String),
    /// A chunk of reasoning text (reasoning models only).
    Reasoning(String),
    /// A tool call became fully known (name + complete arguments).
    ToolCallReady(ToolCall),
    /// The turn finished; carries the fully assembled completion.
    Done(Box<Completion>),
}

// ---------------------------------------------------------------------------
// Request / response envelopes
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub stream: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Serialize, Debug, Clone, Copy)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Deserialize, Debug)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
pub struct Choice {
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// One `data:` frame of a streaming response.
#[derive(Deserialize, Debug, Default)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug, Default)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// A partial tool call. `index` is the only reliable correlation key: `id` and
/// `name` arrive once, on the first fragment, and `arguments` dribbles in over
/// many subsequent fragments carrying nothing but the index.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_messages_serialize_with_their_call_id() {
        let m = Message::tool_result("call_1", "ok");
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert!(json.get("tool_calls").is_none(), "absent fields must be omitted, not null");
    }

    #[test]
    fn empty_arguments_parse_as_an_empty_object() {
        let call = ToolCall::new("1", "noop", "");
        assert_eq!(call.parsed_arguments().unwrap(), serde_json::json!({}));
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        total.add(&Usage { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, prompt_tokens_details: None });
        total.add(&Usage {
            prompt_tokens: 20,
            completion_tokens: 7,
            total_tokens: 27,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 8 }),
        });
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 12);
        assert_eq!(total.cached_tokens(), 8);
    }
}
