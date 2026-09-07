//! xAI API client: wire types, streaming, and transport.

pub mod client;
pub mod sse;
pub mod types;

pub use client::{ApiClient, DEFAULT_BASE_URL, RequestOptions};
pub use types::{Completion, Message, Role, StreamEvent, ToolCall, ToolSpec, Usage};
