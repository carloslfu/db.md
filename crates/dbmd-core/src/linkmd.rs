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
//! | `resolve` | `GET /api/hub/brains/<brain>` (the brain card); records are read from the exact signed snapshot pack named by that card's verified feed head |
//! | `sync` (pull) | `GET /api/hub/brains/<brain>/export?format=pack&atSeq=<n>&feedHash=<hash>` — the exact verified snapshot |
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
//! tree is one commit or one push away from leaking. A store-selected hub may
//! receive an ambient bearer or agent key only when
//! `DBMD_HUB_CREDENTIAL_ORIGIN` binds it to that exact origin. Identity pins
//! and monotonic feed checkpoints live in the user's global state directory,
//! never in store-controlled `.dbmd/`.
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
//! Ed25519 identity. This client verifies the rotation chain, signer epochs,
//! monotonic feed checkpoint, snapshot token, and content-addressed pack before
//! untrusted bytes touch their destination.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::Store;

/// Environment variable naming the hub base URL (e.g. `https://hub.example.com`).
pub const HUB_URL_ENV: &str = "DBMD_HUB_URL";

/// Environment variable carrying the hub bearer credential. The bearer
/// credential source — see the module docs for why it is never store-based.
pub const HUB_KEY_ENV: &str = "DBMD_HUB_KEY";

/// Explicit origin binding required before an ambient bearer or agent key may
/// be sent to a hub selected by untrusted store-local configuration. The value
/// is an origin (`https://host[:port]`), not an arbitrary URL path.
pub const HUB_CREDENTIAL_ORIGIN_ENV: &str = "DBMD_HUB_CREDENTIAL_ORIGIN";

/// Override for dbmd's user-owned trust/checkpoint root. Must be absolute.
/// Intended for managed installations and hermetic tests; untrusted stores
/// cannot select it.
pub const STATE_DIR_ENV: &str = "DBMD_STATE_DIR";

/// Explicit test/development escape hatch for registry homes on loopback or
/// private networks. Production federation rejects every non-public resolved
/// address and pins the validated DNS answer into the HTTP agent.
pub const ALLOW_PRIVATE_REGISTRY_HOME_ENV: &str = "DBMD_ALLOW_PRIVATE_REGISTRY_HOME";

/// Explicit development escape hatch for object-store URLs on loopback/private
/// networks. Production hubs must return public HTTPS URLs. A local loopback hub
/// is allowed to use local object URLs without this switch.
pub const ALLOW_PRIVATE_OBJECT_URL_ENV: &str = "DBMD_ALLOW_PRIVATE_OBJECT_URL";

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

/// Control-plane JSON is metadata, never the store itself. Exact snapshot
/// bytes travel through the separately bounded pack lane.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
/// A feed page can legitimately carry a large snapshot manifest, but remains
/// far below a full pack and is parsed with count-bounded sequence visitors.
const MAX_FEED_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
/// Foreign registry cards are identity metadata only.
const MAX_REGISTRY_CARD_BYTES: u64 = 1024 * 1024;

/// Direct JSON pushes stay below the serverless request-body cap. Larger
/// snapshots switch to the bounded object-store pack lane.
const MAX_PUSH_BYTES: usize = 4 * 1024 * 1024;

/// Canonical raw ZIP32 uses a u16 entry count and stores each UTF-8 name twice.
const MAX_PUSH_FILES: usize = u16::MAX as usize;
const MAX_STORE_PATH_BYTES: usize = 1_024;
const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;
/// Exact worst case for the canonical STORED profile:
/// payload + (local 30 + central 46 + name twice) per entry + EOCD 22.
const MAX_PACK_BYTES: u64 =
    MAX_STORE_BYTES + MAX_PUSH_FILES as u64 * (76 + 2 * MAX_STORE_PATH_BYTES) as u64 + 22;
/// A legitimate brain should rotate rarely. Bound adversarial identity
/// histories before allocating and repeatedly verifying an unbounded chain.
const MAX_IDENTITY_ROTATIONS: usize = 1_024;
/// A client never needs to replay an unbounded feed in one invocation. Large
/// histories are mirrored incrementally; a first mirror beyond this cap needs a
/// checkpoint/export rather than allocating attacker-controlled metadata.
const MAX_FEED_REPLAY_ENTRIES: u64 = 100_000;
const MAX_FEED_REPLAY_BYTES: u64 = 64 * 1024 * 1024;
const FEED_PAGE_LIMIT: usize = 100;

/// The hub's inbox cap on one `propose` submission body, mirrored client-side
/// so an oversized body fails before the upload, not after (the same
/// fail-before-upload contract as the push caps). Public so the CLI can
/// pre-check a `--body-file` from file metadata without reading it.
pub const MAX_PROPOSE_BYTES: u64 = 16 * 1024;

/// Bounded connect so a dead hub fails fast; a generous read window so a
/// large export on a slow link still completes.
const CONNECT_TIMEOUT_SECS: u64 = 10;
const READ_TIMEOUT_SECS: u64 = 120;
/// Hard wall-clock budget for one HTTP attempt, spanning socket writes,
/// response headers, and the complete response body. DNS performed for pinned
/// agents has its own interruptible deadline below.
const OVERALL_REQUEST_TIMEOUT_SECS: u64 = 120;
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

    /// A store selected the destination while an ambient credential was
    /// present, but the operator did not bind that credential to the same
    /// origin. This is a hard refusal, not an anonymous fallback: silently
    /// dropping a credential can turn an intended private operation into a
    /// confusing public one.
    #[error(
        "refusing to send an ambient credential to the hub selected by {CONFIG_REL_PATH} — set {HUB_CREDENTIAL_ORIGIN_ENV} to that exact origin, or choose the hub explicitly with --hub/{HUB_URL_ENV}"
    )]
    UnboundCredential,

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

    /// The hub response exceeded the selected endpoint's byte cap.
    #[error("hub response exceeded the {limit_bytes}-byte endpoint cap — refusing to buffer it")]
    ResponseTooLarge {
        /// The endpoint-specific cap applied before JSON parsing.
        limit_bytes: u64,
    },

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
        "push too large ({detail}) — one snapshot caps at {} MB uncompressed, {} MB as a pack, and {MAX_PUSH_FILES} files",
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

    /// This build cannot provide the no-follow, directory-handle-relative
    /// filesystem semantics required for trust/key/snapshot state.
    #[error(
        "{operation} is unavailable on this platform — use the official macOS/Linux build or WSL"
    )]
    UnsupportedPlatform {
        /// The operation that requires hardened local filesystem primitives.
        operation: &'static str,
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

/// Security-sensitive link.md state and destination writes rely on Unix
/// `openat`/`O_NOFOLLOW` semantics. Native Windows is not an official release
/// target yet, so fail closed there instead of silently using a weaker
/// path-based approximation.
fn require_hardened_filesystem(operation: &'static str) -> LinkResult<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let _ = operation;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(LinkError::UnsupportedPlatform { operation })
    }
}

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
    if p.is_empty() || p.len() > MAX_STORE_PATH_BYTES || p.starts_with('/') {
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
    /// User-owned global toolkit state root. Identity pins and monotonic feed
    /// checkpoints live below `<state_dir>/trust/`, never under a store.
    pub state_dir: PathBuf,
    /// True only when the origin came from untrusted store-local configuration.
    /// Such origins are resolved and public-IP-pinned for every request.
    store_selected: bool,
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
    let explicit_hub = flag_hub
        .map(str::to_string)
        .or_else(|| env_nonempty(HUB_URL_ENV));
    let selected_by_store = explicit_hub.is_none();
    let hub = explicit_hub
        .or_else(|| config_file_hub(&dir.join(CONFIG_REL_PATH)))
        .ok_or(LinkError::NoHub)?;
    let hub = hub.trim().trim_end_matches('/').to_string();
    assert_safe_hub(&hub)?;
    if selected_by_store {
        let parsed =
            url::Url::parse(&hub).map_err(|_| LinkError::UnsafeHub { hub: hub.clone() })?;
        // A cloned store is not an operator opt-in to local-network access.
        // Local/private development hubs must be selected explicitly by flag or
        // environment, never by bytes inside the store.
        if !parsed.scheme().eq_ignore_ascii_case("https")
            || (parsed.path() != "/" && !parsed.path().is_empty())
        {
            return Err(LinkError::UnsafeHub { hub });
        }
    }

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

    // A cloned store controls `.dbmd/config`. It must never be able to point
    // the process at an attacker origin and harvest the user's ambient account
    // bearer, signed agent identity, or self-custodied brain signatures/content.
    // Explicit --hub/DBMD_HUB_URL selection already pairs target + credential
    // in the invocation environment; a store-selected target additionally
    // needs an exact origin binding.
    if selected_by_store && (key.is_some() || agent_key.is_some() || brain_key.is_some()) {
        let bound = env_nonempty(HUB_CREDENTIAL_ORIGIN_ENV)
            .and_then(|value| normalized_origin(&value).ok());
        let selected_origin = normalized_origin(&hub)?;
        if bound.as_deref() != Some(selected_origin.as_str()) {
            return Err(LinkError::UnboundCredential);
        }
    }

    Ok(HubConfig {
        hub,
        key,
        agent_key,
        brain_key,
        state_dir: toolkit_state_dir()?,
        store_selected: selected_by_store,
    })
}

fn toolkit_state_dir() -> LinkResult<PathBuf> {
    if let Some(path) = env_nonempty(STATE_DIR_ENV) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(LinkError::UnsafePath {
                path: path.display().to_string(),
            });
        }
        return Ok(path);
    }
    #[cfg(windows)]
    if let Some(base) = env_nonempty("LOCALAPPDATA") {
        let base = PathBuf::from(base);
        if base.is_absolute() {
            return Ok(base.join("dbmd").join("state"));
        }
    }
    #[cfg(not(windows))]
    if let Some(base) = env_nonempty("XDG_STATE_HOME") {
        let base = PathBuf::from(base);
        if base.is_absolute() {
            return Ok(base.join("dbmd"));
        }
    }
    let home = PathBuf::from(env_nonempty("HOME").ok_or_else(|| {
        LinkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot locate user state; set {STATE_DIR_ENV}"),
        ))
    })?);
    if !home.is_absolute() {
        return Err(LinkError::UnsafePath {
            path: home.display().to_string(),
        });
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("dbmd")
            .join("state"))
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        Ok(home.join(".local").join("state").join("dbmd"))
    }
}

fn normalized_origin(value: &str) -> LinkResult<String> {
    let parsed = url::Url::parse(value).map_err(|_| LinkError::UnsafeHub {
        hub: value.to_string(),
    })?;
    if !(parsed.scheme().eq_ignore_ascii_case("https")
        || parsed.scheme().eq_ignore_ascii_case("http"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.path() != "/" && !parsed.path().is_empty())
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(LinkError::UnsafeHub {
            hub: value.to_string(),
        });
    }
    let host = parsed.host_str().ok_or_else(|| LinkError::UnsafeHub {
        hub: value.to_string(),
    })?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_ascii_lowercase()
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| LinkError::UnsafeHub {
            hub: value.to_string(),
        })?;
    let default = (parsed.scheme().eq_ignore_ascii_case("https") && port == 443)
        || (parsed.scheme().eq_ignore_ascii_case("http") && port == 80);
    Ok(format!(
        "{}://{}{}",
        parsed.scheme().to_ascii_lowercase(),
        host,
        if default {
            String::new()
        } else {
            format!(":{port}")
        }
    ))
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
    #[cfg(unix)]
    let file = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        let parent = open_existing_dir_nofollow(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|e| bad_agent_key(&format!("cannot open the key parent: {e}")))?;
        let leaf = path
            .file_name()
            .ok_or_else(|| bad_agent_key("the key path has no file name"))?;
        let leaf = c_name(leaf.as_bytes(), &path.display().to_string())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(bad_agent_key(
                "the key path must be an existing regular file without symlink ancestors",
            ));
        }
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)
        .map_err(|e| bad_agent_key(&format!("cannot read the key file: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| bad_agent_key(&format!("cannot inspect the key file: {e}")))?;
    if !metadata.is_file() {
        return Err(bad_agent_key("the key path must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(bad_agent_key(
                "the key file is accessible to group/other; set mode 0600",
            ));
        }
    }
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|e| bad_agent_key(&format!("cannot read the key file: {e}")))?;
    if text.len() > 1024 * 1024 {
        return Err(bad_agent_key("the key file exceeds the size limit"));
    }
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

/// Durably create a new secret file without an exists→write race and without
/// ever exposing a default-mode (commonly 0644) key between write and chmod.
/// `create_new` also refuses a planted symlink. The file and its parent
/// directory are synced before the caller can publish the corresponding
/// public identity remotely.
fn write_secret_new(path: &Path, bytes: &[u8]) -> LinkResult<()> {
    #[cfg(unix)]
    let (mut file, parent, leaf) = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        let parent = open_or_create_dir_nofollow(path.parent().unwrap_or_else(|| Path::new(".")))?;
        let leaf_name = path
            .file_name()
            .ok_or_else(|| bad_agent_key("the output key path has no file name"))?;
        let leaf = c_name(leaf_name.as_bytes(), &path.display().to_string())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(bad_agent_key(
                    "the output file already exists — refusing to overwrite a key",
                ));
            }
            return Err(error.into());
        }
        (unsafe { std::fs::File::from_raw_fd(fd) }, parent, leaf)
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                bad_agent_key("the output file already exists — refusing to overwrite a key")
            } else {
                LinkError::Io(error)
            }
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        #[cfg(unix)]
        let _ =
            unsafe { libc::unlinkat(std::os::fd::AsRawFd::as_raw_fd(&parent), leaf.as_ptr(), 0) };
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(path);
        return Err(LinkError::Io(error));
    }
    drop(file);
    #[cfg(unix)]
    parent.sync_all()?;
    Ok(())
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
    require_hardened_filesystem("key generation")?;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| bad_agent_key("key generation failed"))?;
    let pair = agent_keypair(pkcs8.as_ref())?;
    let (spki_b64u, multikey) = public_identity_for(&pair);

    write_secret_new(
        out,
        format!("{}\n", URL_SAFE_NO_PAD.encode(pkcs8.as_ref())).as_bytes(),
    )?;

    Ok(GeneratedAgentKey {
        multikey,
        public_key_spki: spki_b64u,
        key_file: out.display().to_string(),
    })
}

/// Build the origin-bound `LinkMD-Sig` v2 header for one request:
/// `canonical = "v2" LF origin LF METHOD LF path+query LF ts LF
/// (sha256hex(body) | "-")`.
///
/// The origin is derived from the already-validated hub URL, never from
/// attacker-controlled request metadata. Binding it closes the v1 replay
/// class where a proof captured by one hub could be replayed to another hub
/// serving the same path inside the timestamp window.
fn linkmd_sig_header(
    key: &AgentSigningKey,
    origin: &str,
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
        "v2\n{}\n{}\n{}\n{}\n{}",
        origin,
        method.to_uppercase(),
        path,
        ts,
        body_hash
    );
    let pair = agent_keypair(&key.pkcs8)?;
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(canonical.as_bytes()).as_ref());
    let fingerprint = key.multikey.trim_start_matches("ed25519:");
    Ok(format!(
        "LinkMD-Sig v2,key=ed25519:{fingerprint},ts={ts},sig={sig}"
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
    const MAX_CONFIG_BYTES: u64 = 64 * 1024;
    #[cfg(unix)]
    let file = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        let parent =
            open_existing_dir_nofollow(path.parent().unwrap_or_else(|| Path::new("."))).ok()?;
        let leaf = c_name(path.file_name()?.as_bytes(), &path.display().to_string()).ok()?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return None;
        }
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
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
    if !(parsed.scheme().eq_ignore_ascii_case("https")
        || parsed.scheme().eq_ignore_ascii_case("http"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.path() != "/" && !parsed.path().is_empty())
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

struct RawHubResponse {
    status: u16,
    body: Vec<u8>,
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

fn agent_builder_with_timeout(overall: std::time::Duration) -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .user_agent(concat!("dbmd/", env!("CARGO_PKG_VERSION")))
        // Never follow a redirect while a bearer, a store pack, or a signed
        // response is in flight. Callers see the 3xx as a non-success instead
        // of letting an origin steer sensitive material elsewhere.
        .redirects(0)
        .timeout_connect(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout_read(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
        .timeout_write(overall)
        .timeout(overall)
}

fn agent_builder() -> ureq::AgentBuilder {
    agent_builder_with_timeout(std::time::Duration::from_secs(OVERALL_REQUEST_TIMEOUT_SECS))
}

fn agent() -> ureq::Agent {
    agent_builder().build()
}

fn hub_agent(cfg: &HubConfig) -> LinkResult<ureq::Agent> {
    if !cfg.store_selected {
        return Ok(agent());
    }
    let parsed = url::Url::parse(&cfg.hub).map_err(|_| LinkError::UnsafeHub {
        hub: cfg.hub.clone(),
    })?;
    pinned_public_agent(&parsed, false, "store-selected hub")
}

/// Perform one hub request. `path` is the binding path (starts with `/`);
/// `body` posts JSON. Transport failures, oversized bodies, and non-UTF-8 are
/// all surfaced as typed [`LinkError`]s; HTTP error statuses are returned in
/// the [`HubResponse`] for [`ensure_ok`] to shape.
fn request_raw(
    cfg: &HubConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
    auth: Auth,
    max_response_bytes: u64,
) -> LinkResult<RawHubResponse> {
    let url = format!("{}{}", cfg.hub, path);
    let encoded_body = body.map(Value::to_string);
    let origin = normalized_origin(&cfg.hub)?;
    // An agent signing key outranks the bearer: possession proofs put nothing
    // reusable on the wire, so when both are configured the stronger one wins.
    let credential = match auth {
        Auth::Required => Some(match &cfg.agent_key {
            Some(key) => linkmd_sig_header(key, &origin, method, path, encoded_body.as_deref())?,
            None => format!("Bearer {}", cfg.require_key()?),
        }),
        Auth::Optional => match &cfg.agent_key {
            Some(key) => Some(linkmd_sig_header(
                key,
                &origin,
                method,
                path,
                encoded_body.as_deref(),
            )?),
            None => cfg.key.as_deref().map(|k| format!("Bearer {k}")),
        },
        Auth::None => None,
    };
    let http = hub_agent(cfg)?;
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
        .take(max_response_bytes + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > max_response_bytes {
        return Err(LinkError::ResponseTooLarge {
            limit_bytes: max_response_bytes,
        });
    }
    Ok(RawHubResponse { status, body: buf })
}

fn request_capped(
    cfg: &HubConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
    auth: Auth,
    max_response_bytes: u64,
) -> LinkResult<HubResponse> {
    let raw = request_raw(cfg, method, path, body, auth, max_response_bytes)?;
    let parsed: Option<Value> = serde_json::from_slice(&raw.body).ok();
    Ok(HubResponse {
        status: raw.status,
        body: parsed,
    })
}

fn request(
    cfg: &HubConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
    auth: Auth,
) -> LinkResult<HubResponse> {
    request_capped(cfg, method, path, body, auth, MAX_RESPONSE_BYTES)
}

fn ensure_raw_ok(r: RawHubResponse, what: &'static str) -> LinkResult<Vec<u8>> {
    if (200..300).contains(&r.status) {
        return Ok(r.body);
    }
    ensure_ok(
        HubResponse {
            status: r.status,
            body: serde_json::from_slice(&r.body).ok(),
        },
        what,
    )
    .and_then(|_| Err(invalid_feed("a non-2xx response was accepted unexpectedly")))
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

fn hub_is_loopback(hub: &str) -> bool {
    url::Url::parse(hub).ok().is_some_and(|parsed| {
        parsed.host().is_some_and(|host| match host {
            url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
        })
    })
}

fn presigned_agent(cfg: &HubConfig, raw: &str) -> LinkResult<ureq::Agent> {
    let parsed = url::Url::parse(raw).map_err(|_| LinkError::InvalidPack {
        message: "the hub returned an invalid object-store URL".to_string(),
    })?;
    let allow_private = hub_is_loopback(&cfg.hub)
        || env_nonempty(ALLOW_PRIVATE_OBJECT_URL_ENV).as_deref() == Some("1");
    if (!parsed.scheme().eq_ignore_ascii_case("https") && !allow_private)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(LinkError::InvalidPack {
            message: "the hub returned an unsafe object-store URL".to_string(),
        });
    }
    pinned_public_agent(&parsed, allow_private, "object-store URL").map_err(|_| {
        LinkError::InvalidPack {
            message: "the hub returned an object-store URL with an unsafe network target"
                .to_string(),
        }
    })
}

fn put_presigned(cfg: &HubConfig, raw: &str, headers: &Value, bytes: &[u8]) -> LinkResult<()> {
    let http = presigned_agent(cfg, raw)?;
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
        Ok(resp) if (200..300).contains(&resp.status()) => Ok(()),
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

fn get_presigned(cfg: &HubConfig, raw: &str) -> LinkResult<Vec<u8>> {
    let http = presigned_agent(cfg, raw)?;
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
    if !(200..300).contains(&resp.status()) {
        return Err(LinkError::Http {
            what: "pack download",
            status: resp.status(),
            message: "object store rejected the download".to_string(),
            code: None,
        });
    }
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
    if !(200..300).contains(&r.status) {
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
fn is_public_registry_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Conservatively accept only global unicast 2000::/3, excluding
            // special-purpose/transition blocks. In particular, 6to4 embeds
            // an IPv4 destination and must not tunnel a validated public
            // connect to 127/8 or RFC1918.
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0)
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && segments[0] != 0x2002
                && !(segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        }
    }
}

#[derive(Clone)]
struct PinnedRegistryResolver {
    netloc: String,
    addresses: Vec<std::net::SocketAddr>,
}

impl ureq::Resolver for PinnedRegistryResolver {
    fn resolve(&self, requested: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        if requested == self.netloc {
            Ok(self.addresses.clone())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "registry request attempted to resolve an unvalidated authority",
            ))
        }
    }
}

fn pinned_public_agent(
    url: &url::Url,
    allow_private: bool,
    label: &str,
) -> LinkResult<ureq::Agent> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid_feed(format!("{label} has no host")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid_feed(format!("{label} has no port")))?;
    let addresses = resolve_addresses_with_deadline(
        host,
        port,
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
    )
    .map_err(|error| invalid_feed(format!("{label} DNS resolution failed: {error}")))?;
    if addresses.is_empty() {
        return Err(invalid_feed(format!("{label} DNS returned no addresses")));
    }
    if !allow_private
        && addresses
            .iter()
            .any(|address| !is_public_registry_ip(address.ip()))
    {
        return Err(invalid_feed(format!(
            "{label} resolves to a non-public address"
        )));
    }
    let netloc = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Ok(agent_builder()
        .resolver(PinnedRegistryResolver { netloc, addresses })
        .build())
}

/// Resolve one authority without allowing libc DNS to hold the caller forever.
/// `ToSocketAddrs` itself has no timeout API, so resolution runs in a detached
/// worker and only its result channel is awaited. A late resolver result has no
/// side effects and is discarded after the deadline.
fn resolve_addresses_with_deadline(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs as _;

    let host = host.to_string();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("dbmd-dns".to_string())
        .spawn(move || {
            let result = (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect());
            let _ = send.send(result);
        })
        .map_err(|error| std::io::Error::other(format!("cannot start resolver: {error}")))?;
    match receive.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "resolution exceeded its deadline",
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
            "resolver stopped without returning a result",
        )),
    }
}

fn registry_agent(url: &url::Url) -> LinkResult<ureq::Agent> {
    let allow_private = env_nonempty(ALLOW_PRIVATE_REGISTRY_HOME_ENV).as_deref() == Some("1");
    pinned_public_agent(url, allow_private, "registry home")
}

/// GET an absolute URL as JSON with NO credential — used to fetch a brain card
/// from a FOREIGN home during registry resolution. DNS is resolved once,
/// every answer is required to be public, and that exact answer set is pinned
/// into a no-redirect HTTP agent to close private-network SSRF and rebinding.
fn get_json_absolute(url: &str) -> LinkResult<Value> {
    let parsed = url::Url::parse(url).map_err(|_| invalid_feed("invalid registry home URL"))?;
    let allow_private = env_nonempty(ALLOW_PRIVATE_REGISTRY_HOME_ENV).as_deref() == Some("1");
    if (!parsed.scheme().eq_ignore_ascii_case("https") && !allow_private)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid_feed("unsafe registry home URL"));
    }
    let http = registry_agent(&parsed)?;
    let resp = match with_connect_retries(|| http.get(url).call().map_err(Box::new)) {
        Ok(resp) => resp,
        Err(error) => match *error {
            ureq::Error::Status(status, resp) => {
                let _ = resp;
                return Err(LinkError::Http {
                    what: "registry home fetch",
                    status,
                    message: "the home node rejected the card request".to_string(),
                    code: None,
                });
            }
            ureq::Error::Transport(err) => {
                return Err(LinkError::Transport {
                    hub: url.to_string(),
                    message: err.to_string(),
                });
            }
        },
    };
    if !(200..300).contains(&resp.status()) {
        return Err(LinkError::Http {
            what: "registry home fetch",
            status: resp.status(),
            message: "the home node returned a redirect or error".to_string(),
            code: None,
        });
    }
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_REGISTRY_CARD_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_REGISTRY_CARD_BYTES {
        return Err(LinkError::ResponseTooLarge {
            limit_bytes: MAX_REGISTRY_CARD_BYTES,
        });
    }
    serde_json::from_slice(&buf).map_err(|_| LinkError::InvalidFeed {
        message: "the home node returned invalid JSON".to_string(),
    })
}

/// Resolve a bare `@handle` through the federation registry (link.md §7.1,
/// E5): look the handle up in the hub's registry, fetch the brain card from
/// the returned HOME node, and PIN — the card's identity fingerprint must
/// equal the registry's, or resolution fails. Returns the card enriched with
/// the resolved `home`, or `Ok(None)` when the registry has no such handle
/// (so the caller can fall back to a direct lookup).
pub fn resolve_registry(cfg: &HubConfig, handle: &str) -> LinkResult<Option<Value>> {
    require_safe_ref(handle)?;
    // Hold the validated trust-directory capability before any network I/O.
    // Every lock/load/save below remains relative to this exact inode even if
    // an attacker swaps an ancestor while the registry or home is answering.
    let trust_directory = open_trust_dir(cfg)?;
    let reg = request_capped(
        cfg,
        "GET",
        &format!("/api/hub/registry/{handle}"),
        None,
        Auth::None,
        MAX_REGISTRY_CARD_BYTES,
    )?;
    if reg.status == 404 {
        return Ok(None);
    }
    let body = ensure_ok(reg, "registry resolve")?;
    let home = body
        .get("home")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("registry entry has no home"))?;
    let brain = body
        .get("brain")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("registry entry has no brain"))?;
    if !crate::ulid::is_ulid(brain) {
        return Err(invalid_feed(
            "registry entry brain is not a canonical lowercase ULID",
        ));
    }
    let want_fp = body
        .get("identity")
        .and_then(|i| i.get("fingerprint"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("registry entry has no identity fingerprint"))?;

    let home = home.trim_end_matches('/');
    let origin = normalized_origin(home)?;
    if origin != home {
        return Err(invalid_feed(
            "registry home must be an origin without a path, query, or fragment",
        ));
    }
    let _trust_locks = lock_trust_many(cfg, &trust_directory, &[handle, brain])?;
    let (pinned, alias_binding) = load_canonical_pin(cfg, &trust_directory, handle, brain)?;
    if let Some(binding) = &alias_binding {
        if binding
            .home
            .as_deref()
            .is_some_and(|pinned_home| pinned_home != home)
        {
            return Err(invalid_feed(
                "registry relocated a pinned handle to a different home",
            ));
        }
    }
    let card = get_json_absolute(&format!("{home}/api/hub/brains/{brain}"))?;
    if card.get("id").and_then(Value::as_str) != Some(brain) {
        return Err(invalid_feed(
            "the home node served a card for a different brain",
        ));
    }
    let identity: FeedIdentity = serde_json::from_value(
        card.get("identity")
            .cloned()
            .ok_or_else(|| invalid_feed("the home node served no identity"))?,
    )
    .map_err(|_| invalid_feed("the home node served an invalid identity"))?;
    let anchor = verify_identity_chain(&identity, pinned.as_ref())?;
    let got_fp = card
        .get("identity")
        .and_then(|i| i.get("fingerprint"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if got_fp != want_fp {
        return Err(invalid_feed(
            "the home node served an identity that does not match the registry — refusing",
        ));
    }
    let current = format!("ed25519:{}", identity.fingerprint);
    let advertised_seq = card
        .get("headSeq")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_feed("the home node served an invalid head sequence"))?;
    let advertised_hash = card.get("feedHash").and_then(Value::as_str);
    if (advertised_seq == 0 && !card.get("feedHash").is_some_and(Value::is_null))
        || (advertised_seq > 0 && advertised_hash.is_none_or(|hash| !is_sha256(hash)))
    {
        return Err(invalid_feed(
            "the home node served an invalid feed head boundary",
        ));
    }
    // This is required even on first contact. Otherwise a syntactically valid
    // identity may claim that a rotation happened after a head the home has
    // never reached, then become the permanent TOFU anchor.
    verify_rotation_feed_boundaries(&identity, pinned.as_ref(), &[], advertised_seq)?;
    let registry_alias = AliasBinding {
        v: 1,
        origin: normalized_origin(&cfg.hub)?,
        requested: handle.to_string(),
        brain: brain.to_string(),
        home: Some(home.to_string()),
    };
    save_canonical_pin_and_alias(
        cfg,
        &trust_directory,
        handle,
        brain,
        TrustState {
            v: 2,
            origin: normalized_origin(&cfg.hub)?,
            requested: brain.to_string(),
            brain: brain.to_string(),
            home: None,
            anchor,
            current,
            head_seq: pinned.as_ref().map_or(0, |checkpoint| checkpoint.head_seq),
            feed_hash: pinned
                .as_ref()
                .and_then(|checkpoint| checkpoint.feed_hash.clone()),
            rotations: identity.rotations.clone(),
        },
        Some(&registry_alias),
    )?;
    let mut out = card;
    if let Value::Object(map) = &mut out {
        map.insert("home".to_string(), Value::String(home.to_string()));
        map.insert(
            "resolvedVia".to_string(),
            Value::String("registry".to_string()),
        );
    }
    Ok(Some(out))
}

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

    // A record is never accepted from the hub's mutable query/index response:
    // that response was not covered by the brain's feed signature and could
    // return arbitrary frontmatter/body while an unrelated signed head still
    // verified. Materialize the record from the exact content-addressed pack
    // named by the verified signed head instead.
    if let Some(target) = &addr.target {
        let remote = verified_remote_head(cfg, &addr.brain, false)?;
        if !remote.head.verified {
            return Err(invalid_feed(
                "a path-scoped feed cannot prove a record against the full signed snapshot",
            ));
        }
        if remote.head.seq == 0 {
            return Err(LinkError::Http {
                what: "resolve",
                status: 404,
                message: "record not found".to_string(),
                code: Some("NOT_FOUND".to_string()),
            });
        }
        let brain = remote.head.brain.clone();
        let pack = download_verified_snapshot_pack(cfg, &brain, &remote)?;
        return resolve_from_verified_pack(&brain, target, pack);
    }

    let path = format!("/api/hub/brains/{}", addr.brain);
    // Direct first: the caller's own slug and hub-hosted public handles resolve
    // here unchanged. Only a bare `@handle` the hub can't resolve directly
    // (404) falls through to the federation registry — how a handle reaches a
    // brain on ANOTHER node (link.md §7.1, E5).
    let direct = request(cfg, "GET", &path, None, Auth::Required)?;
    if direct.status == 404 && addr.target.is_none() && !crate::ulid::is_ulid(&addr.brain) {
        if let Some(card) = resolve_registry(cfg, &addr.brain)? {
            return Ok(card);
        }
    }
    let resolved = ensure_ok(direct, "resolve")?;
    // A successful direct response is accepted only after the same centralized
    // identity/rotation/feed checkpoint verification used by sync and
    // subscribe. No verb gets a weaker ad-hoc pinning path.
    let remote = verified_remote_head(cfg, &addr.brain, false)?;
    if resolved.get("id").and_then(Value::as_str) != Some(remote.head.brain.as_str())
        || resolved.get("headSeq").and_then(Value::as_u64) != Some(remote.head.seq)
        || resolved.get("feedHash").and_then(Value::as_str) != remote.head.feed_hash.as_deref()
    {
        return Err(invalid_feed(
            "resolve card is not bound to the exact verified feed checkpoint",
        ));
    }
    let card_identity: FeedIdentity = serde_json::from_value(
        resolved
            .get("identity")
            .cloned()
            .ok_or_else(|| invalid_feed("resolve card has no signed identity"))?,
    )
    .map_err(|_| invalid_feed("resolve card has an invalid identity"))?;
    if remote.identity.as_ref() != Some(&card_identity) {
        return Err(invalid_feed(
            "resolve card identity differs from the verified feed identity",
        ));
    }
    Ok(resolved)
}

/// Resolve one record strictly from the exact signed snapshot pack. The hub's
/// mutable query/index result is intentionally not consulted: a signed pack
/// digest is the only cryptographic binding between a feed checkpoint and
/// record bytes in wire profile v1.
fn resolve_from_verified_pack(
    brain: &str,
    target: &AddressTarget,
    pack: Vec<u8>,
) -> LinkResult<Value> {
    let entries = parse_store_pack(pack)?;
    let mut matched: Option<(String, Vec<u8>)> = None;

    for (path, bytes) in entries {
        let is_candidate = match target {
            AddressTarget::Path(want) => &path == want,
            AddressTarget::Id(_) => {
                path.ends_with(".md")
                    && (path.starts_with("records/") || path.starts_with("sources/"))
            }
        };
        if !is_candidate {
            continue;
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid_feed(format!("signed snapshot record `{path}` is not UTF-8")))?;
        let parsed = crate::parser::split_frontmatter(text, Path::new(&path))
            .map_err(|_| invalid_feed(format!("signed snapshot record `{path}` is malformed")))?;
        if let AddressTarget::Id(want) = target {
            let frontmatter =
                crate::parser::Frontmatter::parse(&parsed.frontmatter_yaml, Path::new(&path))
                    .map_err(|_| {
                        invalid_feed(format!("signed snapshot record `{path}` is malformed"))
                    })?;
            if frontmatter.id.as_deref() != Some(want) {
                continue;
            }
        }
        if matched.is_some() {
            return Err(invalid_feed(
                "signed snapshot contains more than one record for the requested target",
            ));
        }
        matched = Some((path, bytes));
    }

    let (path, bytes) = matched.ok_or_else(|| LinkError::Http {
        what: "resolve",
        status: 404,
        message: "record not found".to_string(),
        code: Some("NOT_FOUND".to_string()),
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid_feed(format!("signed snapshot record `{path}` is not UTF-8")))?;
    let parsed = crate::parser::split_frontmatter(text, Path::new(&path))
        .map_err(|_| invalid_feed(format!("signed snapshot record `{path}` is malformed")))?;
    let frontmatter: Value = serde_norway::from_str(&parsed.frontmatter_yaml)
        .map_err(|_| invalid_feed(format!("signed snapshot record `{path}` is malformed")))?;
    let Value::Object(fields) = frontmatter else {
        return Err(invalid_feed(format!(
            "signed snapshot record `{path}` frontmatter is not a mapping"
        )));
    };
    let mut document = serde_json::Map::new();
    document.insert("path".to_string(), Value::String(path));
    for (key, value) in fields {
        document.insert(key, value);
    }
    document.insert("body".to_string(), Value::String(parsed.body));
    document.insert(
        "contentSha".to_string(),
        Value::String(content_sha256(&bytes)),
    );
    Ok(json!({
        "brain": brain,
        "document": Value::Object(document),
    }))
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

fn download_verified_snapshot_pack(
    cfg: &HubConfig,
    brain: &str,
    remote: &VerifiedRemote,
) -> LinkResult<Vec<u8>> {
    let feed_hash = remote
        .head
        .feed_hash
        .as_deref()
        .ok_or_else(|| invalid_feed("non-empty snapshot has no verified feed hash"))?;
    let signed_head = remote
        .head_entry
        .as_ref()
        .ok_or_else(|| invalid_feed("verified snapshot has no signed head entry"))?;
    let expected = &signed_head.entry.pack_sha256;
    if !is_sha256(expected) {
        return Err(invalid_feed(
            "signed head carries an invalid snapshot pack digest",
        ));
    }
    let path = format!(
        "/api/hub/brains/{brain}/export?format=pack&atSeq={}&feedHash={feed_hash}",
        remote.head.seq
    );
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "sync pull",
    )?;
    if body.get("headSeq").and_then(Value::as_u64) != Some(remote.head.seq)
        || body.get("feedHash").and_then(Value::as_str) != Some(feed_hash)
        || body.get("brain").and_then(Value::as_str) != Some(remote.head.brain.as_str())
        || body.get("sha256").and_then(Value::as_str) != Some(expected.as_str())
    {
        return Err(invalid_feed(
            "export response is not bound to the exact verified snapshot",
        ));
    }
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("verified snapshot export carried no exact pack URL"))?;
    let bytes = get_presigned(cfg, url)?;
    if content_sha256(&bytes) != *expected {
        return Err(LinkError::InvalidPack {
            message: "downloaded pack does not match the signed snapshot digest".to_string(),
        });
    }
    let entries = parse_store_pack(bytes.clone())?;
    if signed_head.entry.kind == "push" {
        verify_snapshot_manifest(&entries, &signed_head.entry.files)?;
    }
    Ok(bytes)
}

/// Pull the granted slice of `brain` to `out` (default: `./<slug>`). Every
/// exported path is safety-gated before it touches disk; files are written
/// atomically; nothing local is ever deleted (locals the export lacks are
/// *reported* in `extra_local` instead). Returns the report; rebuilding the
/// local index catalog afterwards is the caller's (cheap, optional) step.
pub fn sync_pull(cfg: &HubConfig, brain: &str, out: Option<&Path>) -> LinkResult<PullReport> {
    require_hardened_filesystem("sync pull")?;
    require_safe_ref(brain)?;
    let remote = verified_remote_head(cfg, brain, false)?;
    if !remote.head.verified {
        return Err(invalid_feed(
            "the grant exposes head movement but no signed snapshot; refusing an unverifiable pull",
        ));
    }
    let snapshot_hash = remote.head.feed_hash.as_deref().unwrap_or("none");
    let path = format!(
        "/api/hub/brains/{brain}/export?format=pack&atSeq={}&feedHash={snapshot_hash}",
        remote.head.seq
    );
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "sync pull",
    )?;
    if body.get("headSeq").and_then(Value::as_u64) != Some(remote.head.seq)
        || body.get("feedHash").and_then(Value::as_str) != remote.head.feed_hash.as_deref()
    {
        return Err(invalid_feed(
            "export response is not bound to the verified snapshot token",
        ));
    }

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
        .unwrap_or(&remote.head.brain)
        .to_string();
    if brain_id != remote.head.brain {
        return Err(invalid_feed(
            "export response names a different brain than the verified head",
        ));
    }
    let head_seq = remote.head.seq;
    let dest: PathBuf = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(&slug),
    };
    let entries = if head_seq == 0 {
        let files = body
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_feed("empty snapshot export did not carry an empty manifest"))?;
        if !files.is_empty() || body.get("url").is_some() {
            return Err(invalid_feed(
                "empty signed feed cannot authorize non-empty exported content",
            ));
        }
        Vec::new()
    } else {
        let signed_head = remote
            .head_entry
            .as_ref()
            .ok_or_else(|| invalid_feed("verified snapshot has no signed head entry"))?;
        let expected = &signed_head.entry.pack_sha256;
        if !is_sha256(expected) {
            return Err(invalid_feed(
                "signed head carries an invalid snapshot pack digest",
            ));
        }
        if let Some(url) = body.get("url").and_then(Value::as_str) {
            if body.get("sha256").and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(invalid_feed(
                    "export pack digest does not match the signed head entry",
                ));
            }
            let bytes = get_presigned(cfg, url)?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if actual != *expected {
                return Err(LinkError::InvalidPack {
                    message: "downloaded pack does not match the signed snapshot digest"
                        .to_string(),
                });
            }
            let entries = parse_store_pack(bytes)?;
            if signed_head.entry.kind == "push" {
                verify_snapshot_manifest(&entries, &signed_head.entry.files)?;
            }
            entries
        } else {
            if signed_head.entry.kind != "push" {
                return Err(invalid_feed(
                    "delta snapshots must export the exact signed pack",
                ));
            }
            let files = body.get("files").and_then(Value::as_array).ok_or_else(|| {
                invalid_feed("verified snapshot export carried neither a pack nor files")
            })?;
            let mut entries = Vec::with_capacity(files.len());
            for file in files {
                let path = file
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("exported file path is not a string"))?;
                let content = file
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("exported inline content is not UTF-8 text"))?;
                entries.push((path.to_string(), content.as_bytes().to_vec()));
            }
            verify_snapshot_manifest(&entries, &signed_head.entry.files)?;
            entries
        }
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
    // Compute divergence against the still-live tree. The staged clone keeps
    // these extra locals byte-for-byte while overlaying the signed snapshot.
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
    #[cfg(unix)]
    install_pulled_snapshot(&dest, &entries)?;

    Ok(PullReport {
        brain: brain_id,
        slug,
        head_seq,
        files: entries.len(),
        dest: dest.to_string_lossy().into_owned(),
        extra_local,
    })
}

#[cfg(unix)]
fn c_name(bytes: &[u8], display: &str) -> LinkResult<std::ffi::CString> {
    std::ffi::CString::new(bytes).map_err(|_| LinkError::UnsafePath {
        path: display.to_string(),
    })
}

#[cfg(unix)]
fn open_dir_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    display: &str,
) -> LinkResult<std::fs::File> {
    use std::os::fd::FromRawFd as _;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(LinkError::UnsafePath {
            path: display.to_string(),
        });
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Open (and, where absent, create) a directory path without following a
/// symlink in any component. The returned directory capability remains bound
/// to the opened inode even if an attacker renames or replaces an ancestor.
#[cfg(unix)]
fn open_dir_path_nofollow(path: &Path, create: bool) -> LinkResult<std::fs::File> {
    use std::os::fd::AsRawFd as _;

    // macOS exposes a few system-owned compatibility symlinks at the root.
    // Normalize only those fixed OS aliases; never canonicalize an arbitrary
    // caller-controlled ancestor.
    #[cfg(target_os = "macos")]
    let normalized = [("/var", "/private/var"), ("/tmp", "/private/tmp")]
        .into_iter()
        .find_map(|(alias, real)| {
            path.strip_prefix(alias)
                .ok()
                .map(|rest| Path::new(real).join(rest))
        })
        .unwrap_or_else(|| path.to_path_buf());
    #[cfg(not(target_os = "macos"))]
    let normalized = path.to_path_buf();

    let start = if normalized.is_absolute() {
        std::fs::File::open("/")?
    } else {
        std::fs::File::open(".")?
    };
    let mut directory = start;
    for component in normalized.components() {
        use std::path::Component;
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(LinkError::UnsafePath {
                    path: path.display().to_string(),
                });
            }
        };
        use std::os::unix::ffi::OsStrExt as _;
        let name = c_name(name.as_bytes(), &path.display().to_string())?;
        if create {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if made != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error.into());
                }
            }
        }
        directory = open_dir_at(directory.as_raw_fd(), &name, &path.display().to_string())?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_or_create_dir_nofollow(path: &Path) -> LinkResult<std::fs::File> {
    open_dir_path_nofollow(path, true)
}

#[cfg(unix)]
fn open_existing_dir_nofollow(path: &Path) -> LinkResult<std::fs::File> {
    open_dir_path_nofollow(path, false)
}

#[cfg(unix)]
fn entry_is_dir_at(parent: std::os::fd::RawFd, name: &std::ffi::CStr) -> LinkResult<Option<bool>> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::fstatat(parent, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if result == 0 {
        return Ok(Some((stat.st_mode & libc::S_IFMT) == libc::S_IFDIR));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error.into())
    }
}

#[cfg(unix)]
fn create_dir_exclusive_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    display: &str,
) -> LinkResult<std::fs::File> {
    let made = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
    if made != 0 {
        return Err(LinkError::UnsafePath {
            path: display.to_string(),
        });
    }
    open_dir_at(parent, name, display)
}

#[cfg(unix)]
fn directory_entry_names(directory: &std::fs::File) -> LinkResult<Vec<std::ffi::CString>> {
    use std::os::fd::AsRawFd as _;

    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(duplicate);
        }
        return Err(error.into());
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let raw = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if raw.to_bytes() != b"." && raw.to_bytes() != b".." {
            names.push(raw.to_owned());
        }
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(names)
}

/// Remove an entry tree relative to a held directory capability. Symlinks are
/// unlinked, never traversed, even when an old mirror contains hostile names.
#[cfg(unix)]
fn remove_tree_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    display: &str,
) -> LinkResult<()> {
    use std::os::fd::AsRawFd as _;

    match entry_is_dir_at(parent, name)? {
        None => return Ok(()),
        Some(false) => {
            if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Some(true) => {
            let directory = open_dir_at(parent, name, display)?;
            for child in directory_entry_names(&directory)? {
                let child_display =
                    format!("{display}/{}", String::from_utf8_lossy(child.to_bytes()));
                remove_tree_at(directory.as_raw_fd(), &child, &child_display)?;
            }
            drop(directory);
            if unsafe { libc::unlinkat(parent, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}

/// Clone a live destination into a private sibling stage without following
/// any symlink. Regular files are streamed between held descriptors; symlinks
/// are reproduced as links, never opened. Special files are refused.
#[cfg(unix)]
fn clone_tree_contents(
    source: &std::fs::File,
    destination: &std::fs::File,
    display: &str,
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    for name in directory_entry_names(source)? {
        let child_display = format!("{display}/{}", String::from_utf8_lossy(name.to_bytes()));
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                source.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                if unsafe {
                    libc::mkdirat(destination.as_raw_fd(), name.as_ptr(), stat.st_mode & 0o777)
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                let source_child = open_dir_at(source.as_raw_fd(), &name, &child_display)?;
                let destination_child =
                    open_dir_at(destination.as_raw_fd(), &name, &child_display)?;
                clone_tree_contents(&source_child, &destination_child, &child_display)?;
                destination_child.sync_all()?;
            }
            libc::S_IFREG => {
                let source_fd = unsafe {
                    libc::openat(
                        source.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if source_fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let destination_fd = unsafe {
                    libc::openat(
                        destination.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_CLOEXEC
                            | libc::O_NOFOLLOW,
                        (stat.st_mode & 0o777) as libc::c_uint,
                    )
                };
                if destination_fd < 0 {
                    unsafe {
                        libc::close(source_fd);
                    }
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut input = unsafe { std::fs::File::from_raw_fd(source_fd) };
                let mut output = unsafe { std::fs::File::from_raw_fd(destination_fd) };
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
            }
            libc::S_IFLNK => {
                let mut target = vec![0_u8; 4097];
                let length = unsafe {
                    libc::readlinkat(
                        source.as_raw_fd(),
                        name.as_ptr(),
                        target.as_mut_ptr().cast(),
                        target.len(),
                    )
                };
                if length < 0 || length as usize >= target.len() {
                    return Err(LinkError::UnsafePath {
                        path: child_display,
                    });
                }
                target.truncate(length as usize);
                let target = c_name(&target, &child_display)?;
                if unsafe {
                    libc::symlinkat(target.as_ptr(), destination.as_raw_fd(), name.as_ptr())
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            _ => {
                return Err(LinkError::UnsafePath {
                    path: child_display,
                });
            }
        }
    }
    destination.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_stage_at(
    parent: std::os::fd::RawFd,
    stage: &std::ffi::CStr,
    dest: &std::ffi::CStr,
    dest_exists: bool,
) -> LinkResult<()> {
    let flags = if dest_exists {
        libc::RENAME_EXCHANGE
    } else {
        libc::RENAME_NOREPLACE
    };
    let result = unsafe { libc::renameat2(parent, stage.as_ptr(), parent, dest.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(target_os = "macos")]
fn install_stage_at(
    parent: std::os::fd::RawFd,
    stage: &std::ffi::CStr,
    dest: &std::ffi::CStr,
    dest_exists: bool,
) -> LinkResult<()> {
    let flags = if dest_exists {
        libc::RENAME_SWAP
    } else {
        libc::RENAME_EXCL
    };
    let result =
        unsafe { libc::renameatx_np(parent, stage.as_ptr(), parent, dest.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(unix)]
fn write_pull_entries_beneath_dir(
    root: &std::fs::File,
    entries: &[(String, Vec<u8>)],
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    for (path, content) in entries {
        let components: Vec<&str> = path.split('/').collect();
        let (leaf, parents) = components
            .split_last()
            .ok_or_else(|| LinkError::UnsafePath { path: path.clone() })?;
        let mut directory = root.try_clone()?;
        for component in parents {
            let name = c_name(component.as_bytes(), path)?;
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if made != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error.into());
                }
            }
            directory = open_dir_at(directory.as_raw_fd(), &name, path)?;
        }

        let leaf_name = c_name(leaf.as_bytes(), path)?;
        let mut existing: libc::stat = unsafe { std::mem::zeroed() };
        let inspected = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                leaf_name.as_ptr(),
                &mut existing,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if inspected == 0 && (existing.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            return Err(LinkError::UnsafePath { path: path.clone() });
        }

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_name = format!(
            ".dbmd-pull-{}-{nonce}-{}",
            std::process::id(),
            content_sha256(format!("{path}\0{}", content.len()).as_bytes())
        );
        let temp = c_name(temp_name.as_bytes(), path)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(error.into());
        }
        drop(file);
        let renamed = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp.as_ptr(),
                directory.as_raw_fd(),
                leaf_name.as_ptr(),
            )
        };
        if renamed != 0 {
            let error = std::io::Error::last_os_error();
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(error.into());
        }
        directory.sync_all()?;
    }
    root.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn install_pulled_snapshot(dest: &Path, entries: &[(String, Vec<u8>)]) -> LinkResult<()> {
    use ring::rand::SecureRandom as _;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| LinkError::UnsafePath {
            path: dest.display().to_string(),
        })?;
    let parent_dir = open_or_create_dir_nofollow(parent)?;
    let dest_name = c_name(name.as_bytes(), &dest.display().to_string())?;
    let dest_exists = match entry_is_dir_at(parent_dir.as_raw_fd(), &dest_name)? {
        None => false,
        Some(true) => true,
        Some(false) => {
            return Err(LinkError::UnsafePath {
                path: dest.display().to_string(),
            });
        }
    };

    let mut nonce = [0_u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| invalid_feed("could not mint a pull staging name"))?;
    let stage_label = format!(
        ".{}.dbmd-pull-stage-{}",
        name.to_string_lossy(),
        URL_SAFE_NO_PAD.encode(nonce)
    );
    let stage_name = c_name(stage_label.as_bytes(), &dest.display().to_string())?;
    let stage_dir = create_dir_exclusive_at(
        parent_dir.as_raw_fd(),
        &stage_name,
        &dest.display().to_string(),
    )?;

    let prepared = (|| -> LinkResult<()> {
        if dest_exists {
            let live = open_dir_at(
                parent_dir.as_raw_fd(),
                &dest_name,
                &dest.display().to_string(),
            )?;
            clone_tree_contents(&live, &stage_dir, &dest.display().to_string())?;
        }
        write_pull_entries_beneath_dir(&stage_dir, entries)?;
        stage_dir.sync_all()?;
        Ok(())
    })();
    if let Err(error) = prepared {
        let _ = remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        );
        return Err(error);
    }

    if let Err(error) =
        install_stage_at(parent_dir.as_raw_fd(), &stage_name, &dest_name, dest_exists)
    {
        let _ = remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        );
        return Err(error);
    }
    parent_dir.sync_all()?;
    if dest_exists {
        // Commit already happened atomically. Cleanup is best-effort so a
        // failure cannot be reported as a failed pull after the live tree
        // changed; a crash may leave only this private old-tree sibling.
        let _ = remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        );
        let _ = parent_dir.sync_all();
    }
    Ok(())
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

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn preflight_zip_central_directory(
    bytes: &[u8],
    offset: usize,
    size: usize,
    count: u64,
) -> LinkResult<()> {
    const CENTRAL_ENTRY_SIG: &[u8; 4] = b"PK\x01\x02";
    let end = offset
        .checked_add(size)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| LinkError::InvalidPack {
            message: "ZIP central directory is out of bounds".to_string(),
        })?;
    let mut cursor = offset;
    for _ in 0..count {
        if bytes.get(cursor..cursor + 4) != Some(CENTRAL_ENTRY_SIG.as_slice()) {
            return Err(LinkError::InvalidPack {
                message: "ZIP central directory entry count is inconsistent".to_string(),
            });
        }
        if le_u16(bytes, cursor + 34) != Some(0) {
            return Err(LinkError::InvalidPack {
                message: "multi-disk ZIP archives are not supported".to_string(),
            });
        }
        let variable = [28, 30, 32].into_iter().try_fold(0usize, |total, at| {
            total.checked_add(le_u16(bytes, cursor + at)? as usize)
        });
        cursor = cursor
            .checked_add(46)
            .and_then(|fixed| fixed.checked_add(variable?))
            .filter(|cursor| *cursor <= end)
            .ok_or_else(|| LinkError::InvalidPack {
                message: "ZIP central directory entry is truncated".to_string(),
            })?;
    }
    if cursor != end {
        return Err(LinkError::InvalidPack {
            message: "ZIP central directory size is inconsistent".to_string(),
        });
    }
    Ok(())
}

/// Read only the bounded ZIP trailer before `ZipArchive::new` allocates one
/// metadata object per central-directory entry. Supports ordinary EOCD and the
/// Zip64 locator/record emitted for >65,535-entry archives.
fn preflight_zip_entry_count(bytes: &[u8], max_entries: usize) -> LinkResult<()> {
    const EOCD_SIG: &[u8; 4] = b"PK\x05\x06";
    const ZIP64_LOCATOR_SIG: &[u8; 4] = b"PK\x06\x07";
    const ZIP64_EOCD_SIG: &[u8; 4] = b"PK\x06\x06";
    let search_start = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let eocd = bytes[search_start..]
        .windows(4)
        .rposition(|window| window == EOCD_SIG)
        .map(|offset| search_start + offset)
        .ok_or_else(|| LinkError::InvalidPack {
            message: "ZIP has no end-of-central-directory record".to_string(),
        })?;
    let invalid_end = || LinkError::InvalidPack {
        message: "ZIP has an invalid end-of-central-directory structure".to_string(),
    };
    let comment_len = le_u16(bytes, eocd + 20).ok_or_else(invalid_end)? as usize;
    if eocd
        .checked_add(22)
        .and_then(|end| end.checked_add(comment_len))
        != Some(bytes.len())
    {
        // Do not fall back to an earlier signature. ZipArchive does, and a
        // fake low-count EOCD appended after a real Zip64 entry-count bomb
        // would otherwise bypass this allocation preflight.
        return Err(invalid_end());
    }
    let disk = le_u16(bytes, eocd + 4);
    let central_disk = le_u16(bytes, eocd + 6);
    if disk != Some(0) || central_disk != Some(0) {
        return Err(LinkError::InvalidPack {
            message: "multi-disk ZIP archives are not supported".to_string(),
        });
    }
    let entries_on_disk = le_u16(bytes, eocd + 8).ok_or_else(invalid_end)?;
    let ordinary = le_u16(bytes, eocd + 10).ok_or_else(invalid_end)?;
    if entries_on_disk != ordinary {
        return Err(LinkError::InvalidPack {
            message: "multi-disk ZIP archives are not supported".to_string(),
        });
    }
    let zip64_locator = eocd
        .checked_sub(20)
        .filter(|at| bytes.get(*at..*at + 4) == Some(ZIP64_LOCATOR_SIG.as_slice()));
    let (count, central_offset, central_size) = if ordinary != u16::MAX || zip64_locator.is_none() {
        let central_size = le_u32(bytes, eocd + 12).ok_or_else(invalid_end)? as usize;
        let central_offset = le_u32(bytes, eocd + 16).ok_or_else(invalid_end)? as usize;
        if central_offset
            .checked_add(central_size)
            .filter(|end| *end == eocd)
            .is_none()
        {
            return Err(invalid_end());
        }
        (ordinary as u64, central_offset, central_size)
    } else {
        let Some(locator) = zip64_locator else {
            return Err(invalid_end());
        };
        if le_u32(bytes, locator + 4) != Some(0) || le_u32(bytes, locator + 16) != Some(1) {
            return Err(LinkError::InvalidPack {
                message: "multi-disk ZIP64 archives are not supported".to_string(),
            });
        }
        let record = le_u64(bytes, locator + 8)
            .and_then(|offset| usize::try_from(offset).ok())
            .filter(|at| bytes.get(*at..*at + 4) == Some(ZIP64_EOCD_SIG.as_slice()))
            .ok_or_else(|| LinkError::InvalidPack {
                message: "ZIP64 archive has an invalid end record".to_string(),
            })?;
        let record_size = le_u64(bytes, record + 4)
            .and_then(|size| usize::try_from(size).ok())
            .filter(|size| *size >= 44)
            .ok_or_else(invalid_end)?;
        if record
            .checked_add(12)
            .and_then(|end| end.checked_add(record_size))
            != Some(locator)
            || le_u32(bytes, record + 16) != Some(0)
            || le_u32(bytes, record + 20) != Some(0)
        {
            return Err(invalid_end());
        }
        let zip64_on_disk = le_u64(bytes, record + 24).ok_or_else(invalid_end)?;
        let zip64_total = le_u64(bytes, record + 32).ok_or_else(invalid_end)?;
        let central_size = le_u64(bytes, record + 40)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(invalid_end)?;
        let central_offset = le_u64(bytes, record + 48)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(invalid_end)?;
        if zip64_on_disk != zip64_total
            || central_offset
                .checked_add(central_size)
                .filter(|end| *end == record)
                .is_none()
        {
            return Err(invalid_end());
        }
        let legacy_size = le_u32(bytes, eocd + 12).ok_or_else(invalid_end)?;
        let legacy_offset = le_u32(bytes, eocd + 16).ok_or_else(invalid_end)?;
        if (legacy_size != u32::MAX && legacy_size as usize != central_size)
            || (legacy_offset != u32::MAX && legacy_offset as usize != central_offset)
        {
            return Err(invalid_end());
        }
        (zip64_total, central_offset, central_size)
    };
    if count == 0 || count > max_entries as u64 {
        return Err(LinkError::InvalidPack {
            message: format!("invalid file count {count}"),
        });
    }
    preflight_zip_central_directory(bytes, central_offset, central_size, count)?;
    Ok(())
}

fn parse_store_pack(bytes: Vec<u8>) -> LinkResult<Vec<(String, Vec<u8>)>> {
    preflight_zip_entry_count(&bytes, MAX_PUSH_FILES)?;
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
    let mut seen = std::collections::HashSet::new();
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
        if !seen.insert(path.clone()) {
            return Err(LinkError::InvalidPack {
                message: format!("duplicate path `{path}`"),
            });
        }
        let remaining = MAX_STORE_BYTES.saturating_sub(total);
        if file.size() > remaining {
            return Err(LinkError::InvalidPack {
                message: "expanded content exceeds the 512 MB limit".to_string(),
            });
        }
        let mut content = Vec::new();
        (&mut file)
            .take(remaining + 1)
            .read_to_end(&mut content)
            .map_err(|err| LinkError::InvalidPack {
                message: format!("could not decompress `{path}`: {err}"),
            })?;
        if content.len() as u64 > remaining {
            return Err(LinkError::InvalidPack {
                message: "expanded content exceeds the 512 MB limit".to_string(),
            });
        }
        if content.len() as u64 != file.size() {
            return Err(LinkError::InvalidPack {
                message: format!("length mismatch for `{path}`"),
            });
        }
        total += content.len() as u64;
        entries.push((path, content));
    }
    if entries.is_empty() {
        return Err(LinkError::InvalidPack {
            message: "pack contains no files".to_string(),
        });
    }
    Ok(entries)
}

fn verify_snapshot_manifest(entries: &[(String, Vec<u8>)], signed: &[FeedFile]) -> LinkResult<()> {
    let mut expected = std::collections::BTreeMap::new();
    for file in signed {
        if !safe_store_rel_path(&file.path) {
            return Err(LinkError::UnsafePath {
                path: file.path.clone(),
            });
        }
        if !is_sha256(&file.sha256)
            || expected
                .insert(file.path.as_str(), (file.sha256.as_str(), file.bytes))
                .is_some()
        {
            return Err(invalid_feed(
                "signed snapshot manifest contains an invalid or duplicate file",
            ));
        }
    }
    if expected.len() != entries.len() {
        return Err(invalid_feed(
            "downloaded pack file set differs from the signed snapshot manifest",
        ));
    }
    for (path, bytes) in entries {
        let Some((sha256, declared_bytes)) = expected.get(path.as_str()) else {
            return Err(invalid_feed(format!(
                "downloaded pack contains unsigned path `{path}`"
            )));
        };
        if *declared_bytes != bytes.len() as u64
            || *sha256 != format!("{:x}", Sha256::digest(bytes))
        {
            return Err(invalid_feed(format!(
                "downloaded file `{path}` differs from its signed manifest"
            )));
        }
    }
    Ok(())
}

/// Collect the files a push sends: the store's owned text — `DB.md`,
/// `assets.jsonl` when present, and every content `.md` under `records/` and
/// `sources/` (the store walk, which already excludes hidden dirs like
/// `.dbmd/`, the `log/` archive, and derived `index.*` catalogs; the hub
/// derives its own index, and local history stays local). Returns
/// `(store-relative path, content)` pairs, path-sorted.
pub fn collect_push_files(store: &Store) -> LinkResult<Vec<(String, String)>> {
    require_hardened_filesystem("sync push")?;
    preflight_push_ownership(store)?;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut total = 0u64;

    let mut read_text = |rel: &str| -> LinkResult<String> {
        let bytes = store.read_bounded(Path::new(rel), MAX_STORE_BYTES.saturating_sub(total))?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| LinkError::PushTooLarge {
                detail: "uncompressed byte count overflow".to_string(),
            })?;
        if total > MAX_STORE_BYTES {
            return Err(LinkError::PushTooLarge {
                detail: format!("{total} uncompressed bytes"),
            });
        }
        String::from_utf8(bytes).map_err(|_| LinkError::NotUtf8 {
            path: rel.to_string(),
        })
    };

    out.push(("DB.md".to_string(), read_text("DB.md")?));
    if store
        .regular_file_exists(Path::new("assets.jsonl"))
        .unwrap_or(false)
    {
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

/// Refuse to build a destructive whole-store snapshot from an ambiguous local
/// tree. Ordinary read-only walks safely prune foreign paths, but a push that
/// silently omitted them could delete the hosted copies of those paths.
fn preflight_push_ownership(store: &Store) -> LinkResult<()> {
    if let Some(nested) = store.nested_store_roots()?.into_iter().next() {
        return Err(LinkError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("cannot push: nested db.md store at {}", nested.display()),
        )));
    }

    if let Some(symlink) = store.unowned_symlinks()?.into_iter().next() {
        return Err(LinkError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cannot push: {} is a symlink outside the store ownership model",
                symlink.display()
            ),
        )));
    }
    Ok(())
}

/// Push `files` to `brain` as a whole-store snapshot — the hub's push
/// semantics: the hosted copy becomes exactly this set (pull first if the
/// hosted side may have records the local copy lacks). Client-side caps
/// mirror the hub's JSON-path limits so an oversized push fails before the
/// upload.
pub fn sync_push(cfg: &HubConfig, brain: &str, files: &[(String, String)]) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    let remote = verified_remote_head(cfg, brain, false)?;
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
            let pushed = ensure_ok(
                request(cfg, "POST", &path, Some(&body), Auth::Required)?,
                "sync push",
            )?;
            return Ok(pushed);
        }
    }

    let pack = build_store_pack(files)?;
    if pack.len() as u64 > MAX_PACK_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: format!("{} pack bytes", pack.len()),
        });
    }
    let sha256 = format!("{:x}", Sha256::digest(&pack));
    let mut meta = json!({ "sha256": sha256, "bytes": pack.len() });
    if let Some(key) = &cfg.brain_key {
        if !remote.head.verified {
            return Err(invalid_feed(
                "self-custody push requires a fully verified, unscoped feed head",
            ));
        }
        let identity = remote
            .identity
            .as_ref()
            .ok_or_else(|| invalid_feed("verified brain has no current identity"))?;
        let current_multikey = format!("ed25519:{}", identity.fingerprint);
        if key.multikey != current_multikey || key.public_key_spki != identity.public_key_spki {
            return Err(invalid_feed(
                "configured brain key is not the verified current brain identity",
            ));
        }
        // The verified state pins seq + prev; a concurrent writer surfaces as
        // the hub's 422 on commit (re-run to retry against the new head).
        let next_seq = remote
            .head
            .seq
            .checked_add(1)
            .ok_or_else(|| invalid_feed("feed sequence overflow"))?;
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
            next_seq,
            ts,
            &sha256,
            &manifest,
            remote.head.feed_hash.as_deref(),
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
    put_presigned(
        cfg,
        url,
        presigned.get("headers").unwrap_or(&Value::Null),
        &pack,
    )?;
    let committed = ensure_ok(
        request(
            cfg,
            "POST",
            &format!("/api/hub/brains/{brain}/packs/commit"),
            Some(&meta),
            Auth::Required,
        )?,
        "commit pack",
    )?;
    Ok(committed)
}

fn build_store_pack(files: &[(String, String)]) -> LinkResult<Vec<u8>> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const CENTRAL_HEADER: u32 = 0x0201_4b50;
    const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
    const VERSION_20: u16 = 20;
    const MADE_BY_UNIX_20: u16 = (3 << 8) | VERSION_20;
    const UTF8_FLAG: u16 = 1 << 11;
    const STORED: u16 = 0;
    const DOS_TIME_MIDNIGHT: u16 = 0;
    const DOS_DATE_1980_01_01: u16 = (1 << 5) | 1;
    const UNIX_REGULAR_0600: u32 = 0o100600 << 16;

    struct CentralEntry<'a> {
        name: &'a [u8],
        crc32: u32,
        size: u32,
        local_offset: u32,
    }

    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    if files.is_empty() {
        return Err(LinkError::InvalidPack {
            message: "cannot create an empty snapshot pack".to_string(),
        });
    }
    if files.len() > u16::MAX as usize {
        return Err(LinkError::PushTooLarge {
            detail: format!(
                "{} files (canonical ZIP32 packs cap at {})",
                files.len(),
                u16::MAX
            ),
        });
    }

    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut previous: Option<&str> = None;
    for (path, content) in &sorted {
        if !safe_store_rel_path(path) {
            return Err(LinkError::UnsafePath {
                path: (*path).clone(),
            });
        }
        if previous == Some(path.as_str()) {
            return Err(LinkError::InvalidPack {
                message: format!("duplicate path `{path}`"),
            });
        }
        previous = Some(path.as_str());
        u32::try_from(content.len()).map_err(|_| LinkError::PushTooLarge {
            detail: format!("file `{path}` exceeds the ZIP32 per-file limit"),
        })?;
    }

    let mut out = Vec::new();
    let mut central = Vec::with_capacity(sorted.len());
    for (path, content) in sorted {
        let name = path.as_bytes();
        let name_len = u16::try_from(name.len()).map_err(|_| LinkError::InvalidPack {
            message: format!("ZIP entry name is too long: `{path}`"),
        })?;
        let bytes = content.as_bytes();
        let size = u32::try_from(bytes.len()).map_err(|_| LinkError::PushTooLarge {
            detail: format!("file `{path}` exceeds the ZIP32 per-file limit"),
        })?;
        let local_offset = u32::try_from(out.len()).map_err(|_| LinkError::PushTooLarge {
            detail: "canonical ZIP32 pack exceeds its offset limit".to_string(),
        })?;
        let crc32 = crc32fast::hash(bytes);

        // Canonical local header: sizes and CRC are known up front. Bit 3 is
        // deliberately clear, so there is no trailing data descriptor.
        push_u32(&mut out, LOCAL_HEADER);
        push_u16(&mut out, VERSION_20);
        push_u16(&mut out, UTF8_FLAG);
        push_u16(&mut out, STORED);
        push_u16(&mut out, DOS_TIME_MIDNIGHT);
        push_u16(&mut out, DOS_DATE_1980_01_01);
        push_u32(&mut out, crc32);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name_len);
        push_u16(&mut out, 0); // no local extra data
        out.extend_from_slice(name);
        out.extend_from_slice(bytes);

        central.push(CentralEntry {
            name,
            crc32,
            size,
            local_offset,
        });
    }

    let central_offset = u32::try_from(out.len()).map_err(|_| LinkError::PushTooLarge {
        detail: "canonical ZIP32 pack exceeds its offset limit".to_string(),
    })?;
    for entry in &central {
        push_u32(&mut out, CENTRAL_HEADER);
        push_u16(&mut out, MADE_BY_UNIX_20);
        push_u16(&mut out, VERSION_20);
        push_u16(&mut out, UTF8_FLAG);
        push_u16(&mut out, STORED);
        push_u16(&mut out, DOS_TIME_MIDNIGHT);
        push_u16(&mut out, DOS_DATE_1980_01_01);
        push_u32(&mut out, entry.crc32);
        push_u32(&mut out, entry.size);
        push_u32(&mut out, entry.size);
        push_u16(&mut out, entry.name.len() as u16);
        push_u16(&mut out, 0); // no central extra data
        push_u16(&mut out, 0); // no file comment
        push_u16(&mut out, 0); // disk number
        push_u16(&mut out, 0); // internal attributes
        push_u32(&mut out, UNIX_REGULAR_0600);
        push_u32(&mut out, entry.local_offset);
        out.extend_from_slice(entry.name);
    }
    let central_size = u32::try_from(out.len())
        .ok()
        .and_then(|end| end.checked_sub(central_offset))
        .ok_or_else(|| LinkError::PushTooLarge {
            detail: "canonical ZIP32 central directory exceeds its limit".to_string(),
        })?;
    let entry_count = central.len() as u16;

    push_u32(&mut out, END_OF_CENTRAL_DIRECTORY);
    push_u16(&mut out, 0); // this disk
    push_u16(&mut out, 0); // central directory disk
    push_u16(&mut out, entry_count);
    push_u16(&mut out, entry_count);
    push_u32(&mut out, central_size);
    push_u32(&mut out, central_offset);
    push_u16(&mut out, 0); // no archive comment

    if out.len() > u32::MAX as usize {
        return Err(LinkError::PushTooLarge {
            detail: "canonical ZIP32 pack exceeds 4 GiB".to_string(),
        });
    }
    Ok(out)
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
    let _ = verified_remote_head(cfg, brain, false)?;
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
    let _ = verified_remote_head(cfg, brain, false)?;
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
    let _ = verified_remote_head(cfg, brain, false)?;
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

struct BoundedVecVisitor<T, const MAX: usize> {
    label: &'static str,
    marker: std::marker::PhantomData<T>,
}

impl<'de, T, const MAX: usize> serde::de::Visitor<'de> for BoundedVecVisitor<T, MAX>
where
    T: Deserialize<'de>,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "at most {MAX} {}", self.label)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        if sequence.size_hint().is_some_and(|size| size > MAX) {
            return Err(serde::de::Error::custom(format!(
                "{} exceeds the {MAX}-item limit",
                self.label
            )));
        }
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX));
        while let Some(value) = sequence.next_element()? {
            if values.len() == MAX {
                return Err(serde::de::Error::custom(format!(
                    "{} exceeds the {MAX}-item limit",
                    self.label
                )));
            }
            values.push(value);
        }
        Ok(values)
    }
}

fn deserialize_bounded_vec<'de, D, T, const MAX: usize>(
    deserializer: D,
    label: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX> {
        label,
        marker: std::marker::PhantomData,
    })
}

fn deserialize_feed_files<'de, D>(deserializer: D) -> Result<Vec<FeedFile>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, FeedFile, MAX_PUSH_FILES>(deserializer, "feed files")
}

fn deserialize_removed_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, String, MAX_PUSH_FILES>(deserializer, "removed paths")
}

fn deserialize_previous_identities<'de, D>(
    deserializer: D,
) -> Result<Vec<PreviousIdentity>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, PreviousIdentity, MAX_IDENTITY_ROTATIONS>(
        deserializer,
        "previous identities",
    )
}

fn deserialize_rotations<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, String, MAX_IDENTITY_ROTATIONS>(
        deserializer,
        "rotation statements",
    )
}

fn deserialize_feed_items<'de, D>(deserializer: D) -> Result<Vec<FeedItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, FeedItem, FEED_PAGE_LIMIT>(deserializer, "feed entries")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FeedFile {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FeedEntry {
    v: u8,
    seq: u64,
    ts: String,
    brain: String,
    public_key: String,
    kind: String,
    op: String,
    pack_sha256: String,
    #[serde(deserialize_with = "deserialize_feed_files")]
    files: Vec<FeedFile>,
    #[serde(deserialize_with = "deserialize_removed_paths")]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FeedItem {
    hash: String,
    entry: FeedEntry,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct FeedIdentity {
    fingerprint: String,
    #[serde(rename = "publicKeySpki")]
    public_key_spki: String,
    /// Rotation history (link.md §9.1): identities this brain previously
    /// signed as. Entries verify against current OR previous — rotation
    /// never invalidates history.
    #[serde(default, deserialize_with = "deserialize_previous_identities")]
    previous: Vec<PreviousIdentity>,
    /// Exact normative rotation statements, oldest first. A list of previous
    /// public keys without these old-key signatures is not a trust chain.
    #[serde(default, deserialize_with = "deserialize_rotations")]
    rotations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
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
    #[serde(deserialize_with = "deserialize_feed_items")]
    entries: Vec<FeedItem>,
    #[serde(rename = "scopeLimited")]
    scope_limited: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RotationStatement {
    v: u8,
    op: String,
    brain: String,
    public_key: String,
    new_brain: String,
    new_public_key: String,
    prior_head_seq: u64,
    prior_feed_hash: Option<String>,
    ts: String,
    sig: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TrustState {
    v: u8,
    origin: String,
    /// The exact caller-visible ref whose resolution this checkpoint pins.
    /// Slugs/handles are mutable names at the hub; once observed, they may not
    /// silently resolve to a different canonical brain.
    #[serde(default)]
    requested: String,
    /// Canonical hub brain id returned for `requested`.
    brain: String,
    /// Federation home origin when this ref was learned from a registry.
    /// Once observed, the registry cannot silently relocate the same handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    anchor: String,
    current: String,
    #[serde(rename = "headSeq")]
    head_seq: u64,
    #[serde(rename = "feedHash")]
    feed_hash: Option<String>,
    /// Exact accepted old-key-signed rotation statements, oldest first. v2
    /// checkpoints require this vector to be an immutable prefix of every
    /// subsequently served identity chain.
    #[serde(default)]
    rotations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AliasBinding {
    v: u8,
    origin: String,
    requested: String,
    brain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home: Option<String>,
}

struct VerifiedRemote {
    head: Head,
    identity: Option<FeedIdentity>,
    head_entry: Option<FeedItem>,
    /// Present when the caller requested and verified the complete chain.
    entries: Vec<FeedItem>,
    anchor: Option<String>,
}

fn invalid_feed(message: impl Into<String>) -> LinkError {
    LinkError::InvalidFeed {
        message: message.into(),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn identity_fingerprint(public_key_spki: &str) -> LinkResult<String> {
    let der = URL_SAFE_NO_PAD
        .decode(public_key_spki)
        .map_err(|_| invalid_feed("identity public key is not base64url"))?;
    if der.len() != ED25519_SPKI_PREFIX.len() + 32 || !der.starts_with(&ED25519_SPKI_PREFIX) {
        return Err(invalid_feed(
            "identity public key is not a valid Ed25519 SPKI",
        ));
    }
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(&der)))
}

/// Verify the hub's current identity and every old-key-signed rotation that
/// leads from the original TOFU anchor to it. `previous` alone is metadata,
/// never authority.
fn verify_identity_chain(
    identity: &FeedIdentity,
    pinned: Option<&TrustState>,
) -> LinkResult<String> {
    if identity.previous.len() > MAX_IDENTITY_ROTATIONS
        || identity.rotations.len() > MAX_IDENTITY_ROTATIONS
    {
        return Err(invalid_feed(
            "identity rotation history exceeds the client cap",
        ));
    }
    if identity.fingerprint != identity_fingerprint(&identity.public_key_spki)? {
        return Err(invalid_feed(
            "current identity fingerprint does not match its public key",
        ));
    }
    for previous in &identity.previous {
        if previous.fingerprint != identity_fingerprint(&previous.public_key_spki)? {
            return Err(invalid_feed(
                "previous identity fingerprint does not match its public key",
            ));
        }
    }
    if identity.rotations.len() != identity.previous.len() {
        return Err(invalid_feed(
            "identity history is missing an old-key-signed rotation statement",
        ));
    }

    // The HTTP identity lists prior identities newest first. Rotation
    // statements are chronological, so verification walks the reversed list
    // and ends at the current identity.
    let mut chain: Vec<(&str, &str)> = identity
        .previous
        .iter()
        .rev()
        .map(|p| (p.fingerprint.as_str(), p.public_key_spki.as_str()))
        .collect();
    chain.push((&identity.fingerprint, &identity.public_key_spki));

    for (index, raw) in identity.rotations.iter().enumerate() {
        let statement: RotationStatement = serde_json::from_str(raw)
            .map_err(|_| invalid_feed("rotation statement did not parse exactly"))?;
        let (old_fingerprint, old_spki) = chain[index];
        let (new_fingerprint, new_spki) = chain[index + 1];
        if statement.v != 1
            || statement.op != "rotate"
            || statement.brain != format!("ed25519:{old_fingerprint}")
            || statement.public_key != old_spki
            || statement.new_brain != format!("ed25519:{new_fingerprint}")
            || statement.new_public_key != new_spki
            || (statement.prior_head_seq == 0 && statement.prior_feed_hash.is_some())
            || (statement.prior_head_seq > 0
                && statement
                    .prior_feed_hash
                    .as_deref()
                    .is_none_or(|hash| !is_sha256(hash)))
        {
            return Err(invalid_feed(
                "rotation statement does not connect adjacent identities",
            ));
        }
        let unsigned = serde_json::to_string(&UnsignedRotation {
            v: statement.v,
            op: &statement.op,
            brain: &statement.brain,
            public_key: &statement.public_key,
            new_brain: &statement.new_brain,
            new_public_key: &statement.new_public_key,
            prior_head_seq: statement.prior_head_seq,
            prior_feed_hash: statement.prior_feed_hash.as_deref(),
            ts: statement.ts.clone(),
        })
        .map_err(|_| invalid_feed("could not canonicalize rotation statement"))?;
        let exact = format!(
            "{},\"sig\":\"{}\"}}",
            &unsigned[..unsigned.len() - 1],
            statement.sig
        );
        if exact != *raw {
            return Err(invalid_feed(
                "rotation statement is not in normative serialization",
            ));
        }
        let der = URL_SAFE_NO_PAD
            .decode(old_spki)
            .map_err(|_| invalid_feed("rotation public key is not base64url"))?;
        let signature = URL_SAFE_NO_PAD
            .decode(&statement.sig)
            .map_err(|_| invalid_feed("rotation signature is not base64url"))?;
        UnparsedPublicKey::new(&ED25519, &der[ED25519_SPKI_PREFIX.len()..])
            .verify(unsigned.as_bytes(), &signature)
            .map_err(|_| invalid_feed("rotation signature verification failed"))?;
        if index > 0 {
            let prior: RotationStatement = serde_json::from_str(&identity.rotations[index - 1])
                .map_err(|_| invalid_feed("prior rotation statement did not parse"))?;
            if statement.prior_head_seq < prior.prior_head_seq {
                return Err(invalid_feed("rotation feed boundaries move backward"));
            }
        }
    }

    let anchor = format!("ed25519:{}", chain[0].0);
    let current = format!("ed25519:{}", identity.fingerprint);
    if let Some(pin) = pinned {
        if pin.anchor != anchor {
            return Err(invalid_feed(
                "served identity chain does not descend from the pinned anchor",
            ));
        }
        if !chain
            .iter()
            .any(|(fingerprint, _)| pin.current == format!("ed25519:{fingerprint}"))
        {
            return Err(invalid_feed(
                "served identity chain forked away from the last pinned identity",
            ));
        }
        if pin.current == current && pin.anchor != current && identity.rotations.is_empty() {
            return Err(invalid_feed("served identity discarded its rotation chain"));
        }
        if pin.v >= 2
            && (identity.rotations.len() < pin.rotations.len()
                || identity.rotations[..pin.rotations.len()] != pin.rotations)
        {
            return Err(invalid_feed(
                "served identity rewrote the locally accepted rotation history",
            ));
        }
    }
    Ok(anchor)
}

fn verify_rotation_feed_boundaries(
    identity: &FeedIdentity,
    pinned: Option<&TrustState>,
    observed: &[FeedItem],
    advertised_seq: u64,
) -> LinkResult<()> {
    let mut chain: Vec<String> = identity
        .previous
        .iter()
        .rev()
        .map(|previous| format!("ed25519:{}", previous.fingerprint))
        .collect();
    chain.push(format!("ed25519:{}", identity.fingerprint));
    let pinned_index = pinned.and_then(|pin| chain.iter().position(|key| key == &pin.current));

    for (index, raw) in identity.rotations.iter().enumerate() {
        let rotation: RotationStatement = serde_json::from_str(raw)
            .map_err(|_| invalid_feed("rotation statement did not parse"))?;
        if rotation.prior_head_seq > advertised_seq {
            return Err(invalid_feed(
                "rotation claims a feed boundary beyond the advertised head",
            ));
        }
        if let (Some(pin), Some(pin_index)) = (pinned, pinned_index) {
            if index >= pin_index && rotation.prior_head_seq < pin.head_seq {
                return Err(invalid_feed(
                    "newly disclosed rotation predates the local feed checkpoint",
                ));
            }
        }
        let actual = if rotation.prior_head_seq == 0 {
            None
        } else if pinned.is_some_and(|pin| pin.head_seq == rotation.prior_head_seq) {
            pinned.and_then(|pin| pin.feed_hash.as_deref())
        } else {
            observed
                .iter()
                .find(|item| item.entry.seq == rotation.prior_head_seq)
                .map(|item| item.hash.as_str())
        };
        if let Some(actual) = actual {
            if rotation.prior_feed_hash.as_deref() != Some(actual) {
                return Err(invalid_feed(
                    "rotation statement does not commit the verified feed boundary",
                ));
            }
        } else if rotation.prior_head_seq == 0 {
            // The empty-feed boundary is represented by (0, null); there is
            // no entry hash to find in `observed`.
        } else if pinned.is_some_and(|pin| {
            pinned_index.is_some_and(|pin_index| index >= pin_index)
                || rotation.prior_head_seq >= pin.head_seq
        }) {
            return Err(invalid_feed(
                "rotation feed boundary was not present in the verified chain",
            ));
        }
    }
    Ok(())
}

/// Once a client has checkpointed identity K, identities retired before K may
/// verify old history but can never regain authority over a later sequence.
/// This is also the safe migration rule for legacy v1 checkpoints that did not
/// persist the exact accepted rotation statements.
fn reject_retired_signer_after_checkpoint(
    identity: &FeedIdentity,
    pinned: Option<&TrustState>,
    item: &FeedItem,
) -> LinkResult<()> {
    let Some(pin) = pinned else {
        return Ok(());
    };
    if item.entry.seq <= pin.head_seq {
        return Ok(());
    }
    let mut chain: Vec<String> = identity
        .previous
        .iter()
        .rev()
        .map(|previous| format!("ed25519:{}", previous.fingerprint))
        .collect();
    chain.push(format!("ed25519:{}", identity.fingerprint));
    let pinned_index = chain
        .iter()
        .position(|key| key == &pin.current)
        .ok_or_else(|| invalid_feed("pinned identity is absent from the served chain"))?;
    let signer_index = chain
        .iter()
        .position(|key| key == &item.entry.brain)
        .ok_or_else(|| invalid_feed("feed signer is absent from the served identity chain"))?;
    if signer_index < pinned_index {
        return Err(invalid_feed(
            "a retired identity attempted to sign after the local checkpoint",
        ));
    }
    Ok(())
}

fn trust_file_name(cfg: &HubConfig, brain: &str) -> LinkResult<String> {
    let origin = normalized_origin(&cfg.hub)?;
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{origin}\0{brain}").as_bytes())
    );
    Ok(format!("{key}.json"))
}

fn alias_file_name(cfg: &HubConfig, alias: &str) -> LinkResult<String> {
    let origin = normalized_origin(&cfg.hub)?;
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{origin}\0alias\0{alias}").as_bytes())
    );
    Ok(format!("alias-{key}.json"))
}

#[cfg(unix)]
struct TrustLock {
    _file: std::fs::File,
}

#[cfg(unix)]
fn lock_trust_name(directory: &std::fs::File, state_name: &str) -> LinkResult<TrustLock> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let lock_string = format!(".{state_name}.lock");
    let lock_name = c_name(lock_string.as_bytes(), &lock_string)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            lock_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(LinkError::UnsafePath { path: lock_string });
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(TrustLock { _file: file })
}

#[cfg(unix)]
fn lock_trust_many(
    cfg: &HubConfig,
    directory: &std::fs::File,
    refs: &[&str],
) -> LinkResult<Vec<TrustLock>> {
    let mut names = refs
        .iter()
        .map(|reference| trust_file_name(cfg, reference))
        .collect::<LinkResult<Vec<_>>>()?;
    names.sort();
    names.dedup();
    names
        .iter()
        .map(|name| lock_trust_name(directory, name))
        .collect()
}

#[cfg(not(unix))]
fn lock_trust_many(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _refs: &[&str],
) -> LinkResult<Vec<()>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(unix)]
type TrustDirectory = std::fs::File;

#[cfg(not(unix))]
struct TrustDirectory;

#[cfg(unix)]
fn open_trust_dir(cfg: &HubConfig) -> LinkResult<TrustDirectory> {
    use std::os::fd::AsRawFd as _;

    let directory = open_or_create_dir_nofollow(&cfg.state_dir.join("trust"))?;
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    directory.sync_all()?;
    Ok(directory)
}

#[cfg(not(unix))]
fn open_trust_dir(_cfg: &HubConfig) -> LinkResult<TrustDirectory> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(unix)]
fn load_trust_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
) -> LinkResult<Option<TrustState>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name_string = trust_file_name(cfg, requested)?;
    let name = c_name(name_string.as_bytes(), &name_string)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(LinkError::UnsafePath { path: name_string });
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(LinkError::UnsafePath { path: name_string });
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid_feed("local identity/feed checkpoint is oversized"));
    }
    let mut state: TrustState = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_feed("local identity/feed checkpoint is corrupt"))?;
    if !matches!(state.v, 1 | 2) || state.origin != normalized_origin(&cfg.hub)? {
        return Err(invalid_feed(
            "local identity/feed checkpoint does not match this hub and brain",
        ));
    }
    if state.v == 1 {
        // Legacy files were keyed by canonical brain id. They can migrate only
        // when the caller used that exact id; old slug lookups had no durable
        // alias binding and therefore cannot be guessed safely.
        if state.brain != requested {
            return Err(invalid_feed(
                "legacy checkpoint is not bound to the requested brain id",
            ));
        }
        state.requested = requested.to_string();
    } else if state.requested != requested {
        return Err(invalid_feed(
            "local identity/feed checkpoint is bound to a different requested ref",
        ));
    }
    Ok(Some(state))
}

#[cfg(not(unix))]
fn load_trust_in(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _brain: &str,
) -> LinkResult<Option<TrustState>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(all(test, unix))]
fn load_trust(cfg: &HubConfig, requested: &str) -> LinkResult<Option<TrustState>> {
    let directory = open_trust_dir(cfg)?;
    load_trust_in(cfg, &directory, requested)
}

#[cfg(unix)]
fn save_trust_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    state: &TrustState,
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name_string = trust_file_name(cfg, &state.requested)?;
    let name = c_name(name_string.as_bytes(), &name_string)?;
    let mut bytes = serde_json::to_vec(state)
        .map_err(|_| invalid_feed("could not serialize local trust checkpoint"))?;
    bytes.push(b'\n');

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_string = format!(".{name_string}.tmp.{}-{nonce}", std::process::id());
    let temp = c_name(temp_string.as_bytes(), &temp_string)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(file);
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            directory.as_raw_fd(),
            name.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    directory.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn save_trust_in(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _state: &TrustState,
) -> LinkResult<()> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(unix)]
fn load_alias_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
) -> LinkResult<Option<AliasBinding>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name_string = alias_file_name(cfg, requested)?;
    let name = c_name(name_string.as_bytes(), &name_string)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(LinkError::UnsafePath { path: name_string });
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(LinkError::UnsafePath { path: name_string });
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(invalid_feed("local alias binding is oversized"));
    }
    let alias: AliasBinding = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_feed("local alias binding is corrupt"))?;
    if alias.v != 1 || alias.origin != normalized_origin(&cfg.hub)? || alias.requested != requested
    {
        return Err(invalid_feed(
            "local alias binding does not match this hub and requested ref",
        ));
    }
    Ok(Some(alias))
}

#[cfg(not(unix))]
fn load_alias_in(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _requested: &str,
) -> LinkResult<Option<AliasBinding>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(unix)]
fn save_alias_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    alias: &AliasBinding,
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let name_string = alias_file_name(cfg, &alias.requested)?;
    let name = c_name(name_string.as_bytes(), &name_string)?;
    let mut bytes = serde_json::to_vec(alias)
        .map_err(|_| invalid_feed("could not serialize local alias binding"))?;
    bytes.push(b'\n');
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_string = format!(".{name_string}.tmp.{}-{nonce}", std::process::id());
    let temp = c_name(temp_string.as_bytes(), &temp_string)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(file);
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            directory.as_raw_fd(),
            name.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    directory.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn save_alias_in(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _alias: &AliasBinding,
) -> LinkResult<()> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

/// Load the canonical checkpoint shared by every spelling of one brain and
/// the separate alias binding. This also performs the one-way migration from
/// pre-v3 checkpoints that stored a full trust state under the alias itself.
/// Callers hold both alias and canonical locks before entering.
fn load_canonical_pin(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
    resolved_brain: &str,
) -> LinkResult<(Option<TrustState>, Option<AliasBinding>)> {
    let mut canonical = load_trust_in(cfg, directory, resolved_brain)?;
    if requested == resolved_brain {
        return Ok((canonical, None));
    }

    let mut alias = load_alias_in(cfg, directory, requested)?;
    if let Some(binding) = &alias {
        if binding.brain != resolved_brain {
            return Err(invalid_feed(
                "requested brain alias now resolves to a different canonical brain",
            ));
        }
        return Ok((canonical, alias));
    }

    // A v2 build stored the complete checkpoint under the requested slug.
    // Promote it to the canonical ULID key before creating the lightweight
    // alias binding. Never silently merge two independently advanced states.
    if let Some(legacy) = load_trust_in(cfg, directory, requested)? {
        if legacy.brain != resolved_brain {
            return Err(invalid_feed(
                "legacy alias checkpoint names a different canonical brain",
            ));
        }
        if let Some(existing) = &canonical {
            if existing.brain != legacy.brain
                || existing.anchor != legacy.anchor
                || existing.current != legacy.current
                || existing.head_seq != legacy.head_seq
                || existing.feed_hash != legacy.feed_hash
                || existing.rotations != legacy.rotations
            {
                return Err(invalid_feed(
                    "legacy alias checkpoint conflicts with the canonical checkpoint",
                ));
            }
        } else {
            let mut promoted = legacy.clone();
            promoted.requested = resolved_brain.to_string();
            promoted.home = None;
            save_trust_in(cfg, directory, &promoted)?;
            canonical = Some(promoted);
        }
        alias = Some(AliasBinding {
            v: 1,
            origin: normalized_origin(&cfg.hub)?,
            requested: requested.to_string(),
            brain: resolved_brain.to_string(),
            home: legacy.home,
        });
        save_alias_in(cfg, directory, alias.as_ref().expect("alias just created"))?;
    }
    Ok((canonical, alias))
}

fn save_canonical_pin_and_alias(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
    resolved_brain: &str,
    mut state: TrustState,
    existing_alias: Option<&AliasBinding>,
) -> LinkResult<()> {
    state.requested = resolved_brain.to_string();
    state.brain = resolved_brain.to_string();
    state.home = None;
    save_trust_in(cfg, directory, &state)?;
    if requested != resolved_brain {
        save_alias_in(
            cfg,
            directory,
            &AliasBinding {
                v: 1,
                origin: normalized_origin(&cfg.hub)?,
                requested: requested.to_string(),
                brain: resolved_brain.to_string(),
                home: existing_alias.and_then(|alias| alias.home.clone()),
            },
        )?;
    }
    Ok(())
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
    {
        return Err(invalid_feed("entry public key is not a valid Ed25519 SPKI"));
    }
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&public_der));
    if entry.brain != format!("ed25519:{fingerprint}") {
        return Err(invalid_feed(
            "brain fingerprint does not match its public key",
        ));
    }
    // Verify the complete rotation authority before using any previous key.
    let _ = verify_identity_chain(identity, None)?;
    let mut chain: Vec<(&str, &str)> = identity
        .previous
        .iter()
        .rev()
        .map(|previous| {
            (
                previous.fingerprint.as_str(),
                previous.public_key_spki.as_str(),
            )
        })
        .collect();
    chain.push((&identity.fingerprint, &identity.public_key_spki));
    let signer_index = chain.iter().position(|(known_fingerprint, spki)| {
        *known_fingerprint == fingerprint && *spki == entry.public_key
    });
    let Some(signer_index) = signer_index else {
        return Err(invalid_feed(
            "entry signer is not this brain's identity (current or rotated-from)",
        ));
    };
    let lower_boundary = if signer_index == 0 {
        None
    } else {
        let prior: RotationStatement = serde_json::from_str(&identity.rotations[signer_index - 1])
            .map_err(|_| invalid_feed("rotation statement did not parse"))?;
        Some(prior.prior_head_seq)
    };
    let upper_boundary = if signer_index == identity.rotations.len() {
        None
    } else {
        let next: RotationStatement = serde_json::from_str(&identity.rotations[signer_index])
            .map_err(|_| invalid_feed("rotation statement did not parse"))?;
        Some(next.prior_head_seq)
    };
    if lower_boundary.is_some_and(|boundary| entry.seq <= boundary)
        || upper_boundary.is_some_and(|boundary| entry.seq > boundary)
    {
        return Err(invalid_feed(
            "entry signer is outside its authenticated rotation epoch",
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
    prior_head_seq: u64,
    prior_feed_hash: Option<&'a str>,
    ts: String,
}

/// Durable intent for an in-flight key rotation. The hub's recovery contract
/// identifies an ambiguous retry by the exact statement bytes, so the
/// statement cannot be reconstructed from the key and feed boundary later:
/// its timestamp and signature would differ.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RotationJournal {
    v: u8,
    origin: String,
    brain: String,
    old_brain: String,
    new_brain: String,
    prior_head_seq: u64,
    prior_feed_hash: Option<String>,
    statement: String,
}

fn rotation_journal_path(key_path: &Path) -> PathBuf {
    let mut path = key_path.as_os_str().to_os_string();
    path.push(".rotation.json");
    PathBuf::from(path)
}

fn read_rotation_journal(path: &Path) -> LinkResult<RotationJournal> {
    #[cfg(unix)]
    let file = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        let parent = open_existing_dir_nofollow(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| {
                bad_agent_key(&format!("cannot open the rotation journal parent: {error}"))
            })?;
        let leaf_name = path
            .file_name()
            .ok_or_else(|| bad_agent_key("the rotation journal path has no file name"))?;
        let leaf = c_name(leaf_name.as_bytes(), &path.display().to_string())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(bad_agent_key(
                "the rotation journal must be an existing regular file without symlink ancestors",
            ));
        }
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)
        .map_err(|error| bad_agent_key(&format!("cannot read the rotation journal: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| bad_agent_key(&format!("cannot inspect the rotation journal: {error}")))?;
    if !metadata.is_file() || metadata.len() > MAX_REGISTRY_CARD_BYTES {
        return Err(bad_agent_key(
            "the rotation journal must be a bounded regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(bad_agent_key(
                "the rotation journal is accessible to group/other; set mode 0600",
            ));
        }
    }
    serde_json::from_reader(file)
        .map_err(|_| bad_agent_key("the rotation journal is not valid exact JSON"))
}

fn remove_rotation_journal(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(parent) =
            open_existing_dir_nofollow(path.parent().unwrap_or_else(|| Path::new(".")))
        else {
            return;
        };
        let Some(leaf_name) = path.file_name() else {
            return;
        };
        let Ok(leaf) = c_name(leaf_name.as_bytes(), &path.display().to_string()) else {
            return;
        };
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0 {
            let _ = parent.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::remove_file(path);
    }
}

fn validate_rotation_journal(
    journal: &RotationJournal,
    cfg: &HubConfig,
    canonical_brain: &str,
    old_key: &AgentSigningKey,
    new_key: &AgentSigningKey,
    head: &Head,
) -> LinkResult<()> {
    if journal.v != 1
        || journal.origin != normalized_origin(&cfg.hub)?
        || journal.brain != canonical_brain
        || journal.old_brain != old_key.multikey
        || journal.new_brain != new_key.multikey
        || journal.prior_head_seq != head.seq
        || journal.prior_feed_hash != head.feed_hash
    {
        return Err(invalid_feed(
            "rotation journal does not match the verified key and feed boundary",
        ));
    }
    let statement: RotationStatement = serde_json::from_str(&journal.statement)
        .map_err(|_| invalid_feed("rotation journal statement did not parse exactly"))?;
    if statement.prior_head_seq != journal.prior_head_seq
        || statement.prior_feed_hash != journal.prior_feed_hash
        || statement.brain != old_key.multikey
        || statement.public_key != old_key.public_key_spki
        || statement.new_brain != new_key.multikey
        || statement.new_public_key != new_key.public_key_spki
    {
        return Err(invalid_feed(
            "rotation journal statement does not match its durable intent",
        ));
    }
    let identity = FeedIdentity {
        fingerprint: new_key.multikey.trim_start_matches("ed25519:").to_string(),
        public_key_spki: new_key.public_key_spki.clone(),
        previous: vec![PreviousIdentity {
            fingerprint: old_key.multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: old_key.public_key_spki.clone(),
        }],
        rotations: vec![journal.statement.clone()],
    };
    verify_identity_chain(&identity, None)?;
    Ok(())
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
/// §9.1 statement — the new key plus exact prior feed boundary, signed by the
/// OLD key in normative serialization — and send it to the hub. The new secret
/// is durably created at 0600 before the POST; an existing output is reused for
/// idempotent retry/reconciliation. The old key is left untouched.
pub fn rotate_brain_key(
    cfg: &HubConfig,
    brain: &str,
    old_key: &AgentSigningKey,
    out: &Path,
) -> LinkResult<RotationReport> {
    require_hardened_filesystem("key rotation")?;
    require_safe_ref(brain)?;
    // The new private key must be durable *before* the hub can accept its
    // public half. An existing file is the retry/reconciliation path after an
    // ambiguous network failure: reuse it, never generate another identity.
    let new_key = if out.exists() {
        load_signing_key(out)?
    } else {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| bad_agent_key("key generation failed"))?;
        let pair = agent_keypair(pkcs8.as_ref())?;
        let (public_key_spki, multikey) = public_identity_for(&pair);
        write_secret_new(
            out,
            format!("{}\n", URL_SAFE_NO_PAD.encode(pkcs8.as_ref())).as_bytes(),
        )?;
        AgentSigningKey {
            pkcs8: pkcs8.as_ref().to_vec(),
            multikey,
            public_key_spki,
        }
    };
    let new_spki = new_key.public_key_spki.clone();
    let new_multikey = new_key.multikey.clone();
    let journal_path = rotation_journal_path(out);
    let before = verified_remote_head(cfg, brain, false)?;
    let served_identity = before
        .identity
        .as_ref()
        .ok_or_else(|| invalid_feed("cannot rotate a brain with no signed identity"))?;
    let served_multikey = format!("ed25519:{}", served_identity.fingerprint);
    if served_multikey == new_multikey {
        remove_rotation_journal(&journal_path);
        return Ok(RotationReport {
            brain: brain.to_string(),
            multikey: new_multikey,
            key_file: out.display().to_string(),
            previous: served_identity
                .previous
                .iter()
                .map(|identity| format!("ed25519:{}", identity.fingerprint))
                .collect(),
        });
    }
    if served_multikey != old_key.multikey {
        return Err(invalid_feed(
            "the supplied old key is not the brain's verified current identity",
        ));
    }

    let journal = if journal_path.exists() {
        read_rotation_journal(&journal_path)?
    } else {
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
            prior_head_seq: before.head.seq,
            prior_feed_hash: before.head.feed_hash.as_deref(),
            ts,
        })
        .expect("serialize rotation");
        let old_pair = agent_keypair(&old_key.pkcs8)?;
        let sig = URL_SAFE_NO_PAD.encode(old_pair.sign(unsigned.as_bytes()).as_ref());
        let statement = format!("{},\"sig\":\"{}\"}}", &unsigned[..unsigned.len() - 1], sig);
        let journal = RotationJournal {
            v: 1,
            origin: normalized_origin(&cfg.hub)?,
            brain: before.head.brain.clone(),
            old_brain: old_key.multikey.clone(),
            new_brain: new_multikey.clone(),
            prior_head_seq: before.head.seq,
            prior_feed_hash: before.head.feed_hash.clone(),
            statement,
        };
        let mut exact = serde_json::to_vec(&journal)
            .map_err(|_| invalid_feed("could not serialize rotation journal"))?;
        exact.push(b'\n');
        if write_secret_new(&journal_path, &exact).is_err() {
            // A concurrent retry may have won the O_EXCL race. Only an exact,
            // fully validated journal is allowed to recover that race.
            read_rotation_journal(&journal_path)?
        } else {
            journal
        }
    };
    validate_rotation_journal(
        &journal,
        cfg,
        &before.head.brain,
        old_key,
        &new_key,
        &before.head,
    )?;

    let body = json!({ "statement": journal.statement });
    let path = format!("/api/hub/brains/{brain}/rotate");
    let attempted = request(cfg, "POST", &path, Some(&body), Auth::Required);
    let attempted_failure = match attempted {
        Ok(response) if (200..300).contains(&response.status) => None,
        Ok(response) => Some(ensure_ok(response, "key rotate").unwrap_err()),
        Err(error) => Some(error),
    };

    // A 2xx body is not authority, and a failed response may have followed a
    // committed mutation. In both cases success means the normal verifier sees
    // an append-only rotation chain ending at the durable new key.
    let after = match verified_remote_head(cfg, brain, false) {
        Ok(after) => after,
        Err(error) => return Err(attempted_failure.unwrap_or(error)),
    };
    let identity = after
        .identity
        .as_ref()
        .ok_or_else(|| invalid_feed("rotated brain has no verified identity"))?;
    if format!("ed25519:{}", identity.fingerprint) != new_multikey
        || identity.public_key_spki != new_spki
    {
        return Err(attempted_failure.unwrap_or_else(|| {
            invalid_feed("hub acknowledged rotation without committing the verified new identity")
        }));
    }
    let previous = identity
        .previous
        .iter()
        .map(|prior| format!("ed25519:{}", prior.fingerprint))
        .collect();
    remove_rotation_journal(&journal_path);

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

/// Fully re-verified mirror material suitable for a read-only re-server.
#[derive(Debug)]
pub struct VerifiedMirrorMaterial {
    pub brain: String,
    pub head_seq: u64,
    pub feed_hash: Option<String>,
    pub identity: serde_json::Value,
    /// Sequence, exact normative entry JSON without newline, and entry hash.
    pub entries: Vec<(u64, String, String)>,
    pub pack_sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMirrorHead {
    brain: String,
    #[serde(rename = "headSeq")]
    head_seq: u64,
    #[serde(rename = "feedHash")]
    feed_hash: Option<String>,
}

/// Re-verify mirror metadata, every signature/hash/rotation boundary, and the
/// exact snapshot pack before `dbmd serve` exposes any bytes.
pub fn verify_mirror_material(
    head_bytes: &[u8],
    identity_bytes: &[u8],
    feed_bytes: &[Vec<u8>],
    snapshot_pack: Option<&[u8]>,
    expected_anchor: &str,
) -> LinkResult<VerifiedMirrorMaterial> {
    let snapshot_hash = snapshot_pack
        .filter(|pack| !pack.is_empty())
        .map(content_sha256);
    verify_mirror_material_with_pack_hash(
        head_bytes,
        identity_bytes,
        feed_bytes,
        snapshot_hash.as_deref(),
        expected_anchor,
    )
}

/// Re-verify mirror metadata against a snapshot digest computed from a held
/// no-follow file capability. This lets `dbmd serve` authenticate and retain a
/// large pack without ever buffering the pack in process memory.
pub fn verify_mirror_material_with_pack_hash(
    head_bytes: &[u8],
    identity_bytes: &[u8],
    feed_bytes: &[Vec<u8>],
    snapshot_pack_sha256: Option<&str>,
    expected_anchor: &str,
) -> LinkResult<VerifiedMirrorMaterial> {
    let head: StoredMirrorHead = serde_json::from_slice(head_bytes)
        .map_err(|_| invalid_feed("stored mirror head did not parse exactly"))?;
    require_safe_ref(&head.brain)?;
    if head.head_seq > MAX_FEED_REPLAY_ENTRIES || feed_bytes.len() as u64 != head.head_seq {
        return Err(invalid_feed(
            "stored mirror feed count does not match its bounded head sequence",
        ));
    }
    let aggregate = feed_bytes
        .iter()
        .try_fold(0u64, |total, bytes| total.checked_add(bytes.len() as u64))
        .ok_or_else(|| invalid_feed("stored mirror feed size overflow"))?;
    if aggregate > MAX_FEED_REPLAY_BYTES {
        return Err(invalid_feed(
            "stored mirror feed metadata exceeds the aggregate limit",
        ));
    }
    let identity: FeedIdentity = serde_json::from_slice(identity_bytes)
        .map_err(|_| invalid_feed("stored mirror identity did not parse exactly"))?;
    let anchor = verify_identity_chain(&identity, None)?;
    if anchor != expected_anchor {
        return Err(invalid_feed(
            "stored mirror identity does not descend from the explicitly trusted anchor",
        ));
    }

    let mut entries = Vec::with_capacity(feed_bytes.len());
    let mut items = Vec::with_capacity(feed_bytes.len());
    let mut previous_hash = None;
    let mut pack_sha256 = None;
    for (index, bytes) in feed_bytes.iter().enumerate() {
        let exact = bytes
            .strip_suffix(b"\n")
            .ok_or_else(|| invalid_feed("stored feed entry lacks its exact trailing newline"))?;
        if exact.ends_with(b"\n") {
            return Err(invalid_feed("stored feed entry has extra trailing bytes"));
        }
        let entry: FeedEntry = serde_json::from_slice(exact)
            .map_err(|_| invalid_feed("stored feed entry did not parse exactly"))?;
        let expected_seq = index as u64 + 1;
        if entry.seq != expected_seq || entry.prev_entry_hash != previous_hash {
            return Err(invalid_feed(
                "stored mirror feed is not contiguous and hash-chained",
            ));
        }
        let canonical = serde_json::to_vec(&entry)
            .map_err(|_| invalid_feed("could not canonicalize stored feed entry"))?;
        if canonical != exact {
            return Err(invalid_feed(
                "stored feed entry is not in normative serialization",
            ));
        }
        let hash = content_sha256(bytes);
        let item = FeedItem {
            hash: hash.clone(),
            entry,
        };
        verify_feed_item(&item, &identity)?;
        previous_hash = Some(hash.clone());
        if expected_seq == head.head_seq {
            pack_sha256 = Some(item.entry.pack_sha256.clone());
        }
        entries.push((
            expected_seq,
            std::str::from_utf8(exact)
                .map_err(|_| invalid_feed("stored feed entry is not UTF-8"))?
                .to_string(),
            hash,
        ));
        items.push(item);
    }
    if previous_hash != head.feed_hash {
        return Err(invalid_feed(
            "stored mirror feed does not converge on its advertised head",
        ));
    }
    verify_rotation_feed_boundaries(&identity, None, &items, head.head_seq)?;
    match (head.head_seq, snapshot_pack_sha256, pack_sha256.as_deref()) {
        (0, None, None) => {}
        (_, Some(actual), Some(expected)) if actual == expected => {}
        _ => {
            return Err(LinkError::InvalidPack {
                message: "stored snapshot pack does not match the signed head digest".to_string(),
            });
        }
    }
    let identity_value = serde_json::to_value(&identity)
        .map_err(|_| invalid_feed("could not serialize verified mirror identity"))?;
    Ok(VerifiedMirrorMaterial {
        brain: head.brain,
        head_seq: head.head_seq,
        feed_hash: head.feed_hash,
        identity: identity_value,
        entries,
        pack_sha256,
    })
}

/// SHA-256 hex of one feed entry's stored bytes (`exact JSON + "\n"`) — the
/// entry hash every consumer recomputes (SPEC §5.3).
pub fn feed_entry_hash(exact_sans_newline: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{exact_sans_newline}\n").as_bytes())
    )
}

/// SHA-256 hex for content a signed manifest names. Exposed for the thin
/// `dbmd serve` adapter; cryptographic verification remains centralized here.
pub fn content_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// SHA-256 a stream with a fixed-size working buffer.
pub fn content_sha256_reader(mut reader: impl Read) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Replicate a brain with full verification (link.md §5.4 over the WHOLE
/// chain, not just the head): every entry's signature, hash, sequence
/// contiguity, prev-hash linkage, rotation chain, and exact signed pack are
/// checked in a sibling staging directory. Only then is the old mirror swapped
/// out through an atomic directory exchange. Every stage, install, and cleanup
/// operation is relative to one held no-follow parent-directory capability, so
/// renaming an ancestor cannot redirect any write or deletion.
pub fn mirror(cfg: &HubConfig, brain: &str, dest: &Path) -> LinkResult<MirrorReport> {
    require_hardened_filesystem("mirror")?;
    require_safe_ref(brain)?;
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| LinkError::UnsafePath {
            path: dest.display().to_string(),
        })?;
    #[cfg(unix)]
    let parent_dir = open_or_create_dir_nofollow(parent)?;
    #[cfg(unix)]
    use std::os::fd::AsRawFd as _;
    #[cfg(unix)]
    let dest_name = c_name(name.as_bytes(), &dest.display().to_string())?;
    #[cfg(unix)]
    let dest_exists = match entry_is_dir_at(parent_dir.as_raw_fd(), &dest_name)? {
        None => false,
        Some(true) => true,
        Some(false) => {
            return Err(LinkError::UnsafePath {
                path: dest.display().to_string(),
            });
        }
    };

    // A fixed backup was used by pre-hardening builds. Never interpret or
    // delete an attacker-planted entry at that name; require manual recovery.
    #[cfg(unix)]
    let legacy_backup_name = c_name(
        format!(".{name}.dbmd-backup").as_bytes(),
        &dest.display().to_string(),
    )?;
    #[cfg(unix)]
    if entry_is_dir_at(parent_dir.as_raw_fd(), &legacy_backup_name)?.is_some() {
        return Err(LinkError::UnsafePath {
            path: parent
                .join(format!(".{name}.dbmd-backup"))
                .display()
                .to_string(),
        });
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stage_label = format!(".{name}.dbmd-stage-{}-{nonce}", std::process::id());
    #[cfg(unix)]
    let stage_name = c_name(stage_label.as_bytes(), &dest.display().to_string())?;
    #[cfg(unix)]
    let stage_dir = create_dir_exclusive_at(
        parent_dir.as_raw_fd(),
        &stage_name,
        &dest.display().to_string(),
    )?;

    let assembled = (|| -> LinkResult<MirrorReport> {
        let remote = verified_remote_head(cfg, brain, true)?;
        let brain_id = remote.head.brain.clone();
        let identity = remote
            .identity
            .as_ref()
            .ok_or_else(|| invalid_feed("brain has no signed identity"))?;
        let anchor = remote
            .anchor
            .clone()
            .ok_or_else(|| invalid_feed("brain has no verified identity anchor"))?;
        let pack = download_verified_snapshot_pack(cfg, &brain_id, &remote)?;
        let snapshot_entries = parse_store_pack(pack.clone())?;
        let snapshot_count = snapshot_entries.len();
        let mut staged_entries = snapshot_entries;
        staged_entries.push((format!("{MIRROR_REL_DIR}/snapshot.pack"), pack));
        for item in &remote.entries {
            let mut exact = serde_json::to_vec(&item.entry)
                .map_err(|_| invalid_feed("could not serialize signed feed entry"))?;
            exact.push(b'\n');
            if format!("{:x}", Sha256::digest(&exact)) != item.hash {
                return Err(invalid_feed(
                    "serialized mirror entry differs from its verified hash",
                ));
            }
            staged_entries.push((
                format!("{MIRROR_REL_DIR}/feed/{}.json", item.entry.seq),
                exact,
            ));
        }
        let mut identity_bytes = serde_json::to_vec(identity)
            .map_err(|_| invalid_feed("could not serialize verified identity chain"))?;
        identity_bytes.push(b'\n');
        staged_entries.push((format!("{MIRROR_REL_DIR}/identity.json"), identity_bytes));
        let mut head_bytes = serde_json::to_vec(&json!({
            "brain": brain_id,
            "headSeq": remote.head.seq,
            "feedHash": remote.head.feed_hash,
        }))
        .map_err(|_| invalid_feed("could not serialize verified mirror head"))?;
        head_bytes.push(b'\n');
        staged_entries.push((format!("{MIRROR_REL_DIR}/head.json"), head_bytes));
        staged_entries.push((
            CONFIG_REL_PATH.to_string(),
            format!("hub = {}\npin = {anchor}\n", cfg.hub).into_bytes(),
        ));
        #[cfg(unix)]
        write_pull_entries_beneath_dir(&stage_dir, &staged_entries)?;

        Ok(MirrorReport {
            brain: brain_id,
            head_seq: remote.head.seq,
            feed_hash: remote.head.feed_hash,
            entries: remote.entries.len() as u64,
            pinned: anchor,
            files: snapshot_count,
        })
    })();

    let report = match assembled {
        Ok(report) => report,
        Err(error) => {
            #[cfg(unix)]
            let _ = remove_tree_at(
                parent_dir.as_raw_fd(),
                &stage_name,
                &dest.display().to_string(),
            );
            return Err(error);
        }
    };

    #[cfg(unix)]
    if let Err(error) =
        install_stage_at(parent_dir.as_raw_fd(), &stage_name, &dest_name, dest_exists)
    {
        let _ = remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        );
        return Err(error);
    }
    // An exchange leaves the old mirror at the unique stage name. Cleanup is
    // capability-relative and never follows symlinks in hostile old content.
    #[cfg(unix)]
    if dest_exists {
        remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        )?;
    }
    #[cfg(unix)]
    parent_dir.sync_all()?;
    Ok(report)
}

fn verified_remote_head(
    cfg: &HubConfig,
    brain: &str,
    require_full_chain: bool,
) -> LinkResult<VerifiedRemote> {
    require_hardened_filesystem("verified link.md state")?;
    require_safe_ref(brain)?;
    // Refuse an unsafe state root before sending credentials or consulting an
    // untrusted card, and retain this exact directory inode for the complete
    // checkpoint transaction.
    let trust_directory = open_trust_dir(cfg)?;
    let path = format!("/api/hub/brains/{brain}");
    let body = ensure_ok(
        request(cfg, "GET", &path, None, Auth::Required)?,
        "subscribe",
    )?;
    let resolved_brain = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| crate::ulid::is_ulid(id))
        .ok_or_else(|| invalid_feed("brain card has no canonical ULID id"))?
        .to_string();
    if crate::ulid::is_ulid(brain) && resolved_brain != brain {
        return Err(invalid_feed(
            "brain card id differs from the explicitly requested brain id",
        ));
    }
    // The card supplies the canonical ULID. Lock alias + canonical keys in
    // deterministic filename order, then hold both through verify + save.
    // Concurrent aliases for the same brain therefore converge on one
    // checkpoint instead of establishing independent TOFU universes.
    let _trust_locks = lock_trust_many(cfg, &trust_directory, &[brain, &resolved_brain])?;
    let (pinned, alias_binding) =
        load_canonical_pin(cfg, &trust_directory, brain, &resolved_brain)?;
    let seq = body.get("headSeq").and_then(Value::as_u64).unwrap_or(0);
    let advertised_hash = body
        .get("feedHash")
        .and_then(Value::as_str)
        .map(str::to_string);
    let updated_at = body
        .get("updatedAt")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(pin) = &pinned {
        if seq < pin.head_seq {
            return Err(invalid_feed(format!(
                "feed rollback: hub advertised sequence {seq}, local checkpoint is {}",
                pin.head_seq
            )));
        }
        if seq == pin.head_seq && advertised_hash != pin.feed_hash {
            return Err(invalid_feed(
                "feed equivocation: the checkpoint sequence now has a different hash",
            ));
        }
    }
    if seq == 0 {
        if advertised_hash.is_some() {
            return Err(invalid_feed("an empty feed advertised a head hash"));
        }
        let identity: FeedIdentity = serde_json::from_value(
            body.get("identity")
                .cloned()
                .ok_or_else(|| invalid_feed("empty brain card has no signed identity"))?,
        )
        .map_err(|_| invalid_feed("empty brain card has an invalid identity"))?;
        let anchor = verify_identity_chain(&identity, pinned.as_ref())?;
        // A valid old-key-signed rotation is still inconsistent if it commits
        // to history that the same card now claims never existed. Run the
        // identical feed-boundary proof used by non-empty heads before this
        // identity can become a durable TOFU checkpoint.
        verify_rotation_feed_boundaries(&identity, pinned.as_ref(), &[], seq)?;
        save_canonical_pin_and_alias(
            cfg,
            &trust_directory,
            brain,
            &resolved_brain,
            TrustState {
                v: 2,
                origin: normalized_origin(&cfg.hub)?,
                requested: resolved_brain.clone(),
                brain: resolved_brain.clone(),
                home: None,
                anchor: anchor.clone(),
                current: format!("ed25519:{}", identity.fingerprint),
                head_seq: 0,
                feed_hash: None,
                rotations: identity.rotations.clone(),
            },
            alias_binding.as_ref(),
        )?;
        return Ok(VerifiedRemote {
            head: Head {
                brain: resolved_brain,
                seq,
                updated_at,
                feed_hash: None,
                verified: true,
            },
            identity: Some(identity),
            head_entry: None,
            entries: Vec::new(),
            anchor: Some(anchor),
        });
    }
    if advertised_hash.as_ref().is_none_or(|hash| !is_sha256(hash)) {
        return Err(invalid_feed(
            "non-empty feed did not advertise a valid SHA-256 head",
        ));
    }

    // On first contact the signed head itself is the TOFU checkpoint. A full
    // history replay cannot add authority before an anchor exists; mirrors
    // still request the complete chain because they promise an archival copy.
    let replay_head_only = !require_full_chain
        && pinned
            .as_ref()
            .is_none_or(|checkpoint| checkpoint.head_seq == seq);
    let mut after = if replay_head_only {
        seq - 1
    } else if require_full_chain || pinned.is_none() {
        0
    } else {
        pinned.as_ref().map_or(0, |checkpoint| checkpoint.head_seq)
    };
    let mut expected_seq = after + 1;
    let mut previous_hash = if require_full_chain || pinned.is_none() || replay_head_only {
        None
    } else {
        pinned
            .as_ref()
            .and_then(|checkpoint| checkpoint.feed_hash.clone())
    };
    let mut identity: Option<FeedIdentity> = None;
    let mut anchor: Option<String> = None;
    let mut head_entry: Option<FeedItem> = None;
    let mut all_entries = Vec::new();
    let mut observed_entries = Vec::new();
    let replay_count = seq
        .checked_sub(after)
        .ok_or_else(|| invalid_feed("feed replay range moved backward"))?;
    if replay_count > MAX_FEED_REPLAY_ENTRIES {
        return Err(invalid_feed(format!(
            "feed replay requires {replay_count} entries, over the client cap"
        )));
    }
    let mut replay_bytes = 0u64;

    loop {
        let feed_bytes = ensure_raw_ok(
            request_raw(
                cfg,
                "GET",
                &format!("/api/hub/brains/{brain}/feed?after={after}&limit=100"),
                None,
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "subscribe feed",
        )?;
        let feed: FeedResponse = serde_json::from_slice(&feed_bytes)
            .map_err(|_| invalid_feed("hub returned an invalid feed shape"))?;
        if feed.head_seq != seq || feed.feed_hash != advertised_hash {
            return Err(invalid_feed("brain card and feed head disagree"));
        }
        if feed.entries.len() > FEED_PAGE_LIMIT {
            return Err(invalid_feed("feed page exceeds the requested entry limit"));
        }
        if feed.scope_limited {
            if require_full_chain {
                return Err(invalid_feed(
                    "path-scoped grants cannot verify a full snapshot chain",
                ));
            }
            return Ok(VerifiedRemote {
                head: Head {
                    brain: resolved_brain,
                    seq,
                    updated_at,
                    feed_hash: advertised_hash,
                    verified: false,
                },
                identity: None,
                head_entry: None,
                entries: Vec::new(),
                anchor: None,
            });
        }
        let page_identity = feed
            .identity
            .ok_or_else(|| invalid_feed("feed has no brain identity"))?;
        let page_anchor = verify_identity_chain(&page_identity, pinned.as_ref())?;
        if identity
            .as_ref()
            .is_some_and(|existing| existing != &page_identity)
        {
            return Err(invalid_feed("identity changed while reading the feed"));
        }
        if anchor
            .as_ref()
            .is_some_and(|existing| existing != &page_anchor)
        {
            return Err(invalid_feed(
                "identity anchor changed while reading the feed",
            ));
        }
        identity = Some(page_identity.clone());
        if anchor.is_none() {
            anchor = Some(page_anchor);
        }
        if feed.entries.is_empty() {
            return Err(invalid_feed("feed page was empty before the signed head"));
        }

        for item in feed.entries {
            if item.entry.seq != expected_seq {
                return Err(invalid_feed(format!(
                    "expected entry {expected_seq}, feed served {}",
                    item.entry.seq
                )));
            }
            if item.entry.seq > seq {
                return Err(invalid_feed("feed advanced past the card snapshot"));
            }
            if !replay_head_only && item.entry.prev_entry_hash != previous_hash {
                return Err(invalid_feed(format!(
                    "entry {} does not chain to the local checkpoint",
                    item.entry.seq
                )));
            }
            verify_feed_item(&item, &page_identity)?;
            reject_retired_signer_after_checkpoint(&page_identity, pinned.as_ref(), &item)?;
            replay_bytes = replay_bytes.saturating_add(
                serde_json::to_vec(&item)
                    .map_err(|_| invalid_feed("could not size feed entry"))?
                    .len() as u64,
            );
            if replay_bytes > MAX_FEED_REPLAY_BYTES {
                return Err(invalid_feed("feed replay metadata exceeds the client cap"));
            }
            previous_hash = Some(item.hash.clone());
            after = item.entry.seq;
            expected_seq = expected_seq
                .checked_add(1)
                .ok_or_else(|| invalid_feed("feed sequence overflow"))?;
            if require_full_chain {
                all_entries.push(item.clone());
            }
            observed_entries.push(item.clone());
            head_entry = Some(item);
        }
        if after == seq {
            break;
        }
    }

    if head_entry.as_ref().map(|item| &item.hash) != advertised_hash.as_ref() {
        return Err(invalid_feed(
            "verified chain does not converge on the advertised head",
        ));
    }
    let identity = identity.ok_or_else(|| invalid_feed("feed has no brain identity"))?;
    let anchor = anchor.ok_or_else(|| invalid_feed("feed has no identity anchor"))?;
    verify_rotation_feed_boundaries(&identity, pinned.as_ref(), &observed_entries, seq)?;
    save_canonical_pin_and_alias(
        cfg,
        &trust_directory,
        brain,
        &resolved_brain,
        TrustState {
            v: 2,
            origin: normalized_origin(&cfg.hub)?,
            requested: resolved_brain.clone(),
            brain: resolved_brain.clone(),
            home: None,
            anchor: anchor.clone(),
            current: format!("ed25519:{}", identity.fingerprint),
            head_seq: seq,
            feed_hash: advertised_hash.clone(),
            rotations: identity.rotations.clone(),
        },
        alias_binding.as_ref(),
    )?;
    Ok(VerifiedRemote {
        head: Head {
            brain: resolved_brain,
            seq,
            updated_at,
            feed_hash: advertised_hash,
            verified: true,
        },
        identity: Some(identity),
        head_entry,
        entries: all_entries,
        anchor: Some(anchor),
    })
}

/// Read and locally verify the brain's current signed feed head. Identity
/// rotation is accepted only through an old-key-signed chain rooted at the
/// local TOFU anchor; sequence and hash checkpoints reject rollback and
/// equivocation across invocations.
pub fn head(cfg: &HubConfig, brain: &str) -> LinkResult<Head> {
    Ok(verified_remote_head(cfg, brain, false)?.head)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BRAIN_ID: &str = "01j5qc3v9k4ym8rwbn2tqe6f7d";

    struct SignedRemoteFixture {
        card: String,
        feed: String,
        key: AgentSigningKey,
        identity: FeedIdentity,
    }

    fn signed_remote_fixture() -> SignedRemoteFixture {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let (public_key, multikey) = public_identity_for(&pair);
        let identity = FeedIdentity {
            fingerprint: multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: public_key.clone(),
            previous: Vec::new(),
            rotations: Vec::new(),
        };
        let mut entry = FeedEntry {
            v: 1,
            seq: 1,
            ts: "2026-07-30T12:00:00.000Z".to_string(),
            brain: multikey.clone(),
            public_key: public_key.clone(),
            kind: "push".to_string(),
            op: "snapshot".to_string(),
            pack_sha256: "a".repeat(64),
            files: Vec::new(),
            removed: Vec::new(),
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
        let hash = content_sha256(&exact);
        let card = json!({
            "id": TEST_BRAIN_ID,
            "headSeq": 1,
            "feedHash": hash,
            "identity": identity.clone(),
        })
        .to_string();
        let feed = json!({
            "headSeq": 1,
            "feedHash": hash,
            "identity": identity.clone(),
            "entries": [{"hash": hash, "entry": entry}],
            "scopeLimited": false,
        })
        .to_string();
        SignedRemoteFixture {
            card,
            feed,
            key: AgentSigningKey {
                pkcs8: pkcs8.as_ref().to_vec(),
                multikey,
                public_key_spki: public_key,
            },
            identity,
        }
    }

    #[test]
    fn wire_sequences_are_rejected_at_their_endpoint_count_limits() {
        let fixture = signed_remote_fixture();
        let feed: Value = serde_json::from_str(&fixture.feed).unwrap();
        let item = feed["entries"][0].to_string();
        let oversized_page = format!(
            "{{\"headSeq\":1,\"feedHash\":null,\"identity\":null,\"entries\":[{}],\"scopeLimited\":false}}",
            std::iter::repeat_n(item.as_str(), FEED_PAGE_LIMIT + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(serde_json::from_str::<FeedResponse>(&oversized_page).is_err());

        let oversized_identity = format!(
            "{{\"fingerprint\":\"fp\",\"publicKeySpki\":\"spki\",\"previous\":[],\"rotations\":[{}]}}",
            std::iter::repeat_n("\"rotation\"", MAX_IDENTITY_ROTATIONS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(serde_json::from_str::<FeedIdentity>(&oversized_identity).is_err());

        let file = r#"{"path":"a","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bytes":1}"#;
        let oversized_entry = format!(
            "{{\"v\":1,\"seq\":1,\"ts\":\"t\",\"brain\":\"b\",\"public_key\":\"k\",\"kind\":\"push\",\"op\":\"snapshot\",\"pack_sha256\":\"{}\",\"files\":[{}],\"removed\":[],\"prev_entry_hash\":null,\"sig\":\"s\"}}",
            "a".repeat(64),
            std::iter::repeat_n(file, MAX_PUSH_FILES + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(serde_json::from_str::<FeedEntry>(&oversized_entry).is_err());
    }

    fn scripted_json_hub(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut content_length = 0usize;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line == "\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut request_body = vec![0_u8; content_length];
                reader.read_exact(&mut request_body).unwrap();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        (url, handle)
    }

    fn routed_json_hub(
        requests: usize,
        mut respond: impl FnMut(&str) -> (u16, String) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for _ in 0..requests {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut content_length = 0usize;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line == "\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut request_body = vec![0_u8; content_length];
                reader.read_exact(&mut request_body).unwrap();
                let (status, body) = respond(&path);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        (url, handle)
    }

    fn test_hub_config(hub: String, state_dir: PathBuf) -> HubConfig {
        HubConfig {
            hub,
            key: Some("test-key".to_string()),
            agent_key: None,
            brain_key: None,
            state_dir,
            store_selected: false,
        }
    }

    #[test]
    fn linkmd_sig_v2_is_bound_to_the_exact_hub_origin() {
        use ring::signature::KeyPair as _;

        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let (spki, multikey) = public_identity_for(&pair);
        let key = AgentSigningKey {
            pkcs8: pkcs8.as_ref().to_vec(),
            multikey,
            public_key_spki: spki,
        };
        let header = linkmd_sig_header(
            &key,
            "https://hub-a.example",
            "post",
            "/api/hub/brains/brain/push?mode=exact",
            Some("{\"ok\":true}"),
        )
        .unwrap();
        assert!(header.starts_with("LinkMD-Sig v2,"));
        let ts = header
            .split(",ts=")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        let signature = URL_SAFE_NO_PAD
            .decode(header.rsplit(",sig=").next().unwrap())
            .unwrap();
        let body_hash = format!("{:x}", Sha256::digest(b"{\"ok\":true}"));
        let accepted = format!(
            "v2\nhttps://hub-a.example\nPOST\n/api/hub/brains/brain/push?mode=exact\n{ts}\n{body_hash}"
        );
        let replayed = format!(
            "v2\nhttps://hub-b.example\nPOST\n/api/hub/brains/brain/push?mode=exact\n{ts}\n{body_hash}"
        );
        let public = pair.public_key().as_ref();
        assert!(UnparsedPublicKey::new(&ED25519, public)
            .verify(accepted.as_bytes(), &signature)
            .is_ok());
        assert!(
            UnparsedPublicKey::new(&ED25519, public)
                .verify(replayed.as_bytes(), &signature)
                .is_err(),
            "a proof captured at hub A must not authenticate at hub B"
        );
    }

    #[test]
    fn explicit_brain_id_rejects_a_substituted_card_before_feed_trust() {
        let other = "01j5qc3v9k4ym8rwbn2tqe6f7e";
        let card = json!({
            "id": other,
            "headSeq": 0,
            "identity": signed_remote_fixture().identity,
        })
        .to_string();
        let (hub, server) = scripted_json_hub(vec![(200, card)]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        let error = head(&cfg, TEST_BRAIN_ID).unwrap_err().to_string();
        assert!(
            error.contains("differs from the explicitly requested"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn empty_brain_identity_is_pinned_and_cannot_be_substituted_later() {
        let first = signed_remote_fixture().identity;
        let second = signed_remote_fixture().identity;
        let card = |identity: FeedIdentity| {
            json!({
                "id": TEST_BRAIN_ID,
                "headSeq": 0,
                "identity": identity,
            })
            .to_string()
        };
        let (hub, server) = scripted_json_hub(vec![(200, card(first)), (200, card(second))]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        assert!(head(&cfg, TEST_BRAIN_ID).unwrap().verified);
        let error = head(&cfg, TEST_BRAIN_ID).unwrap_err().to_string();
        assert!(
            error.contains("pinned anchor") || error.contains("forked away"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn empty_head_rejects_a_rotation_that_commits_to_hidden_history() {
        let old = signed_remote_fixture();
        let new = signed_remote_fixture();
        let old_pair = ring::signature::Ed25519KeyPair::from_pkcs8(&old.key.pkcs8).unwrap();
        let unsigned = serde_json::to_string(&UnsignedRotation {
            v: 1,
            op: "rotate",
            brain: &old.key.multikey,
            public_key: &old.key.public_key_spki,
            new_brain: &new.key.multikey,
            new_public_key: &new.key.public_key_spki,
            prior_head_seq: 1,
            prior_feed_hash: Some(&"a".repeat(64)),
            ts: "2026-07-30T12:00:00.000Z".to_string(),
        })
        .unwrap();
        let signature = URL_SAFE_NO_PAD.encode(old_pair.sign(unsigned.as_bytes()).as_ref());
        let rotation = format!(
            "{},\"sig\":\"{}\"}}",
            &unsigned[..unsigned.len() - 1],
            signature
        );
        let identity = FeedIdentity {
            fingerprint: new.key.multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: new.key.public_key_spki,
            previous: vec![PreviousIdentity {
                fingerprint: old.key.multikey.trim_start_matches("ed25519:").to_string(),
                public_key_spki: old.key.public_key_spki,
            }],
            rotations: vec![rotation],
        };
        let card = json!({
            "id": TEST_BRAIN_ID,
            "headSeq": 0,
            "feedHash": null,
            "identity": identity,
        })
        .to_string();
        let (hub, server) = scripted_json_hub(vec![(200, card)]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        let error = head(&cfg, TEST_BRAIN_ID).unwrap_err().to_string();
        assert!(
            error.contains("rotation claims a feed boundary beyond the advertised head"),
            "{error}"
        );
        assert!(
            load_trust(&cfg, TEST_BRAIN_ID).unwrap().is_none(),
            "an inconsistent empty-head identity must not become the TOFU checkpoint"
        );
        server.join().unwrap();
    }

    #[test]
    fn trust_checkpoint_rejects_a_later_fork() {
        let fixture = signed_remote_fixture();
        let mut fork: Value = serde_json::from_str(&fixture.card).unwrap();
        fork["feedHash"] = Value::String("b".repeat(64));
        let (hub, server) = scripted_json_hub(vec![
            (200, fixture.card),
            (200, fixture.feed),
            (200, fork.to_string()),
        ]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        assert!(head(&cfg, TEST_BRAIN_ID).unwrap().verified);
        assert!(head(&cfg, TEST_BRAIN_ID).is_err());
        server.join().unwrap();
    }

    #[test]
    fn alias_and_canonical_id_share_one_identity_checkpoint() {
        let trusted = signed_remote_fixture();
        let attacker = signed_remote_fixture();
        let (hub, server) = scripted_json_hub(vec![
            (200, trusted.card),
            (200, trusted.feed),
            (200, attacker.card),
        ]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        assert!(head(&cfg, "trusted-slug").unwrap().verified);
        let error = head(&cfg, TEST_BRAIN_ID).unwrap_err().to_string();
        assert!(
            error.contains("equivocation")
                || error.contains("pinned")
                || error.contains("identity"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn concurrent_aliases_cannot_establish_conflicting_tofu_pins() {
        let alpha = signed_remote_fixture();
        let beta = signed_remote_fixture();
        let alpha_card = alpha.card.clone();
        let alpha_feed = alpha.feed.clone();
        let beta_card = beta.card.clone();
        let beta_feed = beta.feed.clone();
        let (hub, server) = routed_json_hub(3, move |path| {
            if path.contains("/alpha/feed?") {
                (200, alpha_feed.clone())
            } else if path.contains("/beta/feed?") {
                (200, beta_feed.clone())
            } else if path.ends_with("/alpha") {
                (200, alpha_card.clone())
            } else if path.ends_with("/beta") {
                (200, beta_card.clone())
            } else {
                (500, r#"{"error":"unexpected path"}"#.to_string())
            }
        });
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        let alpha_cfg = cfg.clone();
        let beta_cfg = cfg;
        let first = std::thread::spawn(move || head(&alpha_cfg, "alpha"));
        let second = std::thread::spawn(move || head(&beta_cfg, "beta"));
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "only one alias identity may establish canonical TOFU: {results:?}"
        );
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trust_transaction_survives_an_ancestor_swap_without_writing_outside() {
        use std::os::unix::fs::symlink;

        let fixture = signed_remote_fixture();
        let card = json!({
            "id": TEST_BRAIN_ID,
            "headSeq": 0,
            "feedHash": Value::Null,
            "identity": fixture.identity,
        })
        .to_string();
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = work.path().join("state");
        let moved = work.path().join("state-held");
        let swap_state = state.clone();
        let swap_moved = moved.clone();
        let outside_path = outside.path().to_path_buf();
        let (hub, server) = routed_json_hub(1, move |_| {
            // The client has already opened state/trust before this response.
            std::fs::rename(&swap_state, &swap_moved).unwrap();
            symlink(&outside_path, &swap_state).unwrap();
            (200, card.clone())
        });
        let cfg = test_hub_config(hub, state);

        let verified = verified_remote_head(&cfg, TEST_BRAIN_ID, false).unwrap();
        assert_eq!(verified.head.seq, 0);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(std::fs::read_dir(moved.join("trust"))
            .unwrap()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "json")));
        server.join().unwrap();
    }

    #[test]
    fn self_custody_push_refuses_an_unrelated_key_before_requesting_an_upload() {
        let remote = signed_remote_fixture();
        let unrelated = signed_remote_fixture().key;
        let (hub, server) = scripted_json_hub(vec![(200, remote.card), (200, remote.feed)]);
        let state = tempfile::tempdir().unwrap();
        let mut cfg = test_hub_config(hub, state.path().to_path_buf());
        cfg.brain_key = Some(unrelated);
        let error = sync_push(
            &cfg,
            TEST_BRAIN_ID,
            &[("DB.md".to_string(), "signed local content".to_string())],
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("not the verified current brain identity"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn rotation_ignores_a_forged_2xx_body_and_requires_verified_postcondition() {
        let remote = signed_remote_fixture();
        let new = signed_remote_fixture().key;
        let state = tempfile::tempdir().unwrap();
        let new_file = state.path().join("new.key");
        std::fs::write(
            &new_file,
            format!("{}\n", URL_SAFE_NO_PAD.encode(&new.pkcs8)),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&new_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let forged = json!({
            "brain": TEST_BRAIN_ID,
            "identity": {
                "fingerprint": new.multikey.trim_start_matches("ed25519:"),
                "publicKeySpki": new.public_key_spki,
            }
        })
        .to_string();
        let (hub, server) = scripted_json_hub(vec![
            (200, remote.card.clone()),
            (200, remote.feed.clone()),
            (200, forged),
            (200, remote.card),
            (200, remote.feed),
        ]);
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        let error = rotate_brain_key(&cfg, TEST_BRAIN_ID, &remote.key, &new_file)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("without committing the verified new identity"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn resolve_record_is_derived_from_signed_pack_not_a_query_response() {
        let record_id = "01j5qc3v9k4ym8rwbn2tqe6f7e";
        let raw = format!(
            "---\ntype: client\nid: {record_id}\nsummary: Signed truth\n---\n# Signed truth\n"
        );
        let pack = build_store_pack(&[
            (
                "DB.md".to_string(),
                "---\ntype: db-md\nscope: company\nowner: Test\n---\n".to_string(),
            ),
            ("records/clients/truth.md".to_string(), raw.clone()),
        ])
        .unwrap();
        let by_id = resolve_from_verified_pack(
            "01j5qc3v9k4ym8rwbn2tqe6f7d",
            &AddressTarget::Id(record_id.to_string()),
            pack.clone(),
        )
        .unwrap();
        assert_eq!(by_id["document"]["summary"], "Signed truth");
        assert_eq!(by_id["document"]["body"], "# Signed truth\n");
        assert_eq!(
            by_id["document"]["contentSha"],
            content_sha256(raw.as_bytes())
        );

        let by_path = resolve_from_verified_pack(
            "01j5qc3v9k4ym8rwbn2tqe6f7d",
            &AddressTarget::Path("records/clients/truth.md".to_string()),
            pack,
        )
        .unwrap();
        assert_eq!(by_path["document"]["id"], record_id);
        assert_eq!(by_path["document"]["path"], "records/clients/truth.md");
    }

    #[test]
    fn canonical_store_pack_matches_the_cross_language_zip32_golden() {
        let unsorted = vec![
            ("records/a.md".to_string(), "alpha\n".to_string()),
            ("DB.md".to_string(), "# db\n".to_string()),
        ];
        let sorted = vec![
            ("DB.md".to_string(), "# db\n".to_string()),
            ("records/a.md".to_string(), "alpha\n".to_string()),
        ];
        let pack = build_store_pack(&unsorted).unwrap();

        // This digest is shared with the hub implementation. It locks every
        // byte of the wire profile: raw UTF-8 order, STORED payloads, fixed DOS
        // epoch, explicit CRC/sizes, Unix regular-0600 attributes, and the
        // absence of descriptors/extras/comments/ZIP64.
        assert_eq!(pack.len(), 219);
        assert_eq!(
            content_sha256(&pack),
            "972fb2045becaa21588baaf4b349e62a430687fa2c21167b53f4ca0efa6c9408"
        );
        assert_eq!(pack, build_store_pack(&sorted).unwrap());
        assert!(!pack.windows(4).any(|bytes| bytes == b"PK\x07\x08"));
        assert!(!pack.windows(4).any(|bytes| bytes == b"PK\x06\x06"));
        assert!(!pack.windows(4).any(|bytes| bytes == b"PK\x06\x07"));

        assert_eq!(
            parse_store_pack(pack).unwrap(),
            vec![
                ("DB.md".to_string(), b"# db\n".to_vec()),
                ("records/a.md".to_string(), b"alpha\n".to_vec()),
            ]
        );
    }

    #[test]
    fn canonical_store_pack_validates_every_path_before_writing() {
        let duplicate = vec![
            ("DB.md".to_string(), "first".to_string()),
            ("DB.md".to_string(), "second".to_string()),
        ];
        assert!(build_store_pack(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate path"));
        assert!(matches!(
            build_store_pack(&[("../escape.md".to_string(), "no".to_string())]),
            Err(LinkError::UnsafePath { .. })
        ));
    }

    #[test]
    fn zip64_preflight_rejects_an_entry_count_bomb_before_zip_parsing() {
        const COUNT: u64 = MAX_PUSH_FILES as u64 + 1;
        // One byte stands in for the central directory. The trailer itself is
        // structurally valid; the count is the sole reason for refusal.
        let mut bytes = vec![0_u8];
        let zip64_offset = bytes.len() as u64;
        bytes.extend_from_slice(b"PK\x06\x06");
        bytes.extend_from_slice(&44_u64.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 12]);
        bytes.extend_from_slice(&COUNT.to_le_bytes());
        bytes.extend_from_slice(&COUNT.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(b"PK\x06\x07");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&zip64_offset.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());

        let error = preflight_zip_entry_count(&bytes, MAX_PUSH_FILES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid file count"), "{error}");
    }

    #[test]
    fn zip_preflight_rejects_a_fake_trailing_eocd_before_zip_parsing() {
        const COUNT: u64 = MAX_PUSH_FILES as u64 + 1;
        let mut bytes = vec![0_u8];
        let zip64_offset = bytes.len() as u64;
        bytes.extend_from_slice(b"PK\x06\x06");
        bytes.extend_from_slice(&44_u64.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 12]);
        bytes.extend_from_slice(&COUNT.to_le_bytes());
        bytes.extend_from_slice(&COUNT.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(b"PK\x06\x07");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&zip64_offset.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        // The old parser trusted this last low-count signature, while
        // ZipArchive fell back to the real Zip64 directory and allocated for
        // COUNT entries. Its central-directory offsets are deliberately fake.
        let fake_eocd = bytes.len() as u32;
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&fake_eocd.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());

        let error = preflight_zip_entry_count(&bytes, MAX_PUSH_FILES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("central directory"), "{error}");
    }

    #[test]
    fn strict_http_status_handling_rejects_redirects_without_panicking() {
        let error = ensure_ok(
            HubResponse {
                status: 302,
                body: Some(json!({"redirect": "/elsewhere"})),
            },
            "mutation",
        )
        .unwrap_err();
        assert!(matches!(error, LinkError::Http { status: 302, .. }));

        let error = ensure_raw_ok(
            RawHubResponse {
                status: 302,
                body: br#"{"redirect":"/elsewhere"}"#.to_vec(),
            },
            "feed",
        )
        .unwrap_err();
        assert!(matches!(error, LinkError::Http { status: 302, .. }));
    }

    #[cfg(unix)]
    #[test]
    fn collect_push_files_refuses_external_symlink_and_nested_store() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: Test\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("records/notes")).unwrap();

        let external = tempfile::tempdir().unwrap();
        let secret = external.path().join("secret.md");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        symlink(&secret, root.path().join("records/notes/secret.md")).unwrap();

        let store = Store::open_strict(root.path()).unwrap();
        let err = collect_push_files(&store).unwrap_err().to_string();
        assert!(err.contains("cannot push"), "{err}");
        assert!(
            !err.contains("TOP SECRET"),
            "external bytes must never leak"
        );

        std::fs::remove_file(root.path().join("records/notes/secret.md")).unwrap();
        let nested = root.path().join("records/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("DB.md"),
            "---\ntype: db-md\nscope: research\nowner: Nested\n---\n",
        )
        .unwrap();
        let err = collect_push_files(&store).unwrap_err().to_string();
        assert!(err.contains("nested db.md store"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn remote_push_uses_opened_root_after_path_replacement() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("store");
        std::fs::create_dir_all(root.join("records/notes")).unwrap();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        std::fs::write(
            root.join("records/notes/owned.md"),
            "---\ntype: note\nsummary: owned\n---\nowned upload\n",
        )
        .unwrap();
        let store = Store::open_strict(&root).unwrap();
        let detached = sandbox.path().join("detached");
        std::fs::rename(&root, &detached).unwrap();

        let replacement = sandbox.path().join("replacement");
        std::fs::create_dir_all(replacement.join("records/notes")).unwrap();
        std::fs::write(replacement.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        std::fs::write(
            replacement.join("records/notes/secret.md"),
            "---\ntype: note\nsummary: secret\n---\nreplacement sentinel\n",
        )
        .unwrap();
        symlink(&replacement, &root).unwrap();

        let files = collect_push_files(&store).unwrap();
        let wire_text = files
            .iter()
            .map(|(path, content)| format!("{path}\n{content}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wire_text.contains("owned upload"));
        assert!(!wire_text.contains("replacement sentinel"));
        assert!(!wire_text.contains("records/notes/secret.md"));

        let remote = signed_remote_fixture();
        let (hub, server) = scripted_json_hub(vec![
            (200, remote.card),
            (200, remote.feed),
            (200, json!({"ok": true}).to_string()),
        ]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        let pushed = sync_push(&cfg, TEST_BRAIN_ID, &files).unwrap();
        assert_eq!(pushed, json!({"ok": true}));
        server.join().unwrap();
    }

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
            rotations: Vec::new(),
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
            rotations: Vec::new(),
        };
        assert!(verify_feed_item(&item, &identity).is_ok());
    }

    #[test]
    fn identity_rotation_requires_old_key_signature_and_preserves_the_pin() {
        let rng = ring::rand::SystemRandom::new();
        let old_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let old = ring::signature::Ed25519KeyPair::from_pkcs8(old_pkcs8.as_ref()).unwrap();
        let new_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let new = ring::signature::Ed25519KeyPair::from_pkcs8(new_pkcs8.as_ref()).unwrap();
        let (old_spki, old_multikey) = public_identity_for(&old);
        let (new_spki, new_multikey) = public_identity_for(&new);
        let unsigned = serde_json::to_string(&UnsignedRotation {
            v: 1,
            op: "rotate",
            brain: &old_multikey,
            public_key: &old_spki,
            new_brain: &new_multikey,
            new_public_key: &new_spki,
            prior_head_seq: 1,
            prior_feed_hash: Some(&"a".repeat(64)),
            ts: "2026-07-30T12:00:00.000Z".to_string(),
        })
        .unwrap();
        let signature = URL_SAFE_NO_PAD.encode(old.sign(unsigned.as_bytes()).as_ref());
        let rotation = format!(
            "{},\"sig\":\"{}\"}}",
            &unsigned[..unsigned.len() - 1],
            signature
        );
        let identity = FeedIdentity {
            fingerprint: new_multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: new_spki,
            previous: vec![PreviousIdentity {
                fingerprint: old_multikey.trim_start_matches("ed25519:").to_string(),
                public_key_spki: old_spki,
            }],
            rotations: vec![rotation],
        };
        let pin = TrustState {
            v: 2,
            origin: "https://hub.example".to_string(),
            requested: "brain".to_string(),
            brain: "brain".to_string(),
            home: None,
            anchor: old_multikey.clone(),
            current: old_multikey.clone(),
            head_seq: 1,
            feed_hash: Some("a".repeat(64)),
            rotations: Vec::new(),
        };
        assert_eq!(
            verify_identity_chain(&identity, Some(&pin)).unwrap(),
            old_multikey
        );
        let mut accepted = pin.clone();
        accepted.current = new_multikey.clone();
        accepted.rotations = identity.rotations.clone();
        let alternate_unsigned = serde_json::to_string(&UnsignedRotation {
            v: 1,
            op: "rotate",
            brain: &old_multikey,
            public_key: &identity.previous[0].public_key_spki,
            new_brain: &new_multikey,
            new_public_key: &identity.public_key_spki,
            prior_head_seq: 1,
            prior_feed_hash: Some(&"a".repeat(64)),
            ts: "2026-07-30T12:00:01.000Z".to_string(),
        })
        .unwrap();
        let alternate_signature =
            URL_SAFE_NO_PAD.encode(old.sign(alternate_unsigned.as_bytes()).as_ref());
        let mut rewritten = identity.clone();
        rewritten.rotations[0] = format!(
            "{},\"sig\":\"{}\"}}",
            &alternate_unsigned[..alternate_unsigned.len() - 1],
            alternate_signature
        );
        assert!(
            verify_identity_chain(&rewritten, Some(&accepted)).is_err(),
            "an alternate valid statement must not rewrite accepted history"
        );

        let mut stale_entry = FeedEntry {
            v: 1,
            seq: 2,
            ts: "2026-07-30T12:01:00.000Z".to_string(),
            brain: pin.current.clone(),
            public_key: identity.previous[0].public_key_spki.clone(),
            kind: "push".to_string(),
            op: "snapshot".to_string(),
            pack_sha256: "b".repeat(64),
            files: Vec::new(),
            removed: Vec::new(),
            prev_entry_hash: pin.feed_hash.clone(),
            sig: String::new(),
        };
        let stale_unsigned = UnsignedFeedEntry {
            v: stale_entry.v,
            seq: stale_entry.seq,
            ts: &stale_entry.ts,
            brain: &stale_entry.brain,
            public_key: &stale_entry.public_key,
            kind: &stale_entry.kind,
            op: &stale_entry.op,
            pack_sha256: &stale_entry.pack_sha256,
            files: &stale_entry.files,
            removed: &stale_entry.removed,
            prev_entry_hash: &stale_entry.prev_entry_hash,
        };
        stale_entry.sig = URL_SAFE_NO_PAD.encode(
            old.sign(&serde_json::to_vec(&stale_unsigned).unwrap())
                .as_ref(),
        );
        let mut stale_exact = serde_json::to_vec(&stale_entry).unwrap();
        stale_exact.push(b'\n');
        let stale_item = FeedItem {
            hash: content_sha256(&stale_exact),
            entry: stale_entry,
        };
        assert!(
            reject_retired_signer_after_checkpoint(&identity, Some(&accepted), &stale_item)
                .is_err(),
            "a key retired before the checkpoint must never append after it"
        );
        assert!(
            verify_feed_item(&stale_item, &identity).is_err(),
            "an old key must never append after its signed rotation boundary"
        );

        let mut missing = identity.clone();
        missing.rotations.clear();
        assert!(verify_identity_chain(&missing, Some(&pin)).is_err());

        let mut tampered = identity;
        tampered.rotations[0] = tampered.rotations[0].replace("\"sig\":\"", "\"sig\":\"A");
        assert!(verify_identity_chain(&tampered, Some(&pin)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_creation_refuses_a_planted_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("valuable.txt");
        let planted = dir.path().join("agent.key");
        std::fs::write(&target, "do not overwrite").unwrap();
        symlink(&target, &planted).unwrap();

        assert!(matches!(
            generate_agent_key(&planted),
            Err(LinkError::BadAgentKey { .. })
        ));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "do not overwrite");
    }

    #[cfg(unix)]
    #[test]
    fn key_creation_refuses_a_symlinked_parent_without_writing_through_it() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("redirect")).unwrap();

        assert!(generate_agent_key(&root.path().join("redirect/agent.key")).is_err());
        assert!(!outside.path().join("agent.key").exists());
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

    #[cfg(unix)]
    #[test]
    fn opened_destination_capability_survives_an_ancestor_path_swap() {
        use std::os::unix::fs::symlink;

        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let original = work.path().join("destination");
        let moved = work.path().join("destination-moved");
        let directory = open_or_create_dir_nofollow(&original).unwrap();

        std::fs::rename(&original, &moved).unwrap();
        symlink(outside.path(), &original).unwrap();
        write_pull_entries_beneath_dir(
            &directory,
            &[("records/note.md".to_string(), b"held inode".to_vec())],
        )
        .unwrap();

        assert_eq!(
            std::fs::read(moved.join("records/note.md")).unwrap(),
            b"held inode"
        );
        assert!(!outside.path().join("records/note.md").exists());
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
        assert!(matches!(
            assert_safe_hub("https://hub.example.com/base"),
            Err(LinkError::UnsafeHub { .. })
        ));
    }

    #[test]
    fn registry_ssrf_classifier_rejects_local_private_and_documentation_ranges() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "192.88.99.1",
            "198.18.0.1",
            "203.0.113.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "2001:db8::1",
            "2001:1::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(
                !is_public_registry_ip(blocked.parse().unwrap()),
                "must block {blocked}"
            );
        }
        assert!(is_public_registry_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_registry_ip(
            "2606:4700:4700::1111".parse().unwrap()
        ));
        assert!(is_public_registry_ip("3fff:1000::1".parse().unwrap()));
    }

    #[test]
    fn registry_dns_answer_is_pinned_and_cannot_rebind_or_change_authority() {
        use ureq::Resolver as _;

        let pinned: std::net::SocketAddr = "1.1.1.1:443".parse().unwrap();
        let resolver = PinnedRegistryResolver {
            netloc: "home.example:443".to_string(),
            addresses: vec![pinned],
        };
        assert_eq!(resolver.resolve("home.example:443").unwrap(), vec![pinned]);
        assert!(resolver.resolve("127.0.0.1:443").is_err());
        assert_eq!(
            resolver.resolve("home.example:443").unwrap(),
            vec![pinned],
            "subsequent connects reuse the validated answer instead of DNS"
        );
    }

    #[test]
    fn production_object_urls_and_store_selected_hubs_cannot_reach_private_ips() {
        let cfg = HubConfig {
            hub: "https://hub.example".to_string(),
            key: None,
            agent_key: None,
            brain_key: None,
            state_dir: tempfile::tempdir().unwrap().keep(),
            store_selected: false,
        };
        assert!(
            presigned_agent(&cfg, "https://127.0.0.1/private").is_err(),
            "a production hub must not turn its presigned URL into an SSRF primitive"
        );

        let store_selected = HubConfig {
            hub: "https://127.0.0.1".to_string(),
            store_selected: true,
            ..cfg
        };
        assert!(
            hub_agent(&store_selected).is_err(),
            "bytes in a cloned store must not select a private-network hub"
        );
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
            state_dir: PathBuf::from("."),
            store_selected: false,
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
            state_dir: tempfile::tempdir().unwrap().keep(),
            store_selected: false,
        };

        let response = request(&cfg, "GET", "/retry", None, Auth::None).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, Some(json!({ "ok": true })));
        server.join().unwrap();
    }

    #[test]
    fn endpoint_cap_refuses_a_body_before_json_parsing() {
        let (hub, server) = scripted_json_hub(vec![(200, "x".repeat(2_048))]);
        let cfg = HubConfig {
            hub,
            key: None,
            agent_key: None,
            brain_key: None,
            state_dir: tempfile::tempdir().unwrap().keep(),
            store_selected: false,
        };

        assert!(matches!(
            request_capped(&cfg, "GET", "/bounded", None, Auth::None, 1_024),
            Err(LinkError::ResponseTooLarge { .. })
        ));
        server.join().unwrap();
    }

    #[test]
    fn overall_deadline_stops_a_dribbled_response_body() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/dribble", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\n\r\n")
                .unwrap();
            for byte in [b'x'; 32] {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let http = agent_builder_with_timeout(Duration::from_millis(150)).build();
        let started = Instant::now();
        let response = http.get(&url).call().unwrap();
        let mut body = Vec::new();
        let error = response
            .into_reader()
            .read_to_end(&mut body)
            .expect_err("per-read progress must not reset the overall deadline");
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "dribbled body exceeded the wall-clock budget: {error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn overall_deadline_stops_a_stalled_upload() {
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/upload", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            // Never consume the request body. Once the kernel send buffer fills,
            // the client must leave on its absolute write deadline.
            std::thread::sleep(Duration::from_millis(600));
        });
        let http = agent_builder_with_timeout(Duration::from_millis(150)).build();
        let body = vec![0x5a; 32 * 1024 * 1024];
        let started = Instant::now();
        let error = http
            .put(&url)
            .send_bytes(&body)
            .expect_err("stalled request-body writes must time out");
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "stalled upload exceeded the wall-clock budget: {error}"
        );
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
