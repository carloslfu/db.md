// SPDX-License-Identifier: Apache-2.0

//! `dbmd api` — the local app API, end to end.
//!
//! Each test spawns the real server (`dbmd api --addr 127.0.0.1:0`) against
//! a scratch store, parses the bound address from the first stdout line, and
//! speaks plain HTTP/1.1 over a raw `TcpStream` (the server always answers
//! one request per connection with `connection: close`, so read-to-EOF is
//! the whole client). The core contract pinned here: a route's body is the
//! same-named CLI verb's `--json` output verbatim, and CLI exit codes map to
//! HTTP statuses (0→200, 1/2→400, 3→404, 4→403, 5→409, 6→422).

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{dbmd, write_file};

struct Api {
    child: Child,
    addr: String,
}

impl Api {
    fn spawn(store: &std::path::Path) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("dbmd"))
            .args(["--json", "api", "--addr", "127.0.0.1:0", "--dir"])
            .arg(store)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dbmd api");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut first = String::new();
        BufReader::new(stdout)
            .read_line(&mut first)
            .expect("read the serving line");
        let serving: serde_json::Value =
            serde_json::from_str(&first).expect("serving line is JSON");
        let url = serving["serving"]
            .as_str()
            .expect("serving url")
            .to_string();
        let addr = url.strip_prefix("http://").expect("http url").to_string();
        Self { child, addr }
    }

    /// One request, one connection; returns (status, headers, body).
    fn request(
        &self,
        method: &str,
        target: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\n",
            self.addr,
            body.len()
        );
        if let Some(ct) = content_type {
            head.push_str(&format!("content-type: {ct}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response has a header/body split");
        let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status: u16 = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status line");
        (status, headers, raw[split + 4..].to_vec())
    }

    fn get(&self, target: &str) -> (u16, serde_json::Value) {
        let (status, _, body) = self.request("GET", target, b"", None);
        let parsed = serde_json::from_slice(&body).unwrap_or_else(|e| {
            panic!(
                "GET {target}: body not JSON ({e}): {}",
                String::from_utf8_lossy(&body)
            )
        });
        (status, parsed)
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path().to_path_buf();
    write_file(
        &store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Schemas\n\n### widget\n- status (enum: active, done)\n",
    );
    for (path, summary) in [
        ("records/widgets/a.md", "Widget A"),
        ("records/widgets/b.md", "Widget B"),
    ] {
        dbmd()
            .args([
                "write",
                path,
                "--type",
                "widget",
                "--summary",
                summary,
                "--fm",
                "status=active",
                "--dir",
            ])
            .arg(&store)
            .assert()
            .success();
    }
    (dir, store)
}

/// Reads pass the same-named CLI verb's `--json` output through verbatim —
/// checked byte-for-byte for show, query, and schema.
#[test]
fn api_reads_equal_cli_json_output() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);

    for (target, cli_args) in [
        (
            "/v1/show?file=records/widgets/a.md",
            vec!["--json", "show", "records/widgets/a.md"],
        ),
        (
            "/v1/query?type=widget&where=status%3Dactive",
            vec![
                "--json",
                "query",
                "--type",
                "widget",
                "--where",
                "status=active",
            ],
        ),
        ("/v1/schema?type=widget", vec!["--json", "schema", "widget"]),
        (
            "/v1/sections?file=records/widgets/a.md",
            vec!["--json", "sections", "records/widgets/a.md"],
        ),
    ] {
        let (status, _, body) = api.request("GET", target, b"", None);
        assert_eq!(status, 200, "{target}");
        let assert = dbmd()
            .args(&cli_args)
            .current_dir(&store)
            .assert()
            .success();
        assert_eq!(
            body,
            assert.get_output().stdout,
            "{target} must equal the CLI verb's output verbatim"
        );
    }
}

/// The write cycle over HTTP: create, read back, edit frontmatter, edit
/// body, edit a section, link, then a guarded delete — every mutation is a
/// real verb execution with its full contract.
#[test]
fn api_write_cycle() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);

    // create
    let spec = serde_json::json!({
        "path": "records/widgets/c.md",
        "type": "widget",
        "summary": "Widget C",
        "fm": { "status": "active" },
        "body": "# C\n\n## Log\n- born\n",
    })
    .to_string();
    let (status, _, body) = api.request(
        "POST",
        "/v1/write",
        spec.as_bytes(),
        Some("application/json"),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let written: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        written["written"],
        serde_json::json!("records/widgets/c.md")
    );
    assert!(store.join("records/widgets/c.md").exists());

    // read back
    let (status, shown) = api.get("/v1/show?file=records/widgets/c.md");
    assert_eq!(status, 200);
    assert_eq!(shown["summary"], serde_json::json!("Widget C"));
    assert_eq!(shown["body"], serde_json::json!("# C\n\n## Log\n- born\n"));

    // fm set via raw-value body
    let (status, _, _) = api.request(
        "PUT",
        "/v1/fm?file=records/widgets/c.md&key=status",
        b"done",
        Some("text/plain"),
    );
    assert_eq!(status, 200);
    let (_, fm) = api.get("/v1/fm?file=records/widgets/c.md&key=status");
    assert_eq!(fm["value"], serde_json::json!("done"));

    // body set with raw bytes
    let (status, _, _) = api.request(
        "PUT",
        "/v1/body?file=records/widgets/c.md",
        b"## Log\n- reborn\n",
        Some("text/plain"),
    );
    assert_eq!(status, 200);

    // section append + section get
    let (status, _, _) = api.request(
        "POST",
        "/v1/section/append?file=records/widgets/c.md&heading=Log",
        b"- appended over http",
        Some("text/plain"),
    );
    assert_eq!(status, 200);
    let (status, section) = api.get("/v1/section?file=records/widgets/c.md&heading=Log");
    assert_eq!(status, 200);
    assert_eq!(
        section["body"],
        serde_json::json!("## Log\n- reborn\n- appended over http\n")
    );

    // section upsert with --create
    let (status, _, _) = api.request(
        "PUT",
        "/v1/section?file=records/widgets/c.md&heading=Notes&create=1&level=3",
        b"fresh",
        Some("text/plain"),
    );
    assert_eq!(status, 200);

    // link b -> c, then rm c refuses with 409 RM_LINKED
    let (status, _, _) = api.request(
        "POST",
        "/v1/link?from=records/widgets/b.md&to=records/widgets/c.md",
        b"",
        None,
    );
    assert_eq!(status, 200);
    let (status, _, body) = api.request("DELETE", "/v1/rm?path=records/widgets/c.md", b"", None);
    assert_eq!(status, 409);
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["error"]["code"], serde_json::json!("RM_LINKED"));
    assert_eq!(
        err["error"]["details"]["backlinks"],
        serde_json::json!(["records/widgets/b.md"])
    );

    // forced delete succeeds; validate then reports the break as 422
    let (status, _, _) = api.request(
        "DELETE",
        "/v1/rm?path=records/widgets/c.md&force=1",
        b"",
        None,
    );
    assert_eq!(status, 200);
    let (status, report) = api.get("/v1/validate?all=1");
    assert_eq!(status, 422);
    assert!(
        report["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == serde_json::json!("WIKI_LINK_BROKEN")),
        "validate must surface the forced break: {report}"
    );
}

/// The status mapping carries the CLI's structured refusals: a frozen page
/// is 403 with `POLICY_FROZEN_PAGE`, a missing section 400 with
/// `SECTION_NOT_FOUND`, an unknown route 404, a wrong method 405.
#[test]
fn api_error_contract() {
    let (_tmp, store) = scratch_store();
    write_file(
        &store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Policies\n\n### Frozen pages\n- `records/widgets/a.md` — signed off.\n",
    );
    let api = Api::spawn(&store);

    let (status, _, body) = api.request(
        "PUT",
        "/v1/body?file=records/widgets/a.md",
        b"nope",
        Some("text/plain"),
    );
    assert_eq!(status, 403);
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        err["error"]["code"],
        serde_json::json!("POLICY_FROZEN_PAGE")
    );

    let (status, err) = api.get("/v1/section?file=records/widgets/b.md&heading=Nope");
    assert_eq!(status, 400);
    assert_eq!(err["error"]["code"], serde_json::json!("SECTION_NOT_FOUND"));

    let (status, err) = api.get("/v1/nope");
    assert_eq!(status, 404);
    assert_eq!(err["error"]["code"], serde_json::json!("UNKNOWN_ROUTE"));

    let (status, _, _) = api.request("PUT", "/v1/show?file=x", b"", None);
    assert_eq!(status, 405);

    let (status, err) = api.get("/v1/show?file=missing.md");
    assert_eq!(status, 400, "{err}");
}

/// CORS: a preflight gets 204 with the open grant, and every response
/// carries `access-control-allow-origin: *` — a local browser app can call
/// the API directly.
#[test]
fn api_cors_for_local_browser_apps() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);

    let (status, headers, _) = api.request("OPTIONS", "/v1/show", b"", None);
    assert_eq!(status, 204);
    let lower = headers.to_ascii_lowercase();
    assert!(
        lower.contains("access-control-allow-origin: *"),
        "{headers}"
    );
    assert!(lower.contains("access-control-allow-methods:"), "{headers}");

    let (_, headers, _) = api.request("GET", "/v1/version", b"", None);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin: *"),
        "{headers}"
    );
}

/// `/v1/events` re-frames the watch feed as SSE: a baseline frame, then a
/// created frame when a record is written through the API concurrently.
#[test]
fn api_events_stream_sse() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);

    let mut stream = TcpStream::connect(&api.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /v1/events?interval=1 HTTP/1.1\r\nhost: {}\r\n\r\n",
                api.addr
            )
            .as_bytes(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream);
    // headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header line");
        if line == "\r\n" {
            break;
        }
        if line.starts_with("HTTP/1.1") {
            assert!(line.contains("200"), "{line}");
        }
    }
    let mut next_frame = || -> serde_json::Value {
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("sse line within timeout");
            if let Some(data) = line.strip_prefix("data: ") {
                return serde_json::from_str(data.trim_end()).expect("frame is JSON");
            }
        }
    };
    let baseline = next_frame();
    assert_eq!(baseline["event"], serde_json::json!("baseline"));
    assert_eq!(baseline["files"], serde_json::json!(3)); // DB.md + a + b

    let spec = serde_json::json!({
        "path": "records/widgets/d.md", "type": "widget", "summary": "Widget D",
        "fm": { "status": "active" },
    })
    .to_string();
    let (status, _, _) = api.request(
        "POST",
        "/v1/write",
        spec.as_bytes(),
        Some("application/json"),
    );
    assert_eq!(status, 200);

    let created = next_frame();
    assert_eq!(created["event"], serde_json::json!("created"));
    assert_eq!(created["path"], serde_json::json!("records/widgets/d.md"));
}

/// Concurrent mutations through the API serialize on the store transaction
/// lock — both writes land, both records exist, the store validates clean.
#[test]
fn api_concurrent_writes_serialize() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);
    let addr = api.addr.clone();

    let workers: Vec<_> = (0..4)
        .map(|i| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let spec = serde_json::json!({
                    "path": format!("records/widgets/conc-{i}.md"),
                    "type": "widget",
                    "summary": format!("Concurrent {i}"),
                    "fm": { "status": "active" },
                })
                .to_string();
                let mut stream = TcpStream::connect(&addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let head = format!(
                    "POST /v1/write HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    spec.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(spec.as_bytes()).unwrap();
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).unwrap();
                String::from_utf8_lossy(&raw)
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string()
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), "200");
    }
    for i in 0..4 {
        assert!(store.join(format!("records/widgets/conc-{i}.md")).exists());
    }
    dbmd()
        .args(["validate", "--all"])
        .arg(&store)
        .assert()
        .success();
}

/// The server refuses a non-loopback bind (there is no escape hatch) and
/// a non-store directory fails fast before binding.
#[test]
fn api_refuses_public_bind_and_non_store() {
    let (_tmp, store) = scratch_store();
    let assert = dbmd()
        .args(["--json", "api", "--addr", "0.0.0.0:0", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(
        err["error"]["code"],
        serde_json::json!("API_PUBLIC_REFUSED")
    );

    let empty = tempfile::TempDir::new().unwrap();
    dbmd()
        .args(["api", "--addr", "127.0.0.1:0", "--dir"])
        .arg(empty.path())
        .assert()
        .failure()
        .code(3); // ExitCode::NotAStore
}

/// Discovery: `GET /v1` lists the routes; `/v1/version` matches the crate.
#[test]
fn api_discovery() {
    let (_tmp, store) = scratch_store();
    let api = Api::spawn(&store);

    let (status, index) = api.get("/v1");
    assert_eq!(status, 200);
    assert_eq!(index["dbmd"], serde_json::json!("api"));
    assert!(index["routes"]["reads"].as_array().unwrap().len() > 10);
    assert!(index["routes"]["writes"].as_array().unwrap().len() > 10);

    let (status, version) = api.get("/v1/version");
    assert_eq!(status, 200);
    assert_eq!(
        version["version"],
        serde_json::json!(env!("CARGO_PKG_VERSION"))
    );
}
