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
//! polling and signed-entry verification — lives here, as a *client
//! capability*, never a format requirement.
//!
//! # What this client speaks
//!
//! The v0 HTTP binding a hub serves under its base URL:
//!
//! | verb | binding |
//! | --- | --- |
//! | `resolve` | `GET /api/hub/brains/<brain>` (the brain card) and `GET /api/hub/brains/<brain>/resolve?id=…` / `?path=…` (a record) |
//! | `sync` (pull) | `GET /api/hub/brains/<brain>/export?format=pack` — an immutable pack, or the granted slice as plain files |
//! | `sync` (push) | `POST /api/hub/brains/<brain>/push` for small snapshots; presign/upload/commit for large snapshots |
//! | `grant` | `GET` / `POST /api/hub/brains/<brain>/grants`, `DELETE /api/hub/brains/<brain>/grants/<id>` |
//! | `propose` | `POST /api/hub/sites/<handle>/inbox` — evidence in, without trust (unauthenticated by design) |
//! | `subscribe` | `GET /api/hub/brains/<brain>` + `/feed` for a locally verified signed head |
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
//! whole-store snapshots, and `subscribe` reports feed-head movement. The hub
//! signs each committed snapshot in a hash-chained feed with a per-brain
//! Ed25519 identity; this client verifies the content-addressed pack before it
//! touches disk and verifies the signed feed head on every subscription read.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::fsx::write_atomic;
use crate::store::Store;

/// Environment variable naming the hub base URL (e.g. `https://hub.example.com`).
pub const HUB_URL_ENV: &str = "DBMD_HUB_URL";

/// Environment variable carrying the hub bearer credential. The bearer
/// credential source — see the module docs for why it is never store-based.
pub const HUB_KEY_ENV: &str = "DBMD_HUB_KEY";

/// Environment variable naming the PATH of a self-custodied BRAIN key file
/// (link.md §2.4). When set, `sync --push` signs each feed entry locally and
/// ships it through the pack flow — the hub verifies and stores the exact
/// client bytes and can never sign for the brain. Same file format as agent
/// keys (`dbmd key generate`).
pub const BRAIN_KEY_FILE_ENV: &str = "DBMD_BRAIN_KEY_FILE";

/// Environment variable naming the PATH of an agent signing key file
/// (link.md §8 `LinkMD-Sig` proof of possession). When set, authenticated
/// requests are signed per-request with the agent's Ed25519 key instead of
/// carrying a bearer: the signature binds method + path + body + a ±60s
/// window, so nothing reusable ever crosses the wire or lands in a log or an
/// agent transcript. The file holds the base64url PKCS#8 key minted by
/// `dbmd key generate`; the path is not a secret, the file is (mode 0600).
pub const AGENT_KEY_FILE_ENV: &str = "DBMD_AGENT_KEY_FILE";

/// The store-local config file, relative to the store root. Holds non-secret
/// toolkit state (`hub = <URL>`); hidden, so every store walk skips it.
pub const CONFIG_REL_PATH: &str = ".dbmd/config";

/// The most this client will buffer from one hub response. A full-store
/// export is the biggest honest payload; anything past this is refused loudly
/// rather than silently truncated.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

/// Direct JSON pushes stay below the serverless request-body cap. Larger
/// snapshots switch to the bounded object-store pack lane.
const MAX_PUSH_BYTES: usize = 4 * 1024 * 1024;

/// The hub's per-push file-count cap, mirrored client-side.
const MAX_PUSH_FILES: usize = 100_000;
const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;

/// The hub's inbox cap on one `propose` submission body, mirrored client-side
/// so an oversized body fails before the upload, not after (the same
/// fail-before-upload contract as the push caps). Public so the CLI can
/// pre-check a `--body-file` from file metadata without reading it.
pub const MAX_PROPOSE_BYTES: u64 = 16 * 1024;

/// Bounded connect so a dead hub fails fast; a generous read window so a
/// large export on a slow link still completes.
const CONNECT_TIMEOUT_SECS: u64 = 10;
const READ_TIMEOUT_SECS: u64 = 120;
const CONNECT_ATTEMPTS: usize = 3;
const CONNECT_RETRY_BACKOFF_MS: [u64; CONNECT_ATTEMPTS - 1] = [100, 300];

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

    /// The agent signing key file named by [`AGENT_KEY_FILE_ENV`] is missing,
    /// unreadable, or not a valid Ed25519 PKCS#8 — key material is never
    /// echoed.
    #[error("invalid agent signing key ({message}) — mint one with `dbmd key generate`")]
    BadAgentKey {
        /// What failed, without any key material.
        message: String,
    },

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

    /// The store exceeds the hub's bounded whole-snapshot caps.
    #[error(
        "push too large ({detail}) — one snapshot caps at {} MB uncompressed, {} MB compressed, and {MAX_PUSH_FILES} files",
        MAX_STORE_BYTES / (1024 * 1024),
        MAX_PACK_BYTES / (1024 * 1024)
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

    /// A downloaded pack failed validation before any local write.
    #[error("invalid store pack: {message}")]
    InvalidPack {
        /// Hash, ZIP, path, count, or expansion failure.
        message: String,
    },

    /// A signed feed entry, hash chain, or advertised feed head did not verify.
    #[error("invalid signed feed: {message}")]
    InvalidFeed {
        /// The failed integrity condition, without untrusted secret material.
        message: String,
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
    /// The agent signing key, when [`AGENT_KEY_FILE_ENV`] names one. Wins
    /// over the bearer for authenticated requests (link.md §8).
    pub agent_key: Option<AgentSigningKey>,
    /// The self-custodied brain signing key, when [`BRAIN_KEY_FILE_ENV`]
    /// names one — `sync --push` then signs feed entries locally (§2.4).
    pub brain_key: Option<AgentSigningKey>,
}

/// A loaded agent signing key: the PKCS#8 secret plus its derived public
/// multikey. Debug never prints key material.
#[derive(Clone)]
pub struct AgentSigningKey {
    pkcs8: Vec<u8>,
    /// The key's public identity, `ed25519:<base64url sha256(SPKI)>`.
    pub multikey: String,
    /// The full public key, `base64url(SPKI DER)` — what feed entries carry.
    pub public_key_spki: String,
}

impl std::fmt::Debug for AgentSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSigningKey")
            .field("multikey", &self.multikey)
            .field("pkcs8", &"<redacted>")
            .finish()
    }
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

    let agent_key = match env_nonempty(AGENT_KEY_FILE_ENV) {
        Some(path) => Some(load_agent_key(Path::new(&path))?),
        None => None,
    };

    let brain_key = match env_nonempty(BRAIN_KEY_FILE_ENV) {
        Some(path) => Some(load_agent_key(Path::new(&path))?),
        None => None,
    };

    Ok(HubConfig {
        hub,
        key,
        agent_key,
        brain_key,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Agent signing keys — link.md §8 `LinkMD-Sig` proof of possession
// ─────────────────────────────────────────────────────────────────────────────

/// The DER prefix that wraps a raw Ed25519 public key into a
/// SubjectPublicKeyInfo (RFC 8410).
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn bad_agent_key(message: &str) -> LinkError {
    LinkError::BadAgentKey {
        message: message.to_string(),
    }
}

fn agent_keypair(pkcs8: &[u8]) -> LinkResult<ring::signature::Ed25519KeyPair> {
    // `from_pkcs8` wants ring's own v2 encoding (private + public); keys from
    // other tools are often PKCS#8 v1, which `maybe_unchecked` accepts by
    // deriving the public half itself.
    ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8)
        .or_else(|_| ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8))
        .map_err(|_| bad_agent_key("not an Ed25519 PKCS#8 key"))
}

/// Derive `(publicKeySpki b64u, multikey)` from a keypair.
fn public_identity_for(pair: &ring::signature::Ed25519KeyPair) -> (String, String) {
    use ring::signature::KeyPair as _;
    let mut spki = Vec::with_capacity(44);
    spki.extend_from_slice(&ED25519_SPKI_PREFIX);
    spki.extend_from_slice(pair.public_key().as_ref());
    (
        URL_SAFE_NO_PAD.encode(&spki),
        format!("ed25519:{}", URL_SAFE_NO_PAD.encode(Sha256::digest(&spki))),
    )
}

/// Load and validate a signing-key file (agent or brain — same format):
/// one base64url line of PKCS#8. Public so `dbmd key rotate` can load the
/// old key explicitly.
pub fn load_signing_key(path: &Path) -> LinkResult<AgentSigningKey> {
    load_agent_key(path)
}

/// Load and validate the agent key file: one base64url line of PKCS#8.
fn load_agent_key(path: &Path) -> LinkResult<AgentSigningKey> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| bad_agent_key(&format!("cannot read the key file: {e}")))?;
    let pkcs8 = URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| bad_agent_key("the key file is not one base64url line"))?;
    let (public_key_spki, multikey) = public_identity_for(&agent_keypair(&pkcs8)?);
    Ok(AgentSigningKey {
        pkcs8,
        multikey,
        public_key_spki,
    })
}

/// What `dbmd key generate` returns: the public identity to register plus
/// where the secret landed.
#[derive(Debug, Serialize)]
pub struct GeneratedAgentKey {
    /// `ed25519:<fingerprint>` — the grantable/registerable identity.
    pub multikey: String,
    /// base64url SPKI DER — what a hub's register endpoint takes.
    #[serde(rename = "publicKeySpki")]
    pub public_key_spki: String,
    /// Where the PKCS#8 secret was written (mode 0600).
    #[serde(rename = "keyFile")]
    pub key_file: String,
}

/// Mint a fresh Ed25519 agent keypair. The secret is written to `out`
/// (base64url PKCS#8, one line, 0600, refusing to overwrite); only public
/// identity is returned. The private key never enters a store and never
/// travels — requests carry per-request signatures instead (link.md §8).
pub fn generate_agent_key(out: &Path) -> LinkResult<GeneratedAgentKey> {
    if out.exists() {
        return Err(bad_agent_key(
            "the output file already exists — refusing to overwrite a key",
        ));
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| bad_agent_key("key generation failed"))?;
    let pair = agent_keypair(pkcs8.as_ref())?;
    let (spki_b64u, multikey) = public_identity_for(&pair);

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(out, format!("{}\n", URL_SAFE_NO_PAD.encode(pkcs8.as_ref())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(GeneratedAgentKey {
        multikey,
        public_key_spki: spki_b64u,
        key_file: out.display().to_string(),
    })
}

/// Build the `LinkMD-Sig` v1 header for one request:
/// `canonical = "v1" LF METHOD LF path+query LF ts LF (sha256hex(body) | "-")`.
fn linkmd_sig_header(
    key: &AgentSigningKey,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> LinkResult<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| bad_agent_key("system clock is before the epoch"))?
        .as_secs();
    let body_hash = match body {
        Some(b) => format!("{:x}", Sha256::digest(b.as_bytes())),
        None => "-".to_string(),
    };
    let canonical = format!(
        "v1\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        ts,
        body_hash
    );
    let pair = agent_keypair(&key.pkcs8)?;
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(canonical.as_bytes()).as_ref());
    let fingerprint = key.multikey.trim_start_matches("ed25519:");
    Ok(format!(
        "LinkMD-Sig v1,key=ed25519:{fingerprint},ts={ts},sig={sig}"
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Self-custody feed entries — the client signs what the hub only verifies
// ─────────────────────────────────────────────────────────────────────────────

/// One `files` element of a wire-profile-v1 feed entry (SPEC §5.1: fields in
/// exactly this order).
#[derive(Serialize)]
struct WireFeedFile {
    path: String,
    sha256: String,
    bytes: u64,
}

/// The unsigned entry in the normative §5.1 field order — serde serializes
/// struct fields in declaration order, which IS the wire contract.
#[derive(Serialize)]
struct UnsignedWireEntry<'a> {
    v: u8,
    seq: u64,
    ts: String,
    brain: &'a str,
    public_key: &'a str,
    kind: &'a str,
    op: &'a str,
    pack_sha256: &'a str,
    files: &'a [WireFeedFile],
    removed: &'a [String],
    prev_entry_hash: Option<&'a str>,
}

/// Build and sign a wire-profile-v1 `push` feed entry with a self-custodied
/// brain key: serialize the unsigned entry compactly in the normative order,
/// Ed25519-sign those exact bytes, splice `sig` on as the final field. The
/// returned string is the exact serialization the hub stores verbatim (plus
/// one trailing newline) and every independent reader re-derives.
fn self_custody_entry(
    key: &AgentSigningKey,
    seq: u64,
    ts: String,
    pack_sha256: &str,
    files: &[WireFeedFile],
    prev_entry_hash: Option<&str>,
) -> LinkResult<String> {
    let removed: [String; 0] = [];
    let unsigned = serde_json::to_string(&UnsignedWireEntry {
        v: 1,
        seq,
        ts,
        brain: &key.multikey,
        public_key: &key.public_key_spki,
        kind: "push",
        op: "snapshot",
        pack_sha256,
        files,
        removed: &removed,
        prev_entry_hash,
    })
    .expect("serialize feed entry");
    let pair = agent_keypair(&key.pkcs8)?;
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(unsigned.as_bytes()).as_ref());
    Ok(format!(
        "{},\"sig\":\"{}\"}}",
        &unsigned[..unsigned.len() - 1],
        sig
    ))
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
    let parsed = url::Url::parse(hub).map_err(|_| LinkError::UnsafeHub {
        hub: hub.to_string(),
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(LinkError::UnsafeHub {
            hub: hub.to_string(),
        });
    }
    let loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if parsed.scheme().eq_ignore_ascii_case("https") || loopback {
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
    /// Send the configured credential when one exists, otherwise nothing —
    /// brain-addressed propose works anonymously on public brains, and an
    /// authenticated caller earns a bigger actor-class budget.
    Optional,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(concat!("dbmd/", env!("CARGO_PKG_VERSION")))
        // Never follow a redirect while a bearer, a store pack, or a signed
        // response is in flight. Callers see the 3xx as a non-success instead
        // of letting an origin steer sensitive material elsewhere.
        .redirects(0)
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
    let encoded_body = body.map(Value::to_string);
    // An agent signing key outranks the bearer: possession proofs put nothing
    // reusable on the wire, so when both are configured the stronger one wins.
    let credential = match auth {
        Auth::Required => Some(match &cfg.agent_key {
            Some(key) => linkmd_sig_header(key, method, path, encoded_body.as_deref())?,
            None => format!("Bearer {}", cfg.require_key()?),
        }),
        Auth::Optional => match &cfg.agent_key {
            Some(key) => Some(linkmd_sig_header(
                key,
                method,
                path,
                encoded_body.as_deref(),
            )?),
            None => cfg.key.as_deref().map(|k| format!("Bearer {k}")),
        },
        Auth::None => None,
    };
    let http = agent();
    let result = with_connect_retries(|| {
        let mut req = http.request(method, &url);
        if let Some(value) = &credential {
            req = req.set("authorization", value);
        }
        match &encoded_body {
            Some(value) => req
                .set("content-type", "application/json")
                .send_string(value)
                .map_err(Box::new),
            None => req.call().map_err(Box::new),
        }
    });
    let resp = match result {
        Ok(resp) => resp,
        Err(error) => match *error {
            ureq::Error::Status(_, resp) => resp,
            ureq::Error::Transport(error) => {
                return Err(LinkError::Transport {
                    hub: cfg.hub.clone(),
                    message: error.to_string(),
                });
            }
        },
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

/// These failures happen before any HTTP request reaches the hub, so retrying
/// cannot duplicate a mutation. Mid-stream I/O is deliberately excluded: once
/// bytes may have crossed the wire, the caller must rely on the verb's own
/// idempotency contract instead of guessing.
fn is_pre_request_transport(kind: ureq::ErrorKind) -> bool {
    matches!(
        kind,
        ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::ProxyConnect
    )
}

fn with_connect_retries(
    mut send: impl FnMut() -> Result<ureq::Response, Box<ureq::Error>>,
) -> Result<ureq::Response, Box<ureq::Error>> {
    let mut attempt = 0;
    loop {
        match send() {
            Err(error)
                if matches!(
                    error.as_ref(),
                    ureq::Error::Transport(transport)
                        if is_pre_request_transport(transport.kind())
                ) && attempt + 1 < CONNECT_ATTEMPTS =>
            {
                std::thread::sleep(std::time::Duration::from_millis(
                    CONNECT_RETRY_BACKOFF_MS[attempt],
                ));
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn assert_safe_presigned_url(raw: &str) -> LinkResult<()> {
    let parsed = url::Url::parse(raw).map_err(|_| LinkError::InvalidPack {
        message: "the hub returned an invalid object-store URL".to_string(),
    })?;
    if !parsed.scheme().eq_ignore_ascii_case("https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(LinkError::InvalidPack {
            message: "the hub returned an unsafe object-store URL".to_string(),
        });
    }
    Ok(())
}

fn put_presigned(raw: &str, headers: &Value, bytes: &[u8]) -> LinkResult<()> {
    assert_safe_presigned_url(raw)?;
    let http = agent();
    let result = with_connect_retries(|| {
        let mut req = http.put(raw);
        if let Some(map) = headers.as_object() {
            for (name, value) in map {
                if let Some(value) = value.as_str() {
                    req = req.set(name, value);
                }
            }
        }
        req.send_bytes(bytes).map_err(Box::new)
    });
    match result {
        Ok(resp) if resp.status() < 300 => Ok(()),
        Ok(resp) => Err(LinkError::Http {
            what: "pack upload",
            status: resp.status(),
            message: "object store rejected the upload".to_string(),
            code: None,
        }),
        Err(error) => match *error {
            ureq::Error::Status(_, resp) => Err(LinkError::Http {
                what: "pack upload",
                status: resp.status(),
                message: "object store rejected the upload".to_string(),
                code: None,
            }),
            ureq::Error::Transport(err) => Err(LinkError::Transport {
                hub: "the object store".to_string(),
                message: err.to_string(),
            }),
        },
    }
}

fn get_presigned(raw: &str) -> LinkResult<Vec<u8>> {
    assert_safe_presigned_url(raw)?;
    let http = agent();
    let resp = match with_connect_retries(|| http.get(raw).call().map_err(Box::new)) {
        Ok(resp) => resp,
        Err(error) => match *error {
            ureq::Error::Status(_, resp) => {
                return Err(LinkError::Http {
                    what: "pack download",
                    status: resp.status(),
                    message: "object store rejected the download".to_string(),
                    code: None,
                });
            }
            ureq::Error::Transport(err) => {
                return Err(LinkError::Transport {
                    hub: "the object store".to_string(),
                    message: err.to_string(),
                });
            }
        },
    };
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_PACK_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PACK_BYTES {
        return Err(LinkError::InvalidPack {
            message: "download exceeds the compressed-size limit".to_string(),
        });
    }
    Ok(bytes)
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
    let path = format!("/api/hub/brains/{brain}/export?format=pack");
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "sync pull",
    )?;

    let remote_slug = body
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| is_safe_slug(slug));
    let slug = remote_slug
        .or_else(|| is_safe_slug(brain).then_some(brain))
        .unwrap_or("brain")
        .to_string();
    let brain_id = body
        .get("brain")
        .and_then(Value::as_str)
        .unwrap_or(brain)
        .to_string();
    let head_seq = body.get("headSeq").and_then(Value::as_u64).unwrap_or(0);
    let dest: PathBuf = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(&slug),
    };
    let entries =
        if let Some(url) = body.get("url").and_then(Value::as_str) {
            let expected = body
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                .ok_or_else(|| LinkError::InvalidPack {
                    message: "the hub returned an invalid SHA-256".to_string(),
                })?;
            let bytes = get_presigned(url)?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != expected {
                return Err(LinkError::InvalidPack {
                    message: "SHA-256 verification failed".to_string(),
                });
            }
            parse_store_pack(bytes)?
        } else {
            let files = body.get("files").and_then(Value::as_array).ok_or_else(|| {
                LinkError::InvalidPack {
                    message: "the hub returned neither a pack nor a file manifest".to_string(),
                }
            })?;
            let mut entries = Vec::with_capacity(files.len());
            for file in files {
                let path = file.get("path").and_then(Value::as_str).ok_or_else(|| {
                    LinkError::InvalidPack {
                        message: "a file entry has no string path".to_string(),
                    }
                })?;
                let content = file.get("content").and_then(Value::as_str).ok_or_else(|| {
                    LinkError::InvalidPack {
                        message: format!("file `{path}` has no string content"),
                    }
                })?;
                entries.push((path.to_string(), content.as_bytes().to_vec()));
            }
            entries
        };

    // Gate the complete manifest before the first filesystem mutation.
    let mut seen = std::collections::HashSet::new();
    for (path, _) in &entries {
        if !safe_store_rel_path(path) {
            return Err(LinkError::UnsafePath { path: path.clone() });
        }
        if !seen.insert(path) {
            return Err(LinkError::InvalidPack {
                message: format!("duplicate path `{path}`"),
            });
        }
    }
    std::fs::create_dir_all(&dest)?;
    let real_dest = std::fs::canonicalize(&dest)?;

    for (p, content) in &entries {
        let abs = dest.join(p);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
            let real_parent = std::fs::canonicalize(parent)?;
            if !real_parent.starts_with(&real_dest) {
                return Err(LinkError::UnsafePath { path: p.clone() });
            }
        }
        if std::fs::symlink_metadata(&abs).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(LinkError::UnsafePath { path: p.clone() });
        }
        write_atomic(&abs, content)?;
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

fn is_safe_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 63
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn parse_store_pack(bytes: Vec<u8>) -> LinkResult<Vec<(String, Vec<u8>)>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|err| LinkError::InvalidPack {
            message: format!("ZIP parse failed: {err}"),
        })?;
    if archive.is_empty() || archive.len() > MAX_PUSH_FILES {
        return Err(LinkError::InvalidPack {
            message: format!("invalid file count {}", archive.len()),
        });
    }
    let mut total = 0u64;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|err| LinkError::InvalidPack {
                message: format!("ZIP entry failed: {err}"),
            })?;
        if file.is_dir() {
            continue;
        }
        let path = file.name().to_string();
        if file.enclosed_name().is_none() || !safe_store_rel_path(&path) {
            return Err(LinkError::UnsafePath { path });
        }
        if file
            .unix_mode()
            .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000))
        {
            return Err(LinkError::InvalidPack {
                message: format!("non-file entry `{path}`"),
            });
        }
        total = total.saturating_add(file.size());
        if total > MAX_STORE_BYTES {
            return Err(LinkError::InvalidPack {
                message: "expanded content exceeds the 512 MB limit".to_string(),
            });
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content)
            .map_err(|err| LinkError::InvalidPack {
                message: format!("could not decompress `{path}`: {err}"),
            })?;
        if content.len() as u64 != file.size() {
            return Err(LinkError::InvalidPack {
                message: format!("length mismatch for `{path}`"),
            });
        }
        entries.push((path, content));
    }
    if entries.is_empty() {
        return Err(LinkError::InvalidPack {
            message: "pack contains no files".to_string(),
        });
    }
    Ok(entries)
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
    let raw_total: u64 = files.iter().map(|(_, content)| content.len() as u64).sum();
    if raw_total > MAX_STORE_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: format!("{raw_total} uncompressed bytes"),
        });
    }

    // Self-custody (a brain key is configured): the JSON fast path is
    // hub-signed by construction, so every push goes through the pack flow
    // with a locally signed entry — the hub verifies and can never sign.
    if cfg.brain_key.is_none() {
        let body = json!({
            "files": files
                .iter()
                .map(|(p, c)| json!({ "path": p, "content": c }))
                .collect::<Vec<_>>(),
        });
        if body.to_string().len() <= MAX_PUSH_BYTES {
            let path = format!("/api/hub/brains/{brain}/push");
            return ensure_ok(
                request(cfg, "POST", &path, Some(&body), Auth::Required)?,
                "sync push",
            );
        }
    }

    let pack = build_store_pack(files)?;
    if pack.len() as u64 > MAX_PACK_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: format!("{} compressed bytes", pack.len()),
        });
    }
    let sha256 = format!("{:x}", Sha256::digest(&pack));
    let mut meta = json!({ "sha256": sha256, "bytes": pack.len() });
    if let Some(key) = &cfg.brain_key {
        // Head state pins seq + prev; a concurrent writer surfaces as the
        // hub's 422 on commit (re-run to retry against the new head).
        let current = head(cfg, brain)?;
        let mut manifest: Vec<WireFeedFile> = files
            .iter()
            .map(|(path, content)| WireFeedFile {
                path: path.clone(),
                sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
                bytes: content.len() as u64,
            })
            .collect();
        manifest.sort_by(|a, b| a.path.cmp(&b.path));
        let ts = crate::now()
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let entry = self_custody_entry(
            key,
            current.seq + 1,
            ts,
            &sha256,
            &manifest,
            current.feed_hash.as_deref(),
        )?;
        meta["entry"] = Value::String(entry);
    }
    let presigned = ensure_ok(
        request(
            cfg,
            "POST",
            &format!("/api/hub/brains/{brain}/packs/presign"),
            Some(&meta),
            Auth::Required,
        )?,
        "prepare pack upload",
    )?;
    let url = presigned
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| LinkError::InvalidPack {
            message: "the hub returned no upload URL".to_string(),
        })?;
    put_presigned(url, presigned.get("headers").unwrap_or(&Value::Null), &pack)?;
    ensure_ok(
        request(
            cfg,
            "POST",
            &format!("/api/hub/brains/{brain}/packs/commit"),
            Some(&meta),
            Auth::Required,
        )?,
        "commit pack",
    )
}

fn build_store_pack(files: &[(String, String)]) -> LinkResult<Vec<u8>> {
    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o600);
    for (path, content) in sorted {
        writer
            .start_file(path, options)
            .map_err(|err| LinkError::InvalidPack {
                message: format!("could not create ZIP entry `{path}`: {err}"),
            })?;
        writer.write_all(content.as_bytes())?;
    }
    writer
        .finish()
        .map(Cursor::into_inner)
        .map_err(|err| LinkError::InvalidPack {
            message: format!("could not finish ZIP: {err}"),
        })
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
    // Grantee shape decides the axis: a base64url Ed25519 SPKI is a bare
    // multikey holder (link.md §6 cross-party keys — no hub account; the
    // printed `publicKeySpki` from `dbmd key generate`); anything else is a
    // hub principal named by email.
    let is_key_grantee = URL_SAFE_NO_PAD
        .decode(grantee)
        .map(|der| der.len() == 44 && der.starts_with(&ED25519_SPKI_PREFIX))
        .unwrap_or(false);
    let mut body = if is_key_grantee {
        json!({ "keySpki": grantee, "capability": can.as_str() })
    } else {
        json!({ "email": grantee, "capability": can.as_str() })
    };
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
    // A ULID-shaped target is a bare brain address (link.md §7.4's
    // generalization): the brain inbox door, open on public brains, where a
    // configured credential earns a bigger actor-class budget. Anything else
    // is a published-site handle: that door is unauthenticated by design.
    let (path, auth) = if crate::ulid::is_ulid(handle) {
        (format!("/api/hub/brains/{handle}/inbox"), Auth::Optional)
    } else {
        (format!("/api/hub/sites/{handle}/inbox"), Auth::None)
    };
    ensure_ok(
        request(cfg, "POST", &path, Some(&payload), auth)?,
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
    /// The hub's durable feed cursor — advances on every accepted write.
    pub seq: u64,
    /// The hub's `updatedAt` for the brain, when present.
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// SHA-256 of the exact signed head entry.
    #[serde(rename = "feedHash", skip_serializing_if = "Option::is_none")]
    pub feed_hash: Option<String>,
    /// Whether the head entry's content hash, identity, and Ed25519 signature
    /// were verified locally. Path-scoped grants get head movement only.
    pub verified: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct FeedFile {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct FeedEntry {
    v: u8,
    seq: u64,
    ts: String,
    brain: String,
    public_key: String,
    kind: String,
    op: String,
    pack_sha256: String,
    files: Vec<FeedFile>,
    removed: Vec<String>,
    prev_entry_hash: Option<String>,
    sig: String,
}

#[derive(Serialize)]
struct UnsignedFeedEntry<'a> {
    v: u8,
    seq: u64,
    ts: &'a str,
    brain: &'a str,
    public_key: &'a str,
    kind: &'a str,
    op: &'a str,
    pack_sha256: &'a str,
    files: &'a [FeedFile],
    removed: &'a [String],
    prev_entry_hash: &'a Option<String>,
}

#[derive(Debug, Deserialize)]
struct FeedItem {
    hash: String,
    entry: FeedEntry,
}

#[derive(Debug, Deserialize)]
struct FeedIdentity {
    fingerprint: String,
    #[serde(rename = "publicKeySpki")]
    public_key_spki: String,
    /// Rotation history (link.md §9.1): identities this brain previously
    /// signed as. Entries verify against current OR previous — rotation
    /// never invalidates history.
    #[serde(default)]
    previous: Vec<PreviousIdentity>,
}

#[derive(Debug, Deserialize)]
struct PreviousIdentity {
    fingerprint: String,
    #[serde(rename = "publicKeySpki")]
    public_key_spki: String,
}

#[derive(Debug, Deserialize)]
struct FeedResponse {
    #[serde(rename = "headSeq")]
    head_seq: u64,
    #[serde(rename = "feedHash")]
    feed_hash: Option<String>,
    identity: Option<FeedIdentity>,
    entries: Vec<FeedItem>,
    #[serde(rename = "scopeLimited")]
    scope_limited: bool,
}

fn invalid_feed(message: impl Into<String>) -> LinkError {
    LinkError::InvalidFeed {
        message: message.into(),
    }
}

fn verify_feed_item(item: &FeedItem, identity: &FeedIdentity) -> LinkResult<()> {
    const ED25519_SPKI_PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let entry = &item.entry;
    let public_der = URL_SAFE_NO_PAD
        .decode(&entry.public_key)
        .map_err(|_| invalid_feed("public key is not base64url"))?;
    if public_der.len() != ED25519_SPKI_PREFIX.len() + 32
        || !public_der.starts_with(ED25519_SPKI_PREFIX)
        || identity.public_key_spki != entry.public_key
    {
        return Err(invalid_feed("public key does not match the brain card"));
    }
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&public_der));
    if entry.brain != format!("ed25519:{fingerprint}") {
        return Err(invalid_feed(
            "brain fingerprint does not match its public key",
        ));
    }
    // The signer must be the brain's CURRENT identity or a PREVIOUS one
    // (link.md §9.1 rotation) — never an arbitrary self-consistent key.
    let known = fingerprint == identity.fingerprint
        || identity
            .previous
            .iter()
            .any(|p| p.fingerprint == fingerprint && p.public_key_spki == entry.public_key);
    if !known {
        return Err(invalid_feed(
            "entry signer is not this brain's identity (current or rotated-from)",
        ));
    }
    let unsigned = UnsignedFeedEntry {
        v: entry.v,
        seq: entry.seq,
        ts: &entry.ts,
        brain: &entry.brain,
        public_key: &entry.public_key,
        kind: &entry.kind,
        op: &entry.op,
        pack_sha256: &entry.pack_sha256,
        files: &entry.files,
        removed: &entry.removed,
        prev_entry_hash: &entry.prev_entry_hash,
    };
    let message =
        serde_json::to_vec(&unsigned).map_err(|_| invalid_feed("could not canonicalize entry"))?;
    let signature = URL_SAFE_NO_PAD
        .decode(&entry.sig)
        .map_err(|_| invalid_feed("signature is not base64url"))?;
    UnparsedPublicKey::new(&ED25519, &public_der[ED25519_SPKI_PREFIX.len()..])
        .verify(&message, &signature)
        .map_err(|_| invalid_feed("Ed25519 signature verification failed"))?;

    let mut exact = serde_json::to_vec(entry).map_err(|_| invalid_feed("could not hash entry"))?;
    exact.push(b'\n');
    let actual_hash = format!("{:x}", Sha256::digest(&exact));
    if actual_hash != item.hash {
        return Err(invalid_feed("entry SHA-256 does not match"));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// key rotation — link.md §9.1: the new key, signed by the old one
// ─────────────────────────────────────────────────────────────────────────────

/// The unsigned rotation statement in its normative field order.
#[derive(Serialize)]
struct UnsignedRotation<'a> {
    v: u8,
    op: &'a str,
    brain: &'a str,
    public_key: &'a str,
    new_brain: &'a str,
    new_public_key: &'a str,
    ts: String,
}

/// What `dbmd key rotate` returns.
#[derive(Debug, Serialize)]
pub struct RotationReport {
    /// The brain id rotated.
    pub brain: String,
    /// The NEW identity the hub now serves.
    pub multikey: String,
    /// Where the new PKCS#8 secret landed (0600).
    #[serde(rename = "keyFile")]
    pub key_file: String,
    /// Prior identities (newest first) the feed still verifies against.
    pub previous: Vec<String>,
}

/// Rotate a self-custodied brain's key: mint a fresh keypair, build the
/// §9.1 statement — the new key, signed by the OLD key, normative
/// serialization — send it to the hub, and only after the hub accepts write
/// the new secret to `out` (0600, refusing overwrite). The old key file is
/// left untouched for the owner to retire.
pub fn rotate_brain_key(
    cfg: &HubConfig,
    brain: &str,
    old_key: &AgentSigningKey,
    out: &Path,
) -> LinkResult<RotationReport> {
    require_safe_ref(brain)?;
    if out.exists() {
        return Err(bad_agent_key(
            "the output file already exists — refusing to overwrite a key",
        ));
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| bad_agent_key("key generation failed"))?;
    let pair = agent_keypair(pkcs8.as_ref())?;
    let (new_spki, new_multikey) = public_identity_for(&pair);

    let ts = crate::now()
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let unsigned = serde_json::to_string(&UnsignedRotation {
        v: 1,
        op: "rotate",
        brain: &old_key.multikey,
        public_key: &old_key.public_key_spki,
        new_brain: &new_multikey,
        new_public_key: &new_spki,
        ts,
    })
    .expect("serialize rotation");
    let old_pair = agent_keypair(&old_key.pkcs8)?;
    let sig = URL_SAFE_NO_PAD.encode(old_pair.sign(unsigned.as_bytes()).as_ref());
    let statement = format!("{},\"sig\":\"{}\"}}", &unsigned[..unsigned.len() - 1], sig);

    let body = json!({ "statement": statement });
    let path = format!("/api/hub/brains/{brain}/rotate");
    let response = ensure_ok(
        request(cfg, "POST", &path, Some(&body), Auth::Required)?,
        "key rotate",
    )?;
    let previous = response
        .get("identity")
        .and_then(|i| i.get("previous"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("fingerprint").and_then(Value::as_str))
                .map(|f| format!("ed25519:{f}"))
                .collect()
        })
        .unwrap_or_default();

    std::fs::write(out, format!("{}\n", URL_SAFE_NO_PAD.encode(pkcs8.as_ref())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(RotationReport {
        brain: brain.to_string(),
        multikey: new_multikey,
        key_file: out.display().to_string(),
        previous,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// mirror — verified replication: the whole feed + files, re-servable
// ─────────────────────────────────────────────────────────────────────────────

/// What `dbmd mirror` materialized.
#[derive(Debug, Serialize)]
pub struct MirrorReport {
    /// The brain id.
    pub brain: String,
    /// The mirrored feed head.
    #[serde(rename = "headSeq")]
    pub head_seq: u64,
    /// The head entry hash (the feed's advertised converged state).
    #[serde(rename = "feedHash")]
    pub feed_hash: Option<String>,
    /// Signed feed entries verified and stored.
    pub entries: u64,
    /// The brain's multikey, pinned in `.dbmd/config` (TOFU).
    pub pinned: String,
    /// Store files materialized by the pull.
    pub files: usize,
}

/// The mirror state directory, relative to the mirror root.
pub const MIRROR_REL_DIR: &str = ".dbmd/mirror";

/// SHA-256 hex of one feed entry's stored bytes (`exact JSON + "\n"`) — the
/// entry hash every consumer recomputes (SPEC §5.3).
pub fn feed_entry_hash(exact_sans_newline: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{exact_sans_newline}\n").as_bytes())
    )
}

fn config_pin(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pin") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Replicate a brain with full verification (link.md §5.4 over the WHOLE
/// chain, not just the head): every entry's signature, hash, sequence
/// contiguity, and prev-hash linkage are checked before its exact bytes are
/// stored under `.dbmd/mirror/feed/<seq>.json`; the identity is pinned in
/// `.dbmd/config` (trust-on-first-use — a later mirror against a different
/// identity refuses); the store files are pulled beside it. feed + files =
/// the provable full copy, re-servable by `dbmd serve` — signatures survive
/// re-hosting, which is what makes the export an export.
pub fn mirror(cfg: &HubConfig, brain: &str, dest: &Path) -> LinkResult<MirrorReport> {
    require_safe_ref(brain)?;
    let card = head(cfg, brain)?;
    let brain_id = card.brain.clone();

    let mirror_dir = dest.join(MIRROR_REL_DIR);
    let feed_dir = mirror_dir.join("feed");
    std::fs::create_dir_all(&feed_dir)?;

    let mut expected_seq: u64 = 1;
    let mut prev_hash: Option<String> = None;
    let mut identity: Option<FeedIdentity> = None;
    let mut stored: u64 = 0;
    let advertised: Option<String> = loop {
        let path = format!(
            "/api/hub/brains/{brain_id}/feed?after={}&limit=100",
            expected_seq - 1
        );
        let body = ensure_ok(request(cfg, "GET", &path, None, Auth::Required)?, "mirror")?;
        let page: FeedResponse = serde_json::from_value(body)
            .map_err(|_| invalid_feed("feed response did not parse"))?;
        if page.scope_limited {
            return Err(invalid_feed(
                "this grant is path-scoped — mirroring needs full-store read",
            ));
        }
        let page_identity = page
            .identity
            .ok_or_else(|| invalid_feed("feed response carried no identity"))?;
        if let Some(existing) = &identity {
            if existing.fingerprint != page_identity.fingerprint {
                return Err(invalid_feed("identity changed mid-mirror"));
            }
        }
        for item in &page.entries {
            if item.entry.seq != expected_seq {
                return Err(invalid_feed(format!(
                    "expected entry {expected_seq}, feed served {}",
                    item.entry.seq
                )));
            }
            if item.entry.prev_entry_hash != prev_hash {
                return Err(invalid_feed(format!(
                    "entry {} does not chain to its predecessor",
                    item.entry.seq
                )));
            }
            verify_feed_item(item, &page_identity)?;
            let mut exact = serde_json::to_vec(&item.entry)
                .map_err(|_| invalid_feed("could not serialize entry"))?;
            exact.push(b'\n');
            crate::fsx::write_atomic(&feed_dir.join(format!("{}.json", item.entry.seq)), &exact)?;
            prev_hash = Some(item.hash.clone());
            expected_seq += 1;
            stored += 1;
        }
        identity = Some(page_identity);
        if expected_seq > page.head_seq || page.entries.is_empty() {
            if expected_seq <= page.head_seq {
                return Err(invalid_feed("feed page was empty before the head"));
            }
            break page.feed_hash.clone();
        }
    };
    if prev_hash != advertised {
        return Err(invalid_feed(
            "the verified chain does not converge on the advertised head",
        ));
    }
    let identity = identity.ok_or_else(|| invalid_feed("brain has no identity"))?;
    let multikey = format!("ed25519:{}", identity.fingerprint);

    // TOFU pin: first mirror writes it; every later mirror must match it.
    let config_path = dest.join(CONFIG_REL_PATH);
    match config_pin(&config_path) {
        Some(pinned) if pinned != multikey => {
            return Err(invalid_feed(format!(
                "pinned identity {pinned} does not match served identity {multikey} — refusing"
            )));
        }
        Some(_) => {}
        None => {
            let mut text = std::fs::read_to_string(&config_path).unwrap_or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("pin = {multikey}\n"));
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::fsx::write_atomic(&config_path, text.as_bytes())?;
        }
    }

    let previous: Vec<serde_json::Value> = identity
        .previous
        .iter()
        .map(|p| {
            serde_json::json!({
                "fingerprint": p.fingerprint,
                "publicKeySpki": p.public_key_spki,
            })
        })
        .collect();
    crate::fsx::write_atomic(
        &mirror_dir.join("identity.json"),
        format!(
            "{}\n",
            serde_json::json!({
                "fingerprint": identity.fingerprint,
                "publicKeySpki": identity.public_key_spki,
                "previous": previous,
            })
        )
        .as_bytes(),
    )?;
    crate::fsx::write_atomic(
        &mirror_dir.join("head.json"),
        format!(
            "{}\n",
            serde_json::json!({
                "brain": brain_id,
                "headSeq": card.seq,
                "feedHash": prev_hash,
            })
        )
        .as_bytes(),
    )?;

    let pulled = sync_pull(cfg, &brain_id, Some(dest))?;
    Ok(MirrorReport {
        brain: brain_id,
        head_seq: card.seq,
        feed_hash: prev_hash,
        entries: stored,
        pinned: multikey,
        files: pulled.files,
    })
}

/// Read and locally verify the brain's current signed feed head. `subscribe`
/// polls this as movement detection; the caller re-pulls or re-queries after
/// an advance.
pub fn head(cfg: &HubConfig, brain: &str) -> LinkResult<Head> {
    require_safe_ref(brain)?;
    let path = format!("/api/hub/brains/{brain}");
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "subscribe",
    )?;
    let resolved_brain = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(brain)
        .to_string();
    let seq = body.get("headSeq").and_then(Value::as_u64).unwrap_or(0);
    let advertised_hash = body
        .get("feedHash")
        .and_then(Value::as_str)
        .map(str::to_string);
    let updated_at = body
        .get("updatedAt")
        .and_then(Value::as_str)
        .map(str::to_string);
    if seq == 0 {
        return Ok(Head {
            brain: resolved_brain,
            seq,
            updated_at,
            feed_hash: None,
            verified: true,
        });
    }

    let feed_value = ensure_ok(
        request(
            cfg,
            "GET",
            &format!("/api/hub/brains/{brain}/feed?after={}&limit=1", seq - 1),
            None,
            Auth::Required,
        )?,
        "subscribe feed",
    )?;
    let feed: FeedResponse = serde_json::from_value(feed_value)
        .map_err(|_| invalid_feed("hub returned an invalid feed shape"))?;
    if feed.head_seq != seq || feed.feed_hash != advertised_hash {
        return Err(invalid_feed("brain card and feed head disagree"));
    }
    if feed.scope_limited {
        return Ok(Head {
            brain: resolved_brain,
            seq,
            updated_at,
            feed_hash: advertised_hash,
            verified: false,
        });
    }
    let identity = feed
        .identity
        .as_ref()
        .ok_or_else(|| invalid_feed("feed has no brain identity"))?;
    let item = feed
        .entries
        .first()
        .ok_or_else(|| invalid_feed("feed head entry is missing"))?;
    if item.entry.seq != seq || Some(&item.hash) != advertised_hash.as_ref() {
        return Err(invalid_feed(
            "advertised feed hash does not address the head entry",
        ));
    }
    verify_feed_item(item, identity)?;
    Ok(Head {
        brain: resolved_brain,
        seq,
        updated_at,
        feed_hash: advertised_hash,
        verified: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_feed_item_verifies_identity_hash_and_signature() {
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair};

        const PREFIX: &[u8] = &[
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut spki = PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let public_key = URL_SAFE_NO_PAD.encode(&spki);
        let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&spki));
        let mut entry = FeedEntry {
            v: 1,
            seq: 1,
            ts: "2026-07-14T00:00:00.000Z".to_string(),
            brain: format!("ed25519:{fingerprint}"),
            public_key: public_key.clone(),
            kind: "push".to_string(),
            op: "snapshot".to_string(),
            pack_sha256: "a".repeat(64),
            files: vec![FeedFile {
                path: "DB.md".to_string(),
                sha256: "b".repeat(64),
                bytes: 3,
            }],
            removed: vec![],
            prev_entry_hash: None,
            sig: String::new(),
        };
        let unsigned = UnsignedFeedEntry {
            v: entry.v,
            seq: entry.seq,
            ts: &entry.ts,
            brain: &entry.brain,
            public_key: &entry.public_key,
            kind: &entry.kind,
            op: &entry.op,
            pack_sha256: &entry.pack_sha256,
            files: &entry.files,
            removed: &entry.removed,
            prev_entry_hash: &entry.prev_entry_hash,
        };
        entry.sig =
            URL_SAFE_NO_PAD.encode(pair.sign(&serde_json::to_vec(&unsigned).unwrap()).as_ref());
        let mut exact = serde_json::to_vec(&entry).unwrap();
        exact.push(b'\n');
        let item = FeedItem {
            hash: format!("{:x}", Sha256::digest(&exact)),
            entry,
        };
        let identity = FeedIdentity {
            fingerprint,
            public_key_spki: public_key,
            previous: Vec::new(),
        };
        assert!(verify_feed_item(&item, &identity).is_ok());
        let mut tampered = item;
        tampered.entry.pack_sha256 = "c".repeat(64);
        assert!(verify_feed_item(&tampered, &identity).is_err());
    }

    #[test]
    fn a_self_custody_entry_verifies_like_any_hub_entry() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let (spki, multikey) = public_identity_for(&pair);
        let key = AgentSigningKey {
            pkcs8: pkcs8.as_ref().to_vec(),
            multikey: multikey.clone(),
            public_key_spki: spki.clone(),
        };
        let files = vec![WireFeedFile {
            path: "DB.md".to_string(),
            sha256: "a".repeat(64),
            bytes: 3,
        }];
        let raw = self_custody_entry(
            &key,
            1,
            "2026-07-23T12:00:00.000Z".to_string(),
            &"c".repeat(64),
            &files,
            None,
        )
        .unwrap();
        // The exact client serialization parses as a feed entry and passes the
        // SAME verifier every subscribe read runs — the self-custody path
        // produces first-class wire-profile-v1 entries.
        let entry: FeedEntry = serde_json::from_str(&raw).unwrap();
        let hash = format!("{:x}", Sha256::digest(format!("{raw}\n").as_bytes()));
        let item = FeedItem { hash, entry };
        let identity = FeedIdentity {
            fingerprint: multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: spki,
            previous: Vec::new(),
        };
        assert!(verify_feed_item(&item, &identity).is_ok());
    }

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
        assert!(matches!(
            assert_safe_hub("http://localhost:80@127.0.0.1:1"),
            Err(LinkError::UnsafeHub { .. })
        ));
        assert!(matches!(
            assert_safe_hub("https://hub.example.com@attacker.example"),
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
            agent_key: None,
            brain_key: None,
        }
    }

    #[test]
    fn request_retries_a_connection_failure_before_sending() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let listener = TcpListener::bind(address).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut request_bytes = [0_u8; 1024];
            let _ = stream.read(&mut request_bytes).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                )
                .unwrap();
        });
        let cfg = HubConfig {
            hub: format!("http://{address}"),
            key: None,
            agent_key: None,
            brain_key: None,
        };

        let response = request(&cfg, "GET", "/retry", None, Auth::None).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, Some(json!({ "ok": true })));
        server.join().unwrap();
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
