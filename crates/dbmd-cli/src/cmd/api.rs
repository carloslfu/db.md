// SPDX-License-Identifier: Apache-2.0

//! `dbmd api` — the local app API: the store's full local verb surface over
//! loopback HTTP.
//!
//! The design rule is ONE SEMANTICS: every route executes the corresponding
//! `dbmd` verb by spawning this very binary (`current_exe`) with `--json`,
//! child CWD at the store root, and passes the verb's stdout through
//! verbatim. The API therefore cannot drift from the CLI — flags, guards,
//! frozen-page policy, index write-through, and the store-wide transaction
//! flock all apply identically (the lock is cross-process by construction),
//! and a future verb fix reaches both surfaces at once. Exit codes map to
//! HTTP statuses (0→200, 1/2→400, 3→404, 5→409, 4→403, 6→422); the CLI's
//! structured `{"error":{…}}` stderr line rides as the error body, so a
//! client branches on `error.code` exactly like a CLI consumer.
//!
//! Transport is the shared hardened plumbing in `cmd/httpd.rs` (bounded
//! heads and bodies, idle timeouts under absolute deadlines, a concurrency
//! cap). Two long-lived streaming routes bypass the absolute response
//! deadline deliberately: `/v1/events` (the `watch` feed re-framed as
//! Server-Sent Events) and `/v1/emit?ndjson=1` (the whole-store dump,
//! line-streamed). CORS is wide open (`*`) — the server is loopback-only
//! and unauthenticated by design (the machine's own apps are one trust
//! domain), and there is deliberately no public-bind escape hatch: this
//! serves the folder, not the network. Cross-party verbs (sync, grant,
//! propose, keys, mirror) are NOT exposed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;

use dbmd_core::parser::MAX_DBMD_FILE_BYTES;

use crate::cli::ApiArgs;
use crate::cmd::httpd::{self, respond_bytes, DeadlineWriter, Timing, MAX_CONCURRENT_CLIENTS};
use crate::cmd::write::open_store;
use crate::context::Context;
use crate::error::CliResult;

/// The CORS grant every response carries: the server is loopback-only and
/// single-trust-domain, so any local page may call it.
fn cors_headers() -> Vec<(&'static str, String)> {
    vec![
        ("access-control-allow-origin", "*".to_string()),
        (
            "access-control-allow-methods",
            "GET, POST, PUT, DELETE, OPTIONS".to_string(),
        ),
        ("access-control-allow-headers", "content-type".to_string()),
    ]
}

/// Run `dbmd api`.
pub fn run(ctx: &Context, args: &ApiArgs) -> CliResult {
    // Fail fast on a non-store before binding anything, and pin the canonical
    // root every child process will run in.
    let store = open_store(&args.dir)?;
    let store_root = std::fs::canonicalize(&store.root).unwrap_or_else(|_| store.root.clone());
    drop(store);

    let (listener, addr) = httpd::bind_checked(
        &args.addr,
        "API_BIND",
        "API_PUBLIC_REFUSED",
        false,
        "the api is an unauthenticated read-write surface; it only binds loopback",
    )?;
    let url = format!("http://{addr}");
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({
                "serving": url,
                "store": store_root.to_string_lossy(),
                "routes": format!("{url}/v1"),
            })
        );
    } else {
        println!("serving the store's local API at {url} (routes: {url}/v1)");
    }
    // The URL line must be readable by a parent process before we block.
    let _ = std::io::stdout().flush();

    let exe = Arc::new(current_exe());
    let root = Arc::new(store_root);
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let exe = Arc::clone(&exe);
        let root = Arc::clone(&root);
        httpd::spawn_bounded(stream, Arc::clone(&active), MAX_CONCURRENT_CLIENTS, {
            move |s| handle_connection(s, &exe, &root)
        });
    }
    Ok(())
}

/// This binary's path, for self-exec; falls back to PATH lookup.
fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dbmd"))
}

fn handle_connection(mut stream: TcpStream, exe: &PathBuf, store_root: &PathBuf) {
    let timing = Timing::default();
    let Some(request) = httpd::read_request(&mut stream, timing, MAX_DBMD_FILE_BYTES) else {
        return;
    };

    // CORS preflight is answered before any routing.
    if request.method == "OPTIONS" {
        let mut writer = DeadlineWriter {
            stream: &mut stream,
            deadline: Instant::now() + timing.response_total,
            idle: timing.idle,
        };
        respond_bytes(&mut writer, 204, "text/plain", &cors_headers(), b"");
        return;
    }

    let (path, query) = match request.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (request.target.clone(), String::new()),
    };
    let params = parse_query(&query);

    // The two long-lived streams manage their own socket (no absolute
    // response deadline — they are explicitly open-ended).
    if request.method == "GET" && path == "/v1/events" {
        stream_events(stream, exe, store_root, &params);
        return;
    }
    if request.method == "GET" && path == "/v1/emit" && flag(&params, "ndjson") {
        stream_ndjson(stream, exe, store_root);
        return;
    }

    let mut writer = DeadlineWriter {
        stream: &mut stream,
        deadline: Instant::now() + timing.response_total,
        idle: timing.idle,
    };
    match route(&request.method, &path, &params, &request.body) {
        Ok(Route::Local {
            status,
            content_type,
            body,
        }) => {
            respond_bytes(&mut writer, status, content_type, &cors_headers(), &body);
        }
        Ok(Route::Exec { argv, text_output }) => {
            let outcome = exec_verb(exe, store_root, &argv, &request.body_file_content(&argv));
            emit_outcome(&mut writer, outcome, text_output);
        }
        Err(error) => {
            let body = serde_json::json!({
                "error": { "code": error.code, "message": error.message }
            })
            .to_string();
            respond_bytes(
                &mut writer,
                error.status,
                "application/json",
                &cors_headers(),
                body.as_bytes(),
            );
        }
    }
}

/// A routing failure the API layer itself produces (missing parameter,
/// unknown route) — same `{"error":{code,message}}` shape as CLI errors.
struct ApiError {
    status: u16,
    code: &'static str,
    message: String,
}

fn bad(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        code,
        message: message.into(),
    }
}

/// A resolved route: either answered locally, or an argv to execute.
enum Route {
    Local {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
    },
    Exec {
        argv: Vec<String>,
        /// The verb prints plain text, not JSON (`spec`, `extract`).
        text_output: bool,
    },
}

/// The request body rides to content-taking verbs through a temp file whose
/// placeholder path is already in `argv`; everything else ignores it.
trait BodyFileContent {
    fn body_file_content(&self, argv: &[String]) -> Option<Vec<u8>>;
}

impl BodyFileContent for httpd::Request {
    fn body_file_content(&self, argv: &[String]) -> Option<Vec<u8>> {
        argv.iter()
            .any(|a| a == BODY_FILE_PLACEHOLDER)
            .then(|| self.body.clone())
    }
}

/// Placeholder replaced with the temp-file path at exec time.
const BODY_FILE_PLACEHOLDER: &str = "\u{0}body-file\u{0}";

/// Map one request onto a verb invocation. Route names ARE verb names — the
/// parity rule made visible — and parameter names mirror the CLI flags.
fn route(
    method: &str,
    path: &str,
    params: &[(String, String)],
    body: &[u8],
) -> Result<Route, ApiError> {
    let segments: Vec<&str> = path
        .strip_prefix("/v1")
        .map(|rest| rest.trim_matches('/').split('/').collect())
        .unwrap_or_default();
    let seg: Vec<&str> = if segments == [""] { vec![] } else { segments };

    let one = |name: &str| -> Option<String> {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let need = |name: &str| -> Result<String, ApiError> {
        one(name).filter(|v| !v.is_empty()).ok_or_else(|| {
            bad(
                400,
                "MISSING_PARAM",
                format!("query parameter `{name}` is required"),
            )
        })
    };
    let path_arg = |name: &str| -> Result<String, ApiError> {
        let value = need(name)?;
        if value.starts_with('-') {
            return Err(bad(
                400,
                "BAD_PARAM",
                format!("`{name}` must not start with '-'"),
            ));
        }
        Ok(value)
    };
    let many = |name: &str| -> Vec<String> {
        params
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    // Optional flags copied through verbatim when present: `--<name> <value>`.
    let opt_flags = |argv: &mut Vec<String>, names: &[&str]| {
        for name in names {
            if let Some(value) = one(name) {
                argv.push(format!("--{name}"));
                argv.push(value);
            }
        }
    };

    let json = |argv: Vec<String>| {
        Ok(Route::Exec {
            argv,
            text_output: false,
        })
    };
    let text = |argv: Vec<String>| {
        Ok(Route::Exec {
            argv,
            text_output: true,
        })
    };
    let j = |s: &str| s.to_string();

    match (method, seg.as_slice()) {
        // ── discovery ────────────────────────────────────────────────────
        ("GET", []) => Ok(Route::Local {
            status: 200,
            content_type: "application/json",
            body: routes_index(),
        }),
        ("GET", ["version"]) => Ok(Route::Local {
            status: 200,
            content_type: "application/json",
            body: serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })
                .to_string()
                .into_bytes(),
        }),
        ("GET", ["spec"]) => text(vec![j("spec")]),

        // ── reads ────────────────────────────────────────────────────────
        ("GET", ["show"]) => json(vec![j("--json"), j("show"), path_arg("file")?]),
        ("GET", ["query"]) => {
            let mut argv = vec![j("--json"), j("query")];
            opt_flags(
                &mut argv,
                &[
                    "type",
                    "in",
                    "limit",
                    "updated-after",
                    "updated-before",
                    "created-after",
                    "created-before",
                ],
            );
            for clause in many("where") {
                argv.push(j("--where"));
                argv.push(clause);
            }
            json(argv)
        }
        ("GET", ["search"]) => {
            let mut argv = vec![j("--json"), j("search"), need("q")?];
            opt_flags(
                &mut argv,
                &[
                    "type",
                    "in",
                    "linked-from",
                    "linked-to",
                    "updated-after",
                    "updated-before",
                    "created-after",
                    "created-before",
                ],
            );
            for clause in many("where") {
                argv.push(j("--where"));
                argv.push(clause);
            }
            json(argv)
        }
        ("GET", ["schema"]) => {
            let mut argv = vec![j("--json"), j("schema")];
            if let Some(type_) = one("type") {
                argv.push(type_);
            }
            json(argv)
        }
        ("GET", ["sections"]) => json(vec![j("--json"), j("sections"), path_arg("file")?]),
        ("GET", ["outline"]) => json(vec![j("--json"), j("outline"), path_arg("file")?]),
        ("GET", ["section"]) => json(vec![
            j("--json"),
            j("section"),
            j("get"),
            path_arg("file")?,
            need("heading")?,
        ]),
        ("GET", ["fm"]) => json(vec![
            j("--json"),
            j("fm"),
            j("get"),
            path_arg("file")?,
            need("key")?,
        ]),
        ("GET", ["graph", kind @ ("backlinks" | "forwardlinks" | "neighborhood")]) => {
            let mut argv = vec![j("--json"), j("graph"), j(kind), path_arg("path")?];
            opt_flags(&mut argv, &["hops", "limit", "type", "in"]);
            json(argv)
        }
        ("GET", ["graph", "orphans"]) => {
            let mut argv = vec![j("--json"), j("graph"), j("orphans")];
            opt_flags(&mut argv, &["limit", "type", "in"]);
            json(argv)
        }
        ("GET", ["tree"]) => {
            let mut argv = vec![j("--json"), j("tree")];
            opt_flags(&mut argv, &["layer", "type"]);
            json(argv)
        }
        ("GET", ["stats"]) => json(vec![j("--json"), j("stats")]),
        ("GET", ["validate"]) => {
            let mut argv = vec![j("--json"), j("validate")];
            if flag(params, "all") {
                argv.push(j("--all"));
            }
            json(argv)
        }
        ("GET", ["emit"]) => json(vec![j("--json"), j("emit")]),
        ("GET", ["index"]) => {
            let mut argv = vec![j("--json"), j("index"), j("show")];
            if let Some(path) = one("path") {
                argv.push(path);
            }
            json(argv)
        }
        ("GET", ["log", "tail"]) => {
            let mut argv = vec![j("log"), j("tail")];
            if let Some(n) = one("n") {
                argv.push(n);
            }
            argv.push(j("--json"));
            json(argv)
        }
        ("GET", ["log", "since"]) => json(vec![j("log"), j("since"), need("ts")?, j("--json")]),
        ("GET", ["assets", sub @ ("verify" | "status" | "paths")]) => {
            json(vec![j("--json"), j("assets"), j(sub)])
        }
        ("GET", ["extract"]) => text(vec![j("extract"), path_arg("file")?]),

        // ── writes ───────────────────────────────────────────────────────
        ("POST", ["write"]) => {
            let spec: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| bad(400, "BAD_JSON", format!("write body must be JSON: {e}")))?;
            let path = spec["path"]
                .as_str()
                .filter(|p| !p.is_empty() && !p.starts_with('-'))
                .ok_or_else(|| bad(400, "MISSING_PARAM", "write body needs a `path` string"))?;
            let type_ = spec["type"]
                .as_str()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| bad(400, "MISSING_PARAM", "write body needs a `type` string"))?;
            let mut argv = vec![j("--json"), j("write"), j(path), j("--type"), j(type_)];
            if let Some(summary) = spec["summary"].as_str() {
                argv.push(j("--summary"));
                argv.push(j(summary));
            }
            if let Some(fm) = spec["fm"].as_object() {
                for (key, value) in fm {
                    let value = match value {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    argv.push(j("--fm"));
                    argv.push(format!("{key}={value}"));
                }
            }
            if let Some(record_body) = spec["body"].as_str() {
                argv.push(j("--body-file"));
                argv.push(j(BODY_FILE_PLACEHOLDER));
                // The record body replaces the HTTP body as the temp-file
                // content: rebuild the request around it.
                return Ok(Route::Exec {
                    argv: argv
                        .into_iter()
                        .chain(std::iter::once(format!("{PAYLOAD_MARKER}{record_body}")))
                        .collect(),
                    text_output: false,
                });
            }
            json(argv)
        }
        ("PUT", ["fm"]) => {
            let value = utf8_body(body)?;
            json(vec![
                j("--json"),
                j("fm"),
                j("set"),
                path_arg("file")?,
                format!("{}={value}", need("key")?),
            ])
        }
        ("POST", ["fm", "init"]) => {
            let mut argv = vec![j("--json"), j("fm"), j("init"), path_arg("file")?];
            opt_flags(&mut argv, &["summary"]);
            json(argv)
        }
        ("PUT", ["body"]) => json(vec![
            j("--json"),
            j("body"),
            j("set"),
            path_arg("file")?,
            j("--body-file"),
            j(BODY_FILE_PLACEHOLDER),
        ]),
        ("POST", ["body", "append"]) => json(vec![
            j("--json"),
            j("body"),
            j("append"),
            path_arg("file")?,
            j("--body-file"),
            j(BODY_FILE_PLACEHOLDER),
        ]),
        ("PUT", ["section"]) | ("POST", ["section", "append"]) => {
            let sub = if method == "PUT" { "set" } else { "append" };
            let mut argv = vec![
                j("--json"),
                j("section"),
                j(sub),
                path_arg("file")?,
                need("heading")?,
                j("--body-file"),
                j(BODY_FILE_PLACEHOLDER),
            ];
            if flag(params, "create") {
                argv.push(j("--create"));
            }
            opt_flags(&mut argv, &["level"]);
            json(argv)
        }
        ("POST", ["link"]) => json(vec![
            j("--json"),
            j("link"),
            path_arg("from")?,
            path_arg("to")?,
        ]),
        ("POST", ["rename"]) => json(vec![
            j("--json"),
            j("rename"),
            path_arg("from")?,
            path_arg("to")?,
        ]),
        ("DELETE", ["rm"]) => {
            let mut argv = vec![j("--json"), j("rm"), path_arg("path")?];
            if flag(params, "force") {
                argv.push(j("--force"));
            }
            json(argv)
        }
        ("POST", ["format"]) => json(vec![j("--json"), j("format"), path_arg("file")?]),
        ("POST", ["index", "rebuild"]) => {
            let mut argv = vec![j("--json"), j("index"), j("rebuild")];
            opt_flags(&mut argv, &["layer", "folder"]);
            json(argv)
        }
        ("POST", ["log"]) => {
            let mut argv = vec![
                j("log"),
                need("kind")?,
                one("object").unwrap_or_else(|| j("-")),
            ];
            if let Some(note) = one("m") {
                argv.push(j("-m"));
                argv.push(note);
            }
            argv.push(j("--json"));
            json(argv)
        }
        ("POST", ["assets", "scan"]) => json(vec![j("--json"), j("assets"), j("scan")]),
        ("POST", ["assets", "refresh"]) => json(vec![
            j("--json"),
            j("assets"),
            j("refresh"),
            path_arg("path")?,
        ]),

        // ── misses ───────────────────────────────────────────────────────
        (_, []) | (_, _) if !path.starts_with("/v1") => Err(bad(
            404,
            "UNKNOWN_ROUTE",
            "routes live under /v1 (GET /v1 lists them)",
        )),
        _ => {
            if known_path(&seg) {
                Err(bad(
                    405,
                    "METHOD_NOT_ALLOWED",
                    format!("`{method} {path}` — see GET /v1"),
                ))
            } else {
                Err(bad(404, "UNKNOWN_ROUTE", format!("`{path}` — see GET /v1")))
            }
        }
    }
}

/// Marker prefixing an inline payload smuggled through the argv vector (the
/// `write` route's record body); stripped before exec.
const PAYLOAD_MARKER: &str = "\u{0}payload\u{0}";

/// Whether the path names a known route (so a wrong method gets 405, not 404).
fn known_path(seg: &[&str]) -> bool {
    matches!(
        seg,
        ["show"]
            | ["query"]
            | ["search"]
            | ["schema"]
            | ["sections"]
            | ["outline"]
            | ["section"]
            | ["section", "append"]
            | ["fm"]
            | ["fm", "init"]
            | ["graph", _]
            | ["tree"]
            | ["stats"]
            | ["validate"]
            | ["emit"]
            | ["index"]
            | ["index", "rebuild"]
            | ["log"]
            | ["log", "tail"]
            | ["log", "since"]
            | ["assets", _]
            | ["extract"]
            | ["write"]
            | ["body"]
            | ["body", "append"]
            | ["link"]
            | ["rename"]
            | ["rm"]
            | ["format"]
            | ["events"]
            | ["spec"]
            | ["version"]
    )
}

/// The `GET /v1` discovery document.
fn routes_index() -> Vec<u8> {
    serde_json::json!({
        "dbmd": "api",
        "version": env!("CARGO_PKG_VERSION"),
        "contract": "every route executes the same-named dbmd verb with --json; bodies are passed through verbatim; exit codes map 0→200, 1/2→400, 3→404, 4→403, 5→409, 6→422",
        "routes": {
            "reads": [
                "GET /v1/show?file=", "GET /v1/query?type=&where=k=v&in=&limit=",
                "GET /v1/search?q=&type=&where=", "GET /v1/schema?type=",
                "GET /v1/sections?file=", "GET /v1/outline?file=",
                "GET /v1/section?file=&heading=", "GET /v1/fm?file=&key=",
                "GET /v1/graph/{backlinks|forwardlinks|neighborhood|orphans}?path=&hops=",
                "GET /v1/tree?layer=&type=", "GET /v1/stats",
                "GET /v1/validate?all=1", "GET /v1/emit[?ndjson=1]",
                "GET /v1/index?path=", "GET /v1/log/tail?n=", "GET /v1/log/since?ts=",
                "GET /v1/assets/{verify|status|paths}", "GET /v1/extract?file=",
                "GET /v1/events?path=&interval=  (SSE watch feed)",
                "GET /v1/spec", "GET /v1/version"
            ],
            "writes": [
                "POST /v1/write  {path,type,summary?,fm?,body?}",
                "PUT /v1/fm?file=&key=  (raw value body)",
                "POST /v1/fm/init?file=&summary=",
                "PUT /v1/body?file=  (raw body)", "POST /v1/body/append?file=",
                "PUT /v1/section?file=&heading=&create=1&level=  (raw body)",
                "POST /v1/section/append?file=&heading=",
                "POST /v1/link?from=&to=", "POST /v1/rename?from=&to=",
                "DELETE /v1/rm?path=&force=1", "POST /v1/format?file=",
                "POST /v1/index/rebuild?layer=&folder=",
                "POST /v1/log?kind=&object=&m=",
                "POST /v1/assets/{scan|refresh?path=}"
            ]
        }
    })
    .to_string()
    .into_bytes()
}

/// One finished verb execution.
struct Outcome {
    status: u16,
    body: Vec<u8>,
    stderr_note: Option<String>,
}

/// Execute one verb: temp-file the content payload if the argv carries the
/// placeholder, spawn this binary at the store root, map the exit code.
fn exec_verb(
    exe: &PathBuf,
    store_root: &PathBuf,
    argv: &[String],
    content: &Option<Vec<u8>>,
) -> Outcome {
    // Materialize the content payload (HTTP body, or the `write` route's
    // inline record body) into a private temp file the child reads.
    let mut payload: Option<Vec<u8>> = content.clone();
    let mut resolved: Vec<String> = Vec::with_capacity(argv.len());
    for arg in argv {
        if let Some(inline) = arg.strip_prefix(PAYLOAD_MARKER) {
            payload = Some(inline.as_bytes().to_vec());
            continue;
        }
        resolved.push(arg.clone());
    }
    let temp = if resolved.iter().any(|a| a == BODY_FILE_PLACEHOLDER) {
        let path = std::env::temp_dir().join(format!(
            "dbmd-api-{}-{}.body",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        ));
        if let Err(error) = std::fs::write(&path, payload.as_deref().unwrap_or_default()) {
            return Outcome {
                status: 500,
                body: api_error_body("EXEC_FAILED", &format!("cannot stage content: {error}")),
                stderr_note: None,
            };
        }
        for arg in &mut resolved {
            if arg == BODY_FILE_PLACEHOLDER {
                *arg = path.to_string_lossy().into_owned();
            }
        }
        Some(path)
    } else {
        None
    };
    struct TempGuard(Option<PathBuf>);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let _guard = TempGuard(temp);

    let output = std::process::Command::new(exe)
        .args(&resolved)
        .current_dir(store_root)
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Outcome {
                status: 500,
                body: api_error_body("EXEC_FAILED", &format!("cannot run dbmd: {error}")),
                stderr_note: None,
            };
        }
    };
    let status = match output.status.code() {
        Some(0) => 200,
        Some(1) | Some(2) => 400,
        Some(3) => 404,
        Some(4) => 403,
        Some(5) => 409,
        Some(6) => 422,
        _ => 500,
    };
    // The useful payload is stdout when present (success output; validate's
    // issue report on exit 6), else the structured stderr error line.
    let body = if output.stdout.is_empty() {
        if output.stderr.is_empty() {
            api_error_body("EXEC_FAILED", "the verb produced no output")
        } else {
            output.stderr.clone()
        }
    } else {
        output.stdout
    };
    let stderr_note = (status == 200 && !output.stderr.is_empty())
        .then(|| String::from_utf8_lossy(&output.stderr).into_owned());
    Outcome {
        status,
        body,
        stderr_note,
    }
}

fn api_error_body(code: &str, message: &str) -> Vec<u8> {
    serde_json::json!({ "error": { "code": code, "message": message } })
        .to_string()
        .into_bytes()
}

/// Write one finished outcome, CORS + optional percent-encoded warning
/// header attached (index write-through warnings on success would otherwise
/// be lost to HTTP consumers).
fn emit_outcome<W: Write>(writer: &mut W, outcome: Outcome, text_output: bool) {
    let mut headers = cors_headers();
    if let Some(note) = &outcome.stderr_note {
        headers.push(("x-dbmd-stderr", percent_encode(note.trim())));
    }
    let content_type = if text_output {
        "text/plain; charset=utf-8"
    } else {
        "application/json"
    };
    respond_bytes(
        writer,
        outcome.status,
        content_type,
        &headers,
        &outcome.body,
    );
}

/// `/v1/events` — the `watch` feed as Server-Sent Events: one `data:` frame
/// per NDJSON event line, streamed until either side closes.
fn stream_events(
    mut stream: TcpStream,
    exe: &PathBuf,
    store_root: &PathBuf,
    params: &[(String, String)],
) {
    let mut argv: Vec<String> = vec!["--json".into(), "watch".into()];
    for name in ["path", "interval"] {
        if let Some(value) = params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
        {
            argv.push(format!("--{name}"));
            argv.push(value);
        }
    }
    let child = std::process::Command::new(exe)
        .args(&argv)
        .current_dir(store_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let mut writer = DeadlineWriter {
                stream: &mut stream,
                deadline: Instant::now() + Timing::default().response_total,
                idle: Timing::default().idle,
            };
            respond_bytes(
                &mut writer,
                500,
                "application/json",
                &cors_headers(),
                &api_error_body("EXEC_FAILED", &format!("cannot start watch: {error}")),
            );
            return;
        }
    };
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stdout = child.stdout.take();
    let _guard = ChildGuard(child);
    let Some(stdout) = stdout else { return };

    // Long-lived stream: idle write timeout only, no absolute deadline, no
    // content-length — the message ends when the connection closes.
    let _ = stream.set_write_timeout(Some(Timing::default().idle));
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nconnection: close\r\n",
    );
    for (name, value) in cors_headers() {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(&value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if stream
            .write_all(format!("data: {line}\n\n").as_bytes())
            .and_then(|()| stream.flush())
            .is_err()
        {
            break; // client gone; the guard kills the watcher
        }
    }
}

/// `/v1/emit?ndjson=1` — the whole-store dump, streamed line-by-line so
/// neither the server nor the client ever holds it in memory.
fn stream_ndjson(mut stream: TcpStream, exe: &PathBuf, store_root: &PathBuf) {
    let child = std::process::Command::new(exe)
        .args(["--json", "emit", "--ndjson"])
        .current_dir(store_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let _ = stream.set_write_timeout(Some(Timing::default().idle));
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n",
    );
    for (name, value) in cors_headers() {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(&value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_ok() {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if stream.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Percent+plus decoding for query values.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes.get(i + 1..i + 3);
                match hex.and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a header value conservatively (RFC 3986 unreserved plus
/// space→%20), so multi-line or non-ASCII stderr rides safely in a header.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A boolean query parameter: present as `1` or `true`.
fn flag(params: &[(String, String)], name: &str) -> bool {
    params
        .iter()
        .any(|(k, v)| k == name && (v == "1" || v == "true"))
}

/// The request body as UTF-8, or a 400.
fn utf8_body(body: &[u8]) -> Result<String, ApiError> {
    String::from_utf8(body.to_vec())
        .map_err(|_| bad(400, "CONTENT_NOT_UTF8", "request body must be UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_plus_and_hex() {
        assert_eq!(percent_decode("a+b%20c%3Dd"), "a b c=d");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn query_parsing_keeps_repeats_and_embedded_equals() {
        let params = parse_query("where=status%3Dactive&where=a=b&type=todo");
        assert_eq!(
            params,
            vec![
                ("where".to_string(), "status=active".to_string()),
                ("where".to_string(), "a=b".to_string()),
                ("type".to_string(), "todo".to_string()),
            ]
        );
    }

    #[test]
    fn routes_refuse_hyphen_leading_paths_and_missing_params() {
        let err = route("GET", "/v1/show", &parse_query("file=-x.md"), &[]).err();
        assert!(matches!(err, Some(e) if e.code == "BAD_PARAM"));
        let err = route("GET", "/v1/show", &[], &[]).err();
        assert!(matches!(err, Some(e) if e.code == "MISSING_PARAM"));
    }

    #[test]
    fn unknown_routes_and_wrong_methods_are_distinct() {
        let err = route("GET", "/v1/nope", &[], &[]).err().unwrap();
        assert_eq!(err.status, 404);
        let err = route("PUT", "/v1/show", &[], &[]).err().unwrap();
        assert_eq!(err.status, 405);
    }

    #[test]
    fn write_route_builds_the_full_argv() {
        let body = serde_json::json!({
            "path": "records/widgets/a.md",
            "type": "widget",
            "summary": "A",
            "fm": { "status": "active" },
        })
        .to_string();
        let Ok(Route::Exec { argv, .. }) = route("POST", "/v1/write", &[], body.as_bytes()) else {
            panic!("write must route to exec");
        };
        assert_eq!(
            argv,
            vec![
                "--json",
                "write",
                "records/widgets/a.md",
                "--type",
                "widget",
                "--summary",
                "A",
                "--fm",
                "status=active",
            ]
        );
    }
}
