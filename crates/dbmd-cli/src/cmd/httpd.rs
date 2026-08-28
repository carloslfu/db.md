// SPDX-License-Identifier: Apache-2.0

//! Minimal hardened HTTP/1.1 plumbing shared by the toolkit's two server
//! verbs — `serve` (the read-only federation node) and `api` (the local
//! read-write app surface). Zero dependencies beyond the standard library,
//! the `serve` doctrine.
//!
//! The threat model is a same-host peer misbehaving, not the internet:
//! bounded request heads, an optional bounded body, per-I/O idle timeouts
//! under absolute header/response deadlines (a trickling client cannot pin a
//! slot), and a hard concurrent-connection cap. Extracted verbatim from
//! `cmd/serve.rs` when `api` became its second consumer; `serve`'s
//! trickle-slot and deadline tests exercise these exact paths.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{CliError, ExitCode};

pub(crate) const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;
pub(crate) const MAX_CONCURRENT_CLIENTS: usize = 16;
const IO_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-connection timing: an idle timeout applied to every read/write,
/// under two absolute deadlines (header phase, whole response).
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) idle: Duration,
    pub(crate) header_total: Duration,
    pub(crate) response_total: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle: IO_IDLE_TIMEOUT,
            header_total: HEADER_TOTAL_TIMEOUT,
            response_total: RESPONSE_TOTAL_TIMEOUT,
        }
    }
}

/// A writer that re-arms the socket write timeout before every write so the
/// per-I/O idle limit AND the absolute response deadline both hold.
pub(crate) struct DeadlineWriter<'a> {
    pub(crate) stream: &'a mut TcpStream,
    pub(crate) deadline: Instant,
    pub(crate) idle: Duration,
}

impl DeadlineWriter<'_> {
    fn arm(&self) -> std::io::Result<()> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "absolute response deadline elapsed",
                )
            })?;
        self.stream
            .set_write_timeout(Some(self.idle.min(remaining)))
    }
}

impl Write for DeadlineWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.arm()?;
        self.stream.flush()
    }
}

/// One parsed request: the request line plus lowercased header names and an
/// optional bounded body.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    /// The first header with this (lowercase) name, if any.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Read and parse one request. Protocol violations are answered here (400 on
/// an oversized/unterminated head, 413 on an oversized body) and yield
/// `None`; a torn or non-UTF-8 head yields `None` silently (nothing sane to
/// answer). When `max_body` is 0 the body phase is skipped entirely — the
/// read-only `serve` contract, which processes the head and ignores any
/// body a client might send.
pub(crate) fn read_request(
    stream: &mut TcpStream,
    timing: Timing,
    max_body: u64,
) -> Option<Request> {
    let header_deadline = Instant::now() + timing.header_total;
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while head.len() < MAX_HTTP_HEAD_BYTES {
        let remaining = header_deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        stream
            .set_read_timeout(Some(timing.idle.min(remaining)))
            .ok()?;
        match stream.read(&mut byte) {
            Ok(1) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                    break;
                }
            }
            _ => return None,
        }
    }
    if head.len() == MAX_HTTP_HEAD_BYTES
        || !(head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n"))
    {
        let mut writer = DeadlineWriter {
            stream,
            deadline: Instant::now() + timing.response_total,
            idle: timing.idle,
        };
        respond(
            &mut writer,
            400,
            "{\"error\":\"request headers too large\"}",
        );
        return None;
    }
    let text = std::str::from_utf8(&head).ok()?;
    let mut lines = text.lines();
    let line = lines.next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let mut request = Request {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    if max_body == 0 {
        return Some(request);
    }
    let length: u64 = request
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length == 0 {
        return Some(request);
    }
    if length > max_body {
        let mut writer = DeadlineWriter {
            stream,
            deadline: Instant::now() + timing.response_total,
            idle: timing.idle,
        };
        respond(&mut writer, 413, "{\"error\":\"request body too large\"}");
        return None;
    }
    let body_deadline = Instant::now() + timing.response_total;
    let mut body =
        vec![0u8; usize::try_from(length).expect("length bounded by max_body, which fits usize")];
    let mut filled = 0usize;
    while filled < body.len() {
        let remaining = body_deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        stream
            .set_read_timeout(Some(timing.idle.min(remaining)))
            .ok()?;
        match stream.read(&mut body[filled..]) {
            Ok(0) => return None,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    request.body = body;
    Some(request)
}

/// Write one JSON response with the shared header shape.
pub(crate) fn respond<W: Write>(stream: &mut W, status: u16, body: &str) {
    respond_bytes(stream, status, "application/json", &[], body.as_bytes());
}

/// Write one response with an explicit content type, optional extra headers,
/// and a byte body. Every response closes the connection (the shared
/// one-request-per-connection contract).
pub(crate) fn respond_bytes<W: Write>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    extra_headers: &[(&str, String)],
    body: &[u8],
) {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        reason(status),
        body.len(),
    );
    for (name, value) in extra_headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body));
}

/// The reason phrase for the statuses the toolkit's servers emit.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Parse and bind a listen address, refusing a non-loopback bind unless the
/// caller explicitly allows one. `err_code` names the failing surface in the
/// structured error (`SERVE_BIND` / `API_BIND`).
pub(crate) fn bind_checked(
    addr: &str,
    err_code: &'static str,
    refusal_code: &'static str,
    allow_public: bool,
    public_hint: &str,
) -> Result<(TcpListener, SocketAddr), CliError> {
    let requested: SocketAddr = addr.parse().map_err(|e| {
        CliError::new(
            ExitCode::Runtime,
            err_code,
            format!("address must be an IP socket address: {e}"),
        )
    })?;
    if !requested.ip().is_loopback() && !allow_public {
        return Err(CliError::new(
            ExitCode::Runtime,
            refusal_code,
            "refusing unauthenticated non-loopback bind",
        )
        .with_hint(public_hint));
    }
    let listener = TcpListener::bind(requested).map_err(|e| {
        CliError::new(
            ExitCode::Runtime,
            err_code,
            format!("cannot bind {addr}: {e}"),
        )
    })?;
    let bound = listener
        .local_addr()
        .map_err(|e| CliError::new(ExitCode::Runtime, err_code, e.to_string()))?;
    Ok((listener, bound))
}

/// Run `handler` on its own thread, holding one of `max` connection slots;
/// over the cap the connection is answered 503 inline and dropped.
pub(crate) fn spawn_bounded(
    mut stream: TcpStream,
    active: Arc<AtomicUsize>,
    max: usize,
    handler: impl FnOnce(TcpStream) + Send + 'static,
) {
    if active.fetch_add(1, Ordering::AcqRel) >= max {
        active.fetch_sub(1, Ordering::AcqRel);
        respond(&mut stream, 503, "{\"error\":\"server busy\"}");
        return;
    }
    std::thread::spawn(move || {
        struct ActiveGuard(Arc<AtomicUsize>);
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _guard = ActiveGuard(active);
        handler(stream);
    });
}
