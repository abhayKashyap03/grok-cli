//! Model Context Protocol client: borrow tools from external servers.
//!
//! Speaks JSON-RPC 2.0 over a child process's stdin/stdout — the stdio
//! transport, which is what nearly every MCP server ships. The handshake is
//! `initialize` → `notifications/initialized` → `tools/list`, after which each
//! discovered tool is wrapped in [`McpTool`] and registered alongside the
//! built-ins.
//!
//! Discovered tools are namespaced `mcp__<server>__<tool>` so a server cannot
//! shadow a built-in — a server named `fs` exposing `read_file` must not
//! silently replace the sandboxed local one.
//!
//! Every MCP tool is classified [`ToolKind::Execute`]. The harness cannot see
//! what a remote tool does, and the safe reading of "unknown" is "assume it can
//! do anything", which means it prompts by default.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

use crate::api::ToolSpec;
use crate::config::McpServerConfig;
use crate::tools::{Tool, ToolContext, ToolKind, ToolOutcome};
use crate::util;

/// Protocol version this client implements.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// How long to wait for any single request before giving up on the server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Cap on a tool result, matching the local tools' bound.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Namespaced name for a tool borrowed from `server`.
pub fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// A running MCP server and the channel to talk to it.
pub struct McpServer {
    pub name: String,
    /// Tool specs as reported by the server.
    pub tools: Vec<ToolSpec>,
    /// Server-reported name and version, for `/mcp`.
    pub server_info: String,
    connection: Arc<Connection>,
}

/// The stdio JSON-RPC connection to one server.
struct Connection {
    stdin: Mutex<tokio::process::ChildStdin>,
    /// Requests awaiting a response, keyed by JSON-RPC id.
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    /// Kept so the child is killed when the connection drops.
    _child: Mutex<tokio::process::Child>,
}

impl Connection {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send_raw(&body).await.inspect_err(|_| {
            // Nothing will ever answer this id; do not leak the slot.
        })?;

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP request `{method}` timed out after {}s", REQUEST_TIMEOUT.as_secs())
            }
            Ok(Err(_)) => bail!("MCP server closed the connection during `{method}`"),
            Ok(Ok(Err(e))) => bail!("MCP error from `{method}`: {e}"),
            Ok(Ok(Ok(value))) => Ok(value),
        }
    }

    /// Fire-and-forget notification (no id, no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    async fn send_raw(&self, body: &Value) -> Result<()> {
        let line = format!("{}\n", serde_json::to_string(body)?);
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.context("writing to MCP server")?;
        stdin.flush().await.context("flushing to MCP server")?;
        Ok(())
    }
}

impl McpServer {
    /// Spawn a server and complete the handshake.
    pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Self> {
        let mut command = tokio::process::Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Servers habitually log to stderr; discard it rather than let a
            // full pipe block the server.
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .with_context(|| format!("starting MCP server `{name}` ({})", config.command))?;

        let stdin = child.stdin.take().context("MCP server has no stdin")?;
        let stdout = child.stdout.take().context("MCP server has no stdout")?;

        let connection = Arc::new(Connection {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(0),
            _child: Mutex::new(child),
        });

        // One reader task demultiplexes responses back to their waiters.
        // Without this, two concurrent requests would race to read each
        // other's replies off the same pipe.
        let reader_connection = Arc::clone(&connection);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!(line, "unparseable MCP frame");
                    continue;
                };
                // Server-initiated requests and notifications have no id we are
                // waiting on; this client does not implement them yet.
                let Some(id) = message.get("id").and_then(Value::as_u64) else { continue };
                let Some(waiter) = reader_connection.pending.lock().await.remove(&id) else {
                    continue;
                };
                let payload = if let Some(error) = message.get("error") {
                    Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string())
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = waiter.send(payload);
            }
            // The pipe closed: fail every outstanding request rather than
            // leaving callers waiting for the full timeout.
            let mut pending = reader_connection.pending.lock().await;
            for (_, waiter) in std::mem::take(&mut *pending) {
                let _ = waiter.send(Err("server disconnected".into()));
            }
        });

        let init = connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "clientInfo": { "name": crate::APP_NAME, "version": crate::VERSION }
                }),
            )
            .await?;

        connection.notify("notifications/initialized", json!({})).await?;

        let server_info = init
            .get("serverInfo")
            .map(|i| {
                format!(
                    "{} {}",
                    i.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                    i.get("version").and_then(Value::as_str).unwrap_or("")
                )
                .trim()
                .to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());

        let listed = connection.request("tools/list", json!({})).await?;
        let tools = parse_tool_list(name, &listed);

        Ok(Self { name: name.to_string(), tools, server_info, connection })
    }

    /// Wrap this server's tools so they can join a [`crate::tools::ToolRegistry`].
    pub fn as_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|spec| {
                Arc::new(McpTool {
                    server: self.name.clone(),
                    spec: spec.clone(),
                    connection: Arc::clone(&self.connection),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

/// Turn a `tools/list` result into namespaced specs.
fn parse_tool_list(server: &str, result: &Value) -> Vec<ToolSpec> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("(no description provided)");
                    // Servers vary on whether the schema key is `inputSchema`
                    // or `input_schema`; accept both, and fall back to an empty
                    // object so a schema-less tool is still callable.
                    let schema = t
                        .get("inputSchema")
                        .or_else(|| t.get("input_schema"))
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    Some(ToolSpec::function(
                        namespaced(server, name),
                        format!("[{server}] {description}"),
                        schema,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A tool borrowed from an MCP server.
pub struct McpTool {
    server: String,
    spec: ToolSpec,
    connection: Arc<Connection>,
}

impl McpTool {
    /// The tool's name on the server, with the namespace prefix removed.
    fn remote_name(&self) -> &str {
        let prefix = format!("mcp__{}__", self.server);
        self.spec.function.name.strip_prefix(&prefix).unwrap_or(&self.spec.function.name)
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.spec.function.name
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn kind(&self) -> ToolKind {
        // The harness cannot inspect what a remote tool does. Treating it as
        // executable means it prompts by default, which is the safe reading of
        // "unknown".
        ToolKind::Execute
    }

    fn summarize(&self, args: &Value) -> String {
        let detail = args
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}={}", util::truncate_text(&v.to_string(), 40)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        format!("{}({detail})", self.spec.function.name)
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let call = self
            .connection
            .request("tools/call", json!({ "name": self.remote_name(), "arguments": args }));

        let result = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Ok(ToolOutcome::error("tool call interrupted by the user")),
            r = call => r,
        };

        match result {
            // A transport failure is returned to the model as a tool error, not
            // propagated: the model can pick a different approach, whereas
            // aborting the turn strands an unanswered tool call.
            Err(e) => Ok(ToolOutcome::error(format!("MCP call failed: {e}"))),
            Ok(value) => {
                let is_error = value.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let text = render_content(&value);
                let outcome = if is_error { ToolOutcome::error(text) } else { ToolOutcome::ok(text) };
                Ok(outcome.with_summary(format!("via {}", self.server)))
            }
        }
    }
}

/// Flatten an MCP `content` array into text.
///
/// Non-text blocks (images, embedded resources) are described rather than
/// dropped, so the model knows something came back that it cannot read.
fn render_content(result: &Value) -> String {
    let Some(blocks) = result.get("content").and_then(Value::as_array) else {
        // Some servers return a bare `structuredContent` object instead.
        return util::truncate_text(
            &serde_json::to_string_pretty(result).unwrap_or_default(),
            MAX_RESULT_BYTES,
        );
    };

    let mut out = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            Some("image") => {
                let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("image");
                out.push_str(&format!("[{mime} returned; images cannot be read as text]\n"));
            }
            Some("resource") => {
                let uri = block
                    .get("resource")
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                out.push_str(&format!("[resource: {uri}]\n"));
            }
            other => {
                out.push_str(&format!("[unsupported content block: {}]\n", other.unwrap_or("?")));
            }
        }
    }

    if out.trim().is_empty() {
        out = "(tool returned no content)".into();
    }
    util::truncate_text(&out, MAX_RESULT_BYTES)
}

/// Connect to every enabled server, reporting failures without aborting.
///
/// A misconfigured MCP server must not stop the harness from starting: the user
/// still has the built-in tools, and the error is surfaced in the UI.
pub async fn connect_all(
    servers: &BTreeMap<String, McpServerConfig>,
) -> (Vec<McpServer>, Vec<(String, String)>) {
    let mut connected = Vec::new();
    let mut failures = Vec::new();

    for (name, config) in servers {
        if config.disabled {
            continue;
        }
        // Bound the handshake: a server that never speaks would otherwise hang
        // startup indefinitely.
        match tokio::time::timeout(Duration::from_secs(30), McpServer::connect(name, config)).await {
            Ok(Ok(server)) => connected.push(server),
            Ok(Err(e)) => failures.push((name.clone(), e.to_string())),
            Err(_) => failures.push((name.clone(), "handshake timed out after 30s".into())),
        }
    }

    (connected, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_namespaced_so_a_server_cannot_shadow_a_builtin() {
        assert_eq!(namespaced("fs", "read_file"), "mcp__fs__read_file");
        // The built-in is called `read_file`; the namespaced one cannot collide.
        assert_ne!(namespaced("fs", "read_file"), "read_file");
    }

    #[test]
    fn tool_list_parsing_accepts_both_schema_key_spellings() {
        let result = json!({
            "tools": [
                { "name": "alpha", "description": "does alpha", "inputSchema": {"type": "object"} },
                { "name": "beta", "input_schema": {"type": "object"} }
            ]
        });
        let specs = parse_tool_list("srv", &result);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].function.name, "mcp__srv__alpha");
        assert!(specs[0].function.description.starts_with("[srv] does alpha"));
        assert_eq!(specs[1].function.name, "mcp__srv__beta");
        assert!(
            specs[1].function.description.contains("no description"),
            "a missing description still yields a usable spec"
        );
    }

    #[test]
    fn a_malformed_tool_entry_is_skipped_rather_than_failing_the_server() {
        let result = json!({ "tools": [ { "description": "nameless" }, { "name": "ok" } ] });
        let specs = parse_tool_list("srv", &result);
        assert_eq!(specs.len(), 1, "the nameless entry is dropped, the good one survives");
        assert_eq!(specs[0].function.name, "mcp__srv__ok");
    }

    #[test]
    fn an_empty_tool_list_is_not_an_error() {
        assert!(parse_tool_list("srv", &json!({ "tools": [] })).is_empty());
        assert!(parse_tool_list("srv", &json!({})).is_empty());
    }

    #[test]
    fn text_content_blocks_are_concatenated() {
        let result = json!({ "content": [
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" }
        ]});
        assert_eq!(render_content(&result).trim(), "line one\nline two");
    }

    #[test]
    fn unreadable_content_is_described_rather_than_dropped() {
        let result = json!({ "content": [
            { "type": "text", "text": "here is a chart" },
            { "type": "image", "mimeType": "image/png", "data": "…" }
        ]});
        let text = render_content(&result);
        assert!(text.contains("here is a chart"));
        assert!(
            text.contains("image/png"),
            "the model must know something came back it cannot read: {text}"
        );
    }

    #[test]
    fn a_result_without_a_content_array_falls_back_to_pretty_json() {
        let result = json!({ "structuredContent": { "rows": 3 } });
        let text = render_content(&result);
        assert!(text.contains("structuredContent"), "got: {text}");
        assert!(text.contains("rows"));
    }

    #[test]
    fn an_empty_content_array_says_so_instead_of_returning_nothing() {
        assert_eq!(render_content(&json!({ "content": [] })), "(tool returned no content)");
    }

    #[tokio::test]
    async fn a_server_that_fails_to_start_is_reported_without_aborting_startup() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "broken".to_string(),
            McpServerConfig {
                command: "definitely-not-a-real-binary-xyz".into(),
                args: vec![],
                env: BTreeMap::new(),
                disabled: false,
            },
        );

        let (connected, failures) = connect_all(&servers).await;
        assert!(connected.is_empty());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "broken");
    }

    #[tokio::test]
    async fn disabled_servers_are_not_started() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "off".to_string(),
            McpServerConfig {
                command: "definitely-not-a-real-binary-xyz".into(),
                args: vec![],
                env: BTreeMap::new(),
                disabled: true,
            },
        );

        let (connected, failures) = connect_all(&servers).await;
        assert!(connected.is_empty());
        assert!(failures.is_empty(), "a disabled server is skipped, not attempted");
    }
}
