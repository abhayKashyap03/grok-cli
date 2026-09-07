//! Server-sent-events parsing and streaming-delta accumulation.
//!
//! Two concerns live here, deliberately separated from the HTTP client so both
//! are unit-testable without a network:
//!
//! * [`SseDecoder`] — turns an arbitrarily-chunked byte stream into complete
//!   `data:` payloads. Network chunks do not respect event boundaries, so this
//!   buffers until it sees a blank line.
//! * [`DeltaAccumulator`] — folds streamed [`ChatChunk`]s into a single
//!   [`Completion`], reassembling tool calls whose JSON arguments arrive in
//!   fragments.

use crate::api::types::{
    ChatChunk, Completion, Message, Role, StreamEvent, ToolCall, Usage,
};

/// Largest partial event held while waiting for its terminator.
///
/// An endpoint that never sends a blank line would otherwise grow this without
/// limit. The base URL is user-configurable, so that endpoint is not
/// necessarily trustworthy.
const MAX_PENDING_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Incrementally extracts SSE `data:` payloads from a byte stream.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    /// Set once the buffer has overflowed, so the error is reported once.
    overflowed: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes; returns every complete payload it unlocked.
    ///
    /// Invalid UTF-8 is replaced rather than erroring: a split multi-byte
    /// character at a chunk boundary is a transport artifact, not a protocol
    /// error, and dropping the whole stream over it would be wrong.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        if self.overflowed {
            return Vec::new();
        }
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        if self.buffer.len() > MAX_PENDING_EVENT_BYTES {
            tracing::warn!(
                bytes = self.buffer.len(),
                "discarding an oversized SSE event with no terminator"
            );
            self.buffer.clear();
            self.overflowed = true;
            return Vec::new();
        }
        self.drain()
    }

    /// Whether the stream exceeded the per-event limit and was abandoned.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        // Events are separated by a blank line. Normalize CRLF first so the
        // separator search only has to consider "\n\n".
        if self.buffer.contains('\r') {
            self.buffer = self.buffer.replace("\r\n", "\n");
        }
        while let Some(idx) = self.buffer.find("\n\n") {
            let event = self.buffer[..idx].to_string();
            self.buffer.drain(..idx + 2);
            if let Some(payload) = Self::extract_data(&event) {
                out.push(payload);
            }
        }
        out
    }

    /// Flush a trailing event that was never terminated by a blank line.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = self.drain();
        let rest = std::mem::take(&mut self.buffer);
        if let Some(payload) = Self::extract_data(&rest) {
            out.push(payload);
        }
        out
    }

    /// Join the `data:` lines of a single event, ignoring comments and other
    /// SSE fields (`event:`, `id:`, `retry:`) that this API never uses.
    fn extract_data(event: &str) -> Option<String> {
        let mut data = String::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if data.trim().is_empty() { None } else { Some(data) }
    }
}

/// Reassembles streamed chunks into one completion.
#[derive(Debug, Default)]
pub struct DeltaAccumulator {
    text: String,
    reasoning: String,
    /// Partial tool calls keyed by their streaming index.
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Usage,
    /// Indices already emitted as [`StreamEvent::ToolCallReady`], so we never
    /// announce the same call twice.
    announced: Vec<usize>,
}

#[derive(Debug, Default, Clone)]
struct PartialCall {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

impl DeltaAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one chunk in, returning the events it produced.
    pub fn push(&mut self, chunk: &ChatChunk) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if let Some(usage) = &chunk.usage {
            // Usage arrives on its own final frame when `include_usage` is set.
            self.usage = *usage;
        }

        for choice in &chunk.choices {
            if let Some(text) = &choice.delta.content
                && !text.is_empty()
            {
                self.text.push_str(text);
                events.push(StreamEvent::Text(text.clone()));
            }
            if let Some(r) = &choice.delta.reasoning_content
                && !r.is_empty()
            {
                self.reasoning.push_str(r);
                events.push(StreamEvent::Reasoning(r.clone()));
            }
            if let Some(deltas) = &choice.delta.tool_calls {
                for d in deltas {
                    let slot = self.slot(d.index);
                    if let Some(id) = &d.id
                        && !id.is_empty()
                    {
                        slot.id = id.clone();
                    }
                    if let Some(f) = &d.function {
                        if let Some(name) = &f.name
                            && !name.is_empty()
                        {
                            slot.name.push_str(name);
                        }
                        if let Some(args) = &f.arguments {
                            slot.arguments.push_str(args);
                        }
                    }
                }
            }
            if let Some(reason) = &choice.finish_reason {
                self.finish_reason = Some(reason.clone());
                // Arguments are only guaranteed complete once the turn ends.
                events.extend(self.announce_ready());
            }
        }

        events
    }

    fn slot(&mut self, index: usize) -> &mut PartialCall {
        if let Some(pos) = self.calls.iter().position(|c| c.index == index) {
            return &mut self.calls[pos];
        }
        self.calls.push(PartialCall { index, ..Default::default() });
        self.calls.last_mut().expect("just pushed")
    }

    fn announce_ready(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for call in &self.calls {
            if self.announced.contains(&call.index) || call.name.is_empty() {
                continue;
            }
            events.push(StreamEvent::ToolCallReady(ToolCall::new(
                call.effective_id(),
                &call.name,
                &call.arguments,
            )));
        }
        for call in &self.calls {
            if !call.name.is_empty() && !self.announced.contains(&call.index) {
                self.announced.push(call.index);
            }
        }
        events
    }

    /// Consume the accumulator and produce the finished completion.
    pub fn finish(mut self) -> Completion {
        let tool_calls: Vec<ToolCall> = {
            self.calls.sort_by_key(|c| c.index);
            self.calls
                .iter()
                .filter(|c| !c.name.is_empty())
                .map(|c| ToolCall::new(c.effective_id(), &c.name, &c.arguments))
                .collect()
        };

        Completion {
            message: Message {
                role: Role::Assistant.as_str().into(),
                content: (!self.text.is_empty()).then(|| self.text.clone()),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                tool_call_id: None,
                reasoning_content: (!self.reasoning.is_empty()).then(|| self.reasoning.clone()),
            },
            finish_reason: self.finish_reason.clone(),
            usage: self.usage,
        }
    }
}

impl PartialCall {
    /// Some providers omit the call id on tool calls. The id is only used to
    /// pair a result back to its call, so a synthetic one is fine as long as it
    /// is stable and unique within the turn.
    fn effective_id(&self) -> String {
        if self.id.is_empty() { format!("call_{}", self.index) } else { self.id.clone() }
    }
}

/// Assemble a completion from a non-streaming response body.
pub fn completion_from_response(resp: crate::api::types::ChatResponse) -> anyhow::Result<Completion> {
    let usage = resp.usage.unwrap_or_default();
    let choice = resp.choices.into_iter().next().ok_or_else(|| anyhow::anyhow!("API returned no choices"))?;
    let message = choice.message.unwrap_or_else(|| Message {
        role: Role::Assistant.as_str().into(),
        ..Default::default()
    });
    Ok(Completion { message, finish_reason: choice.finish_reason, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    impl PartialCall {
        fn into_call(self) -> ToolCall {
            ToolCall::new(self.effective_id(), self.name, self.arguments)
        }
    }

    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).expect("valid chunk")
    }

    #[test]
    fn decoder_handles_events_split_across_chunk_boundaries() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"a\":").is_empty(), "incomplete event must not be emitted");
        let out = d.push(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn decoder_normalizes_crlf_and_skips_comments() {
        let mut d = SseDecoder::new();
        let out = d.push(b": ping\r\n\r\ndata: hello\r\n\r\n");
        assert_eq!(out, vec!["hello".to_string()]);
    }

    #[test]
    fn decoder_flushes_unterminated_trailing_event() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: tail").is_empty());
        assert_eq!(d.finish(), vec!["tail".to_string()]);
    }

    #[test]
    fn accumulator_joins_text_deltas() {
        let mut acc = DeltaAccumulator::new();
        acc.push(&chunk(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#));
        acc.push(&chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#));
        let done = acc.finish();
        assert_eq!(done.message.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn accumulator_reassembles_fragmented_tool_arguments() {
        let mut acc = DeltaAccumulator::new();
        acc.push(&chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
        ));
        acc.push(&chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        ));
        let events = acc.push(&chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#));
        assert_eq!(events.len(), 1, "the ready call is announced exactly once");

        let done = acc.finish();
        let calls = done.message.tool_calls.expect("tool calls present");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].parsed_arguments().unwrap()["path"], "a.rs");
    }

    #[test]
    fn accumulator_keeps_parallel_calls_separate_and_ordered() {
        let mut acc = DeltaAccumulator::new();
        // Providers interleave fragments of parallel calls arbitrarily.
        acc.push(&chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"glob","arguments":"{\"p\":2}"}}]}}]}"#,
        ));
        acc.push(&chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"grep","arguments":"{\"p\":1}"}}]}}]}"#,
        ));
        let done = acc.finish();
        let calls = done.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "grep", "calls are ordered by streaming index");
        assert_eq!(calls[1].function.name, "glob");
    }

    #[test]
    fn accumulator_synthesizes_ids_when_the_provider_omits_them() {
        let call = PartialCall { index: 3, id: String::new(), name: "x".into(), arguments: "{}".into() };
        assert_eq!(call.into_call().id, "call_3");
    }

    #[test]
    fn accumulator_captures_usage_from_the_final_frame() {
        let mut acc = DeltaAccumulator::new();
        acc.push(&chunk(r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}"#));
        assert_eq!(acc.finish().usage.total_tokens, 14);
    }
}
