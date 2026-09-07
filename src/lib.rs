//! grok-cli — an agentic coding harness for xAI's Grok models.
//!
//! The crate is split so that everything except [`tui`] is headless and
//! testable without a terminal:
//!
//! ```text
//! cli ──▶ agent ──▶ api          (talk to the model)
//!          │  └───▶ tools        (act on the machine)
//!          │  └───▶ permissions  (decide whether an action is allowed)
//!          │  └───▶ hooks        (let the user veto or observe actions)
//!          │  └───▶ mcp          (borrow tools from external servers)
//!          └──────▶ session      (persist the transcript)
//! tui  ──▶ agent                 (drive it, render its events)
//! ```
//!
//! The agent never talks to the terminal and the terminal never talks to the
//! API. They meet at [`agent::AgentEvent`], a stream of facts about what the
//! agent is doing, which the TUI renders and the headless runner prints.

pub mod api;
pub mod config;
pub mod tools;
pub mod util;

/// The user-facing product name, used in the banner and the system prompt.
pub const APP_NAME: &str = "grok-cli";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
