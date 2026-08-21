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

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::signature::KeyPair as _;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Absolute path to the `dbmd` binary Cargo built for this integration-test
/// target.
const DBMD: &str = env!("CARGO_BIN_EXE_dbmd");

/// A ULID-shaped brain id / record id for address tests.
const BRAIN_ID: &str = "01j5qc3v9k4ym8rwbn2tqe6f7d";
const RECORD_ID: &str = "01j5qc3v9k4ym8rwbn2tqe6f7e";

// A deterministic, independently generated Ed25519 fixture. The feed entry's
// signature covers its unsigned canonical JSON and `feedHash` covers the exact
// signed JSON plus its trailing newline, matching the hub contract.
const SIGNED_HEAD_HASH: &str = "d93db0de1f5f9b7b98da87d34520e02df7aa4a9786da28ce191fdf0ede88a2cd";
const SIGNED_HEAD_CARD: &str = r#"{"id":"01j5qc3v9k4ym8rwbn2tqe6f7d","headSeq":41,"feedHash":"d93db0de1f5f9b7b98da87d34520e02df7aa4a9786da28ce191fdf0ede88a2cd","updatedAt":"2026-07-13T00:00:00.000Z","identity":{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ","publicKeySpki":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU","previous":[],"rotations":[]}}"#;
const SIGNED_HEAD_FEED: &str = r#"{"headSeq":41,"feedHash":"d93db0de1f5f9b7b98da87d34520e02df7aa4a9786da28ce191fdf0ede88a2cd","identity":{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ","publicKeySpki":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU"},"entries":[{"hash":"d93db0de1f5f9b7b98da87d34520e02df7aa4a9786da28ce191fdf0ede88a2cd","entry":{"v":1,"seq":41,"ts":"2026-07-14T00:00:00.000Z","brain":"ed25519:plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ","public_key":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU","kind":"push","op":"snapshot","pack_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","files":[{"path":"DB.md","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","bytes":3}],"removed":[],"prev_entry_hash":null,"sig":"TEozQnDFrOBDvYR2x_pfgah2Oyr3xGZX3acjvAmrniytxN0x6J5bgQwd0Vso1fgWJqvO3UPytDMN8QFJeRRQBw"}}],"scopeLimited":false}"#;

fn signed_head_responses() -> Vec<(u16, String)> {
    vec![
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
    ]
}

fn signed_card_with_metadata() -> String {
    let mut card: serde_json::Value = serde_json::from_str(SIGNED_HEAD_CARD).unwrap();
    card["slug"] = serde_json::json!("acme");
    card["name"] = serde_json::json!("Acme");
    card["visibility"] = serde_json::json!("private");
    card["indexedFeedSeq"] = serde_json::json!(41);
    card["stats"] = serde_json::json!({"records": 4, "sources": 1});
    card.to_string()
}

#[derive(Serialize)]
struct TestFeedFile<'a> {
    path: &'a str,
    sha256: String,
    bytes: u64,
}

#[derive(Serialize)]
struct TestUnsignedFeed<'a> {
    v: u8,
    seq: u64,
    ts: &'a str,
    brain: &'a str,
    public_key: &'a str,
    kind: &'a str,
    op: &'a str,
    pack_sha256: &'a str,
    files: &'a [TestFeedFile<'a>],
    removed: &'a [String],
    prev_entry_hash: &'a Option<String>,
}

#[derive(Serialize)]
struct TestSignedFeed<'a> {
    v: u8,
    seq: u64,
    ts: &'a str,
    brain: &'a str,
    public_key: &'a str,
    kind: &'a str,
    op: &'a str,
    pack_sha256: &'a str,
    files: &'a [TestFeedFile<'a>],
    removed: &'a [String],
    prev_entry_hash: &'a Option<String>,
    sig: &'a str,
}

#[derive(Serialize)]
struct TestUnsignedRotation<'a> {
    v: u8,
    op: &'a str,
    brain: &'a str,
    public_key: &'a str,
    new_brain: &'a str,
    new_public_key: &'a str,
    prior_head_seq: u64,
    prior_feed_hash: Option<&'a str>,
    ts: &'a str,
}

fn signed_inline_snapshot(
    files: &[(&str, &str)],
) -> (String, String, String, ring::signature::Ed25519KeyPair) {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(pair.public_key().as_ref());
    let public_key = URL_SAFE_NO_PAD.encode(&spki);
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&spki));
    let multikey = format!("ed25519:{fingerprint}");
    let manifest: Vec<TestFeedFile<'_>> = files
        .iter()
        .map(|(path, content)| TestFeedFile {
            path,
            sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            bytes: content.len() as u64,
        })
        .collect();
    let removed = Vec::new();
    let previous = None;
    let pack_sha256 = "a".repeat(64);
    let unsigned = TestUnsignedFeed {
        v: 1,
        seq: 1,
        ts: "2026-07-30T12:00:00.000Z",
        brain: &multikey,
        public_key: &public_key,
        kind: "push",
        op: "snapshot",
        pack_sha256: &pack_sha256,
        files: &manifest,
        removed: &removed,
        prev_entry_hash: &previous,
    };
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(&serde_json::to_vec(&unsigned).unwrap()).as_ref());
    let signed = TestSignedFeed {
        v: unsigned.v,
        seq: unsigned.seq,
        ts: unsigned.ts,
        brain: unsigned.brain,
        public_key: unsigned.public_key,
        kind: unsigned.kind,
        op: unsigned.op,
        pack_sha256: unsigned.pack_sha256,
        files: unsigned.files,
        removed: unsigned.removed,
        prev_entry_hash: unsigned.prev_entry_hash,
        sig: &sig,
    };
    let entry = serde_json::to_value(&signed).unwrap();
    let mut exact = serde_json::to_vec(&signed).unwrap();
    exact.push(b'\n');
    let hash = format!("{:x}", Sha256::digest(&exact));
    let card = serde_json::json!({
        "id": BRAIN_ID,
        "headSeq": 1,
        "feedHash": hash,
    })
    .to_string();
    let feed = serde_json::json!({
        "headSeq": 1,
        "feedHash": hash,
        "identity": {
            "fingerprint": fingerprint,
            "publicKeySpki": public_key,
            "previous": [],
            "rotations": [],
        },
        "entries": [{"hash": hash, "entry": entry}],
        "scopeLimited": false,
    })
    .to_string();
    let export = serde_json::json!({
        "brain": BRAIN_ID,
        "slug": "acme",
        "headSeq": 1,
        "feedHash": hash,
        "files": files.iter().map(|(path, content)| {
            serde_json::json!({"path": path, "content": content})
        }).collect::<Vec<_>>(),
    })
    .to_string();
    (card, feed, export, pair)
}

fn signed_head_for_key(pair: &ring::signature::Ed25519KeyPair) -> (String, String, String) {
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(pair.public_key().as_ref());
    let public_key = URL_SAFE_NO_PAD.encode(&spki);
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&spki));
    let multikey = format!("ed25519:{fingerprint}");
    let files = Vec::new();
    let removed = Vec::new();
    let previous = None;
    let pack_sha256 = "a".repeat(64);
    let unsigned = TestUnsignedFeed {
        v: 1,
        seq: 1,
        ts: "2026-07-30T12:00:00.000Z",
        brain: &multikey,
        public_key: &public_key,
        kind: "push",
        op: "snapshot",
        pack_sha256: &pack_sha256,
        files: &files,
        removed: &removed,
        prev_entry_hash: &previous,
    };
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(&serde_json::to_vec(&unsigned).unwrap()).as_ref());
    let signed = TestSignedFeed {
        v: unsigned.v,
        seq: unsigned.seq,
        ts: unsigned.ts,
        brain: unsigned.brain,
        public_key: unsigned.public_key,
        kind: unsigned.kind,
        op: unsigned.op,
        pack_sha256: unsigned.pack_sha256,
        files: unsigned.files,
        removed: unsigned.removed,
        prev_entry_hash: unsigned.prev_entry_hash,
        sig: &sig,
    };
    let entry = serde_json::to_value(&signed).unwrap();
    let mut exact = serde_json::to_vec(&signed).unwrap();
    exact.push(b'\n');
    let hash = format!("{:x}", Sha256::digest(&exact));
    let card = serde_json::json!({"id": BRAIN_ID, "headSeq": 1, "feedHash": hash}).to_string();
    let feed = serde_json::json!({
        "headSeq": 1,
        "feedHash": hash,
        "identity": {
            "fingerprint": fingerprint,
            "publicKeySpki": public_key,
            "previous": [],
            "rotations": [],
        },
        "entries": [{"hash": hash, "entry": entry}],
        "scopeLimited": false,
    })
    .to_string();
    (card, feed, multikey)
}

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
        Self::serve_generated(move |_| {
            responses
                .into_iter()
                .map(|(status, body)| (status, "application/json", body.into_bytes()))
                .collect()
        })
    }

    fn serve_generated(build: impl FnOnce(&str) -> Vec<(u16, &'static str, Vec<u8>)>) -> MockHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock hub");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let responses = build(&url);
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);

        let handle = std::thread::spawn(move || {
            for (status, content_type, body) in responses {
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
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len(),
                );
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
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

fn serve_exact_snapshot_hub(v2_probe_miss: bool) -> MockHub {
    serve_snapshot_hub(vec![
        (
            "DB.md".to_string(),
            "---\ntype: db-md\nscope: company\nname: Mirror\n---\n\n# Mirror\n".to_string(),
        ),
        (
            "records/note.md".to_string(),
            "---\ntype: note\nid: 01j5qc3v9k4ym8rwbn2tqe6f7e\nsummary: Signed note\n---\n\n# Note\n".to_string(),
        ),
    ], v2_probe_miss)
}

fn serve_snapshot_hub(files: Vec<(String, String)>, v2_probe_miss: bool) -> MockHub {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o600);
    for (path, content) in &files {
        writer.start_file(path, options).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    let pack = writer.finish().unwrap().into_inner();
    let pack_sha256 = format!("{:x}", Sha256::digest(&pack));

    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(pair.public_key().as_ref());
    let public_key = URL_SAFE_NO_PAD.encode(&spki);
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&spki));
    let multikey = format!("ed25519:{fingerprint}");
    let manifest: Vec<TestFeedFile<'_>> = files
        .iter()
        .map(|(path, content)| TestFeedFile {
            path: path.as_str(),
            sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
            bytes: content.len() as u64,
        })
        .collect();
    let removed = Vec::new();
    let previous = None;
    let unsigned = TestUnsignedFeed {
        v: 1,
        seq: 1,
        ts: "2026-07-30T12:00:00.000Z",
        brain: &multikey,
        public_key: &public_key,
        kind: "push",
        op: "snapshot",
        pack_sha256: &pack_sha256,
        files: &manifest,
        removed: &removed,
        prev_entry_hash: &previous,
    };
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(&serde_json::to_vec(&unsigned).unwrap()).as_ref());
    let signed = TestSignedFeed {
        v: unsigned.v,
        seq: unsigned.seq,
        ts: unsigned.ts,
        brain: unsigned.brain,
        public_key: unsigned.public_key,
        kind: unsigned.kind,
        op: unsigned.op,
        pack_sha256: unsigned.pack_sha256,
        files: unsigned.files,
        removed: unsigned.removed,
        prev_entry_hash: unsigned.prev_entry_hash,
        sig: &sig,
    };
    let entry = serde_json::to_value(&signed).unwrap();
    let mut exact = serde_json::to_vec(&signed).unwrap();
    exact.push(b'\n');
    let feed_hash = format!("{:x}", Sha256::digest(&exact));
    let card = serde_json::json!({"id": BRAIN_ID, "headSeq": 1, "feedHash": feed_hash}).to_string();
    let feed = serde_json::json!({
        "headSeq": 1,
        "feedHash": feed_hash,
        "identity": {
            "fingerprint": fingerprint,
            "publicKeySpki": public_key,
            "previous": [],
            "rotations": [],
        },
        "entries": [{"hash": feed_hash, "entry": entry}],
        "scopeLimited": false,
    })
    .to_string();
    MockHub::serve_generated(move |url| {
        let export = serde_json::json!({
            "brain": BRAIN_ID,
            "slug": "mirror",
            "headSeq": 1,
            "feedHash": feed_hash,
            "sha256": pack_sha256,
            "url": format!("{url}/snapshot.pack"),
        })
        .to_string();
        let mut responses = Vec::new();
        if v2_probe_miss {
            responses.push((404, "application/json", b"{}".to_vec()));
        }
        responses.extend([
            (200, "application/json", card.into_bytes()),
            (200, "application/json", feed.into_bytes()),
            (200, "application/json", export.into_bytes()),
            (200, "application/zip", pack),
        ]);
        responses
    })
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
    run_dbmd_options(cwd, args, hub, key, false)
}

fn run_dbmd_options(
    cwd: &Path,
    args: &[&str],
    hub: Option<&str>,
    key: Option<&str>,
    allow_private_registry: bool,
) -> Output {
    let mut cmd = Command::new(DBMD);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("DBMD_HUB_URL")
        .env_remove("DBMD_HUB_KEY")
        .env_remove("DBMD_AGENT_KEY_FILE")
        .env_remove("DBMD_BRAIN_KEY_FILE")
        .env_remove("DBMD_HUB_CREDENTIAL_ORIGIN")
        .env_remove("DBMD_ALLOW_PRIVATE_REGISTRY_HOME")
        .env("DBMD_STATE_DIR", cwd.join(".dbmd-test-state"));
    if let Some(h) = hub {
        cmd.env("DBMD_HUB_URL", h);
    }
    if let Some(k) = key {
        cmd.env("DBMD_HUB_KEY", k);
    }
    if allow_private_registry {
        cmd.env("DBMD_ALLOW_PRIVATE_REGISTRY_HOME", "1");
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
    let hub = MockHub::serve(vec![
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".dbmd")).unwrap();
    // The file points at a different production-shaped origin; --hub wins.
    std::fs::write(
        dir.path().join(".dbmd/config"),
        "# toolkit state\nhub = https://hub.example.invalid\n",
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

    // A store-selected hub cannot receive an ambient bearer without an
    // explicit origin binding. The refusal happens before dialing the dead
    // port, closing cloned-store credential exfiltration.
    let out = run_dbmd(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}"), "--json"],
        None,
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "UNBOUND_CREDENTIAL");
}

#[test]
fn cloned_store_cannot_redirect_an_ambient_bearer_to_its_hub() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".dbmd")).unwrap();
    std::fs::write(
        dir.path().join(".dbmd/config"),
        "hub = https://hub.example.invalid\n",
    )
    .unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}"), "--json"],
        None,
        Some("valuable-account-token"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "UNBOUND_CREDENTIAL");
    assert!(!out.stderr.contains("valuable-account-token"));
}

#[test]
fn cloned_store_cannot_use_an_ambient_brain_key_as_a_signing_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("brain.key");
    let generated = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            key_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(generated.code, Some(0), "stderr: {}", generated.stderr);

    std::fs::create_dir_all(dir.path().join(".dbmd")).unwrap();
    std::fs::write(
        dir.path().join(".dbmd/config"),
        "hub = https://hub.example.invalid\n",
    )
    .unwrap();
    let output = Command::new(DBMD)
        .args([
            "sync",
            &format!("@{BRAIN_ID}"),
            "--push",
            "--dir",
            dir.path().to_str().unwrap(),
            "--json",
        ])
        .current_dir(dir.path())
        .env_remove("DBMD_HUB_URL")
        .env_remove("DBMD_HUB_KEY")
        .env_remove("DBMD_AGENT_KEY_FILE")
        .env_remove("DBMD_HUB_CREDENTIAL_ORIGIN")
        .env("DBMD_BRAIN_KEY_FILE", &key_file)
        .env("DBMD_STATE_DIR", dir.path().join(".dbmd-test-state"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        error_code(&String::from_utf8_lossy(&output.stderr)),
        "UNBOUND_CREDENTIAL"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// resolve
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn resolve_bare_brain_gets_card_with_bearer() {
    let hub = MockHub::serve(vec![
        (200, signed_card_with_metadata()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme"],
        Some(&hub.url),
        Some("vc_account_test"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("slug: acme"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("feed seq: 41"));

    let reqs = hub.finish();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/api/hub/brains/acme");
    assert_eq!(
        reqs[0].header("authorization"),
        Some("Bearer vc_account_test")
    );
}

#[test]
fn resolve_ulid_target_queries_by_id_and_path_target_by_path() {
    let dir = tempfile::tempdir().unwrap();

    let hub = serve_exact_snapshot_hub(true);
    let by_id = run_dbmd(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}/{RECORD_ID}")],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(by_id.code, Some(0), "stderr: {}", by_id.stderr);
    assert!(by_id.stdout.contains("# Note"), "stdout: {}", by_id.stdout);
    let reqs = hub.finish();
    assert_eq!(reqs[0].path, format!("/api/hub/brains/{BRAIN_ID}/v2/head"));
    assert_eq!(reqs[1].path, format!("/api/hub/brains/{BRAIN_ID}"));
    assert!(
        reqs.iter()
            .all(|request| !request.path.contains("/resolve?")),
        "record resolution must not trust the unsigned query endpoint"
    );

    let hub = serve_exact_snapshot_hub(true);
    let by_path = run_dbmd(
        dir.path(),
        &["resolve", "@acme/records/note.md", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(by_path.code, Some(0), "stderr: {}", by_path.stderr);

    let reqs = hub.finish();
    assert_eq!(reqs[0].path, "/api/hub/brains/acme/v2/head");
    assert_eq!(reqs[1].path, "/api/hub/brains/acme");
    assert!(
        reqs.iter()
            .all(|request| !request.path.contains("/resolve?")),
        "record resolution must come from the signed pack"
    );
}

#[test]
fn hub_http_error_surfaces_hub_message_and_code() {
    // `@ghost` 404s on the direct lookup; the registry is then consulted and
    // also 404s, so the ORIGINAL direct error surfaces (not the registry miss).
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"Brain not found\"}".to_string()),
        (404, "{\"error\":\"Not found\"}".to_string()),
    ]);
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
fn sync_pull_materializes_files_and_reports() {
    let db = "---\ntype: db-md\nscope: company\nname: Acme\n---\n\n# Acme\n";
    let lumio = format!("---\ntype: client\nid: {RECORD_ID}\nsummary: Lumio\n---\n\n# Lumio\n");
    let (card, feed, export, _) =
        signed_inline_snapshot(&[("DB.md", db), ("records/clients/lumio.md", &lumio)]);
    let feed_hash = serde_json::from_str::<serde_json::Value>(&feed).unwrap()["feedHash"]
        .as_str()
        .unwrap()
        .to_string();
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"v2 unavailable\"}".to_string()),
        (200, card),
        (200, feed),
        (200, export),
    ]);
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
    assert_eq!(v["headSeq"], 1);

    // The files landed, byte-for-byte.
    assert!(dest.join("DB.md").is_file());
    let lumio = std::fs::read_to_string(dest.join("records/clients/lumio.md")).unwrap();
    assert!(lumio.contains("# Lumio"));
    // Derived catalogs are rebuilt separately. Running a second path-based
    // writer here would reopen the ancestor-swap race closed by sync itself.
    assert!(!dest.join("records/clients/index.md").exists());

    let reqs = hub.finish();
    assert_eq!(
        reqs[3].path,
        format!("/api/hub/brains/{BRAIN_ID}/export?format=pack&atSeq=1&feedHash={feed_hash}")
    );
}

#[test]
fn sync_pull_refuses_hostile_paths_with_nothing_written() {
    let (card, feed, export, _) = signed_inline_snapshot(&[
        ("DB.md", "---\ntype: db-md\n---\n# A\n"),
        ("../escape.md", "evil"),
    ]);
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"v2 unavailable\"}".to_string()),
        (200, card),
        (200, feed),
        (200, export),
    ]);
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

#[cfg(unix)]
#[test]
fn sync_pull_refuses_a_symlink_in_the_destination_ancestor_chain() {
    use std::os::unix::fs::symlink;

    let db = "---\ntype: db-md\nscope: company\nname: Acme\n---\n\n# Acme\n";
    let (card, feed, export, _) = signed_inline_snapshot(&[("DB.md", db)]);
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"v2 unavailable\"}".to_string()),
        (200, card),
        (200, feed),
        (200, export),
    ]);
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), work.path().join("redirect")).unwrap();
    let dest = work.path().join("redirect/pulled");

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
    assert!(!outside.path().join("pulled").exists());
    hub.finish();
}

#[cfg(unix)]
#[test]
fn sync_pull_conflict_leaves_every_live_file_unchanged() {
    let new_db = "---\ntype: db-md\nscope: company\nname: New\n---\n\n# New\n";
    let conflict = "---\ntype: note\nsummary: Conflict\n---\n\n# Remote\n";
    let (card, feed, export, _) =
        signed_inline_snapshot(&[("DB.md", new_db), ("records/conflict.md", conflict)]);
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"v2 unavailable\"}".to_string()),
        (200, card),
        (200, feed),
        (200, export),
    ]);
    let work = tempfile::tempdir().unwrap();
    let dest = work.path().join("pulled");
    std::fs::create_dir_all(dest.join("records/conflict.md")).unwrap();
    let old_db = "---\ntype: db-md\nscope: company\nname: Old\n---\n\n# Old\n";
    std::fs::write(dest.join("DB.md"), old_db).unwrap();
    std::fs::write(dest.join("records/conflict.md/sentinel"), "local").unwrap();

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
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(
        std::fs::read_to_string(dest.join("DB.md")).unwrap(),
        old_db,
        "a later conflict must not expose an earlier staged write"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("records/conflict.md/sentinel")).unwrap(),
        "local"
    );
    hub.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// sync — push
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sync_push_sends_owned_content_only_with_bearer() {
    let hub = MockHub::serve(vec![
        (404, "{\"error\":\"v2 unavailable\"}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
        (
            200,
            format!(
            "{{\"brain\":\"{BRAIN_ID}\",\"indexed\":{{\"documents\":2}},\"durable\":true,\"headSeq\":1}}"
        ),
        ),
    ]);
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
    assert_eq!(reqs[3].method, "POST");
    assert_eq!(reqs[3].path, format!("/api/hub/brains/{BRAIN_ID}/push"));
    assert_eq!(
        reqs[3].header("authorization"),
        Some("Bearer vc_account_test")
    );

    let body: serde_json::Value = serde_json::from_str(&reqs[3].body).unwrap();
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
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
        (
            201,
            format!(
                "{{\"id\":\"{grant_id}\",\"brain\":\"{BRAIN_ID}\",\"grantee\":{{\"email\":\"maya@example.com\"}},\"capability\":\"read\",\"scopePrefix\":\"records/clients/\",\"expiresAt\":\"2026-09-01T00:00:00.000Z\"}}"
            ),
        ),
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
        (
            200,
            format!(
                "{{\"brain\":\"{BRAIN_ID}\",\"grants\":[{{\"id\":\"{grant_id}\",\"email\":\"maya@example.com\",\"capability\":\"read\",\"scopePrefix\":\"records/clients/\"}}],\"invites\":[]}}"
            ),
        ),
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
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
    assert_eq!(reqs[3].method, "POST");
    assert_eq!(reqs[3].path, format!("/api/hub/brains/{BRAIN_ID}/grants"));
    let body: serde_json::Value = serde_json::from_str(&reqs[3].body).unwrap();
    assert_eq!(body["email"], "maya@example.com");
    assert_eq!(body["capability"], "read");
    assert_eq!(body["scopePrefix"], "records/clients/");
    assert_eq!(body["expiresAt"], "2026-09-01");
    assert_eq!(reqs[7].method, "GET");
    assert_eq!(reqs[11].method, "DELETE");
    assert_eq!(
        reqs[11].path,
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
fn propose_never_treats_a_json_redirect_as_success() {
    let hub = MockHub::serve(vec![(
        302,
        r#"{"id":"forged-success","location":"/elsewhere"}"#.to_string(),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "propose",
            "@acme-site",
            "--app",
            "intake",
            "--body",
            "evidence",
            "--json",
        ],
        Some(&hub.url),
        None,
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(error_code(&out.stderr), "HUB_ERROR");
    assert!(out.stderr.contains("302"), "stderr: {}", out.stderr);
    hub.finish();
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
    let hub = MockHub::serve(signed_head_responses());
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
    assert_eq!(v["feedHash"], SIGNED_HEAD_HASH);
    assert_eq!(v["verified"], true);
    let requests = hub.finish();
    assert_eq!(
        requests[2].path,
        format!("/api/hub/brains/{BRAIN_ID}/feed?after=40&limit=100")
    );
}

#[test]
fn subscribe_json_redirect_is_an_error_not_a_panic() {
    let hub = MockHub::serve(vec![
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (
            302,
            r#"{"location":"/api/hub/brains/other/feed"}"#.to_string(),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(error_code(&out.stderr), "HUB_ERROR");
    assert!(!out.stderr.contains("panicked"), "stderr: {}", out.stderr);
    hub.finish();
}

#[test]
fn store_local_trust_file_cannot_preplant_the_global_identity_pin() {
    let hub = MockHub::serve(signed_head_responses());
    let dir = tempfile::tempdir().unwrap();
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{BRAIN_ID}", hub.url).as_bytes())
    );
    let planted = dir.path().join(".dbmd/trust").join(format!("{key}.json"));
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, b"{\"attacker\":\"controls this store\"}\n").unwrap();

    let out = run_dbmd(
        dir.path(),
        &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(
        std::fs::read_to_string(planted).unwrap(),
        "{\"attacker\":\"controls this store\"}\n",
        "store-local attacker file must not be read or rewritten"
    );
    assert!(
        dir.path().join(".dbmd-test-state/trust").is_dir(),
        "the explicit user-state root owns the real checkpoint"
    );
    hub.finish();
}

#[cfg(unix)]
#[test]
fn global_trust_state_refuses_a_symlinked_root_without_writing_through_it() {
    use std::os::unix::fs::symlink;

    // The capability-safe trust root is opened before the first network
    // request, so an unsafe root must consume no scripted response.
    let hub = MockHub::serve(vec![]);
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), work.path().join(".dbmd-test-state")).unwrap();

    let out = run_dbmd(
        work.path(),
        &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "UNSAFE_PATH");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    hub.finish();
}

#[test]
fn persisted_checkpoint_rejects_feed_rollback_and_equivocation() {
    for hostile_card in [
        format!(
            "{{\"id\":\"{BRAIN_ID}\",\"headSeq\":40,\"feedHash\":\"{}\"}}",
            "a".repeat(64)
        ),
        format!(
            "{{\"id\":\"{BRAIN_ID}\",\"headSeq\":41,\"feedHash\":\"{}\"}}",
            "b".repeat(64)
        ),
    ] {
        let hub = MockHub::serve(vec![
            (404, "{}".to_string()),
            (200, SIGNED_HEAD_CARD.to_string()),
            (200, SIGNED_HEAD_FEED.to_string()),
            (404, "{}".to_string()),
            (200, hostile_card),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let first = run_dbmd(
            dir.path(),
            &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
            Some(&hub.url),
            Some("k"),
        );
        assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
        let hostile = run_dbmd(
            dir.path(),
            &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
            Some(&hub.url),
            Some("k"),
        );
        assert_eq!(hostile.code, Some(1));
        assert_eq!(error_code(&hostile.stderr), "INVALID_FEED");
        hub.finish();
    }
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
    let record = format!(
        "---\ntype: client\nid: {RECORD_ID}\nsummary: \"\\e[31mEVIL\\asummary\\nforged: yes\u{202e}\"\n---\n# Lumio\u{1b}[2J\u{7} ok\n"
    );
    let dir = tempfile::tempdir().unwrap();
    let addr = format!("@{BRAIN_ID}/{RECORD_ID}");

    let hub = serve_snapshot_hub(
        vec![
            (
                "DB.md".to_string(),
                "---\ntype: db-md\nscope: company\nname: Control test\n---\n".to_string(),
            ),
            ("records/clients/lumio.md".to_string(), record.clone()),
        ],
        true,
    );
    let text = run_dbmd(dir.path(), &["resolve", &addr], Some(&hub.url), Some("k"));
    assert_eq!(text.code, Some(0), "stderr: {}", text.stderr);
    assert!(
        text.stdout.contains(r"summary: EVILsummary\nforged: yes"),
        "stdout: {:?}",
        text.stdout
    );
    assert!(
        text.stdout.contains("# Lumio ok"),
        "stdout: {:?}",
        text.stdout
    );
    assert!(
        !text.stdout.contains('\u{1b}')
            && !text.stdout.contains('\u{7}')
            && !text.stdout.contains('\u{202e}')
            && !text.stdout.lines().any(|line| line == "forged: yes"),
        "text mode must strip control bytes: {:?}",
        text.stdout
    );
    hub.finish();

    let hub = serve_snapshot_hub(
        vec![
            (
                "DB.md".to_string(),
                "---\ntype: db-md\nscope: company\nname: Control test\n---\n".to_string(),
            ),
            ("records/clients/lumio.md".to_string(), record),
        ],
        true,
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
        v["document"]["summary"], "\u{1b}[31mEVIL\u{7}summary\nforged: yes\u{202e}",
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
    let hub = MockHub::serve(signed_head_responses());
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

// ─────────────────────────────────────────────────────────────────────────────
// link.md conformance vectors — cross-implementation (TS mints, Rust verifies)
// ─────────────────────────────────────────────────────────────────────────────

// TS-minted vectors from the link.md spec repo (carloslfu/link.md,
// `vectors/feed-v1.json`), generated by the hub's production signer. The
// reverse direction — the Rust-minted `SIGNED_HEAD_FEED` above verified by
// the hub — lives in the hub's suite. Together the two tests pin wire
// profile v1 across independent implementations in both directions.
const TS_VECTOR_BRAIN: &str = "01k0abcdefghjkmnpqrstvwxyz";
const TS_VECTOR_HEAD_HASH: &str =
    "50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a";
const TS_VECTOR_CARD: &str = r#"{"id":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","updatedAt":"2026-07-23T00:00:03.000Z"}"#;
const TS_VECTOR_FEED: &str = r#"{"brain":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","identity":{"fingerprint":"ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","publicKeySpki":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4"},"entries":[{"hash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","entry":{"v":1,"seq":3,"ts":"2026-07-23T00:00:03.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"edit","op":"snapshot","pack_sha256":"04b744b2038c45a40f921e5985c66e525c352c84eb4306de5784ff00526516c1","files":[],"removed":["records/note.md"],"prev_entry_hash":"f6571c54b7e19b80fce21f134a51ef62f5612b99dd4b537bd49f54dc87d81769","sig":"x4CTOMHWU7KhxldQZWGeoUMhXOnwMW0qsQsFB0mhHbWqyx0kHEnoT4SyzvkhDE6p47pbdW3bZBSuPptQHD5iCQ"}}],"nextAfter":3,"hasMore":false,"scopeLimited":false}"#;
const TS_VECTOR_FEED_TAMPERED: &str = r#"{"brain":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","identity":{"fingerprint":"ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","publicKeySpki":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4"},"entries":[{"hash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","entry":{"v":1,"seq":3,"ts":"2026-07-23T00:00:03.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"edit","op":"snapshot","pack_sha256":"3cfd41512bc835534bc479bb3158d5cab2e5d896fad7829fd0497d03e8334e18","files":[],"removed":["records/note.md"],"prev_entry_hash":"f6571c54b7e19b80fce21f134a51ef62f5612b99dd4b537bd49f54dc87d81769","sig":"x4CTOMHWU7KhxldQZWGeoUMhXOnwMW0qsQsFB0mhHbWqyx0kHEnoT4SyzvkhDE6p47pbdW3bZBSuPptQHD5iCQ"}}],"nextAfter":3,"hasMore":false,"scopeLimited":false}"#;

#[test]
fn subscribe_accepts_the_ts_minted_conformance_vector() {
    let hub = MockHub::serve(vec![
        (404, "{}".to_string()),
        (200, TS_VECTOR_CARD.to_string()),
        (200, TS_VECTOR_FEED.to_string()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "subscribe",
            &format!("@{TS_VECTOR_BRAIN}"),
            "--once",
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let line = out.stdout.lines().next().expect("one NDJSON line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["brain"], TS_VECTOR_BRAIN);
    assert_eq!(v["seq"], 3);
    assert_eq!(v["feedHash"], TS_VECTOR_HEAD_HASH);
    assert_eq!(v["verified"], true);
    hub.finish();
}

#[test]
fn subscribe_refuses_the_tampered_ts_minted_vector() {
    // Same head, one defect: `pack_sha256` altered after signing, so the
    // entry's bytes no longer match its advertised hash or its signature.
    let hub = MockHub::serve(vec![
        (404, "{}".to_string()),
        (200, TS_VECTOR_CARD.to_string()),
        (200, TS_VECTOR_FEED_TAMPERED.to_string()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "subscribe",
            &format!("@{TS_VECTOR_BRAIN}"),
            "--once",
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(error_code(&out.stderr), "INVALID_FEED");
    hub.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// Agent signing keys — `dbmd key generate` + LinkMD-Sig signed requests
// ─────────────────────────────────────────────────────────────────────────────

/// Like `run_dbmd`, but authenticating with an agent key file instead of a
/// bearer (link.md §8 — `DBMD_AGENT_KEY_FILE`).
fn run_dbmd_signed(cwd: &Path, args: &[&str], hub: &str, key_file: &Path) -> Output {
    let mut cmd = Command::new(DBMD);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("DBMD_HUB_URL")
        .env_remove("DBMD_HUB_KEY")
        .env("DBMD_HUB_URL", hub)
        .env("DBMD_AGENT_KEY_FILE", key_file)
        .env("DBMD_STATE_DIR", cwd.join(".dbmd-test-state"));
    let out = cmd.output().expect("spawn dbmd");
    Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn key_generate_mints_an_identity_writes_0600_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("agent.key");
    let out = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            key_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let multikey = v["multikey"].as_str().unwrap();
    // `ed25519:` + a 43-char base64url sha256 fingerprint.
    assert!(multikey.starts_with("ed25519:"), "multikey: {multikey}");
    assert_eq!(multikey.len(), 8 + 43, "multikey: {multikey}");
    assert!(v["publicKeySpki"].as_str().unwrap().len() > 40);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "key file mode: {mode:o}");
    }
    // Refuses to clobber an existing key.
    let again = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            key_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(again.code, Some(1));
    assert_eq!(error_code(&again.stderr), "BAD_AGENT_KEY");
}

#[test]
fn an_agent_key_signs_requests_instead_of_sending_a_bearer() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("agent.key");
    let gen = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            key_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(gen.code, Some(0), "stderr: {}", gen.stderr);
    let minted: serde_json::Value = serde_json::from_str(&gen.stdout).unwrap();
    let multikey = minted["multikey"].as_str().unwrap().to_string();

    let hub = MockHub::serve(vec![
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
    ]);
    let out = run_dbmd_signed(
        dir.path(),
        &["resolve", &format!("@{BRAIN_ID}"), "--json"],
        &hub.url,
        &key_file,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    let auth = requests[0]
        .header("authorization")
        .unwrap_or("")
        .to_string();
    assert!(
        auth.starts_with(&format!("LinkMD-Sig v2,key={multikey},ts=")),
        "authorization: {auth}"
    );
    assert!(
        !auth.contains("Bearer"),
        "authorization leaked a bearer: {auth}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Brain-addressed propose — the §7.4 generalization (link-md-ship E)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn propose_to_a_brain_id_uses_the_brain_inbox_and_optional_auth() {
    // Anonymous (no credential configured): the brain door, no auth header —
    // the public-brain open-door path.
    let hub = MockHub::serve(vec![(
        201,
        r#"{"id":"x","path":"sources/inbox/x.md"}"#.to_string(),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &[
            "propose",
            &format!("@{BRAIN_ID}"),
            "--app",
            "intake",
            "--body",
            "hello",
            "--json",
        ],
        Some(&hub.url),
        None,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    assert_eq!(
        requests[0].path,
        format!("/api/hub/brains/{BRAIN_ID}/inbox")
    );
    assert!(
        requests[0].header("authorization").is_none(),
        "anonymous propose must not invent a credential"
    );

    // With a bearer configured, Optional auth sends it (bigger actor-class
    // budget) — while a SITE-handle propose stays unauthenticated by design.
    let hub = MockHub::serve(vec![(
        201,
        r#"{"id":"x","path":"sources/inbox/x.md"}"#.to_string(),
    )]);
    let out = run_dbmd(
        dir.path(),
        &[
            "propose",
            &format!("@{BRAIN_ID}"),
            "--app",
            "intake",
            "--body",
            "hello",
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    assert_eq!(requests[0].header("authorization"), Some("Bearer k"));
}

// ─────────────────────────────────────────────────────────────────────────────
// mirror + serve — federation v0 (link-md-ship F)
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
const VEC_CARD: &str = r#"{"id":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","updatedAt":"2026-07-23T00:00:03.000Z"}"#;
#[allow(dead_code)]
const VEC_HEADPAGE: &str = r#"{"brain":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","identity":{"fingerprint":"ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","publicKeySpki":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4"},"entries":[{"hash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","entry":{"v":1,"seq":3,"ts":"2026-07-23T00:00:03.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"edit","op":"snapshot","pack_sha256":"04b744b2038c45a40f921e5985c66e525c352c84eb4306de5784ff00526516c1","files":[],"removed":["records/note.md"],"prev_entry_hash":"f6571c54b7e19b80fce21f134a51ef62f5612b99dd4b537bd49f54dc87d81769","sig":"x4CTOMHWU7KhxldQZWGeoUMhXOnwMW0qsQsFB0mhHbWqyx0kHEnoT4SyzvkhDE6p47pbdW3bZBSuPptQHD5iCQ"}}],"nextAfter":3,"hasMore":false,"scopeLimited":false}"#;
const VEC_FULLFEED: &str = r#"{"brain":"01k0abcdefghjkmnpqrstvwxyz","headSeq":3,"feedHash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","identity":{"fingerprint":"ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","publicKeySpki":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4"},"entries":[{"hash":"115d34fe8f8375fae7e82208d679e9031eaf092cdd2ab9aa1c0294e9b9d7abaf","entry":{"v":1,"seq":1,"ts":"2026-07-23T00:00:01.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"push","op":"snapshot","pack_sha256":"6e808470fef12de964a8c9a446c1d60f334e6262c4a84ab1721b265659506146","files":[{"path":"DB.md","sha256":"b5a507d2fc555b66c9a829fcf9a6e2e1e7f351c30af57d9b79d036d8a0bef560","bytes":11},{"path":"records/note.md","sha256":"d5cd8c4150ccdd6969f469a0297f9cc49b0b851ac801415ea842fce7b8ad7026","bytes":8}],"removed":[],"prev_entry_hash":null,"sig":"LuTyjtV3VdBvGp6sBTqHmNwd1ELP4cQx1MPBrZt-9Ec0TfnScBeAFBMtgY-af_2yDXl0zneycoK5oFXaG5OxDw"}},{"hash":"f6571c54b7e19b80fce21f134a51ef62f5612b99dd4b537bd49f54dc87d81769","entry":{"v":1,"seq":2,"ts":"2026-07-23T00:00:02.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"edit","op":"snapshot","pack_sha256":"bc32d2bfda48d1429731eeb598927281a9e99803c6da6cdac752a378a2ef57f5","files":[{"path":"records/note.md","sha256":"4e880fb6c5735c3ce2018a23557429f1ef7c0eb07e2dc0638e4bf955a6665d58","bytes":8}],"removed":[],"prev_entry_hash":"115d34fe8f8375fae7e82208d679e9031eaf092cdd2ab9aa1c0294e9b9d7abaf","sig":"3DaK7U-Ug2cBnQ5qb2G6NPyBDyjWNcH6-G3W1XH_g88M9C5GgqwDf3EsccoTDdgDpFFSQ6hHg61Fcg1R9Io0Ag"}},{"hash":"50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a","entry":{"v":1,"seq":3,"ts":"2026-07-23T00:00:03.000Z","brain":"ed25519:ytUalMZXa86de4qRDBYzlj1TrNnGHPSztfYhVoFfoMM","public_key":"MCowBQYDK2VwAyEAOCFVH30p3nNC7Xd1PMHEsyYJv2TXFFDun0rsBYHRah4","kind":"edit","op":"snapshot","pack_sha256":"04b744b2038c45a40f921e5985c66e525c352c84eb4306de5784ff00526516c1","files":[],"removed":["records/note.md"],"prev_entry_hash":"f6571c54b7e19b80fce21f134a51ef62f5612b99dd4b537bd49f54dc87d81769","sig":"x4CTOMHWU7KhxldQZWGeoUMhXOnwMW0qsQsFB0mhHbWqyx0kHEnoT4SyzvkhDE6p47pbdW3bZBSuPptQHD5iCQ"}}],"nextAfter":3,"hasMore":false,"scopeLimited":false}"#;
const VEC_EXPORT: &str = r#"{"brain":"01k0abcdefghjkmnpqrstvwxyz","slug":"vector-brain","headSeq":3,"files":[{"path":"DB.md","content":"---\ntype: db-md\n---\n\n# Vector\n"}]}"#;
const VEC_BRAIN: &str = "01k0abcdefghjkmnpqrstvwxyz";
const VEC_HASH_1: &str = "115d34fe8f8375fae7e82208d679e9031eaf092cdd2ab9aa1c0294e9b9d7abaf";
const VEC_HASH_3: &str = "50215474e01bb4698729fb1bab1befad430b95011a4d3fba35877591e8418d7a";

#[test]
fn mirror_verifies_the_whole_chain_stores_exact_bytes_and_pins() {
    let hub = serve_exact_snapshot_hub(false);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("mirror");
    let out = run_dbmd(
        dir.path(),
        &[
            "mirror",
            &format!("@{BRAIN_ID}"),
            "--dir",
            dest.to_str().unwrap(),
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(report["entries"], 1);
    assert_eq!(report["headSeq"], 1);
    assert_eq!(report["files"], 2);
    assert!(report["pinned"].as_str().unwrap().starts_with("ed25519:"));
    // Exact bytes stored: entry 1's file carries the signed serialization.
    let raw1 = std::fs::read_to_string(dest.join(".dbmd/mirror/feed/1.json")).unwrap();
    assert!(raw1.contains("\"seq\":1") && raw1.ends_with('\n'));
    let stored_hash = dbmd_core::linkmd::feed_entry_hash(raw1.trim_end());
    assert_eq!(stored_hash, report["feedHash"].as_str().unwrap());
    let pin = std::fs::read_to_string(dest.join(".dbmd/config")).unwrap();
    assert!(pin.contains("pin = ed25519:"), "config: {pin}");
    // Store file materialized by the pull.
    assert!(dest.join("DB.md").exists());
    hub.finish();
}

#[cfg(unix)]
#[test]
fn mirror_refuses_a_planted_legacy_backup_without_touching_it_or_dialing() {
    let work = tempfile::tempdir().unwrap();
    let dest = work.path().join("mirror");
    let backup = work.path().join(".mirror.dbmd-backup");
    std::fs::create_dir(&backup).unwrap();
    std::fs::write(backup.join("sentinel"), b"do not delete").unwrap();

    let out = run_dbmd(
        work.path(),
        &[
            "mirror",
            &format!("@{BRAIN_ID}"),
            "--dir",
            dest.to_str().unwrap(),
            "--json",
        ],
        Some("http://127.0.0.1:9"),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "UNSAFE_PATH");
    assert_eq!(
        std::fs::read(backup.join("sentinel")).unwrap(),
        b"do not delete"
    );
}

#[test]
fn serve_reserves_a_mirror_and_a_second_dbmd_reverifies_hub_independently() {
    // Mirror from the mock hub first.
    let hub = serve_exact_snapshot_hub(false);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("mirror");
    let mirrored = run_dbmd(
        dir.path(),
        &[
            "mirror",
            &format!("@{BRAIN_ID}"),
            "--dir",
            dest.to_str().unwrap(),
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(mirrored.code, Some(0), "stderr: {}", mirrored.stderr);
    let mirror_report: serde_json::Value = serde_json::from_str(&mirrored.stdout).unwrap();
    let pin = mirror_report["pinned"].as_str().unwrap().to_string();
    hub.finish();

    // Serve it, read the bound URL from the first stdout line.
    let mut child = std::process::Command::new(DBMD)
        .args([
            "serve",
            "--dir",
            dest.to_str().unwrap(),
            "--addr",
            "127.0.0.1:0",
            "--pin",
            pin.as_str(),
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn dbmd serve");
    let mut first = String::new();
    {
        use std::io::BufRead as _;
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        reader.read_line(&mut first).expect("serve announce line");
    }
    let announce: serde_json::Value = serde_json::from_str(first.trim()).expect("serve json line");
    let url = announce["serving"].as_str().unwrap().to_string();

    // A SECOND dbmd verifies the ORIGINAL signatures against the reference
    // node — no hub in the loop. Federation v0.
    let sub = run_dbmd(
        dir.path(),
        &["subscribe", &format!("@{BRAIN_ID}"), "--once", "--json"],
        Some(&url),
        Some("k"),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(sub.code, Some(0), "stderr: {}", sub.stderr);
    let line: serde_json::Value = serde_json::from_str(sub.stdout.lines().next().unwrap()).unwrap();
    assert_eq!(line["verified"], true);
    assert_eq!(line["seq"], 1);
    assert_eq!(line["brain"], BRAIN_ID);
    let stored1 = std::fs::read_to_string(dest.join(".dbmd/mirror/feed/1.json")).unwrap();
    assert_eq!(
        dbmd_core::linkmd::feed_entry_hash(stored1.trim_end()),
        line["feedHash"].as_str().unwrap(),
        "the first mirrored entry hashes to its vector hash"
    );
}

#[test]
fn grant_issue_detects_a_key_grantee_and_sends_key_spki() {
    // A base64url Ed25519 SPKI grantee (what `dbmd key generate` prints)
    // rides the keySpki axis; an email stays on the email axis.
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("agent.key");
    let gen = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            key_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(gen.code, Some(0), "stderr: {}", gen.stderr);
    let spki = serde_json::from_str::<serde_json::Value>(&gen.stdout).unwrap()["publicKeySpki"]
        .as_str()
        .unwrap()
        .to_string();

    let hub = MockHub::serve(vec![
        (404, "{}".to_string()),
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
        (201, r#"{"id":"g1"}"#.to_string()),
    ]);
    let out = run_dbmd(
        dir.path(),
        &[
            "grant",
            "issue",
            &format!("@{BRAIN_ID}"),
            &spki,
            "--can",
            "read",
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    let body: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    assert_eq!(body["keySpki"], spki);
    assert!(
        body.get("email").is_none(),
        "a key grantee must not be an email"
    );
}

#[test]
fn key_rotate_signs_with_the_old_key_and_writes_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let old_file = dir.path().join("old.key");
    let new_file = dir.path().join("new.key");
    // Mint the current brain key.
    let gen = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            old_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(gen.code, Some(0), "stderr: {}", gen.stderr);
    let old_multikey = serde_json::from_str::<serde_json::Value>(&gen.stdout).unwrap()["multikey"]
        .as_str()
        .unwrap()
        .to_string();
    let old_pkcs8 = URL_SAFE_NO_PAD
        .decode(std::fs::read_to_string(&old_file).unwrap().trim())
        .unwrap();
    let old_pair = ring::signature::Ed25519KeyPair::from_pkcs8(&old_pkcs8).unwrap();
    let (old_card, old_feed, verified_old_multikey) = signed_head_for_key(&old_pair);
    assert_eq!(verified_old_multikey, old_multikey);

    // Pre-mint the durable retry key so the scripted hub can independently
    // construct the valid post-rotation identity it will serve after the POST.
    let generated_new = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            new_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(
        generated_new.code,
        Some(0),
        "stderr: {}",
        generated_new.stderr
    );
    let new_public: serde_json::Value = serde_json::from_str(&generated_new.stdout).unwrap();
    let new_multikey = new_public["multikey"].as_str().unwrap().to_string();
    let new_spki = new_public["publicKeySpki"].as_str().unwrap().to_string();
    let old_spki = serde_json::from_str::<serde_json::Value>(&old_feed).unwrap()["identity"]
        ["publicKeySpki"]
        .as_str()
        .unwrap()
        .to_string();
    let old_hash = serde_json::from_str::<serde_json::Value>(&old_card).unwrap()["feedHash"]
        .as_str()
        .unwrap()
        .to_string();
    let postcondition_unsigned = serde_json::to_string(&TestUnsignedRotation {
        v: 1,
        op: "rotate",
        brain: &old_multikey,
        public_key: &old_spki,
        new_brain: &new_multikey,
        new_public_key: &new_spki,
        prior_head_seq: 1,
        prior_feed_hash: Some(&old_hash),
        ts: "2026-07-30T12:01:00.000Z",
    })
    .unwrap();
    let postcondition_sig =
        URL_SAFE_NO_PAD.encode(old_pair.sign(postcondition_unsigned.as_bytes()).as_ref());
    let postcondition_statement = format!(
        "{},\"sig\":\"{}\"}}",
        &postcondition_unsigned[..postcondition_unsigned.len() - 1],
        postcondition_sig
    );
    let mut rotated_feed: serde_json::Value = serde_json::from_str(&old_feed).unwrap();
    rotated_feed["identity"] = serde_json::json!({
        "fingerprint": new_multikey.trim_start_matches("ed25519:"),
        "publicKeySpki": new_spki,
        "previous": [{
            "fingerprint": old_multikey.trim_start_matches("ed25519:"),
            "publicKeySpki": old_spki,
        }],
        "rotations": [postcondition_statement],
    });

    // A 2xx is followed by a fresh card/feed verification; only the verified
    // new public key is authoritative.
    let hub = MockHub::serve(vec![
        (404, r#"{"error":"not v2"}"#.to_string()),
        (200, old_card.clone()),
        (200, old_feed),
        (200, r#"{"ok":true}"#.to_string()),
        (200, old_card),
        (200, rotated_feed.to_string()),
    ]);
    let out = run_dbmd(
        dir.path(),
        &[
            "key",
            "rotate",
            &format!("@{BRAIN_ID}"),
            "--key-file",
            old_file.to_str().unwrap(),
            "--out",
            new_file.to_str().unwrap(),
            "--json",
        ],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    assert_eq!(
        requests[3].path,
        format!("/api/hub/brains/{BRAIN_ID}/rotate")
    );
    // The statement's old side is the current key; it carries op=rotate + sig.
    let body: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    let statement_raw = body["statement"].as_str().unwrap().to_string();
    let stmt: serde_json::Value = serde_json::from_str(&statement_raw).unwrap();
    assert_eq!(stmt["op"], "rotate");
    assert_eq!(stmt["brain"], old_multikey);
    assert!(stmt["sig"].as_str().is_some());
    assert!(stmt["new_brain"].as_str().unwrap().starts_with("ed25519:"));
    assert_eq!(stmt["prior_head_seq"], 1);
    assert!(stmt["prior_feed_hash"].as_str().is_some());
    // The new key file exists and differs from the old.
    assert!(new_file.exists());
    assert_ne!(
        std::fs::read_to_string(&old_file).unwrap(),
        std::fs::read_to_string(&new_file).unwrap()
    );
    assert!(
        !Path::new(&format!("{}.rotation.json", new_file.display())).exists(),
        "a verified commit must remove the completed rotation journal"
    );
    // The report surfaces the rotated-from identity.
    let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(report["previous"][0], old_multikey);

    // Recovery/retry: the durable output key already exists and the hub now
    // serves it as current through the exact old-key-signed rotation. The
    // client must reconcile as success without generating/replacing a key or
    // POSTing another rotation.
    let durable_new = std::fs::read_to_string(&new_file).unwrap();
    let new_pkcs8 = URL_SAFE_NO_PAD.decode(durable_new.trim()).unwrap();
    let new_pair = ring::signature::Ed25519KeyPair::from_pkcs8(&new_pkcs8).unwrap();
    let mut new_spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    new_spki.extend_from_slice(new_pair.public_key().as_ref());
    let new_spki = URL_SAFE_NO_PAD.encode(&new_spki);
    let new_fingerprint =
        URL_SAFE_NO_PAD.encode(Sha256::digest(URL_SAFE_NO_PAD.decode(&new_spki).unwrap()));
    let (retry_card, retry_old_feed, _) = signed_head_for_key(&old_pair);
    let mut retry_feed: serde_json::Value = serde_json::from_str(&retry_old_feed).unwrap();
    retry_feed["identity"] = serde_json::json!({
        "fingerprint": new_fingerprint,
        "publicKeySpki": new_spki,
        "previous": [{
            "fingerprint": old_multikey.trim_start_matches("ed25519:"),
            "publicKeySpki": stmt["public_key"],
        }],
        "rotations": [statement_raw],
    });
    let retry_hub = MockHub::serve(vec![
        (404, r#"{"error":"not v2"}"#.to_string()),
        (200, retry_card),
        (200, retry_feed.to_string()),
    ]);
    let retry = run_dbmd(
        dir.path(),
        &[
            "key",
            "rotate",
            &format!("@{BRAIN_ID}"),
            "--key-file",
            old_file.to_str().unwrap(),
            "--out",
            new_file.to_str().unwrap(),
            "--json",
        ],
        Some(&retry_hub.url),
        Some("k"),
    );
    assert_eq!(retry.code, Some(0), "stderr: {}", retry.stderr);
    assert_eq!(std::fs::read_to_string(&new_file).unwrap(), durable_new);
    assert!(
        retry_hub
            .finish()
            .iter()
            .all(|request| request.method == "GET"),
        "already-current retry must not POST"
    );
}

#[test]
fn key_rotate_ambiguous_retry_reuses_the_byte_identical_statement() {
    let dir = tempfile::tempdir().unwrap();
    let old_file = dir.path().join("old.key");
    let new_file = dir.path().join("new.key");
    let old = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            old_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(old.code, Some(0), "stderr: {}", old.stderr);
    let new = run_dbmd(
        dir.path(),
        &[
            "key",
            "generate",
            "--out",
            new_file.to_str().unwrap(),
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(new.code, Some(0), "stderr: {}", new.stderr);
    let old_pkcs8 = URL_SAFE_NO_PAD
        .decode(std::fs::read_to_string(&old_file).unwrap().trim())
        .unwrap();
    let old_pair = ring::signature::Ed25519KeyPair::from_pkcs8(&old_pkcs8).unwrap();
    let (old_card, old_feed, _) = signed_head_for_key(&old_pair);
    let hub = MockHub::serve(vec![
        (404, r#"{"error":"not v2"}"#.to_string()),
        (200, old_card.clone()),
        (200, old_feed.clone()),
        (500, r#"{"error":"ambiguous"}"#.to_string()),
        (200, old_card.clone()),
        (200, old_feed.clone()),
        (404, r#"{"error":"not v2"}"#.to_string()),
        (200, old_card.clone()),
        (200, old_feed.clone()),
        (500, r#"{"error":"ambiguous"}"#.to_string()),
        (200, old_card),
        (200, old_feed),
    ]);
    let args = [
        "key",
        "rotate",
        &format!("@{BRAIN_ID}"),
        "--key-file",
        old_file.to_str().unwrap(),
        "--out",
        new_file.to_str().unwrap(),
        "--json",
    ];
    let first = run_dbmd(dir.path(), &args, Some(&hub.url), Some("k"));
    assert_eq!(first.code, Some(1), "stdout: {}", first.stdout);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = run_dbmd(dir.path(), &args, Some(&hub.url), Some("k"));
    assert_eq!(second.code, Some(1), "stdout: {}", second.stdout);

    let requests = hub.finish();
    assert_eq!(requests[3].method, "POST");
    assert_eq!(requests[9].method, "POST");
    assert_eq!(
        requests[3].body, requests[9].body,
        "the recovery key is insufficient: the exact timestamped, signed statement must be journaled"
    );
    let journal = format!("{}.rotation.json", new_file.display());
    assert!(Path::new(&journal).is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(journal).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Registry resolution — federation v0 phonebook (link-md-ship E5)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn registry_home_cannot_target_loopback_without_the_explicit_test_opt_in() {
    let reg = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (
            200,
            format!(
                r#"{{"handle":"acme-studio","home":"https://127.0.0.1:9","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#
            ),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&reg.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "INVALID_FEED");
    assert!(out.stderr.contains("non-public"));
    reg.finish();
}

#[test]
fn registry_home_redirect_is_not_followed_even_with_local_test_opt_in() {
    let home = MockHub::serve(vec![(
        302,
        r#"{"location":"http://127.0.0.1:1/private"}"#.to_string(),
    )]);
    let reg = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (
            200,
            format!(
                r#"{{"handle":"acme-studio","home":"{}","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#,
                home.url
            ),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&reg.url),
        Some("k"),
        true,
    );
    assert_eq!(out.code, Some(1));
    assert_eq!(error_code(&out.stderr), "HUB_ERROR");
    reg.finish();
    assert_eq!(home.finish().len(), 1);
}

#[test]
fn registry_tofu_rejects_rotation_beyond_seq_zero_without_persisting_trust() {
    let rng = ring::rand::SystemRandom::new();
    let old_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let old = ring::signature::Ed25519KeyPair::from_pkcs8(old_pkcs8.as_ref()).unwrap();
    let new_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let new = ring::signature::Ed25519KeyPair::from_pkcs8(new_pkcs8.as_ref()).unwrap();
    let public_identity = |pair: &ring::signature::Ed25519KeyPair| {
        let mut spki = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        spki.extend_from_slice(pair.public_key().as_ref());
        let public = URL_SAFE_NO_PAD.encode(&spki);
        let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&spki));
        (
            public,
            fingerprint.clone(),
            format!("ed25519:{fingerprint}"),
        )
    };
    let (old_spki, old_fingerprint, old_multikey) = public_identity(&old);
    let (new_spki, new_fingerprint, new_multikey) = public_identity(&new);
    let prior_hash = "a".repeat(64);
    let unsigned = serde_json::to_string(&TestUnsignedRotation {
        v: 1,
        op: "rotate",
        brain: &old_multikey,
        public_key: &old_spki,
        new_brain: &new_multikey,
        new_public_key: &new_spki,
        prior_head_seq: 1,
        prior_feed_hash: Some(&prior_hash),
        ts: "2026-07-30T12:01:00.000Z",
    })
    .unwrap();
    let signature = URL_SAFE_NO_PAD.encode(old.sign(unsigned.as_bytes()).as_ref());
    let statement = format!(
        "{},\"sig\":\"{}\"}}",
        &unsigned[..unsigned.len() - 1],
        signature
    );
    let home = MockHub::serve(vec![(
        200,
        serde_json::json!({
            "id": BRAIN_ID,
            "headSeq": 0,
            "feedHash": serde_json::Value::Null,
            "identity": {
                "fingerprint": new_fingerprint,
                "publicKeySpki": new_spki,
                "previous": [{
                    "fingerprint": old_fingerprint,
                    "publicKeySpki": old_spki,
                }],
                "rotations": [statement],
            },
        })
        .to_string(),
    )]);
    let registry = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (
            200,
            serde_json::json!({
                "handle": "acme-studio",
                "home": home.url,
                "brain": BRAIN_ID,
                "identity": {"fingerprint": new_fingerprint},
            })
            .to_string(),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&registry.url),
        Some("k"),
        true,
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(error_code(&out.stderr), "INVALID_FEED");
    assert!(
        out.stderr.contains("beyond the advertised head"),
        "stderr: {}",
        out.stderr
    );
    let trust = dir.path().join(".dbmd-test-state/trust");
    if trust.exists() {
        assert!(
            std::fs::read_dir(&trust)
                .unwrap()
                .flatten()
                .all(|entry| entry.path().extension().is_none_or(|ext| ext != "json")),
            "neither the handle alias nor canonical identity pin may persist after rejection"
        );
    }
    registry.finish();
    home.finish();
}

#[test]
fn resolve_follows_the_registry_to_a_foreign_home_and_pins_the_key() {
    // A SECOND node (the home) serves the card; the registry (the caller's hub)
    // points at it. `dbmd resolve @handle` follows and verifies the fingerprint.
    let home = MockHub::serve(vec![(
        200,
        format!(
            r#"{{"id":"{BRAIN_ID}","headSeq":0,"feedHash":null,"identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ","publicKeySpki":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU"}}}}"#
        ),
    )]);
    let reg = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (
            200,
            format!(
                r#"{{"handle":"acme-studio","home":"{}","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#,
                home.url
            ),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&reg.url),
        Some("k"),
        true,
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["id"], BRAIN_ID);
    assert_eq!(v["resolvedVia"], "registry");
    assert_eq!(v["home"], home.url);
    reg.finish();
    home.finish();
}

#[test]
fn registry_cannot_relocate_a_handle_after_first_verified_resolution() {
    let home = MockHub::serve(vec![(
        200,
        format!(
            r#"{{"id":"{BRAIN_ID}","headSeq":0,"feedHash":null,"identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ","publicKeySpki":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU"}}}}"#
        ),
    )]);
    let first_registry = format!(
        r#"{{"handle":"acme-studio","home":"{}","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#,
        home.url
    );
    let relocated_registry = format!(
        r#"{{"handle":"acme-studio","home":"http://127.0.0.1:9","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#
    );
    let registry = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (200, first_registry),
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (200, relocated_registry),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let first = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&registry.url),
        Some("k"),
        true,
    );
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    let second = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&registry.url),
        Some("k"),
        true,
    );
    assert_eq!(second.code, Some(1), "stdout: {}", second.stdout);
    assert_eq!(error_code(&second.stderr), "INVALID_FEED");
    assert!(
        second.stderr.contains("relocated"),
        "stderr: {}",
        second.stderr
    );
    registry.finish();
    home.finish();
}

#[test]
fn resolve_refuses_when_the_home_identity_does_not_match_the_registry() {
    // The home serves a DIFFERENT fingerprint than the registry advertised —
    // a rogue home, or a stale/rebound registry. The client must refuse.
    let home = MockHub::serve(vec![(
        200,
        format!(
            r#"{{"id":"{BRAIN_ID}","headSeq":0,"feedHash":null,"identity":{{"fingerprint":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","publicKeySpki":"MCowBQYDK2VwAyEAgJLl1ujKETgW6L9RU4sVvKsDOURNZpjy6KnffeIj4VU"}}}}"#
        ),
    )]);
    let reg = MockHub::serve(vec![
        (404, r#"{"error":"Brain not found"}"#.to_string()),
        (
            200,
            format!(
                r#"{{"handle":"acme-studio","home":"{}","brain":"{BRAIN_ID}","identity":{{"fingerprint":"plXvdIhBGCFUevYYhNO3LX-IEElGNZhgdUnaOIucWFQ"}}}}"#,
                home.url
            ),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd_options(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&reg.url),
        Some("k"),
        true,
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert_eq!(error_code(&out.stderr), "INVALID_FEED");
    reg.finish();
    home.finish();
}

#[test]
fn resolve_hits_direct_first_and_never_touches_the_registry_when_it_resolves() {
    // A hosted handle / the caller's own slug resolves directly — no registry
    // round-trip. The registry is only consulted on a direct 404.
    let hub = MockHub::serve(vec![
        {
            let mut card: serde_json::Value = serde_json::from_str(SIGNED_HEAD_CARD).unwrap();
            card["slug"] = serde_json::json!("acme-studio");
            (200, card.to_string())
        },
        (200, SIGNED_HEAD_CARD.to_string()),
        (200, SIGNED_HEAD_FEED.to_string()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_dbmd(
        dir.path(),
        &["resolve", "@acme-studio", "--json"],
        Some(&hub.url),
        Some("k"),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let requests = hub.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/api/hub/brains/acme-studio");
}
