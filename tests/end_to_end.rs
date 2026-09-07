//! End-to-end tests against a scripted mock of the xAI API.
//!
//! Unit tests cover each piece in isolation; these cover the thing that
//! actually breaks in practice — the seam between the HTTP client, the SSE
//! decoder, the agent loop and the tools. In particular they pin the invariant
//! that every tool call is answered by exactly one `tool` message, which is
//! silent until the *next* request fails.
//!
//! The mock is a raw TCP server that speaks just enough HTTP to be a chat
//! endpoint, and it deliberately splits SSE frames at awkward byte boundaries,
//! because that is what a real network does and what a naive decoder gets
//! wrong.

use std::sync::Arc;

use grok_cli::agent::{Agent, AgentEvent, StopReason};
use grok_cli::api::Message;
use grok_cli::config::{Config, PermissionMode, PermissionRules};
use grok_cli::session::Session;
use grok_cli::tools::ToolRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

/// A scripted chat endpoint. Each queued script is served to one request.
struct MockApi {
    base_url: String,
    /// Request bodies received, in order, for assertions.
    received: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl MockApi {
    /// Start a server that serves `scripts` in order, one per request.
    ///
    /// Each script is a list of SSE `data:` payloads. They are written in small
    /// chunks that intentionally straddle frame boundaries.
    async fn start(scripts: Vec<Vec<String>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let received = Arc::new(Mutex::new(Vec::new()));

        let seen = Arc::clone(&received);
        tokio::spawn(async move {
            let mut queue = scripts.into_iter();
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(script) = queue.next() else { break };

                if let Some(body) = read_http_body(&mut socket).await
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                {
                    seen.lock().await.push(json);
                }

                let headers = "HTTP/1.1 200 OK\r\n\
                               Content-Type: text/event-stream\r\n\
                               Cache-Control: no-cache\r\n\
                               Connection: close\r\n\r\n";
                if socket.write_all(headers.as_bytes()).await.is_err() {
                    continue;
                }

                // Build the whole stream, then dribble it out in small pieces
                // so frames are split mid-JSON and mid-terminator.
                let mut stream = String::new();
                for payload in &script {
                    stream.push_str(&format!("data: {payload}\n\n"));
                }
                stream.push_str("data: [DONE]\n\n");

                let bytes = stream.into_bytes();
                for chunk in bytes.chunks(7) {
                    if socket.write_all(chunk).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        Self { base_url: format!("http://127.0.0.1:{port}"), received }
    }

    async fn request_count(&self) -> usize {
        self.received.lock().await.len()
    }

    async fn request(&self, index: usize) -> serde_json::Value {
        self.received.lock().await.get(index).cloned().expect("request was recorded")
    }
}

/// Read a request and return its body, honouring Content-Length.
async fn read_http_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);

        let text = String::from_utf8_lossy(&buffer);
        let Some(split) = text.find("\r\n\r\n") else { continue };

        let length: usize = text[..split]
            .lines()
            .find_map(|l| {
                let (name, value) = l.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);

        let body_start = split + 4;
        if buffer.len() >= body_start + length {
            return Some(String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned());
        }
    }
}

// -- script builders --------------------------------------------------------

fn text_chunk(text: &str) -> String {
    serde_json::json!({ "choices": [{ "delta": { "content": text } }] }).to_string()
}

fn tool_call_chunk(index: usize, id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({
        "choices": [{
            "delta": { "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            }]}
        }]
    })
    .to_string()
}

/// Arguments continuation, carrying only the index — as providers really send.
fn tool_args_chunk(index: usize, arguments: &str) -> String {
    serde_json::json!({
        "choices": [{
            "delta": { "tool_calls": [{ "index": index, "function": { "arguments": arguments } }] }
        }]
    })
    .to_string()
}

fn finish_chunk(reason: &str) -> String {
    serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": reason }] }).to_string()
}

fn usage_chunk(prompt: u64, completion: u64) -> String {
    serde_json::json!({
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion
        }
    })
    .to_string()
}

// -- harness ----------------------------------------------------------------

fn config_for(workspace: &std::path::Path, base_url: &str, mode: PermissionMode) -> Config {
    Config {
        api_key: "test-key".into(),
        model: "grok-4-1-fast-non-reasoning".into(),
        base_url: base_url.to_string(),
        max_tokens: None,
        temperature: None,
        reasoning_effort: None,
        permission_mode: mode,
        auto_compact_threshold: 0.85,
        max_tool_iterations: 20,
        theme: "dark".into(),
        permissions: PermissionRules::default(),
        mcp_servers: Default::default(),
        hooks: Default::default(),
        workspace: workspace.to_path_buf(),
    }
}

/// Run one turn and collect every event it emitted.
async fn run_turn(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::channel(512);
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    agent.run_turn(prompt, &tx).await;
    drop(tx);
    collector.await.expect("collector")
}

fn assistant_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn stop_reason(events: &[AgentEvent]) -> StopReason {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::TurnComplete { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("every turn ends with TurnComplete")
}

fn build_agent(workspace: &std::path::Path, base_url: &str, mode: PermissionMode) -> Agent {
    let config = config_for(workspace, base_url, mode);
    let session = Session::in_memory("test", &config.model);
    Agent::new(config, session, ToolRegistry::with_builtins(), CancellationToken::new())
        .expect("agent")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_answer_streams_through_to_the_caller() {
    let mock = MockApi::start(vec![vec![
        text_chunk("The bug is "),
        text_chunk("in auth.rs."),
        finish_chunk("stop"),
        usage_chunk(120, 8),
    ]])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let events = run_turn(&mut agent, "where is the bug?").await;

    assert_eq!(assistant_text(&events), "The bug is in auth.rs.");
    assert_eq!(stop_reason(&events), StopReason::Complete);
    assert_eq!(agent.session.usage.total_tokens, 128, "usage from the final frame is recorded");
}

#[tokio::test]
async fn a_tool_call_runs_and_its_result_is_sent_back_in_a_second_request() {
    // This is the whole point of the harness, and exactly what the previous
    // implementation failed to do: it ran the tool and discarded the output.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("answer.txt"), "42\n").unwrap();

    let mock = MockApi::start(vec![
        vec![
            text_chunk("Let me look."),
            // Arguments arrive in fragments, as they really do.
            tool_call_chunk(0, "call_1", "read_file", "{\"pa"),
            tool_args_chunk(0, "th\":\"answer.txt\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("The answer is 42."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let events = run_turn(&mut agent, "what is in answer.txt?").await;

    assert!(assistant_text(&events).contains("The answer is 42."));
    assert_eq!(mock.request_count().await, 2, "the tool result must trigger a second request");

    // The second request carries the assistant message *and* its tool reply.
    let second = mock.request(1).await;
    let messages = second["messages"].as_array().expect("messages");

    let tool_replies: Vec<&serde_json::Value> =
        messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_replies.len(), 1, "exactly one reply per call");
    assert_eq!(tool_replies[0]["tool_call_id"], "call_1", "the reply is paired to its call");
    assert!(
        tool_replies[0]["content"].as_str().unwrap().contains("42"),
        "the file's contents reached the model: {tool_replies:?}"
    );

    // Ordering is part of the contract: assistant, then its results.
    let assistant_at = messages.iter().position(|m| m["role"] == "assistant").expect("assistant");
    let tool_at = messages.iter().position(|m| m["role"] == "tool").expect("tool");
    assert!(assistant_at < tool_at, "the assistant message must precede its tool results");
}

#[tokio::test]
async fn every_tool_call_is_answered_even_when_it_is_refused() {
    // An unanswered tool_call makes the *next* request fail, so a refusal must
    // still produce a `tool` message.
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "call_1", "bash", "{\"command\":\"rm -rf /\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Understood, I will not do that."), finish_chunk("stop")],
    ])
    .await;

    // Plan mode refuses execution outright, and no permission channel exists.
    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Plan);
    let events = run_turn(&mut agent, "delete everything").await;

    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::ToolDenied { .. })),
        "the refusal is surfaced to the UI"
    );

    let second = mock.request(1).await;
    let tool_replies: Vec<&serde_json::Value> =
        second["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_replies.len(), 1, "a refused call is still answered");
    assert_eq!(tool_replies[0]["tool_call_id"], "call_1");
    assert!(
        tool_replies[0]["content"].as_str().unwrap().contains("plan mode"),
        "the model is told why: {tool_replies:?}"
    );
}

#[tokio::test]
async fn two_parallel_tool_calls_each_get_their_own_reply() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "call_a", "read_file", "{\"path\":\"a.txt\"}"),
            tool_call_chunk(1, "call_b", "read_file", "{\"path\":\"b.txt\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Both read."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    run_turn(&mut agent, "read both files").await;

    let second = mock.request(1).await;
    let ids: Vec<String> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(ids, vec!["call_a", "call_b"], "each call is answered, in order");
}

#[tokio::test]
async fn plan_mode_never_advertises_a_tool_it_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![vec![text_chunk("Here is a plan."), finish_chunk("stop")]]).await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Plan);
    run_turn(&mut agent, "how would you fix this?").await;

    let request = mock.request(0).await;
    let advertised: Vec<String> = request["tools"]
        .as_array()
        .expect("tools are advertised")
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();

    assert!(advertised.contains(&"read_file".to_string()), "reads stay available");
    assert!(!advertised.contains(&"bash".to_string()), "execution is hidden, not merely refused");
    assert!(!advertised.contains(&"write_file".to_string()));
}

#[tokio::test]
async fn an_edit_reaches_the_disk_and_the_change_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("code.rs");
    std::fs::write(&file, "let timeout = 30;\n").unwrap();

    let mock = MockApi::start(vec![
        // The tool refuses an edit to a file it has not read, so read first.
        vec![
            tool_call_chunk(0, "r1", "read_file", "{\"path\":\"code.rs\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![
            tool_call_chunk(
                0,
                "e1",
                "edit_file",
                "{\"path\":\"code.rs\",\"old_string\":\"30\",\"new_string\":\"60\"}",
            ),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Raised the timeout to 60."), finish_chunk("stop")],
    ])
    .await;

    // acceptEdits so the edit runs without a permission channel.
    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::AcceptEdits);
    let events = run_turn(&mut agent, "raise the timeout").await;

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "let timeout = 60;\n");

    let finished = events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished { display, is_error: false, .. } if display.diff_stats.is_some() => {
            Some(display.clone())
        }
        _ => None,
    });
    assert_eq!(finished.expect("edit reported a diff").diff_stats, Some((1, 1)));
}

#[tokio::test]
async fn an_unread_file_cannot_be_edited() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("code.rs");
    std::fs::write(&file, "original\n").unwrap();

    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(
                0,
                "e1",
                "edit_file",
                "{\"path\":\"code.rs\",\"old_string\":\"original\",\"new_string\":\"clobbered\"}",
            ),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("I need to read it first."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::AcceptEdits);
    run_turn(&mut agent, "change it").await;

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "original\n",
        "an unread file must not be overwritten"
    );

    let reply = mock.request(1).await;
    let tool_text = reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(tool_text.contains("has not been read"), "the model is told how to proceed: {tool_text}");
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "r1", "read_file", "{\"path\":\"../../../etc/passwd\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("That is outside the project."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    run_turn(&mut agent, "read /etc/passwd").await;

    let reply = mock.request(1).await;
    let tool_text = reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(tool_text.contains("outside the workspace"), "got: {tool_text}");
}

#[tokio::test]
async fn a_malformed_tool_call_is_answered_rather_than_ending_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "bad", "read_file", "{not valid json"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Sorry, retrying."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let events = run_turn(&mut agent, "read something").await;

    assert_eq!(stop_reason(&events), StopReason::Complete, "the turn recovers");
    let reply = mock.request(1).await;
    let tool_text = reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(tool_text.contains("could not parse arguments"), "got: {tool_text}");
}

#[tokio::test]
async fn the_system_prompt_is_sent_first_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![text_chunk("one"), finish_chunk("stop")],
        vec![text_chunk("two"), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    agent.refresh_system_prompt(&[]);
    run_turn(&mut agent, "first").await;
    run_turn(&mut agent, "second").await;

    for index in 0..2 {
        let messages = mock.request(index).await;
        let messages = messages["messages"].as_array().unwrap().clone();
        assert_eq!(messages[0]["role"], "system", "request {index} must open with the system prompt");
        assert_eq!(
            messages.iter().filter(|m| m["role"] == "system").count(),
            1,
            "system prompts must not stack across turns"
        );
    }
}

#[tokio::test]
async fn a_conversation_accumulates_across_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![text_chunk("Hello."), finish_chunk("stop")],
        vec![text_chunk("Still here."), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    run_turn(&mut agent, "hi").await;
    run_turn(&mut agent, "are you there?").await;

    let second = mock.request(1).await;
    let contents: Vec<String> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .collect();

    assert!(contents.iter().any(|c| c == "hi"), "the first prompt is still in context");
    assert!(contents.iter().any(|c| c == "Hello."), "so is the first reply");
    assert!(contents.iter().any(|c| c == "are you there?"));
}

#[tokio::test]
async fn an_api_error_ends_the_turn_with_a_reported_reason() {
    // No scripts queued: the listener closes without responding.
    let mock = MockApi::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let events = run_turn(&mut agent, "anything").await;

    assert!(
        matches!(stop_reason(&events), StopReason::Error(_)),
        "a dead endpoint must surface as an error, not a silent stop"
    );
}

#[tokio::test]
async fn interrupting_a_turn_stops_it_and_reports_interruption() {
    let dir = tempfile::tempdir().unwrap();
    // A long script, so the stream is reliably still in flight when the cancel
    // fires. The mock writes 7 bytes per millisecond.
    let mut script: Vec<String> = (0..200).map(|i| text_chunk(&format!("chunk {i} "))).collect();
    script.push(finish_chunk("stop"));
    let mock = MockApi::start(vec![script]).await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let cancel = agent.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        cancel.cancel();
    });

    let events = run_turn(&mut agent, "long task").await;
    assert_eq!(stop_reason(&events), StopReason::Interrupted);
}

#[tokio::test]
async fn a_session_records_the_whole_exchange_for_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "content\n").unwrap();

    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "r1", "read_file", "{\"path\":\"a.txt\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Read it."), finish_chunk("stop")],
    ])
    .await;

    let config = config_for(dir.path(), &mock.base_url, PermissionMode::Default);
    let store = grok_cli::session::SessionStore::new(store_dir.path().join("sessions"));
    let session = store.create(dir.path(), &config.model);
    let path = session.path.clone().expect("persisted");

    let mut agent =
        Agent::new(config, session, ToolRegistry::with_builtins(), CancellationToken::new()).unwrap();
    run_turn(&mut agent, "read a.txt").await;

    let reloaded = Session::load(&path).unwrap();
    let roles: Vec<&str> = reloaded.messages.iter().map(|m| m.role.as_str()).collect();

    assert!(roles.contains(&"user"));
    assert!(roles.contains(&"assistant"));
    assert!(roles.contains(&"tool"), "tool results are persisted so a resume is coherent");

    // The reloaded conversation must still satisfy the pairing invariant.
    let calls: usize = reloaded
        .messages
        .iter()
        .map(|m| m.tool_calls.as_ref().map_or(0, Vec::len))
        .sum();
    let replies = reloaded.messages.iter().filter(|m| m.is_role(grok_cli::api::Role::Tool)).count();
    assert_eq!(calls, replies, "a resumed session must not carry an unanswered tool call");
}

#[tokio::test]
async fn the_tool_iteration_limit_stops_a_loop() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();

    // A model that calls the same tool forever.
    let looping: Vec<Vec<String>> = (0..25)
        .map(|i| {
            vec![
                tool_call_chunk(0, &format!("c{i}"), "read_file", "{\"path\":\"a.txt\"}"),
                finish_chunk("tool_calls"),
            ]
        })
        .collect();
    let mock = MockApi::start(looping).await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    let events = run_turn(&mut agent, "loop forever").await;

    assert_eq!(
        stop_reason(&events),
        StopReason::ToolLimitReached { limit: 20 },
        "a runaway loop must terminate and say why"
    );
}

#[tokio::test]
async fn messages_sent_to_the_api_never_contain_a_null_field() {
    // xAI rejects explicit nulls for optional fields, and the failure mode is a
    // 400 with an unhelpful message.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();

    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "r1", "read_file", "{\"path\":\"a.txt\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("done"), finish_chunk("stop")],
    ])
    .await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    agent.refresh_system_prompt(&[]);
    run_turn(&mut agent, "read it").await;

    for index in 0..mock.request_count().await {
        let request = mock.request(index).await;
        for message in request["messages"].as_array().unwrap() {
            for (key, value) in message.as_object().unwrap() {
                assert!(!value.is_null(), "request {index} sent a null `{key}`: {message}");
            }
        }
    }
}

#[tokio::test]
async fn a_hook_can_block_a_tool_call_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![
        vec![
            tool_call_chunk(0, "b1", "bash", "{\"command\":\"echo hi\"}"),
            finish_chunk("tool_calls"),
        ],
        vec![text_chunk("Blocked, understood."), finish_chunk("stop")],
    ])
    .await;

    let mut config = config_for(dir.path(), &mock.base_url, PermissionMode::BypassPermissions);
    config.hooks.insert(
        "PreToolUse".to_string(),
        vec![grok_cli::config::HookConfig {
            matcher: Some("^bash$".into()),
            command: r#"echo '{"decision":"deny","reason":"shell disabled by policy"}'"#.into(),
            timeout_secs: 10,
        }],
    );

    let session = Session::in_memory("test", &config.model);
    let mut agent =
        Agent::new(config, session, ToolRegistry::with_builtins(), CancellationToken::new()).unwrap();
    run_turn(&mut agent, "run something").await;

    let reply = mock.request(1).await;
    let tool_text = reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        tool_text.contains("shell disabled by policy"),
        "a hook must be able to veto even under bypassPermissions: {tool_text}"
    );
}

#[tokio::test]
async fn tools_are_advertised_with_a_valid_json_schema() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::start(vec![vec![text_chunk("ok"), finish_chunk("stop")]]).await;

    let mut agent = build_agent(dir.path(), &mock.base_url, PermissionMode::Default);
    run_turn(&mut agent, "hello").await;

    let request = mock.request(0).await;
    let tools = request["tools"].as_array().expect("tools present");
    assert!(!tools.is_empty());

    for tool in tools {
        assert_eq!(tool["type"], "function");
        let function = &tool["function"];
        assert!(function["name"].as_str().is_some_and(|n| !n.is_empty()));
        assert!(
            function["description"].as_str().is_some_and(|d| d.len() > 10),
            "every tool needs a usable description: {function}"
        );
        assert_eq!(function["parameters"]["type"], "object");
        assert!(function["parameters"]["properties"].is_object());
        assert!(function["parameters"]["required"].is_array());
    }
}
