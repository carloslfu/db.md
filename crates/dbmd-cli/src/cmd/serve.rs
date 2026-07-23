// SPDX-License-Identifier: Apache-2.0

//! `dbmd serve` — the reference node: re-serve a mirrored brain read-only
//! over the hub HTTP binding (card, feed, export), zero dependencies beyond
//! the standard library. The entries served are the EXACT bytes `mirror`
//! verified and stored, so a downstream `dbmd subscribe`/`sync` re-verifies
//! the ORIGINAL signatures with no hub in the loop — federation v0: the
//! export is provable because signatures survive re-hosting.
//!
//! Loopback by default. No auth (you serve what you already hold); any
//! `authorization` header is ignored, which lets unmodified clients (whose
//! authenticated verbs always send a credential) speak to it unchanged.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::ServeArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

struct ServeState {
    brain: String,
    head_seq: u64,
    feed_hash: Option<String>,
    fingerprint: String,
    public_key_spki: String,
    /// (seq, exact entry JSON without trailing newline, sha256 hex of bytes).
    entries: Vec<(u64, String, String)>,
    /// (store-relative path, UTF-8 content) — the materialized store.
    files: Vec<(String, String)>,
}

fn load_state(dir: &Path) -> Result<ServeState, String> {
    let mirror = dir.join(dbmd_core::linkmd::MIRROR_REL_DIR);
    let head: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(mirror.join("head.json"))
            .map_err(|_| "no .dbmd/mirror/head.json — run `dbmd mirror` first".to_string())?,
    )
    .map_err(|e| format!("head.json did not parse: {e}"))?;
    let identity: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(mirror.join("identity.json"))
            .map_err(|_| "no .dbmd/mirror/identity.json".to_string())?,
    )
    .map_err(|e| format!("identity.json did not parse: {e}"))?;
    let brain = head["brain"].as_str().unwrap_or_default().to_string();
    let head_seq = head["headSeq"].as_u64().unwrap_or(0);
    let feed_hash = head["feedHash"].as_str().map(str::to_string);
    let fingerprint = identity["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let public_key_spki = identity["publicKeySpki"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if brain.is_empty() || fingerprint.is_empty() {
        return Err("mirror state is incomplete".to_string());
    }

    let mut entries = Vec::new();
    for seq in 1..=head_seq {
        let raw = std::fs::read_to_string(mirror.join("feed").join(format!("{seq}.json")))
            .map_err(|_| format!("feed entry {seq} is missing from the mirror"))?;
        let sans_newline = raw.trim_end_matches('\n').to_string();
        let hash = dbmd_core::linkmd::feed_entry_hash(&sans_newline);
        entries.push((seq, sans_newline, hash));
    }

    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ServeState {
        brain,
        head_seq,
        feed_hash,
        fingerprint,
        public_key_spki,
        entries,
        files,
    })
}

fn collect_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let reader = std::fs::read_dir(current).map_err(|e| e.to_string())?;
    for entry in reader.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // .dbmd (mirror state, pins) and .git never travel
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, content));
        }
    }
    Ok(())
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    };
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        )
        .as_bytes(),
    );
}

fn query_u64(query: &str, key: &str, fallback: u64) -> u64 {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn handle(state: &ServeState, path_and_query: &str, stream: &mut TcpStream) {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };
    let base = format!("/api/hub/brains/{}", state.brain);
    let identity = serde_json::json!({
        "fingerprint": state.fingerprint,
        "publicKeySpki": state.public_key_spki,
    });
    if path == base {
        let card = serde_json::json!({
            "id": state.brain,
            "headSeq": state.head_seq,
            "feedHash": state.feed_hash,
            "identity": identity,
            "updatedAt": serde_json::Value::Null,
            "servedBy": "dbmd-serve",
        });
        respond(stream, 200, &card.to_string());
        return;
    }
    if path == format!("{base}/feed") {
        let after = query_u64(query, "after", 0);
        let limit = query_u64(query, "limit", 100).clamp(1, 100);
        // Entries are the mirror's EXACT bytes, spliced raw into the envelope
        // so no re-serialization can disturb what was signed.
        let page: Vec<String> = state
            .entries
            .iter()
            .filter(|(seq, _, _)| *seq > after)
            .take(limit as usize)
            .map(|(_, raw, hash)| format!("{{\"hash\":\"{hash}\",\"entry\":{raw}}}"))
            .collect();
        let next_after = after + page.len() as u64;
        let body = format!(
            "{{\"brain\":\"{}\",\"headSeq\":{},\"feedHash\":{},\"identity\":{},\"entries\":[{}],\"nextAfter\":{},\"hasMore\":{},\"scopeLimited\":false}}",
            state.brain,
            state.head_seq,
            state
                .feed_hash
                .as_ref()
                .map(|h| format!("\"{h}\""))
                .unwrap_or_else(|| "null".to_string()),
            identity,
            page.join(","),
            next_after,
            next_after < state.head_seq,
        );
        respond(stream, 200, &body);
        return;
    }
    if path == format!("{base}/export") {
        let files: Vec<serde_json::Value> = state
            .files
            .iter()
            .map(|(p, c)| serde_json::json!({ "path": p, "content": c }))
            .collect();
        let body = serde_json::json!({
            "brain": state.brain,
            "slug": "mirror",
            "headSeq": state.head_seq,
            "files": files,
        });
        respond(stream, 200, &body.to_string());
        return;
    }
    respond(stream, 404, "{\"error\":\"not found\"}");
}

/// Run `dbmd serve`.
pub fn run(ctx: &Context, args: &ServeArgs) -> CliResult {
    let state = Arc::new(load_state(Path::new(&args.dir)).map_err(|message| {
        CliError::new(ExitCode::Runtime, "SERVE_STATE", message)
            .with_hint("run `dbmd mirror <brain> --dir <dir>` first")
    })?);
    let listener = TcpListener::bind(&args.addr).map_err(|e| {
        CliError::new(
            ExitCode::Runtime,
            "SERVE_BIND",
            format!("cannot bind {}: {e}", args.addr),
        )
    })?;
    let addr = listener
        .local_addr()
        .map_err(|e| CliError::new(ExitCode::Runtime, "SERVE_BIND", e.to_string()))?;
    let url = format!("http://{addr}");
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "serving": url, "brain": state.brain, "headSeq": state.head_seq })
        );
    } else {
        println!(
            "serving @{} at {url} (read-only: card, feed, export)",
            state.brain
        );
    }
    // The URL line must be readable by a parent process before we block.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("").to_string();
            // Drain headers; requests are bodyless GETs.
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim_end().is_empty() {
                    break;
                }
            }
            if method != "GET" {
                respond(&mut stream, 404, "{\"error\":\"not found\"}");
                return;
            }
            handle(&state, &target, &mut stream);
        });
    }
    Ok(())
}

// Used via dbmd_core; keep the path alive for grep-ability with the mirror.
#[allow(unused)]
type _MirrorDir = PathBuf;
