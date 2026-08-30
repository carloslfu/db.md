// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the embedded harness (`dbmd ask` / `do` / `build`):
//! the real binary driven against a scripted fake LLM endpoint — the same
//! zero-dev-dependency `TcpListener` pattern as `link_verbs.rs`'s MockHub.
//! LLM text is nondeterministic in production but fully scripted here, so
//! assertions follow the repo's snapshot discipline: assert the REQUESTS the
//! adapter produced (tools present, results round-tripped) and the FINAL
//! STORE STATE, never prose.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use common::{dbmd, write_db_md};

/// One captured request to the fake endpoint.
#[derive(Debug, Clone)]
struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn body_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

/// A scripted fake LLM endpoint: SSE responses served strictly in order, one
/// connection each; `finish()` joins — which asserts every scripted response
/// was consumed — and returns the captured requests.
struct MockLlm {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockLlm {
    fn serve(responses: Vec<String>) -> Self {
        Self::serve_with_status(responses.into_iter().map(|body| (200, body)).collect())
    }

    /// Script the status codes too, so a provider that refuses a field can be
    /// reproduced exactly. Every response still drains its request first —
    /// writing before the body is read gives ECONNRESET on Linux.
    fn serve_with_status(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock llm");
        let url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for (status, sse_body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("request line");
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("header line");
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        let value = value.trim().to_string();
                        if name == "content-length" {
                            length = value.parse().unwrap_or(0);
                        }
                        headers.push((name, value));
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).expect("request body");
                captured.lock().expect("capture lock").push(Captured {
                    path,
                    headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
                let content_type = if status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    sse_body.len()
                );
                stream.write_all(head.as_bytes()).expect("response head");
                stream
                    .write_all(sse_body.as_bytes())
                    .expect("response body");
            }
        });
        Self {
            url,
            requests,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<Captured> {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock served every scripted response");
        }
        let requests = self.requests.lock().expect("capture lock").clone();
        requests
    }
}

/// SSE frames from `data:` payload lines.
fn sse(frames: &[&str]) -> String {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(frame);
        body.push_str("\n\n");
    }
    body
}

/// A scratch store with one seeded todo record.
fn seeded_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).expect("store dir");
    write_db_md(&store);
    let seed = dbmd()
        .current_dir(&store)
        .args([
            "write",
            "records/todos/buy-milk.md",
            "--type",
            "todo",
            "--summary",
            "Buy milk",
            "--fm",
            "status=active",
        ])
        .assert();
    seed.success();
    (tmp, store)
}

/// A `dbmd` command with a hermetic harness environment pointed at the mock.
fn harness_cmd(store: &std::path::Path, mock_url: &str, protocol: &str) -> assert_cmd::Command {
    let mut command = dbmd();
    command.current_dir(store);
    for var in [
        "DBMD_LLM_PROVIDER",
        "DBMD_LLM_BASE_URL",
        "DBMD_LLM_PROTOCOL",
        "DBMD_LLM_MODEL",
        "DBMD_LLM_KEY",
        "DBMD_LLM_KEY_ORIGIN",
        "DBMD_LLM_ALLOW_INSECURE_HTTP",
        "DBMD_LLM_EFFORT",
        "DBMD_WORKSPACE",
    ] {
        command.env_remove(var);
    }
    let base = if protocol == "openai" {
        format!("{mock_url}/v1")
    } else {
        mock_url.to_string()
    };
    command
        .env("DBMD_LLM_BASE_URL", base)
        .env("DBMD_LLM_PROTOCOL", protocol)
        .env("DBMD_LLM_MODEL", "fake-model");
    command
}

/// NDJSON events parsed from a `--json` run's stdout.
fn events(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// openai-compat: the full loop
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_runs_one_tool_loop_and_answers() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"query","arguments":"{\"type\":\"todo\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"One active todo: Buy milk."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
            "[DONE]",
        ]),
    ]);

    let assert = harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "ask", "what is on the list?"])
        .assert()
        .success();
    let stdout = assert.get_output().stdout.clone();

    let requests = mock.finish();
    assert_eq!(requests.len(), 2, "one tool round-trip");
    assert_eq!(requests[0].path, "/v1/chat/completions");

    // First request: system prompt + tools + no auth header (no key set).
    let first = requests[0].body_json();
    assert_eq!(first["model"], "fake-model");
    assert_eq!(first["messages"][0]["role"], "system");
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(tools.contains(&"query"));
    assert!(!tools.contains(&"write"), "read mask has no write tools");
    assert!(requests[0].header("authorization").is_none());

    // Second request: the tool result rode back with the call id.
    let second = requests[1].body_json();
    let wire = second["messages"].as_array().expect("messages");
    let tool_msg = wire
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result message");
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    assert!(
        tool_msg["content"]
            .as_str()
            .expect("content")
            .contains("Buy milk"),
        "the query result carried the seeded record"
    );

    // Event stream: tool_call with its CLI one-liner, result, usage, done.
    let events = events(&stdout);
    let call = events
        .iter()
        .find(|e| e["event"] == "tool_call")
        .expect("tool_call event");
    assert!(call["display"]
        .as_str()
        .expect("display")
        .starts_with("dbmd --json query"));
    assert!(events.iter().any(|e| e["event"] == "tool_result"));
    assert!(events
        .iter()
        .any(|e| e["event"] == "usage" && e["input"] == 11));
    let done = events.iter().find(|e| e["event"] == "done").expect("done");
    assert_eq!(done["text"], "One active todo: Buy milk.");
}

#[test]
fn ollama_quirks_are_tolerated() {
    // Whole-args tool calls, index stuck at 0 for BOTH calls, no ids,
    // finish_reason "stop" despite the calls, plus an SSE comment line.
    let (_tmp, store) = seeded_store();
    let mut first = String::from(": KEEPALIVE\n\n");
    first.push_str(&sse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"query","arguments":"{\"type\":\"todo\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"log_tail","arguments":"{\"n\":5}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]));
    let mock = MockLlm::serve(vec![
        first,
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]),
    ]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "ask", "check"])
        .assert()
        .success();

    let requests = mock.finish();
    let second = requests[1].body_json();
    let wire = second["messages"].as_array().expect("messages");
    // Both calls executed and both results returned, with synthesized ids.
    let tool_results: Vec<&serde_json::Value> =
        wire.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tool_results.len(), 2, "both index-0 calls survived");
    let assistant = wire
        .iter()
        .find(|m| m["role"] == "assistant" && !m["tool_calls"].is_null())
        .expect("assistant turn with tool calls");
    let ids: Vec<&str> = assistant["tool_calls"]
        .as_array()
        .expect("calls")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| !id.is_empty()), "ids were synthesized");
}

#[test]
fn unknown_and_masked_tools_come_back_as_error_results() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![
        sse(&[
            // `write` exists — but not on the read mask.
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write","arguments":"{\"path\":\"records/todos/x.md\",\"type\":\"todo\",\"summary\":\"X\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"cannot write here"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]),
    ]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "ask", "add a todo"])
        .assert()
        .success();

    let requests = mock.finish();
    let second = requests[1].body_json();
    let tool_msg = second["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m["role"] == "tool")
        .cloned()
        .expect("error tool result");
    let content = tool_msg["content"].as_str().expect("content");
    assert!(content.starts_with("ERROR:"));
    assert!(content.contains("unknown tool"));
    // Nothing was written.
    assert!(!store.join("records/todos/x.md").exists());
}

// ─────────────────────────────────────────────────────────────────────────────
// dbmd do: writes land through the store contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn do_writes_through_the_store_contract() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write","arguments":"{\"path\":\"records/todos/walk-dog.md\",\"type\":\"todo\",\"summary\":\"Walk the dog\",\"fm\":[\"status=active\"],\"body\":\"- [ ] leash\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"log","arguments":"{\"kind\":\"create\",\"object\":\"records/todos/walk-dog.md\",\"message\":\"added via do\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"Added."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]),
    ]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "do", "add walk the dog"])
        .assert()
        .success();
    mock.finish();

    // The record landed with frontmatter, body, and a minted id.
    let written =
        std::fs::read_to_string(store.join("records/todos/walk-dog.md")).expect("record written");
    assert!(written.contains("summary: Walk the dog"));
    assert!(written.contains("status: active"));
    assert!(written.contains("- [ ] leash"));
    // The store stays valid, and the log carries the model's entry.
    dbmd()
        .current_dir(&store)
        .args(["validate", "--all"])
        .assert()
        .success();
    let log = std::fs::read_to_string(store.join("log.md")).expect("log.md");
    assert!(log.contains("added via do"));
}

// ─────────────────────────────────────────────────────────────────────────────
// anthropic protocol
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn anthropic_protocol_round_trips_tool_use() {
    let (_tmp, store) = seeded_store();
    let first = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"query\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"type\\\":\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"todo\\\"}\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        .to_string();
    let second = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"One todo.\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        .to_string();
    let mock = MockLlm::serve(vec![first, second]);

    let assert = harness_cmd(&store, &mock.url, "anthropic")
        .env("DBMD_LLM_KEY", "sk-test-123")
        .args(["--json", "ask", "how many todos?"])
        .assert()
        .success();
    let stdout = assert.get_output().stdout.clone();

    let requests = mock.finish();
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-test-123"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    let first = requests[0].body_json();
    assert!(first["system"].as_str().expect("system").contains("db.md"));
    assert!(first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .any(|t| t["name"] == "query" && t["input_schema"]["type"] == "object"));

    // The fragmented input_json_delta was reassembled and executed; the
    // result rode back as ONE user message of tool_result blocks.
    let second = requests[1].body_json();
    let last = second["messages"]
        .as_array()
        .expect("messages")
        .last()
        .cloned()
        .expect("result message");
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
    assert!(last["content"][0]["content"]
        .as_str()
        .expect("content")
        .contains("Buy milk"));

    let done = events(&stdout)
        .into_iter()
        .find(|e| e["event"] == "done")
        .expect("done");
    assert_eq!(done["text"], "One todo.");
}

// ─────────────────────────────────────────────────────────────────────────────
// config security rules
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn store_config_endpoint_refuses_unbound_ambient_key() {
    let (_tmp, store) = seeded_store();
    std::fs::create_dir_all(store.join(".dbmd")).expect(".dbmd");
    std::fs::write(
        store.join(".dbmd/config"),
        "llm_base_url = https://evil.example.com/v1\nllm_protocol = openai\nllm_model = x\n",
    )
    .expect("config");

    let mut command = dbmd();
    command.current_dir(&store);
    for var in [
        "DBMD_LLM_PROVIDER",
        "DBMD_LLM_BASE_URL",
        "DBMD_LLM_PROTOCOL",
        "DBMD_LLM_MODEL",
        "DBMD_LLM_KEY_ORIGIN",
    ] {
        command.env_remove(var);
    }
    let assert = command
        .env("DBMD_LLM_KEY", "sk-ambient")
        .args(["--json", "ask", "hi"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("ASK_CONFIG"));
    assert!(stderr.contains("DBMD_LLM_KEY_ORIGIN"));
}

#[test]
fn plain_http_to_non_loopback_is_refused() {
    let (_tmp, store) = seeded_store();
    let mut command = dbmd();
    command.current_dir(&store);
    command.env_remove("DBMD_LLM_ALLOW_INSECURE_HTTP");
    let assert = command
        .env("DBMD_LLM_BASE_URL", "http://192.0.2.10:11434/v1")
        .env("DBMD_LLM_PROTOCOL", "openai")
        .env("DBMD_LLM_MODEL", "x")
        .args(["--json", "ask", "hi"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("refusing plain http"));
}

// ─────────────────────────────────────────────────────────────────────────────
// dbmd build: the workspace mask
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn build_edits_the_workspace_and_refuses_escapes() {
    let (_tmp, store) = seeded_store();
    let workspace = store.parent().expect("parent").to_path_buf();
    std::fs::write(workspace.join("app.ts"), "const version = 1;\n").expect("seed app file");

    let mock = MockLlm::serve(vec![
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"edit_file","arguments":"{\"path\":\"app.ts\",\"old_text\":\"version = 1\",\"new_text\":\"version = 2\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"read_file","arguments":"{\"path\":\"../../etc/hosts\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"Bumped."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]),
    ]);

    harness_cmd(&store, &mock.url, "openai")
        .args([
            "--json",
            "build",
            "bump the version",
            "--workspace",
            workspace.to_str().expect("utf8 path"),
        ])
        .assert()
        .success();

    let requests = mock.finish();
    // The edit landed…
    let edited = std::fs::read_to_string(workspace.join("app.ts")).expect("app file");
    assert!(edited.contains("version = 2"));
    // …and the escape came back as an error result, not a file read.
    let third = requests[2].body_json();
    let escape_result = third["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rfind(|m| m["role"] == "tool")
        .cloned()
        .expect("escape result");
    let content = escape_result["content"].as_str().expect("content");
    assert!(content.starts_with("ERROR:"));
    assert!(content.contains("leaves the workspace"));
}

#[test]
fn build_without_a_declared_workspace_is_refused() {
    let (_tmp, store) = seeded_store();
    // Hermetic provider env (never consulted: the workspace refusal comes
    // first, so no request is ever made at this base URL).
    let assert = harness_cmd(&store, "http://127.0.0.1:9", "openai")
        .args(["--json", "build", "do something"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("BUILD_NO_WORKSPACE"));
}

// ─────────────────────────────────────────────────────────────────────────────
// turn cap
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// the /v1/ask and /v1/do API routes
// ─────────────────────────────────────────────────────────────────────────────

/// A running `dbmd api` with harness env pointed at a mock; killed on drop.
struct ApiServer {
    child: std::process::Child,
    addr: String,
}

impl ApiServer {
    fn spawn(store: &std::path::Path, mock_url: &str, flags: &[&str]) -> Self {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("dbmd"));
        command.args(["--json", "api", "--addr", "127.0.0.1:0", "--dir"]);
        command.arg(store);
        command.args(flags);
        for var in [
            "DBMD_LLM_PROVIDER",
            "DBMD_LLM_BASE_URL",
            "DBMD_LLM_PROTOCOL",
            "DBMD_LLM_MODEL",
            "DBMD_LLM_KEY",
            "DBMD_LLM_KEY_ORIGIN",
        ] {
            command.env_remove(var);
        }
        let mut child = command
            .env("DBMD_LLM_BASE_URL", format!("{mock_url}/v1"))
            .env("DBMD_LLM_PROTOCOL", "openai")
            .env("DBMD_LLM_MODEL", "fake-model")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn dbmd api");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut first = String::new();
        BufReader::new(stdout)
            .read_line(&mut first)
            .expect("serving line");
        let serving: serde_json::Value = serde_json::from_str(&first).expect("serving JSON");
        let addr = serving["serving"]
            .as_str()
            .expect("url")
            .strip_prefix("http://")
            .expect("http url")
            .to_string();
        Self { child, addr }
    }

    /// POST one JSON body and return (status, raw response after headers).
    fn post(&self, target: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(&self.addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .expect("timeout");
        let head = format!(
            "POST {target} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            self.addr,
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(body.as_bytes()).expect("body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status: u16 = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn api_ask_route_streams_the_event_feed() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![sse(&[
        r#"{"choices":[{"index":0,"delta":{"content":"Two todos."}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ])]);
    let api = ApiServer::spawn(&store, &mock.url, &["--ask"]);

    let (status, body) = api.post("/v1/ask", r#"{"prompt":"what's on the list?"}"#);
    assert_eq!(status, 200);
    // SSE frames: at minimum text_delta and done, one JSON object each.
    let frames: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect();
    assert!(frames.iter().any(|f| f["event"] == "text_delta"));
    let done = frames
        .iter()
        .find(|f| f["event"] == "done")
        .expect("done frame");
    assert_eq!(done["text"], "Two todos.");
    mock.finish();
}

#[test]
fn api_harness_routes_are_off_by_default_and_do_needs_its_own_flag() {
    let (_tmp, store) = seeded_store();
    // No flags: both routes refuse without touching any endpoint.
    let api = ApiServer::spawn(&store, "http://127.0.0.1:9", &[]);
    let (status, body) = api.post("/v1/ask", r#"{"prompt":"hi"}"#);
    assert_eq!(status, 403);
    assert!(body.contains("ASK_DISABLED"));
    drop(api);

    // --ask alone: /v1/do still refuses (writes need their own opt-in).
    let api = ApiServer::spawn(&store, "http://127.0.0.1:9", &["--ask"]);
    let (status, body) = api.post("/v1/do", r#"{"prompt":"hi"}"#);
    assert_eq!(status, 403);
    assert!(body.contains("ASK_DISABLED"));
}

#[test]
fn the_turn_cap_forces_a_final_no_tools_answer() {
    let (_tmp, store) = seeded_store();
    let tool_turn = sse(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"log_tail","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let final_turn = sse(&[
        r#"{"choices":[{"index":0,"delta":{"content":"best effort"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    // max-turns 2 ⇒ two tool turns, then the capped final call (no tools).
    let mock = MockLlm::serve(vec![tool_turn.clone(), tool_turn, final_turn]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "ask", "loop forever", "--max-turns", "2"])
        .assert()
        .success();

    let requests = mock.finish();
    assert_eq!(requests.len(), 3);
    let last = requests[2].body_json();
    assert!(
        last.get("tools").is_none(),
        "the capped final call carries no tools"
    );
    let flattened = last["messages"].to_string();
    assert!(flattened.contains("tool-call limit"));
}

// ─────────────────────────────────────────────────────────────────────────────
// reasoning effort: the wire field, the per-protocol shape, and the fallback
// ─────────────────────────────────────────────────────────────────────────────

/// One scripted OpenAI-compat answer with no tool calls.
fn openai_answer(text: &str) -> String {
    sse(&[
        &format!(
            r#"{{"choices":[{{"delta":{{"content":{}}},"index":0}}]}}"#,
            serde_json::Value::String(text.to_string())
        ),
        r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#,
        "[DONE]",
    ])
}

/// One scripted Anthropic answer with no tool calls.
fn anthropic_answer(text: &str) -> String {
    sse(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        &format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":{}}}}}"#,
            serde_json::Value::String(text.to_string())
        ),
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

#[test]
fn openai_effort_rides_as_reasoning_effort() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![openai_answer("ok")]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?", "--effort", "high"])
        .assert()
        .success();

    let requests = mock.finish();
    assert_eq!(requests.len(), 1);
    let body = requests[0].body_json();
    assert_eq!(body["reasoning_effort"], "high");
}

#[test]
fn no_effort_flag_sends_no_reasoning_field() {
    // The load-bearing default: an unset effort must leave the provider on
    // its own, because "unset" and "low" are very different on a server that
    // already has thinking switched on.
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![openai_answer("ok")]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?"])
        .assert()
        .success();

    let requests = mock.finish();
    let body = requests[0].body_json();
    assert!(
        body.get("reasoning_effort").is_none(),
        "unset effort must send no field: {body}"
    );
}

#[test]
fn effort_is_dropped_when_the_endpoint_refuses_it() {
    // An arbitrary OpenAI-compatible server may not know the field at all.
    // Asking it to think harder must not brick the run — the request retries
    // without the field, and the user is told it happened.
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve_with_status(vec![
        (
            400,
            r#"{"error":{"message":"unrecognized request argument: reasoning_effort"}}"#
                .to_string(),
        ),
        (200, openai_answer("ok")),
    ]);

    let output = harness_cmd(&store, &mock.url, "openai")
        .args(["--json", "ask", "how many todos?", "--effort", "max"])
        .assert()
        .success()
        .get_output()
        .clone();

    let requests = mock.finish();
    assert_eq!(requests.len(), 2, "the refused request must be retried");
    assert_eq!(requests[0].body_json()["reasoning_effort"], "high");
    assert!(
        requests[1].body_json().get("reasoning_effort").is_none(),
        "the retry must drop the refused field"
    );

    let notices: Vec<serde_json::Value> = events(&output.stdout)
        .into_iter()
        .filter(|e| e["event"] == "notice")
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "a silently downgraded run must not look like a clean one"
    );
    let message = notices[0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("reasoning_effort=high"),
        "the notice must name what was refused: {message}"
    );
}

#[test]
fn anthropic_effort_uses_output_config_and_adaptive_thinking() {
    // The current Anthropic shape. `budget_tokens` is a 400 on these models,
    // so it must not appear in the first attempt.
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![anthropic_answer("ok")]);

    harness_cmd(&store, &mock.url, "anthropic")
        .args(["ask", "how many todos?", "--effort", "xhigh"])
        .assert()
        .success();

    let requests = mock.finish();
    let body = requests[0].body_json();
    assert_eq!(body["output_config"]["effort"], "xhigh");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(body["thinking"].get("budget_tokens").is_none());
}

#[test]
fn anthropic_effort_off_disables_thinking() {
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![anthropic_answer("ok")]);

    harness_cmd(&store, &mock.url, "anthropic")
        .args(["ask", "how many todos?", "--effort", "off"])
        .assert()
        .success();

    let requests = mock.finish();
    let body = requests[0].body_json();
    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("output_config").is_none());
}

#[test]
fn anthropic_falls_back_to_budget_tokens_on_older_models() {
    // Pre-4.6 models reject `output_config` and take a thinking budget
    // instead. Rather than pin a model list that goes stale, the request
    // degrades on the wire.
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve_with_status(vec![
        (
            400,
            r#"{"type":"error","error":{"message":"output_config: unexpected field"}}"#.to_string(),
        ),
        (200, anthropic_answer("ok")),
    ]);

    harness_cmd(&store, &mock.url, "anthropic")
        .args(["ask", "how many todos?", "--effort", "medium"])
        .assert()
        .success();

    let requests = mock.finish();
    assert_eq!(requests.len(), 2);
    let legacy = requests[1].body_json();
    assert_eq!(legacy["thinking"]["type"], "enabled");
    assert_eq!(legacy["thinking"]["budget_tokens"], 8192);
    // The budget must not eat the answer's room.
    assert_eq!(legacy["max_tokens"], 4096 + 8192);
}

#[test]
fn unknown_effort_is_refused_before_any_request() {
    // Fail fast and name the rungs — never after a network round-trip.
    let (_tmp, store) = seeded_store();
    let mock = MockLlm::serve(vec![]);

    let output = harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?", "--effort", "turbo"])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("turbo"), "{stderr}");
    assert!(
        stderr.contains("xhigh"),
        "the valid rungs go in the error: {stderr}"
    );
    assert!(mock.finish().is_empty(), "nothing may reach the provider");
}

#[test]
fn store_config_can_pin_the_effort() {
    // Same precedence chain as every other knob: `.dbmd/config` supplies it
    // when no flag and no environment variable do.
    let (_tmp, store) = seeded_store();
    std::fs::create_dir_all(store.join(".dbmd")).expect("config dir");
    std::fs::write(store.join(".dbmd").join("config"), "llm_effort = low\n").expect("config");
    let mock = MockLlm::serve(vec![openai_answer("ok")]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?"])
        .assert()
        .success();

    let requests = mock.finish();
    assert_eq!(requests[0].body_json()["reasoning_effort"], "low");
}

#[test]
fn the_flag_beats_the_store_config() {
    let (_tmp, store) = seeded_store();
    std::fs::create_dir_all(store.join(".dbmd")).expect("config dir");
    std::fs::write(store.join(".dbmd").join("config"), "llm_effort = low\n").expect("config");
    let mock = MockLlm::serve(vec![openai_answer("ok")]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?", "--effort", "medium"])
        .assert()
        .success();

    let requests = mock.finish();
    assert_eq!(requests[0].body_json()["reasoning_effort"], "medium");
}

#[test]
fn the_ollama_dialect_and_provider_scoped_effort_reach_the_wire() {
    // Two things at once, both previously wrong in this codebase:
    // 1. Ollama's vocabulary differs — `max`, not `high`, is its top rung,
    //    and that is what a Qwen3.8 chat template reads as `xhigh`.
    // 2. A provider-scoped `llm_effort_<name>` survives an explicit
    //    `--provider`, while an unscoped one would not.
    let (_tmp, store) = seeded_store();
    std::fs::create_dir_all(store.join(".dbmd")).expect("config dir");
    std::fs::write(
        store.join(".dbmd").join("config"),
        "llm_effort = low\nllm_effort_ollama = max\n",
    )
    .expect("config");
    let mock = MockLlm::serve(vec![openai_answer("ok")]);

    harness_cmd(&store, &mock.url, "openai")
        .args(["ask", "how many todos?", "--provider", "ollama"])
        .assert()
        .success();

    let requests = mock.finish();
    let body = requests[0].body_json();
    assert_eq!(
        body["reasoning_effort"], "max",
        "the scoped effort must win, in Ollama's own vocabulary: {body}"
    );
}
