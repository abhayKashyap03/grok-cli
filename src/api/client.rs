//! HTTP client for the xAI chat-completions endpoint.
//!
//! Responsibilities kept here: authentication, retry/backoff, cancellation, and
//! translating transport bytes into [`StreamEvent`]s via [`crate::api::sse`].
//! Everything about *what* to say to the model lives in the agent layer.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::sse::{DeltaAccumulator, SseDecoder, completion_from_response};
use crate::api::types::{
    ChatChunk, ChatRequest, ChatResponse, Completion, Message, StreamEvent, StreamOptions, ToolSpec,
};

pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// Per-request knobs that the agent varies between turns.
#[derive(Debug, Clone)]
pub struct RequestOptions {
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// `low` / `high` for reasoning models. Ignored by non-reasoning models, and
    /// rejected by some, so it is only sent when explicitly configured.
    pub reasoning_effort: Option<String>,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            model: crate::config::DEFAULT_MODEL.to_string(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        }
    }
}

#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    api_key: String,
    base_url: String,
    max_retries: u32,
}

impl ApiClient {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            // No total-request timeout: a long agentic turn can legitimately
            // stream for minutes. Connect and idle-read timeouts still guard
            // against a genuinely dead peer.
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(180))
            .user_agent(concat!("grok-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, api_key: api_key.into(), base_url: base_url.into(), max_retries: 3 })
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn build_request(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolSpec>,
        opts: &RequestOptions,
        stream: bool,
    ) -> ChatRequest {
        ChatRequest {
            model: opts.model.clone(),
            messages,
            stream,
            max_completion_tokens: opts.max_tokens,
            temperature: opts.temperature,
            tools: (!tools.is_empty()).then_some(tools),
            tool_choice: None,
            stream_options: stream.then_some(StreamOptions { include_usage: true }),
            reasoning_effort: opts.reasoning_effort.clone(),
        }
    }

    /// Stream a completion, forwarding incremental events to `sink`.
    ///
    /// Returns the fully assembled completion. If `cancel` fires mid-stream the
    /// partial completion assembled so far is returned rather than an error:
    /// the caller still needs it to keep the transcript consistent, because any
    /// tool calls already emitted must be answered.
    pub async fn stream_chat(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolSpec>,
        opts: &RequestOptions,
        sink: &mpsc::Sender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        let body = self.build_request(messages, tools, opts, true);
        let response = self.send_with_retry(&body, cancel).await?;

        let mut decoder = SseDecoder::new();
        let mut acc = DeltaAccumulator::new();
        let mut bytes = response.bytes_stream();

        'outer: loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => break 'outer,
                item = bytes.next() => item,
            };
            let Some(item) = next else { break };
            let chunk = item.context("reading response stream")?;
            for payload in decoder.push(&chunk) {
                if payload.trim() == "[DONE]" {
                    break 'outer;
                }
                if Self::dispatch(&payload, &mut acc, sink).await? {
                    break 'outer;
                }
            }
        }

        for payload in decoder.finish() {
            if payload.trim() == "[DONE]" {
                continue;
            }
            Self::dispatch(&payload, &mut acc, sink).await?;
        }

        let completion = acc.finish();
        let _ = sink.send(StreamEvent::Done(Box::new(completion.clone()))).await;
        Ok(completion)
    }

    /// Parse one SSE payload into the accumulator and forward its events.
    /// Returns `true` if the caller should stop consuming the stream.
    async fn dispatch(
        payload: &str,
        acc: &mut DeltaAccumulator,
        sink: &mpsc::Sender<StreamEvent>,
    ) -> Result<bool> {
        // A malformed frame is worth surfacing: it usually means the provider
        // returned an error object mid-stream.
        let chunk: ChatChunk = match serde_json::from_str(payload) {
            Ok(c) => c,
            Err(_) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload)
                    && let Some(err) = v.get("error")
                {
                    bail!("API error mid-stream: {err}");
                }
                tracing::warn!(payload, "skipping unparseable stream frame");
                return Ok(false);
            }
        };
        for event in acc.push(&chunk) {
            // A closed sink means the UI went away; stop early rather than
            // burning tokens nobody will read.
            if sink.send(event).await.is_err() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Non-streaming completion. Used for internal calls (compaction summaries,
    /// title generation) where incremental output has no reader.
    pub async fn complete(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolSpec>,
        opts: &RequestOptions,
        cancel: &CancellationToken,
    ) -> Result<Completion> {
        let body = self.build_request(messages, tools, opts, false);
        let response = self.send_with_retry(&body, cancel).await?;
        let text = response.text().await.context("reading response body")?;
        let parsed: ChatResponse = serde_json::from_str(&text)
            .with_context(|| format!("decoding response body: {}", truncate(&text, 800)))?;
        completion_from_response(parsed)
    }

    /// POST with exponential backoff on rate limits and transient server errors.
    async fn send_with_retry(
        &self,
        body: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            if cancel.is_cancelled() {
                bail!("request cancelled");
            }
            let result = self
                .http
                .post(self.endpoint())
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;

            let retry_after = match result {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    if !is_retryable(status) || attempt >= self.max_retries {
                        let detail = resp.text().await.unwrap_or_default();
                        bail!("API error {}: {}", status, truncate(&detail, 1200));
                    }
                    // Honour `Retry-After` when the server sends one.
                    resp.headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs)
                }
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(e).context("sending request to xAI");
                    }
                    None
                }
            };

            attempt += 1;
            let backoff = retry_after.unwrap_or_else(|| Duration::from_millis(500 * (1 << attempt)));
            tracing::warn!(attempt, ?backoff, "retrying request");
            tokio::select! {
                () = cancel.cancelled() => bail!("request cancelled"),
                () = tokio::time::sleep(backoff) => {}
            }
        }
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Never split a UTF-8 code point.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &s[..end], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rate_limits_and_server_errors_are_retried() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn truncate_never_splits_a_code_point() {
        let s = "aaaa😀bbbb";
        let out = truncate(s, 5);
        assert!(out.starts_with("aaaa"), "got {out}");
        assert!(out.contains("bytes total"));
    }

    #[test]
    fn streaming_requests_ask_for_usage_and_omit_empty_tool_lists() {
        let client = ApiClient::new("k", DEFAULT_BASE_URL).unwrap();
        let opts = RequestOptions::default();
        let req = client.build_request(vec![Message::user("hi")], vec![], &opts, true);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert!(json.get("tools").is_none(), "empty tool list must be omitted");
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn non_streaming_requests_omit_stream_options() {
        let client = ApiClient::new("k", DEFAULT_BASE_URL).unwrap();
        let req = client.build_request(vec![], vec![], &RequestOptions::default(), false);
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("stream_options").is_none());
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn endpoint_tolerates_a_trailing_slash_in_the_base_url() {
        let c = ApiClient::new("k", "https://example.com/v1/").unwrap();
        assert_eq!(c.endpoint(), "https://example.com/v1/chat/completions");
    }
}
