// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the link.md client verbs — `resolve`, `sync`,
//! `grant`, `propose`, `subscribe` — driven end-to-end through the compiled
//! `dbmd` binary against a scripted localhost mock hub.
//!
//! The mock is a bare `std::net::TcpListener` speaking just enough HTTP/1.1
//! for one request per connection (`connection: close`), so the tests take
//! ZERO new dev-dependencies and stay hermetic. Plain-HTTP-to-loopback is the
//! client's documented dev exemption, which is exactly what lets a mock exist
//! at all — the HTTPS-refusal test proves the exemption stays loopback-only.
//!
//! Every test pins one contract the verbs promise an agent: the exact request
//! shape on the wire (method, path, bearer, body), the exit-code + machine-
//! code error surface, and the on-disk effect (pull materializes files, pull
//! REFUSES a hostile path with nothing written, push collects the owned store
//! and nothing else).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Absolute path to the `dbmd` binary Cargo built for this integration-test
/// target.
const DBMD: &str = env!("CARGO_BIN_EXE_dbmd");

/// A ULID-shaped brain id / record id for address tests.
const BRAIN_ID: &str = "01j5qc3v9k4ym8rwbn2tqe6f7d";
const RECORD_ID: &str = "01j5qc3v9k4ym8rwbn2tqe6f7e";

// ─────────────────────────────────────────────────────────────────────────────
// The mock hub
// ─────────────────────────────────────────────────────────────────────────────

/// One captured request: everything a contract test needs to pin.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    /// Lowercased `name: value` pairs.
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }
}

/// A scripted mock hub: serves the given `(status, json-body)` responses in
/// order, one connection each, capturing every request. Joining waits until
/// every scripted response was consumed.
struct MockHub {
    url: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockHub {
    fn serve(responses: Vec<(u16, String)>) -> MockHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock hub");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);

        let handle = std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut reader = BufReader::new(stream);

                // Request line.
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();

                // Headers until the blank line.
                let mut headers = Vec::new();
                let mut content_length = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() {
                        return;
                    }
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = h.split_once(':') {
                        let name = name.trim().to_ascii_lowercase();
                        let value = value.trim().to_string();
                        if name == "content-length" {
                            content_length = value.parse().unwrap_or(0);
                        }
                        headers.push((name, value));
                    }
                }

                // Body, when declared.
                let mut body_bytes = vec![0u8; content_length];
                if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
                    return;
                }

                captured.lock().unwrap().push(Captured {
                    method,
                    path,
                    headers,
                    body: String::from_utf8_lossy(&body_bytes).into_owned(),
                });

                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        MockHub {
            url,
            requests,
            handle: Some(handle),
        }
    }

    /// Wait for the scripted conversation to finish and return the captures.
    fn finish(mut self) -> Vec<Captured> {
        if let Some(h) = self.handle.take() {
            h.join().expect("mock hub thread");
        }
        Arc::try_unwrap(self.requests)
            .expect("no other capture handles")
            .into_inner()
            .unwrap()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test scaffolding
// ─────────────────────────────────────────────────────────────────────────────

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `dbmd <args>` from `cwd` with a controlled link-client environment.
/// `hub`/`key` map to the `DBMD_HUB_URL` / `DBMD_HUB_KEY` env vars; both are
/// otherwise scrubbed so the developer's real environment never leaks in.
fn run_dbmd(cwd: &Path, args: &[&str], hub: Option<&str>, key: Option<&str>) -> Output {
    let mut cmd = Command::new(DBMD);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("DBMD_HUB_URL")
        .env_remove("DBMD_HUB_KEY");
    if let Some(h) = hub {
        cmd.env("DBMD_HUB_URL", h);
    }
    if let Some(k) = key {
        cmd.env("DBMD_HUB_KEY", k);
    }
    let out = cmd.output().expect("spawn dbmd");
    Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The machine `code` out of a `--json` stderr error envelope.
fn error_code(stderr: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(stderr.lines().next().unwrap_or("{}"))
        .unwrap_or_else(|_| serde_json::json!({}));
    v["error"]["code"].as_str().unwrap_or("").to_string()
}

/// A minimal throwaway store with content, catalogs, history, and toolkit
/// state — everything the push-collection contract must include AND exclude.
fn seed_store(root: &Path) {
    let w = |rel: &str, content: &str| {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    };
    w(
        "DB.md",
        "---\ntype: db-md\nscope: company\nname: Link Test\n---\n\n# Link Test\n",
    );
    w(
        "records/clients/lumio.md",
        &format!(
            "---\ntype: client\nid: {RECORD_ID}\nsummary: Lumio is a test client\n---\n\n# Lumio\n"
        ),
    );
    w(
        "sources/notes/kickoff.md",
        "---\ntype: note\nsummary: Kickoff notes\n---\n\nNotes.\n",
    );
    w("assets.jsonl", "{\"path\":\"sources/brief.pdf\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"bytes\":1,\"media_type\":\"application/pdf\",\"required\":false}\n");
    // Derived catalogs + history + toolkit state: all must stay OFF the wire.
    w("index.md", "# Index\n");
    w("records/clients/index.md", "# Clients\n");
    w("records/clients/index.jsonl", "{}\n");
    w("log.md", "");
    w("log/2026-06.md", "");
    w(".dbmd/config", "hub = http://127.0.0.1:9\n");
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration + guard rails (no network)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn resolve_without_any_hub_config_fails_no_hub() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(dir.path(), &["resolve", "@acme", "--json"], None, None);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(error_code(&out.stderr), "NO_HUB");
}

#[test]
fn authed_verbs_without_credential_fail_no_credential() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme", "--json"],
        Some("http://127.0.0.1:1"), // loopback: passes the HTTPS guard, never dialed
        None,
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "NO_CREDENTIAL");
}

#[test]
fn plain_http_hub_outside_loopback_is_refused_before_any_dial() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme", "--json"],
        Some("http://hub.example.com"),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "HUB_NOT_HTTPS");
}

#[test]
fn bad_address_shapes_fail_with_bad_address() {
    let dir = tempfile::tempdir().unwrap();
    for addr in ["@", "@acme/", "@acme/../etc.md", "@ACME"] {
        let out = run_dbmd(dir.path(), &["resolve", addr, "--json"], None, None);
        assert_eq!(out.code, Some(1), "address {addr:?}");
        assert_eq!(error_code(&out.stderr), "BAD_ADDRESS", "address {addr:?}");
    }
}

#[test]
fn config_file_supplies_hub_and_flag_overrides_it() {
    let hub = MockHub::serve(vec![(200, format!("{{\"id\":\"{BRAIN_ID}\"}}"))]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".dbmd")).unwrap();
    // The file points at a dead port; the --hub flag must win.
    std::fs::write(
        dir.path().join(".dbmd/config"),
        "# toolkit state\nhub = http://127.0.0.1:9\n",
    )
    .unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "resolve",
            &format!("@{BRAIN_ID}"),
            "--hub",
            &hub.url,
            "--json",
        ],
        None,
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    hub.finish();

    // And with no flag/env, the file is used: dead port → HUB_UNREACHABLE
    // (proving the file was read, not NO_HUB).
    let out = run_dbmd(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}"), "--json"],
        None,
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "HUB_UNREACHABLE");
}

// ─────────────────────────────────────────────────────────────────────────────
// resolve
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn resolve_bare_brain_gets_card_with_bearer() {
    let hub = MockHub::serve(vec![(
        200,
        format!(
            "{{\"id\":\"{BRAIN_ID}\",\"slug\":\"acme\",\"name\":\"Acme\",\"visibility\":\"private\",\"indexedFeedSeq\":3,\"stats\":{{\"records\":4,\"sources\":1}}}}"
        ),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme"],
        Some(&hub.url),
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("slug: acme"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("feed seq: 3"));

    let reqs = hub.finish();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/api/hub/brains/acme");
    assert_eq!(
        reqs[0].header("authorization"),
        Some("Bearer vc_account_test")
    );
}

#[test]
fn resolve_ulid_target_queries_by_id_and_path_target_by_path() {
    let doc = format!(
        "{{\"brain\":\"{BRAIN_ID}\",\"document\":{{\"path\":\"records/clients/lumio.md\",\"id\":\"{RECORD_ID}\",\"type\":\"client\",\"summary\":\"Lumio\",\"body\":\"# Lumio\\n\"}}}}"
    );
    let hub = MockHub::serve(vec![(200, doc.clone()), (200, doc)]);
    let dir = tempfile::tempdir().unwrap();

    let by_id = run_dbmd(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}/{RECORD_ID}")],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(by_id.code, Some(0), "stderr: {}", by_id.stderr);
    assert!(by_id.stdout.contains("# Lumio"), "stdout: {}", by_id.stdout);

    let by_path = run_dbmd(
        dir.path(),
        &["resolve", "@acme/records/clients/lumio.md", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(by_path.code, Some(0), "stderr: {}", by_path.stderr);

    let reqs = hub.finish();
    assert_eq!(
        reqs[0].path,
        format!("/api/hub/brains/{BRAIN_ID}/resolve?id={RECORD_ID}")
    );
    assert_eq!(
        reqs[1].path,
        "/api/hub/brains/acme/resolve?path=records/clients/lumio.md"
    );
}

#[test]
fn hub_http_error_surfaces_hub_message_and_code() {
    let hub = MockHub::serve(vec![(404, "{\"error\":\"Brain not found\"}".to_string())]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@ghost", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "HUB_ERROR");
    assert!(
        out.stderr.contains("Brain not found") && out.stderr.contains("404"),
        "stderr: {}",
        out.stderr
    );
    hub.finish();
}

#[test]
fn non_json_2xx_is_refused_as_not_a_hub_answer() {
    let hub = MockHub::serve(vec![(200, "<html>captive portal</html>".to_string())]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "HUB_NOT_JSON");
    hub.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// sync — pull
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sync_pull_materializes_files_rebuilds_index_and_reports() {
    let export = serde_json::json!({
        "brain": BRAIN_ID,
        "slug": "acme",
        "name": "Acme",
        "headSeq": 7,
        "fileCount": 2,
        "files": [
            {"path": "DB.md", "content": "---\ntype: db-md\nscope: company\nname: Acme\n---\n\n# Acme\n"},
            {"path": "records/clients/lumio.md", "content": format!("---\ntype: client\nid: {RECORD_ID}\nsummary: Lumio\n---\n\n# Lumio\n")},
        ],
    });
    let hub = MockHub::serve(vec![(200, export.to_string())]);
    let work = tempfile::tempdir().unwrap();
    let dest = work.path().join("pulled");

    let out = run_dbmd(
        work.path(),
        &[
            "sync",
            &format!("@{BRAIN_ID}"),
            "--out",
            dest.to_str().unwrap(),
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["files"], 2);
    assert_eq!(v["headSeq"], 7);
    assert_eq!(v["index"], "rebuilt");

    // The files landed, byte-for-byte.
    assert!(dest.join("DB.md").is_file());
    let lumio = std::fs::read_to_string(dest.join("records/clients/lumio.md")).unwrap();
    assert!(lumio.contains("# Lumio"));
    // And the derived catalog exists so loop ops work immediately.
    assert!(dest.join("records/clients/index.md").is_file());

    let reqs = hub.finish();
    assert_eq!(reqs[0].path, format!("/api/hub/brains/{BRAIN_ID}/export"));
}

#[test]
fn sync_pull_refuses_hostile_paths_with_nothing_written() {
    let export = serde_json::json!({
        "brain": BRAIN_ID,
        "slug": "acme",
        "headSeq": 1,
        "files": [
            {"path": "DB.md", "content": "---\ntype: db-md\n---\n# A\n"},
            {"path": "../escape.md", "content": "evil"},
        ],
    });
    let hub = MockHub::serve(vec![(200, export.to_string())]);
    let work = tempfile::tempdir().unwrap();
    let dest = work.path().join("pulled");

    let out = run_dbmd(
        work.path(),
        &[
            "sync",
            &format!("@{BRAIN_ID}"),
            "--out",
            dest.to_str().unwrap(),
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "UNSAFE_PATH");
    // The gate runs before the FIRST write: even the benign file must not land.
    assert!(!dest.join("DB.md").exists(), "nothing may be written");
    assert!(!work.path().join("escape.md").exists());
    hub.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// sync — push
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sync_push_sends_owned_content_only_with_bearer() {
    let hub = MockHub::serve(vec![(
        200,
        format!(
            "{{\"brain\":\"{BRAIN_ID}\",\"indexed\":{{\"documents\":2}},\"durable\":true,\"headSeq\":1}}"
        ),
    )]);
    let store = tempfile::tempdir().unwrap();
    seed_store(store.path());

    let out = run_dbmd(
        store.path(),
        &["sync", &format!("@{BRAIN_ID}"), "--push"],
        Some(&hub.url),
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("pushed 4 files") && out.stdout.contains("durable"),
        "stdout: {}",
        out.stdout
    );

    let reqs = hub.finish();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, format!("/api/hub/brains/{BRAIN_ID}/push"));
    assert_eq!(
        reqs[0].header("authorization"),
        Some("Bearer vc_account_test")
    );

    let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
    let mut paths: Vec<&str> = body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    // The owned content travels; catalogs, history, and toolkit state do not.
    assert_eq!(
        paths,
        vec![
            "DB.md",
            "assets.jsonl",
            "records/clients/lumio.md",
            "sources/notes/kickoff.md",
        ],
        "push must carry exactly the owned store content"
    );
}

#[test]
fn sync_push_outside_a_store_is_the_standard_not_a_store_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["sync", "@acme", "--push", "--json"],
        Some("http://127.0.0.1:9"),
        Some("k"),
    );
    assert_eq!(out.code, Some(3), "stderr: {}", out.stderr);
    assert_eq!(error_code(&out.stderr), "NOT_A_STORE");
}

// ─────────────────────────────────────────────────────────────────────────────
// grant
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn grant_issue_list_revoke_speak_the_grants_binding() {
    let grant_id = "01j5qc3v9k4ym8rwbn2tqe6f7f";
    let hub = MockHub::serve(vec![
        (
            201,
            format!(
                "{{\"id\":\"{grant_id}\",\"brain\":\"{BRAIN_ID}\",\"grantee\":{{\"email\":\"maya@example.com\"}},\"capability\":\"read\",\"scopePrefix\":\"records/clients/\",\"expiresAt\":\"2026-09-01T00:00:00.000Z\"}}"
            ),
        ),
        (
            200,
            format!(
                "{{\"brain\":\"{BRAIN_ID}\",\"grants\":[{{\"id\":\"{grant_id}\",\"email\":\"maya@example.com\",\"capability\":\"read\",\"scopePrefix\":\"records/clients/\"}}],\"invites\":[]}}"
            ),
        ),
        (200, format!("{{\"revoked\":true,\"id\":\"{grant_id}\"}}")),
    ]);
    let dir = tempfile::tempdir().unwrap();

    let issue = run_dbmd(
        dir.path(),
        &[
            "grant",
            "issue",
            &format!("@{BRAIN_ID}"),
            "maya@example.com",
            "--can",
            "read",
            "--scope",
            "records/clients/",
            "--until",
            "2026-09-01",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(issue.code, Some(0), "stderr: {}", issue.stderr);
    assert!(
        issue.stdout.contains("granted read to maya@example.com"),
        "stdout: {}",
        issue.stdout
    );

    let list = run_dbmd(
        dir.path(),
        &["grant", "list", &format!("@{BRAIN_ID}")],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(list.code, Some(0));
    assert!(
        list.stdout.contains("maya@example.com") && list.stdout.contains("scope=records/clients/")
    );

    let revoke = run_dbmd(
        dir.path(),
        &["grant", "revoke", &format!("@{BRAIN_ID}"), grant_id],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(revoke.code, Some(0));

    let reqs = hub.finish();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, format!("/api/hub/brains/{BRAIN_ID}/grants"));
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
    assert_eq!(body["email"], "maya@example.com");
    assert_eq!(body["capability"], "read");
    assert_eq!(body["scopePrefix"], "records/clients/");
    assert_eq!(body["expiresAt"], "2026-09-01");
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[2].method, "DELETE");
    assert_eq!(
        reqs[2].path,
        format!("/api/hub/brains/{BRAIN_ID}/grants/{grant_id}")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// propose
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn propose_posts_to_the_site_inbox_without_a_bearer() {
    let hub = MockHub::serve(vec![(
        201,
        "{\"id\":\"01j5qc3v9k4ym8rwbn2tqe6f7g\",\"path\":\"sources/inbox/01j5qc3v9k4ym8rwbn2tqe6f7g.md\"}"
            .to_string(),
    )]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("evidence.md"), "New invoice: 4400 EUR.\n").unwrap();

    let out = run_dbmd(
        dir.path(),
        &[
            "propose",
            "@acme-site",
            "--app",
            "intake",
            "--body-file",
            "evidence.md",
        ],
        Some(&hub.url),
        Some("k"), // present in the env, but the door must NOT receive it
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("sources/inbox/"),
        "stdout: {}",
        out.stdout
    );

    let reqs = hub.finish();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/api/hub/sites/acme-site/inbox");
    assert_eq!(
        reqs[0].header("authorization"),
        None,
        "propose is unauthenticated by design — the credential must not leak through it"
    );
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
    assert_eq!(body["app"], "intake");
    assert_eq!(body["body"], "New invoice: 4400 EUR.\n");
}

#[test]
fn propose_requires_exactly_one_body_source() {
    let dir = tempfile::tempdir().unwrap();
    let none = run_dbmd(
        dir.path(),
        &["propose", "@s", "--app", "a", "--json"],
        Some("http://127.0.0.1:9"),
        None,
    );
    assert_eq!(none.code, Some(1));
    assert_eq!(error_code(&none.stderr), "BAD_BODY");

    // Both at once is an arg-parse conflict — clap owns exit 2.
    let both = run_dbmd(
        dir.path(),
        &[
            "propose",
            "@s",
            "--app",
            "a",
            "--body",
            "x",
            "--body-file",
            "y",
            "--json",
        ],
        Some("http://127.0.0.1:9"),
        None,
    );
    assert_eq!(both.code, Some(2));
}

// ─────────────────────────────────────────────────────────────────────────────
// subscribe
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn subscribe_once_reports_the_current_head_as_one_json_line() {
    let hub = MockHub::serve(vec![(
        200,
        format!("{{\"id\":\"{BRAIN_ID}\",\"indexedFeedSeq\":41,\"updatedAt\":\"2026-07-13T00:00:00.000Z\"}}"),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // NDJSON: exactly one compact object line.
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout: {}", out.stdout);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["brain"], BRAIN_ID);
    assert_eq!(v["seq"], 41);
    hub.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// Hardening: refs never reshape the request path; hub strings never reach the
// terminal raw; oversize propose bodies never reach the wire
// ─────────────────────────────────────────────────────────────────────────────

/// A dead loopback hub: it passes the HTTPS guard, but any DIAL surfaces
/// `HUB_UNREACHABLE` — so a shape refusal proves the gate fired before a
/// request existed.
const DEAD_HUB: &str = "http://127.0.0.1:9";

#[test]
fn every_verb_refuses_url_reshaping_refs_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    seed_store(store.path());

    for bad in ["../up", "a/b", "a?x=1", "a#frag"] {
        for (cwd, args) in [
            (dir.path(), vec!["sync", bad, "--json"]),
            (store.path(), vec!["sync", bad, "--push", "--json"]),
            (
                dir.path(),
                vec!["grant", "issue", bad, "maya@example.com", "--json"],
            ),
            (dir.path(), vec!["grant", "list", bad, "--json"]),
            (
                dir.path(),
                vec!["grant", "revoke", bad, RECORD_ID, "--json"],
            ),
            (
                dir.path(),
                vec!["propose", bad, "--app", "intake", "--body", "x", "--json"],
            ),
            (dir.path(), vec!["subscribe", bad, "--once", "--json"]),
        ] {
            let out = run_dbmd(cwd, &args, Some(DEAD_HUB), Some("k"));
            assert_eq!(
                out.code,
                Some(1),
                "args {args:?} ref {bad:?}: {}",
                out.stderr
            );
            assert_eq!(
                error_code(&out.stderr),
                "BAD_ADDRESS",
                "args {args:?} ref {bad:?}: {}",
                out.stderr
            );
        }

        // The grant id travels as its own path segment and is gated with its
        // own machine code.
        let out = run_dbmd(
            dir.path(),
            &["grant", "revoke", "acme", bad, "--json"],
            Some(DEAD_HUB),
            Some("k"),
        );
        assert_eq!(out.code, Some(1), "grant id {bad:?}: {}", out.stderr);
        assert_eq!(
            error_code(&out.stderr),
            "BAD_GRANT_ID",
            "grant id {bad:?}: {}",
            out.stderr
        );
    }
}

#[test]
fn propose_body_file_over_the_inbox_cap_fails_before_the_upload() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.md");
    std::fs::write(
        &big,
        vec![b'a'; dbmd_core::linkmd::MAX_PROPOSE_BYTES as usize + 1],
    )
    .unwrap();

    // Dead hub: reaching the wire would surface HUB_UNREACHABLE, so
    // PROPOSE_TOO_LARGE proves the refusal happened before the upload — and
    // before the file was even read (the check runs on metadata).
    let out = run_dbmd(
        dir.path(),
        &[
            "propose",
            "@acme-site",
            "--app",
            "intake",
            "--body-file",
            big.to_str().unwrap(),
            "--json",
        ],
        Some(DEAD_HUB),
        None,
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(error_code(&out.stderr), "PROPOSE_TOO_LARGE");
    assert!(
        out.stderr.contains("16 KB"),
        "the message must name the cap: {}",
        out.stderr
    );
}

#[test]
fn hub_strings_render_terminal_sanitized_in_text_mode_and_verbatim_in_json() {
    // The summary and body carry an ANSI escape sequence and a BEL: text mode
    // strips them; `--json` is a machine surface and stays byte-verbatim.
    let doc = format!(
        "{{\"brain\":\"{BRAIN_ID}\",\"document\":{{\"path\":\"records/clients/lumio.md\",\"id\":\"{RECORD_ID}\",\"type\":\"client\",\"summary\":\"\\u001b[31mEVIL\\u0007summary\",\"body\":\"# Lumio\\u001b[2J\\u0007 ok\\n\"}}}}"
    );
    let hub = MockHub::serve(vec![(200, doc.clone()), (200, doc)]);
    let dir = tempfile::tempdir().unwrap();
    let addr = format!("@{BRAIN_ID}/{RECORD_ID}");

    let text = run_dbmd(dir.path(), &["resolve", &addr], Some(&hub.url), Some("k"));
    assert_eq!(text.code, Some(0), "stderr: {}", text.stderr);
    assert!(
        text.stdout.contains("summary: EVILsummary"),
        "stdout: {:?}",
        text.stdout
    );
    assert!(
        text.stdout.contains("# Lumio ok"),
        "stdout: {:?}",
        text.stdout
    );
    assert!(
        !text.stdout.contains('\u{1b}') && !text.stdout.contains('\u{7}'),
        "text mode must strip control bytes: {:?}",
        text.stdout
    );

    let json = run_dbmd(
        dir.path(),
        &["resolve", &addr, "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(json.code, Some(0), "stderr: {}", json.stderr);
    let v: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(
        v["document"]["summary"], "\u{1b}[31mEVIL\u{7}summary",
        "--json must stay verbatim"
    );

    hub.finish();
}

#[test]
fn hub_error_messages_render_terminal_sanitized_in_text_mode() {
    let error_body = "{\"error\":\"\\u001b[2Jboom\\u0007\",\"code\":\"kaboom\"}".to_string();
    let hub = MockHub::serve(vec![(500, error_body.clone()), (500, error_body)]);
    let dir = tempfile::tempdir().unwrap();

    let text = run_dbmd(dir.path(), &["resolve", "@acme"], Some(&hub.url), Some("k"));
    assert_eq!(text.code, Some(1));
    assert!(text.stderr.contains("boom"), "stderr: {:?}", text.stderr);
    assert!(
        !text.stderr.contains('\u{1b}') && !text.stderr.contains('\u{7}'),
        "text-mode errors must strip control bytes: {:?}",
        text.stderr
    );

    let json = run_dbmd(
        dir.path(),
        &["resolve", "@acme", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(json.code, Some(1));
    let v: serde_json::Value =
        serde_json::from_str(json.stderr.lines().next().unwrap_or("{}")).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains('\u{1b}'),
        "--json errors must stay verbatim: {:?}",
        json.stderr
    );

    hub.finish();
}

#[test]
fn subscribe_once_with_since_reports_head_against_the_baseline() {
    let hub = MockHub::serve(vec![(
        200,
        format!("{{\"id\":\"{BRAIN_ID}\",\"indexedFeedSeq\":41}}"),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "subscribe",
            &format!("@{BRAIN_ID}"),
            "--once",
            "--since",
            "40",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("feed seq 40 -> 41"),
        "stdout: {}",
        out.stdout
    );
    hub.finish();
}
