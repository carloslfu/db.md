// SPDX-License-Identifier: Apache-2.0

//! The **link.md client** — the five interconnect verbs `dbmd` speaks against
//! a hub: `resolve`, `sync`, `grant`, `propose`, `subscribe`.
//!
//! One binary, two specs (the git precedent: one binary carries both the
//! object format and the wire protocol). The db.md FORMAT is untouched by this
//! module: a store never needs link.md to be valid db.md, record files stay
//! plain markdown, and SPEC.md reserves only the `@brain/id` address *shape*.
//! Everything with a wire or a trust boundary — addressing across stores,
//! pulling/pushing a hosted copy, capability grants, the propose door, feed
//! polling — lives here, as a *client capability*, never a format requirement.
//!
//! # What this client speaks
//!
//! The v0 HTTP binding a hub serves under its base URL:
//!
//! | verb | binding |
//! | --- | --- |
//! | `resolve` | `GET /api/hub/brains/<brain>` (the brain card) and `GET /api/hub/brains/<brain>/resolve?id=…` / `?path=…` (a record) |
//! | `sync` (pull) | `GET /api/hub/brains/<brain>/export` — the granted slice as plain files |
//! | `sync` (push) | `POST /api/hub/brains/<brain>/push` — the local store as a snapshot |
//! | `grant` | `GET` / `POST /api/hub/brains/<brain>/grants`, `DELETE /api/hub/brains/<brain>/grants/<id>` |
//! | `propose` | `POST /api/hub/sites/<handle>/inbox` — evidence in, without trust (unauthenticated by design) |
//! | `subscribe` | `GET /api/hub/brains/<brain>` polled for feed-head movement |
//!
//! # Configuration — no default hub, credential never in the store
//!
//! There is **no built-in hub endpoint**: the toolkit is neutral and a hub is
//! whatever the user points it at. Resolution order for the hub URL:
//!
//! 1. the `--hub <URL>` flag,
//! 2. the `DBMD_HUB_URL` environment variable,
//! 3. the `hub = <URL>` line in the store-local `.dbmd/config` file
//!    (toolkit state, not store content — the walkers already skip hidden
//!    directories, so `.dbmd/` never syncs, indexes, or validates).
//!
//! The credential is the `DBMD_HUB_KEY` environment variable, full stop. It is
//! deliberately **not** read from `.dbmd/config`: a secret inside the store
//! tree is one commit or one push away from leaking, so the file carries only
//! non-secret targets and the agent's environment carries the key.
//!
//! Non-HTTPS hubs are refused (the bearer key must never travel in cleartext)
//! with a loopback exemption for local development.
//!
//! # v0 honesty
//!
//! This client binds to what a hub enforces **today**: grantees are hub
//! principals (an email), grant scopes are store-path prefixes, pushes are
//! whole-store snapshots, and `subscribe` reports feed-head movement (the hub
//! does not yet serve per-entry feed reads). Record signing and brain keypairs
//! are the protocol's next layer and are absent here by design — nothing in
//! this module invents key custody.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::fsx::write_atomic;
use crate::store::Store;

/// Environment variable naming the hub base URL (e.g. `https://hub.example.com`).
pub const HUB_URL_ENV: &str = "DBMD_HUB_URL";

/// Environment variable carrying the hub bearer credential. The one and only
/// credential source — see the module docs for why it is never file-based.
pub const HUB_KEY_ENV: &str = "DBMD_HUB_KEY";

/// The store-local config file, relative to the store root. Holds non-secret
/// toolkit state (`hub = <URL>`); hidden, so every store walk skips it.
pub const CONFIG_REL_PATH: &str = ".dbmd/config";

/// The most this client will buffer from one hub response. A full-store
/// export is the biggest honest payload; anything past this is refused loudly
/// rather than silently truncated.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

/// The hub's JSON push path is serverless-capped (~4 MB body); mirror it
/// client-side so an oversized push fails before the upload, not after.
const MAX_PUSH_BYTES: usize = 4 * 1024 * 1024;

/// The hub's per-push file-count cap, mirrored client-side.
const MAX_PUSH_FILES: usize = 50_000;

/// The hub's inbox cap on one `propose` submission body, mirrored client-side
/// so an oversized body fails before the upload, not after (the same
/// fail-before-upload contract as the push caps). Public so the CLI can
/// pre-check a `--body-file` from file metadata without reading it.
pub const MAX_PROPOSE_BYTES: u64 = 16 * 1024;

/// Bounded connect so a dead hub fails fast; a generous read window so a
/// large export on a slow link still completes.
const CONNECT_TIMEOUT_SECS: u64 = 10;
const READ_TIMEOUT_SECS: u64 = 120;

/// Everything that can go wrong on the wire or at its edges. Each variant maps
/// onto one stable CLI error code; messages are single-line and never echo the
/// credential.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// No hub URL was configured anywhere (flag, env, `.dbmd/config`).
    #[error(
        "no hub configured — pass --hub <URL>, set {HUB_URL_ENV}, or add `hub = <URL>` to {CONFIG_REL_PATH}"
    )]
    NoHub,

    /// The verb needs a credential and none was present.
    #[error("no hub credential — set {HUB_KEY_ENV} (credentials never live in {CONFIG_REL_PATH})")]
    NoCredential,

    /// The credential contains whitespace / non-ASCII (a paste artifact). The
    /// key is deliberately not echoed.
    #[error(
        "the hub credential in {HUB_KEY_ENV} contains whitespace or non-ASCII characters — re-copy it (the key is not shown here on purpose)"
    )]
    BadKey,

    /// A non-HTTPS hub outside loopback: the bearer key would travel in cleartext.
    #[error("refusing non-HTTPS hub {hub} — the credential would travel in cleartext (localhost is exempt)")]
    UnsafeHub {
        /// The offending hub URL.
        hub: String,
    },

    /// TCP/TLS-level failure: the hub never answered.
    #[error("hub unreachable at {hub}: {message}")]
    Transport {
        /// The hub base URL.
        hub: String,
        /// The transport-layer error text.
        message: String,
    },

    /// The hub answered with an HTTP error status.
    #[error("{what} failed (HTTP {status}): {message}")]
    Http {
        /// What the client was doing (e.g. `"resolve"`, `"sync pull"`).
        what: &'static str,
        /// The HTTP status code.
        status: u16,
        /// The hub's own `error` string when it sent one, else a placeholder.
        message: String,
        /// The hub's machine `code` field when it sent one.
        code: Option<String>,
    },

    /// A 2xx whose body is not JSON — a captive portal, a proxy, or a wrong
    /// URL — refused here rather than deserializing into nothing downstream.
    #[error("{what}: the hub answered HTTP {status} with a non-JSON body — check the hub URL")]
    NotJson {
        /// What the client was doing.
        what: &'static str,
        /// The (2xx) status that carried the non-JSON body.
        status: u16,
    },

    /// The hub response exceeded [`MAX_RESPONSE_BYTES`].
    #[error("hub response exceeded {} MB — refusing to buffer it", MAX_RESPONSE_BYTES / (1024 * 1024))]
    ResponseTooLarge,

    /// A malformed `@brain/id` address.
    #[error("invalid address `{given}`: {reason}")]
    BadAddress {
        /// The raw address as typed.
        given: String,
        /// Why it did not parse.
        reason: String,
    },

    /// A grant id whose shape cannot travel as a URL path segment.
    #[error(
        "invalid grant id `{given}` — grant ids come from `grant list` (lowercase letters, digits, hyphens)"
    )]
    BadGrantId {
        /// The raw id as typed.
        given: String,
    },

    /// An exported file path that would escape or pollute the destination
    /// (absolute, `..`, a dot-leading segment, or an illegal character). The
    /// hub is not trusted with local path layout.
    #[error("refusing unsafe path from the hub: `{path}`")]
    UnsafePath {
        /// The offending path as received.
        path: String,
    },

    /// The push payload exceeds the hub's JSON-path caps.
    #[error(
        "push too large ({detail}) — the hub's JSON push path caps at ~{} MB / {MAX_PUSH_FILES} files; larger brains need the hub's pack path, which is not a dbmd verb yet",
        MAX_PUSH_BYTES / (1024 * 1024)
    )]
    PushTooLarge {
        /// Which cap was hit, human-readable.
        detail: String,
    },

    /// The propose body exceeds the hub's inbox cap.
    #[error(
        "propose body too large ({bytes} bytes) — the hub's inbox caps one submission at {} KB",
        MAX_PROPOSE_BYTES / 1024
    )]
    ProposeTooLarge {
        /// The offending body size in bytes.
        bytes: u64,
    },

    /// A store file that is not valid UTF-8 cannot travel the JSON push path.
    #[error("store file `{path}` is not valid UTF-8 — the JSON push path carries text only")]
    NotUtf8 {
        /// The store-relative path of the offending file.
        path: String,
    },

    /// Local filesystem failure while materializing a pull or reading a push.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A store-level failure (walking the local store for a push).
    #[error(transparent)]
    Store(#[from] crate::StoreError),
}

/// Result alias for link.md client operations.
pub type LinkResult<T> = std::result::Result<T, LinkError>;

// ─────────────────────────────────────────────────────────────────────────────
// Addressing — `@brain[/id]`, the reserved shape (SPEC § Addressing)
// ─────────────────────────────────────────────────────────────────────────────

/// What the part after `@brain/` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressTarget {
    /// A record `id` — the db.md lowercase ULID (the reserved `@brain/id` shape).
    Id(String),
    /// A store-relative `.md` path — a client-side convenience the hub's
    /// resolve endpoint also accepts (`?path=`). Not part of the reserved
    /// shape; unambiguous because a ULID is never a path.
    Path(String),
}

/// Why a brain reference failed [`is_safe_ref`] — shared by [`Address::parse`]
/// and the per-verb entry gates so the two surfaces never drift.
const BAD_BRAIN_REASON: &str =
    "the brain reference must be a brain id (lowercase ULID) or a slug (lowercase letters, digits, hyphens)";

/// Why an address target failed its shape check — shared by [`Address::parse`]
/// and the [`resolve`] entry gate.
const BAD_TARGET_REASON: &str =
    "the part after `/` must be a record id (lowercase ULID) or a store-relative `.md` path";

/// A parsed `@brain[/target]` address. `brain` is a hub brain reference — the
/// brain's ULID id (works for any caller, including cross-party on a public
/// brain) or a slug (which a hub resolves only against the caller's own
/// brains; slugs are unique per owner, not globally).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// The brain reference (leading `@` stripped).
    pub brain: String,
    /// The record target, when the address names one.
    pub target: Option<AddressTarget>,
}

impl Address {
    /// Parse `@brain`, `@brain/<ulid>`, or `@brain/<store-path>.md`. The `@`
    /// sigil is optional (an agent piping ids around should not have to quote
    /// it back on). Whitespace and empty segments are malformed.
    pub fn parse(raw: &str) -> LinkResult<Address> {
        let bad = |reason: &str| LinkError::BadAddress {
            given: raw.to_string(),
            reason: reason.to_string(),
        };

        let trimmed = raw.trim();
        let body = trimmed.strip_prefix('@').unwrap_or(trimmed);
        if body.is_empty() {
            return Err(bad("empty address"));
        }

        let (brain, rest) = match body.split_once('/') {
            Some((b, r)) => (b, Some(r)),
            None => (body, None),
        };

        if brain.is_empty() {
            return Err(bad("missing brain reference before `/`"));
        }
        if !is_safe_ref(brain) {
            return Err(bad(BAD_BRAIN_REASON));
        }

        let target = match rest {
            None => None,
            Some("") => return Err(bad("trailing `/` with no record id or path")),
            Some(r) if crate::ulid::is_ulid(r) => Some(AddressTarget::Id(r.to_string())),
            Some(r) => {
                if !safe_store_rel_path(r) || !r.ends_with(".md") {
                    return Err(bad(BAD_TARGET_REASON));
                }
                Some(AddressTarget::Path(r.to_string()))
            }
        };

        Ok(Address {
            brain: brain.to_string(),
            target,
        })
    }
}

/// A brain reference safe to embed in a URL path segment: the shapes a hub
/// accepts (ULID id or slug), which are also exactly URL-path-clean.
fn is_safe_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A published-site handle (the `propose` target). Same lexical shape as a
/// slug.
pub fn is_valid_handle(s: &str) -> bool {
    is_safe_ref(s)
}

/// True when `p` is a store-relative path this client will read from or write
/// to disk: relative, no `..`, no empty or dot-leading segment (which shields
/// `.dbmd/` and `.git/`), and only the hub-portable character set. Applied to
/// every path an export hands us (the hub is not trusted with local layout)
/// and to every path a push sends (mirroring the hub's own gate).
pub fn safe_store_rel_path(p: &str) -> bool {
    if p.is_empty() || p.len() > 512 || p.starts_with('/') {
        return false;
    }
    if !p
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return false;
    }
    p.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.starts_with('.'))
}

/// Entry gate for every verb that embeds a caller-supplied brain reference in
/// a URL path segment. `resolve` reaches the same check through
/// [`Address::parse`]; the raw-ref verbs (`sync`, `grant`, `subscribe`) call
/// this directly, so a ref carrying `/`, `..`, `?`, `#`, or any other
/// URL-reshaping byte is refused before a request exists (the `url` crate
/// normalizes dot segments, so an unvalidated ref would redirect the
/// authenticated request to a different hub path).
fn require_safe_ref(brain: &str) -> LinkResult<()> {
    if is_safe_ref(brain) {
        Ok(())
    } else {
        Err(LinkError::BadAddress {
            given: brain.to_string(),
            reason: BAD_BRAIN_REASON.to_string(),
        })
    }
}

/// Entry gate for the published-site handle `propose` embeds in its URL path.
fn require_valid_handle(handle: &str) -> LinkResult<()> {
    if is_valid_handle(handle) {
        Ok(())
    } else {
        Err(LinkError::BadAddress {
            given: handle.to_string(),
            reason: "the site handle must be lowercase letters, digits, hyphens".to_string(),
        })
    }
}

/// Entry gate for the grant id `grant revoke` embeds in its URL path. Hub
/// grant ids are lowercase ULIDs; the gate accepts the same URL-path-clean
/// shape as a brain ref rather than pinning one mint scheme.
fn require_safe_grant_id(id: &str) -> LinkResult<()> {
    if is_safe_ref(id) {
        Ok(())
    } else {
        Err(LinkError::BadGrantId {
            given: id.to_string(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration — flag > env > .dbmd/config; credential from env only
// ─────────────────────────────────────────────────────────────────────────────

/// The resolved client configuration for one invocation.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// The hub base URL, trailing slash stripped, HTTPS-or-loopback enforced.
    pub hub: String,
    /// The bearer credential, when the environment carries one.
    pub key: Option<String>,
}

impl HubConfig {
    /// The credential, or the canonical "not configured" error. Verbs that
    /// authenticate call this; `propose` never does.
    pub fn require_key(&self) -> LinkResult<&str> {
        self.key.as_deref().ok_or(LinkError::NoCredential)
    }
}

/// Resolve the client configuration: `flag_hub` beats [`HUB_URL_ENV`] beats
/// the `hub =` line in `<dir>/.dbmd/config`; no fallback default exists. The
/// credential comes from [`HUB_KEY_ENV`] alone and is validated as a clean
/// header token (never echoed on failure).
pub fn hub_config(flag_hub: Option<&str>, dir: &Path) -> LinkResult<HubConfig> {
    let hub = flag_hub
        .map(str::to_string)
        .or_else(|| env_nonempty(HUB_URL_ENV))
        .or_else(|| config_file_hub(&dir.join(CONFIG_REL_PATH)))
        .ok_or(LinkError::NoHub)?;
    let hub = hub.trim().trim_end_matches('/').to_string();
    assert_safe_hub(&hub)?;

    let key = match env_nonempty(HUB_KEY_ENV) {
        Some(raw) => Some(clean_key(&raw)?),
        None => None,
    };

    Ok(HubConfig { hub, key })
}

/// An env var, treated as absent when unset or empty (an empty
/// `DBMD_HUB_KEY=` falls through rather than becoming an empty credential).
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Read the `hub = <URL>` line out of a `.dbmd/config` file. The format is
/// deliberately minimal: `key = value` lines, `#` comments, unknown keys
/// ignored (forward-compatible). A missing or unreadable file is simply "not
/// configured here".
fn config_file_hub(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "hub" {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// The bearer key must never travel in cleartext; only loopback hosts may
/// skip TLS (local development against a hub on localhost).
fn assert_safe_hub(hub: &str) -> LinkResult<()> {
    let (scheme, rest) = hub.split_once("://").ok_or_else(|| LinkError::UnsafeHub {
        hub: hub.to_string(),
    })?;
    // Host = authority up to the first '/', minus any port — with the
    // bracketed-IPv6 form handled so `http://[::1]:3000` counts as loopback.
    let hostport = rest.split('/').next().unwrap_or("");
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => hostport.split(':').next().unwrap_or(""),
    };
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
    // Scheme matching is case-insensitive (RFC 3986): an uppercase-scheme
    // HTTPS hub is still HTTPS, not a cleartext refusal.
    if scheme.eq_ignore_ascii_case("https") || loopback {
        Ok(())
    } else {
        Err(LinkError::UnsafeHub {
            hub: hub.to_string(),
        })
    }
}

/// Trim paste artifacts and refuse anything outside the printable-ASCII token
/// range WITHOUT echoing the key — an HTTP library rejecting a bad header
/// value tends to echo the whole header line, credential included, so the
/// gate sits here instead.
fn clean_key(raw: &str) -> LinkResult<String> {
    let k = raw.trim();
    if k.is_empty() || k.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(LinkError::BadKey);
    }
    Ok(k.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Transport — one blocking agent, capped reads, the JSON-or-refuse contract
// ─────────────────────────────────────────────────────────────────────────────

/// One hub response: the status plus the parsed JSON body when there was one.
#[derive(Debug)]
pub struct HubResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The parsed JSON body, `None` when the body was empty or not JSON.
    pub body: Option<Value>,
}

/// Whether a request carries the bearer credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// Send `authorization: Bearer <key>`; error without a key.
    Required,
    /// Send no credential — the propose door is unauthenticated by design.
    None,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(concat!("dbmd/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout_read(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
        .build()
}

/// Perform one hub request. `path` is the binding path (starts with `/`);
/// `body` posts JSON. Transport failures, oversized bodies, and non-UTF-8 are
/// all surfaced as typed [`LinkError`]s; HTTP error statuses are returned in
/// the [`HubResponse`] for [`ensure_ok`] to shape.
fn request(
    cfg: &HubConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
    auth: Auth,
) -> LinkResult<HubResponse> {
    let url = format!("{}{}", cfg.hub, path);
    let mut req = agent().request(method, &url);
    if auth == Auth::Required {
        req = req.set("authorization", &format!("Bearer {}", cfg.require_key()?));
    }

    let result = match body {
        Some(v) => req
            .set("content-type", "application/json")
            .send_string(&v.to_string()),
        None => req.call(),
    };

    let resp = match result {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(ureq::Error::Transport(t)) => {
            return Err(LinkError::Transport {
                hub: cfg.hub.clone(),
                message: t.to_string(),
            })
        }
    };

    let status = resp.status();
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(LinkError::ResponseTooLarge);
    }
    let parsed: Option<Value> = serde_json::from_slice(&buf).ok();
    Ok(HubResponse {
        status,
        body: parsed,
    })
}

/// Unwrap a successful JSON body, or shape the failure: a >=400 surfaces the
/// hub's own `error` + `code`; a 2xx without JSON is refused as not a hub
/// answer.
fn ensure_ok(r: HubResponse, what: &'static str) -> LinkResult<Value> {
    if r.status >= 400 {
        let message = r
            .body
            .as_ref()
            .and_then(|b| b.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        let code = r
            .body
            .as_ref()
            .and_then(|b| b.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(LinkError::Http {
            what,
            status: r.status,
            message,
            code,
        });
    }
    r.body.ok_or(LinkError::NotJson {
        what,
        status: r.status,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// resolve — handle → brain card; @brain/id → the record
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve an address. A bare `@brain` returns the brain card (metadata +
/// index stats — the v0 form of the card; keys arrive with the protocol's
/// signing layer). `@brain/<id>` and `@brain/<path>.md` return the full
/// record, frontmatter + body.
pub fn resolve(cfg: &HubConfig, addr: &Address) -> LinkResult<Value> {
    // `Address::parse` refuses these shapes already, but `Address` has public
    // fields — re-assert at the wire so a hand-built address can never
    // reshape the request path.
    require_safe_ref(&addr.brain)?;
    if let Some(target) = &addr.target {
        let (given, ok) = match target {
            AddressTarget::Id(id) => (id, crate::ulid::is_ulid(id)),
            AddressTarget::Path(p) => (p, safe_store_rel_path(p) && p.ends_with(".md")),
        };
        if !ok {
            return Err(LinkError::BadAddress {
                given: given.clone(),
                reason: BAD_TARGET_REASON.to_string(),
            });
        }
    }

    let path = match &addr.target {
        None => format!("/api/hub/brains/{}", addr.brain),
        Some(AddressTarget::Id(id)) => {
            format!("/api/hub/brains/{}/resolve?id={id}", addr.brain)
        }
        Some(AddressTarget::Path(p)) => {
            format!("/api/hub/brains/{}/resolve?path={p}", addr.brain)
        }
    };
    ensure_ok(request(cfg, "GET", &path, None, Auth::Required)?, "resolve")
}

// ─────────────────────────────────────────────────────────────────────────────
// sync — pull the granted slice as files; push the local store as a snapshot
// ─────────────────────────────────────────────────────────────────────────────

/// What a pull materialized.
#[derive(Debug, serde::Serialize)]
pub struct PullReport {
    /// The brain id the hub reported.
    pub brain: String,
    /// The brain's slug.
    pub slug: String,
    /// The hub's feed head at export time.
    #[serde(rename = "headSeq")]
    pub head_seq: u64,
    /// How many files were written.
    pub files: usize,
    /// Where they were written (as given or derived from the slug).
    pub dest: String,
    /// Local content files that the export did not carry — present so a
    /// caller sees divergence; nothing is ever deleted locally.
    #[serde(rename = "extraLocal")]
    pub extra_local: Vec<String>,
}

/// Pull the granted slice of `brain` to `out` (default: `./<slug>`). Every
/// exported path is safety-gated before it touches disk; files are written
/// atomically; nothing local is ever deleted (locals the export lacks are
/// *reported* in `extra_local` instead). Returns the report; rebuilding the
/// local index catalog afterwards is the caller's (cheap, optional) step.
pub fn sync_pull(cfg: &HubConfig, brain: &str, out: Option<&Path>) -> LinkResult<PullReport> {
    require_safe_ref(brain)?;
    let path = format!("/api/hub/brains/{brain}/export");
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "sync pull",
    )?;

    let slug = body
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or(brain)
        .to_string();
    let brain_id = body
        .get("brain")
        .and_then(Value::as_str)
        .unwrap_or(brain)
        .to_string();
    let head_seq = body.get("headSeq").and_then(Value::as_u64).unwrap_or(0);
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let dest: PathBuf = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(&slug),
    };
    std::fs::create_dir_all(&dest)?;

    // Gate every path BEFORE the first write so a hostile entry anywhere in
    // the export aborts the pull with nothing partially materialized.
    let mut entries: Vec<(String, String)> = Vec::with_capacity(files.len());
    for f in &files {
        let p = f.get("path").and_then(Value::as_str).unwrap_or_default();
        let content = f.get("content").and_then(Value::as_str).unwrap_or_default();
        if !safe_store_rel_path(p) {
            return Err(LinkError::UnsafePath {
                path: p.to_string(),
            });
        }
        entries.push((p.to_string(), content.to_string()));
    }

    for (p, content) in &entries {
        let abs = dest.join(p);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&abs, content.as_bytes())?;
    }

    // Divergence report: local content files the export did not carry. Only
    // meaningful when the destination is (now) an openable store; a scoped
    // pull may lack DB.md, in which case there is nothing to compare against.
    let pulled: std::collections::BTreeSet<&str> =
        entries.iter().map(|(p, _)| p.as_str()).collect();
    let mut extra_local = Vec::new();
    if let Ok(store) = Store::open(&dest) {
        if let Ok(walked) = store.walk() {
            for rel in walked {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !pulled.contains(rel_str.as_str()) {
                    extra_local.push(rel_str);
                }
            }
        }
    }

    Ok(PullReport {
        brain: brain_id,
        slug,
        head_seq,
        files: entries.len(),
        dest: dest.to_string_lossy().into_owned(),
        extra_local,
    })
}

/// Collect the files a push sends: the store's owned text — `DB.md`,
/// `assets.jsonl` when present, and every content `.md` under `records/` and
/// `sources/` (the store walk, which already excludes hidden dirs like
/// `.dbmd/`, the `log/` archive, and derived `index.*` catalogs; the hub
/// derives its own index, and local history stays local). Returns
/// `(store-relative path, content)` pairs, path-sorted.
pub fn collect_push_files(store: &Store) -> LinkResult<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();

    let read_text = |rel: &str| -> LinkResult<String> {
        let abs = store.root.join(rel);
        std::fs::read(&abs)
            .map_err(LinkError::from)
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|_| LinkError::NotUtf8 {
                    path: rel.to_string(),
                })
            })
    };

    out.push(("DB.md".to_string(), read_text("DB.md")?));
    if store.root.join("assets.jsonl").is_file() {
        out.push(("assets.jsonl".to_string(), read_text("assets.jsonl")?));
    }

    for rel in store.walk()? {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !safe_store_rel_path(&rel_str) {
            // A locally-legal name outside the hub's portable charset cannot
            // travel this wire; refusing beats silently dropping it.
            return Err(LinkError::UnsafePath { path: rel_str });
        }
        let content = read_text(&rel_str)?;
        out.push((rel_str, content));
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Push `files` to `brain` as a whole-store snapshot — the hub's push
/// semantics: the hosted copy becomes exactly this set (pull first if the
/// hosted side may have records the local copy lacks). Client-side caps
/// mirror the hub's JSON-path limits so an oversized push fails before the
/// upload.
pub fn sync_push(cfg: &HubConfig, brain: &str, files: &[(String, String)]) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    if files.len() > MAX_PUSH_FILES {
        return Err(LinkError::PushTooLarge {
            detail: format!("{} files", files.len()),
        });
    }
    let total: usize = files
        .iter()
        .map(|(p, c)| p.len() + c.len() + 32) // 32 ≈ per-entry JSON framing
        .sum();
    if total > MAX_PUSH_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: format!("~{} bytes of payload", total),
        });
    }

    let body = json!({
        "files": files
            .iter()
            .map(|(p, c)| json!({ "path": p, "content": c }))
            .collect::<Vec<_>>(),
    });
    let path = format!("/api/hub/brains/{brain}/push");
    ensure_ok(
        request(cfg, "POST", &path, Some(&body), Auth::Required)?,
        "sync push",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// grant — issue / list / revoke capabilities (owner-side)
// ─────────────────────────────────────────────────────────────────────────────

/// The two capabilities a v0 hub enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Read the granted slice.
    Read,
    /// Read and push (whole-store; a path-scoped grant is read-only).
    Write,
}

impl Capability {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Read => "read",
            Capability::Write => "write",
        }
    }
}

/// Issue (or refresh) a grant on `brain` to `grantee` — a hub principal named
/// by email in v0 (the protocol's near-term simplification; key-named
/// grantees arrive with the signing layer). `scope` is a store-path prefix
/// (the hub's enforcement unit); `until` an ISO 8601 expiry, absent = until
/// revoked.
pub fn grant_issue(
    cfg: &HubConfig,
    brain: &str,
    grantee: &str,
    can: Capability,
    scope: Option<&str>,
    until: Option<&str>,
) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    let mut body = json!({
        "email": grantee,
        "capability": can.as_str(),
    });
    if let Some(s) = scope {
        body["scopePrefix"] = json!(s);
    }
    if let Some(u) = until {
        body["expiresAt"] = json!(u);
    }
    let path = format!("/api/hub/brains/{brain}/grants");
    ensure_ok(
        request(cfg, "POST", &path, Some(&body), Auth::Required)?,
        "grant issue",
    )
}

/// List the active grants (and pending invites) on `brain`. Owner-side.
pub fn grant_list(cfg: &HubConfig, brain: &str) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    let path = format!("/api/hub/brains/{brain}/grants");
    ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "grant list",
    )
}

/// Revoke a grant (or cancel a pending invite) by id. Owner-side; revocation
/// is soft on the hub (the audit trail survives).
pub fn grant_revoke(cfg: &HubConfig, brain: &str, grant_id: &str) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    require_safe_grant_id(grant_id)?;
    let path = format!("/api/hub/brains/{brain}/grants/{grant_id}");
    ensure_ok(
        request(cfg, "DELETE", &path, None, Auth::Required)?,
        "grant revoke",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// propose — write without trust: evidence into the owner's inbox
// ─────────────────────────────────────────────────────────────────────────────

/// Submit `body` to the published site `handle`, addressed to its app page
/// `app` (a page that declares the `write-inbox` capability). Deliberately
/// unauthenticated — this is the cross-party door; the submission lands as
/// *evidence* in the owner's `sources/inbox/`, never as truth, and the
/// owner's curator accepts or rejects it. Returns the hub's `{id, path}`
/// receipt.
pub fn propose(cfg: &HubConfig, handle: &str, app: &str, body: &str) -> LinkResult<Value> {
    require_valid_handle(handle)?;
    if body.len() as u64 > MAX_PROPOSE_BYTES {
        return Err(LinkError::ProposeTooLarge {
            bytes: body.len() as u64,
        });
    }
    let payload = json!({ "app": app, "body": body });
    let path = format!("/api/hub/sites/{handle}/inbox");
    ensure_ok(
        request(cfg, "POST", &path, Some(&payload), Auth::None)?,
        "propose",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// subscribe — follow feed-head movement
// ─────────────────────────────────────────────────────────────────────────────

/// One observation of a brain's feed head.
#[derive(Debug, serde::Serialize)]
pub struct Head {
    /// The brain id.
    pub brain: String,
    /// The hub's indexed feed cursor — advances on every accepted write.
    pub seq: u64,
    /// The hub's `updatedAt` for the brain, when present.
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Read the brain's current feed head. `subscribe` polls this: the hub does
/// not yet serve per-entry feed reads, so v0 subscription is head-movement
/// detection — the caller re-pulls (or re-queries) on advance.
pub fn head(cfg: &HubConfig, brain: &str) -> LinkResult<Head> {
    require_safe_ref(brain)?;
    let path = format!("/api/hub/brains/{brain}");
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "subscribe",
    )?;
    Ok(Head {
        brain: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(brain)
            .to_string(),
        seq: body
            .get("indexedFeedSeq")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        updated_at: body
            .get("updatedAt")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Address parsing ─────────────────────────────────────────────────────

    #[test]
    fn address_bare_brain_with_and_without_sigil() {
        for raw in ["@acme-ops", "acme-ops"] {
            let a = Address::parse(raw).expect(raw);
            assert_eq!(a.brain, "acme-ops");
            assert_eq!(a.target, None);
        }
    }

    #[test]
    fn address_ulid_target_parses_as_id() {
        let a = Address::parse("@acme/01j5qc3v9k4ym8rwbn2tqe6f7d").unwrap();
        assert_eq!(a.brain, "acme");
        assert_eq!(
            a.target,
            Some(AddressTarget::Id("01j5qc3v9k4ym8rwbn2tqe6f7d".to_string()))
        );
    }

    #[test]
    fn address_md_path_target_parses_as_path() {
        let a = Address::parse("@acme/records/clients/lumio.md").unwrap();
        assert_eq!(
            a.target,
            Some(AddressTarget::Path("records/clients/lumio.md".to_string()))
        );
    }

    #[test]
    fn address_rejects_malformed_forms() {
        for raw in [
            "",
            "@",
            "@/x",
            "@acme/",
            "@acme/../etc/passwd",
            "@acme/records/.hidden.md",
            "@ACME",             // uppercase is not a hub ref shape
            "@acme/notes/x.txt", // target is neither ULID nor .md path
            "@a b",              // whitespace
        ] {
            assert!(Address::parse(raw).is_err(), "should reject {raw:?}");
        }
    }

    // ── Path safety ─────────────────────────────────────────────────────────

    #[test]
    fn safe_paths_accept_store_shapes_and_reject_escapes() {
        for ok in [
            "DB.md",
            "assets.jsonl",
            "records/clients/lumio.md",
            "sources/emails/2026/07/x.md",
        ] {
            assert!(safe_store_rel_path(ok), "should accept {ok:?}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../up.md",
            "records/../../up.md",
            "records//x.md",
            ".dbmd/config",
            "records/.hidden/x.md",
            "records/a b.md",
            "records\\win.md",
        ] {
            assert!(!safe_store_rel_path(bad), "should reject {bad:?}");
        }
    }

    // ── Config resolution (flag + file precedence; env is covered by the CLI
    //    integration tests, where a child process isolates it) ───────────────

    #[test]
    fn hub_config_flag_beats_file_and_requires_some_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".dbmd")).unwrap();
        std::fs::write(
            dir.path().join(CONFIG_REL_PATH),
            "# toolkit state\nhub = https://file.example.com\nunknown = ignored\n",
        )
        .unwrap();

        let from_flag = hub_config(Some("https://flag.example.com/"), dir.path()).unwrap();
        assert_eq!(from_flag.hub, "https://flag.example.com");

        let from_file = hub_config(None, dir.path()).unwrap();
        assert_eq!(from_file.hub, "https://file.example.com");

        let none = hub_config(None, tempfile::tempdir().unwrap().path());
        assert!(matches!(none, Err(LinkError::NoHub)));
    }

    #[test]
    fn https_guard_allows_loopback_only_for_plain_http() {
        assert!(assert_safe_hub("https://hub.example.com").is_ok());
        assert!(assert_safe_hub("http://localhost:3000").is_ok());
        assert!(assert_safe_hub("http://127.0.0.1:3000").is_ok());
        assert!(assert_safe_hub("http://[::1]:3000").is_ok());
        assert!(matches!(
            assert_safe_hub("http://hub.example.com"),
            Err(LinkError::UnsafeHub { .. })
        ));
        assert!(matches!(
            assert_safe_hub("hub.example.com"),
            Err(LinkError::UnsafeHub { .. })
        ));
    }

    #[test]
    fn https_guard_matches_the_scheme_case_insensitively() {
        // RFC 3986 schemes are case-insensitive: an uppercase-scheme HTTPS
        // hub is still HTTPS, never a misleading non-HTTPS refusal.
        assert!(assert_safe_hub("HTTPS://hub.example.com").is_ok());
        assert!(assert_safe_hub("Https://hub.example.com").is_ok());
        // And an uppercase plain-HTTP hub is still refused outside loopback.
        assert!(matches!(
            assert_safe_hub("HTTP://hub.example.com"),
            Err(LinkError::UnsafeHub { .. })
        ));
    }

    #[test]
    fn clean_key_refuses_paste_artifacts_without_echoing() {
        assert_eq!(clean_key("  vc_account_abc  ").unwrap(), "vc_account_abc");
        for bad in ["vc account", "vc\naccount", "ключ", ""] {
            let err = clean_key(bad).unwrap_err();
            assert!(matches!(err, LinkError::BadKey));
            assert!(
                !err.to_string().contains(bad.trim()) || bad.trim().is_empty(),
                "error must not echo the key"
            );
        }
    }

    // ── Verb entry gates: refs must never reshape the request path ──────────

    /// A config whose hub passes the loopback guard but is never listened on:
    /// every refusal below must come from the entry gate BEFORE a request
    /// exists — a dial on this dead port would surface `Transport` instead.
    fn dead_hub() -> HubConfig {
        HubConfig {
            hub: "http://127.0.0.1:9".to_string(),
            key: Some("k".to_string()),
        }
    }

    #[test]
    fn verb_entry_gates_accept_the_hub_ref_shapes() {
        for ok in ["acme-ops", "a", "01j5qc3v9k4ym8rwbn2tqe6f7d"] {
            assert!(require_safe_ref(ok).is_ok(), "brain ref {ok:?}");
            assert!(require_valid_handle(ok).is_ok(), "handle {ok:?}");
            assert!(require_safe_grant_id(ok).is_ok(), "grant id {ok:?}");
        }
    }

    #[test]
    fn raw_ref_verbs_refuse_url_reshaping_brain_refs_before_any_request() {
        let cfg = dead_hub();
        for bad in ["../up", "a/b", "a?x=1", "a#frag", "a%2e%2e", "A", "a b", ""] {
            assert!(
                matches!(
                    sync_pull(&cfg, bad, None),
                    Err(LinkError::BadAddress { .. })
                ),
                "sync_pull must refuse {bad:?}"
            );
            assert!(
                matches!(sync_push(&cfg, bad, &[]), Err(LinkError::BadAddress { .. })),
                "sync_push must refuse {bad:?}"
            );
            assert!(
                matches!(
                    grant_issue(&cfg, bad, "maya@example.com", Capability::Read, None, None),
                    Err(LinkError::BadAddress { .. })
                ),
                "grant_issue must refuse {bad:?}"
            );
            assert!(
                matches!(grant_list(&cfg, bad), Err(LinkError::BadAddress { .. })),
                "grant_list must refuse {bad:?}"
            );
            assert!(
                matches!(
                    grant_revoke(&cfg, bad, "01j5qc3v9k4ym8rwbn2tqe6f7f"),
                    Err(LinkError::BadAddress { .. })
                ),
                "grant_revoke must refuse brain {bad:?}"
            );
            assert!(
                matches!(head(&cfg, bad), Err(LinkError::BadAddress { .. })),
                "head must refuse {bad:?}"
            );
        }
    }

    #[test]
    fn grant_revoke_refuses_url_reshaping_grant_ids() {
        let cfg = dead_hub();
        for bad in ["../01j", "a/b", "id?x=1", "id#frag", "ID", ""] {
            assert!(
                matches!(
                    grant_revoke(&cfg, "acme", bad),
                    Err(LinkError::BadGrantId { .. })
                ),
                "grant_revoke must refuse grant id {bad:?}"
            );
        }
    }

    #[test]
    fn propose_refuses_url_reshaping_handles_and_oversize_bodies_before_upload() {
        let cfg = dead_hub();
        for bad in ["../up", "a/b", "a?x=1", "a#frag", "A", ""] {
            assert!(
                matches!(
                    propose(&cfg, bad, "intake", "hi"),
                    Err(LinkError::BadAddress { .. })
                ),
                "propose must refuse handle {bad:?}"
            );
        }
        let oversize = "a".repeat(MAX_PROPOSE_BYTES as usize + 1);
        assert!(matches!(
            propose(&cfg, "acme-site", "intake", &oversize),
            Err(LinkError::ProposeTooLarge { .. })
        ));
        // A clean handle + in-cap body passes both gates: the failure is now
        // the (dead) wire, proving the gates refuse shape, not the verb.
        assert!(matches!(
            propose(&cfg, "acme-site", "intake", "hi"),
            Err(LinkError::Transport { .. })
        ));
    }

    #[test]
    fn resolve_refuses_a_hand_built_unsafe_address() {
        let cfg = dead_hub();
        for brain in ["../up", "a/b", "a?x", "a#f"] {
            let addr = Address {
                brain: brain.to_string(),
                target: None,
            };
            assert!(
                matches!(resolve(&cfg, &addr), Err(LinkError::BadAddress { .. })),
                "resolve must refuse brain {brain:?}"
            );
        }
        for target in [
            AddressTarget::Id("01j5qc3v9k4ym8rwbn2tqe6f7d?id=other".to_string()),
            AddressTarget::Id("01J5QC3V9K4YM8RWBN2TQE6F7D".to_string()), // not the minted shape
            AddressTarget::Path("../up.md".to_string()),
            AddressTarget::Path("records/x.md#frag".to_string()),
        ] {
            let addr = Address {
                brain: "acme".to_string(),
                target: Some(target.clone()),
            };
            assert!(
                matches!(resolve(&cfg, &addr), Err(LinkError::BadAddress { .. })),
                "resolve must refuse target {target:?}"
            );
        }
    }
}
