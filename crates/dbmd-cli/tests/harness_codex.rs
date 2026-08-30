// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the native ChatGPT (Codex) subscription path:
//! `dbmd login codex` (paste mode, against a scripted fake OAuth endpoint)
//! and `dbmd ask --provider codex` (against a scripted fake ChatGPT backend
//! speaking the Responses wire format).
//!
//! Same discipline as `harness_ask.rs`: zero dev-dependencies, a scripted
//! `TcpListener`, and assertions on the REQUESTS the adapter produced plus
//! the resulting credential state — never on model prose.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use common::{dbmd, write_db_md};

/// One captured request.
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
}

/// A scripted HTTP endpoint: `(content_type, body)` responses served in
/// order, one connection each.
struct MockEndpoint {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockEndpoint {
    fn serve(responses: Vec<(&'static str, String)>) -> Self {
        Self::serve_with_status(
            responses
                .into_iter()
                .map(|(ct, body)| (200, ct, body))
                .collect(),
        )
    }

    /// Script responses with explicit statuses. Every request is read in
    /// full BEFORE the response is written: a server that closes while the
    /// client is still sending resets the connection on Linux, which would
    /// surface as a transport error instead of the status under test.
    fn serve_with_status(responses: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("request line");
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("header");
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
                let mut raw = vec![0u8; length];
                reader.read_exact(&mut raw).expect("body");
                captured.lock().expect("lock").push(Captured {
                    path,
                    headers,
                    body: String::from_utf8_lossy(&raw).into_owned(),
                });
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    429 => "Too Many Requests",
                    _ => "Error",
                };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).expect("head");
                stream.write_all(body.as_bytes()).expect("body");
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
            handle.join().expect("every scripted response was consumed");
        }
        let requests = self.requests.lock().expect("lock").clone();
        requests
    }
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let indices = [n >> 18 & 63, n >> 12 & 63, n >> 6 & 63, n & 63];
        for (i, index) in indices.iter().enumerate() {
            if i <= chunk.len() {
                out.push(TABLE[*index as usize] as char);
            }
        }
    }
    out
}

/// A JWT whose payload carries the ChatGPT account claim the adapter needs.
fn fake_jwt(account: &str, plan: &str) -> String {
    let payload = serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_plan_type": plan,
        }
    });
    format!(
        "header.{}.signature",
        base64url(payload.to_string().as_bytes())
    )
}

fn token_response(access: &str, expires_in: u64) -> (&'static str, String) {
    (
        "application/json",
        serde_json::json!({
            "access_token": access,
            "refresh_token": "refresh-abc",
            "expires_in": expires_in,
        })
        .to_string(),
    )
}

/// Responses-format SSE frames.
fn responses_sse(frames: &[&str]) -> (&'static str, String) {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(frame);
        body.push_str("\n\n");
    }
    ("text/event-stream", body)
}

fn seeded_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = tmp.path().join("store");
    std::fs::create_dir_all(&store).expect("store dir");
    write_db_md(&store);
    dbmd()
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
        .assert()
        .success();
    (tmp, store)
}

/// A command with a hermetic state dir (so real credentials are never read
/// or written) and no ambient harness config.
fn hermetic(state: &std::path::Path) -> assert_cmd::Command {
    let mut command = dbmd();
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
    command.env("DBMD_STATE_DIR", state);
    command
}

// ─────────────────────────────────────────────────────────────────────────────
// login
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn login_status_and_logout_on_an_empty_state_dir() {
    let state = tempfile::TempDir::new().expect("state");
    let assert = hermetic(state.path())
        .args(["--json", "login", "--status"])
        .assert()
        .success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(parsed["logged_in"].as_array().expect("array").len(), 0);
    assert!(parsed["credentials"]
        .as_str()
        .expect("path")
        .ends_with("auth.json"));

    let assert = hermetic(state.path())
        .args(["--json", "logout"])
        .assert()
        .success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(parsed["removed"], false);
}

#[test]
fn login_refuses_a_provider_without_a_native_flow() {
    let state = tempfile::TempDir::new().expect("state");
    let assert = hermetic(state.path())
        .args(["--json", "login", "claude-code"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("LOGIN_UNKNOWN_PROVIDER"));
    // The refusal names the supported path rather than dead-ending.
    assert!(stderr.contains("claude-code"));
}

#[test]
fn paste_mode_login_stores_owner_only_credentials() {
    let state = tempfile::TempDir::new().expect("state");
    let access = fake_jwt("acct_test", "plus");
    let oauth = MockEndpoint::serve(vec![token_response(&access, 3600)]);

    let assert = hermetic(state.path())
        .env("DBMD_OAUTH_TOKEN_URL", format!("{}/oauth/token", oauth.url))
        .args(["--json", "login", "codex", "--code"])
        .write_stdin("the-code\n")
        .assert()
        .success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(parsed["logged_in"], "codex");
    assert_eq!(parsed["plan"], "plus");

    let requests = oauth.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/oauth/token");
    let body = &requests[0].body;
    assert!(body.contains("grant_type=authorization_code"));
    assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(body.contains("code=the-code"));
    assert!(body.contains("code_verifier="));

    // Stored, owner-only, and reported by --status.
    let auth_file = state.path().join("auth.json");
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file).expect("auth.json"))
            .expect("json");
    assert_eq!(stored["codex"]["type"], "oauth");
    assert_eq!(stored["codex"]["access"], access);
    assert_eq!(stored["codex"]["refresh"], "refresh-abc");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&auth_file)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let assert = hermetic(state.path())
        .args(["--json", "login", "--status"])
        .assert()
        .success();
    let parsed: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(parsed["logged_in"][0], "codex");

    // And logout forgets it.
    hermetic(state.path())
        .args(["logout", "codex"])
        .assert()
        .success();
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file).expect("auth.json"))
            .expect("json");
    assert!(stored.get("codex").is_none());
}

#[test]
fn a_pasted_state_from_another_attempt_is_refused() {
    let state = tempfile::TempDir::new().expect("state");
    // No OAuth endpoint is scripted: the refusal must happen BEFORE any
    // token exchange, so a replayed redirect cannot complete a login.
    let assert = hermetic(state.path())
        .args(["--json", "login", "codex", "--code"])
        .write_stdin("code=the-code&state=some-other-attempt\n")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("LOGIN_STATE_MISMATCH"));
    assert!(!state.path().join("auth.json").exists());
}

#[test]
fn an_explicit_provider_ignores_the_stores_model_for_another_one() {
    // Found live: a store configured for a local Ollama model sent that name
    // to the ChatGPT backend, which rejected it. An explicitly named provider
    // overrides the store's setup, and only a provider-scoped key survives.
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );
    std::fs::create_dir_all(store.join(".dbmd")).expect(".dbmd");
    std::fs::write(
        store.join(".dbmd/config"),
        "llm_provider = ollama\nllm_model = qwen3.8-27b-32k\nllm_base_url = http://127.0.0.1:11434/v1\n",
    )
    .expect("config");

    let backend = MockEndpoint::serve(vec![responses_sse(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"ok"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ])]);

    hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
        ])
        .assert()
        .success();

    let requests = backend.finish();
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert_ne!(
        body["model"], "qwen3.8-27b-32k",
        "a local model name must not ride to the ChatGPT backend"
    );
    assert_eq!(body["model"], "gpt-5.6-sol");
}

#[test]
fn a_provider_scoped_model_survives_an_explicit_override() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );
    std::fs::create_dir_all(store.join(".dbmd")).expect(".dbmd");
    std::fs::write(
        store.join(".dbmd/config"),
        "llm_provider = ollama\nllm_model = qwen3.8-27b-32k\nllm_model_codex = gpt-5.3-codex\n",
    )
    .expect("config");

    let backend = MockEndpoint::serve(vec![responses_sse(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"ok"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ])]);

    hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
        ])
        .assert()
        .success();

    let requests = backend.finish();
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert_eq!(body["model"], "gpt-5.3-codex");
}

// ─────────────────────────────────────────────────────────────────────────────
// ask --provider codex
// ─────────────────────────────────────────────────────────────────────────────

fn write_credentials(state: &std::path::Path, access: &str, expires_ms: u128) {
    std::fs::create_dir_all(state).expect("state dir");
    let value = serde_json::json!({
        "codex": {
            "type": "oauth",
            "access": access,
            "refresh": "refresh-abc",
            "expires": u64::try_from(expires_ms).unwrap_or(u64::MAX),
        }
    });
    std::fs::write(
        state.join("auth.json"),
        serde_json::to_string_pretty(&value).expect("json"),
    )
    .expect("write auth.json");
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

#[test]
fn ask_without_a_login_says_how_to_sign_in() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    let assert = hermetic(state.path())
        .current_dir(&store)
        .args(["--json", "ask", "hi", "--provider", "codex"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("dbmd login codex"));
}

#[test]
fn ask_runs_a_tool_loop_over_the_responses_protocol() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    let access = fake_jwt("acct_live", "pro");
    write_credentials(state.path(), &access, now_ms() + 3_600_000);

    let backend = MockEndpoint::serve(vec![
        // Turn 1: one function call.
        responses_sse(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","name":"query"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"type\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"todo\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_a","name":"query","arguments":"{\"type\":\"todo\"}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":5}}}"#,
        ]),
        // Turn 2: the answer.
        responses_sse(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"One todo: Buy milk."}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"One todo: Buy milk."}]}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":30,"output_tokens":8}}}"#,
        ]),
    ]);

    let assert = hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "what is on the list?",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
            "--model",
            "gpt-5.1-codex",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    let requests = backend.finish();
    assert_eq!(requests.len(), 2, "one tool round-trip");
    // Endpoint + the honest, documented headers.
    assert_eq!(requests[0].path, "/codex/responses");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {access}").as_str())
    );
    assert_eq!(requests[0].header("chatgpt-account-id"), Some("acct_live"));
    assert_eq!(requests[0].header("originator"), Some("dbmd"));
    assert_eq!(
        requests[0].header("openai-beta"),
        Some("responses=experimental")
    );
    assert!(requests[0]
        .header("user-agent")
        .expect("user-agent")
        .starts_with("dbmd/"));

    // Request 1: system prompt as `instructions`, tools present, store: false.
    let first: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert_eq!(first["model"], "gpt-5.1-codex");
    assert_eq!(first["store"], false);
    assert_eq!(first["stream"], true);
    assert!(first["instructions"]
        .as_str()
        .expect("instructions")
        .contains("db.md"));
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tools.contains(&"query"));
    assert!(!tools.contains(&"write"), "read mask has no write tools");

    // Request 2: the fragmented arguments were reassembled, the call executed,
    // and its output rode back as function_call_output on the same call_id.
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).expect("json");
    let input = second["input"].as_array().expect("input");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call");
    assert_eq!(call["call_id"], "call_a");
    assert_eq!(call["arguments"], "{\"type\":\"todo\"}");
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("function_call_output");
    assert_eq!(output["call_id"], "call_a");
    assert!(output["output"]
        .as_str()
        .expect("output")
        .contains("Buy milk"));

    // The event stream reached the caller.
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let call_event = events
        .iter()
        .find(|e| e["event"] == "tool_call")
        .expect("tool_call");
    assert!(call_event["display"]
        .as_str()
        .expect("display")
        .starts_with("dbmd --json query"));
    let done = events.iter().find(|e| e["event"] == "done").expect("done");
    assert_eq!(done["text"], "One todo: Buy milk.");
    assert!(events
        .iter()
        .any(|e| e["event"] == "usage" && e["input"] == 30));
}

#[test]
fn an_expired_token_is_refreshed_and_persisted_before_the_call() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    // Stored token already expired; the refreshed one carries a new account.
    write_credentials(state.path(), &fake_jwt("acct_old", "plus"), 0);
    let refreshed_access = fake_jwt("acct_new", "pro");

    let oauth = MockEndpoint::serve(vec![token_response(&refreshed_access, 3600)]);
    let backend = MockEndpoint::serve(vec![responses_sse(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"ok"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ])]);

    hermetic(state.path())
        .current_dir(&store)
        .env("DBMD_OAUTH_TOKEN_URL", format!("{}/oauth/token", oauth.url))
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
            "--model",
            "gpt-5.1-codex",
        ])
        .assert()
        .success();

    let oauth_requests = oauth.finish();
    assert_eq!(oauth_requests.len(), 1, "exactly one refresh");
    assert!(oauth_requests[0].body.contains("grant_type=refresh_token"));
    assert!(oauth_requests[0].body.contains("refresh_token=refresh-abc"));

    // The call used the REFRESHED token and account…
    let backend_requests = backend.finish();
    assert_eq!(
        backend_requests[0].header("authorization"),
        Some(format!("Bearer {refreshed_access}").as_str())
    );
    assert_eq!(
        backend_requests[0].header("chatgpt-account-id"),
        Some("acct_new")
    );
    // …and the new pair was persisted for the next process.
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(state.path().join("auth.json")).expect("auth.json"),
    )
    .expect("json");
    assert_eq!(stored["codex"]["access"], refreshed_access);
}

#[test]
fn a_backend_refusal_is_reported_with_its_remedy() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "plus"),
        now_ms() + 3_600_000,
    );

    let backend = MockEndpoint::serve_with_status(vec![(
        401,
        "application/json",
        r#"{"error":{"message":"token expired"}}"#.to_string(),
    )]);

    let assert = hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
        ])
        .assert()
        .failure();
    backend.finish();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("HTTP 401"), "stderr was: {stderr}");
    assert!(stderr.contains("dbmd login codex"), "stderr was: {stderr}");
}

#[test]
fn an_unsupported_model_says_how_to_pick_another() {
    // The set of models a ChatGPT plan exposes moves; a rejection must be
    // fixable without a release. (Seen live on a Pro account.)
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );

    let backend = MockEndpoint::serve_with_status(vec![(
        400,
        "application/json",
        r#"{"detail":"The 'x' model is not supported when using Codex with a ChatGPT account."}"#
            .to_string(),
    )]);

    let assert = hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
            "--model",
            "x",
        ])
        .assert()
        .failure();
    backend.finish();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("--model"), "stderr was: {stderr}");
}

// ─────────────────────────────────────────────────────────────────────────────
// reasoning effort on the Responses backend
// ─────────────────────────────────────────────────────────────────────────────

/// A scripted answer with no tool calls.
fn plain_answer() -> (&'static str, String) {
    responses_sse(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"ok"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ])
}

#[test]
fn effort_rides_as_the_responses_reasoning_object() {
    // `summary: "auto"` is not decoration: without it the backend streams no
    // reasoning summaries at all, so the thinking deltas this adapter parses
    // would never arrive.
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );
    let backend = MockEndpoint::serve(vec![plain_answer()]);

    hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
            "--effort",
            "max",
        ])
        .assert()
        .success();

    let requests = backend.finish();
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    // The backend's own set, learned from a live 400: none, low, medium,
    // high, xhigh, max. `max` is a real rung above `xhigh` here.
    assert_eq!(body["reasoning"]["effort"], "max");
    assert_eq!(body["reasoning"]["summary"], "auto");
}

#[test]
fn no_effort_leaves_the_chatgpt_backend_on_its_own_default() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );
    let backend = MockEndpoint::serve(vec![plain_answer()]);

    hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
        ])
        .assert()
        .success();

    let requests = backend.finish();
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("json");
    assert!(
        body.get("reasoning").is_none(),
        "an unset effort must send no reasoning object: {body}"
    );
}

#[test]
fn a_refused_effort_level_explains_itself() {
    let (_tmp, store) = seeded_store();
    let state = tempfile::TempDir::new().expect("state");
    write_credentials(
        state.path(),
        &fake_jwt("acct_live", "pro"),
        now_ms() + 3_600_000,
    );
    let backend = MockEndpoint::serve_with_status(vec![(
        400,
        "application/json",
        r#"{"detail":"Unsupported reasoning effort for this model."}"#.to_string(),
    )]);

    let assert = hermetic(state.path())
        .current_dir(&store)
        .args([
            "--json",
            "ask",
            "hi",
            "--provider",
            "codex",
            "--base-url",
            &backend.url,
            "--effort",
            "max",
        ])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("--effort"),
        "the 400 must name the knob to change: {stderr}"
    );
}
