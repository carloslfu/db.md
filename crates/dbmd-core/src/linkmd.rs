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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
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
/// Cold checkouts may need every visible blob, but short-lived direct object
/// capabilities must not create an unbounded connection storm. Keep the
/// verified downloads inside this fixed worker count.
const V2_BLOB_DOWNLOAD_WORKERS: usize = 16;
/// One authenticated link.md v2 bulk frame remains small enough for bounded
/// retries and serverless streaming while amortizing tens of thousands of
/// immutable blobs over a few hundred requests instead of one request each.
const V2_BULK_STREAM_FILES: usize = 256;
const V2_BULK_STREAM_CONTENT_BYTES: u64 = 8 * 1024 * 1024;
const V2_BULK_STREAM_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;
const V2_BULK_STREAM_MAGIC: &[u8; 8] = b"LMD2STRM";

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
        /// Permission-filtered structured refusal details, when supplied.
        details: Option<Value>,
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

    /// Local and remote both changed one or more coordinates since the last
    /// verified sync baseline. No side was overwritten.
    #[error("sync conflict on {paths:?} — resolve the named files and retry")]
    Conflict {
        /// Bounded, portable paths safe to show to the agent/operator.
        paths: Vec<String>,
    },

    /// A readable v2 conflict was preserved as a private, exact-head bundle.
    /// The bundle is local control state; none of its bytes ride with the
    /// brain, and resolving it always creates a fresh explicit mutation.
    #[error(
        "sync conflict preserved in private bundle `{bundle}` for {paths:?} — run `dbmd sync resolve {bundle} --keep-local`, `--take-remote`, or `--from <safe-file>`"
    )]
    ConflictBundle {
        /// Local ULID naming `.dbmd/conflicts/<bundle>/plan.json`.
        bundle: String,
        /// Bounded, readable conflict paths.
        paths: Vec<String>,
    },

    /// `.sevralocal` newly made paths eligible to ride. Uploading them is an
    /// explicit adoption boundary, never an accidental side effect of editing
    /// or removing a local policy file.
    #[error(
        "local sync policy newly exposes {paths:?} — review and retry with --resume-local-policy"
    )]
    LocalPolicyTransition {
        /// Bounded local-only paths. They are never sent in telemetry.
        paths: Vec<String>,
    },

    /// The exact mutation crosses a permissioned bulk-impact boundary. The
    /// hub has not committed it; `preview` is the bounded, permission-filtered
    /// receipt an agent must inspect before explicitly confirming the same
    /// request.
    #[error(
        "bulk change requires explicit confirmation — review the preview and retry the same sync with --confirm-bulk <id>:<digest>"
    )]
    BulkPreviewRequired {
        /// Stable structured preview returned by the hub.
        preview: Value,
    },

    /// A scoped checkout's generated store marker was edited or removed.
    /// It is local projection metadata and is never accepted as brain data.
    #[error(
        "the generated DB.md for this scoped view was modified — clone a fresh scoped checkout"
    )]
    ScopedProjectionModified,

    /// The effective permission slice changed after this checkout was pinned.
    /// Reusing the directory could conceal removals or accidentally adopt files
    /// revealed by a wider grant, so a new checkout is required.
    #[error(
        "the checkout's permission scope changed — clone into a new directory to accept the new view"
    )]
    ScopedViewChanged,

    /// A v2 identity/head was previously accepted for this ref, but the hub
    /// now hides it. Never reinterpret that as a v1 downgrade.
    #[error("the previously verified brain is unavailable — access may have been revoked or the brain removed")]
    BrainUnavailable,

    /// The verified remote head advanced while a pull or post-commit barrier
    /// was in flight. The old baseline is deliberately retained.
    #[error(
        "the remote brain advanced during sync — retry to converge from the new verified head"
    )]
    RemoteAdvancedDuringSync,

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

/// Exact approval token returned by a permissioned v2 bulk preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2BulkConfirmation {
    /// Opaque preview receipt id.
    pub id: String,
    /// Digest binding the receipt to principal, head, control revision, and
    /// the exact strict mutation.
    pub digest: String,
}

impl V2BulkConfirmation {
    /// Parse the CLI/wire handoff form `<id>:<digest>` and reject anything
    /// outside the protocol's exact lowercase shapes before network I/O.
    pub fn parse(value: &str) -> LinkResult<Self> {
        let (id, digest) = value
            .split_once(':')
            .ok_or_else(|| LinkError::InvalidPack {
                message: "bulk confirmation must be <id>:<digest>".to_string(),
            })?;
        if !crate::ulid::is_ulid(id) || !is_sha256(digest) {
            return Err(LinkError::InvalidPack {
                message: "bulk confirmation must contain a lowercase ULID and SHA-256 digest"
                    .to_string(),
            });
        }
        Ok(Self {
            id: id.to_string(),
            digest: digest.to_string(),
        })
    }
}

/// Security-sensitive link.md state and destination writes require a hardened
/// descriptor/handle-relative filesystem implementation. Unix uses
/// `openat`/`O_NOFOLLOW`; Windows uses held directory handles, reparse-point
/// refusal, exclusive share modes, and write-through replacement.
fn require_hardened_filesystem(operation: &'static str) -> LinkResult<()> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        let _ = operation;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
    #[cfg(windows)]
    {
        Err(LinkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot locate user state; set {STATE_DIR_ENV} or LOCALAPPDATA"),
        )))
    }
    #[cfg(not(windows))]
    if let Some(base) = env_nonempty("XDG_STATE_HOME") {
        let base = PathBuf::from(base);
        if base.is_absolute() {
            return Ok(base.join("dbmd"));
        }
    }
    #[cfg(not(windows))]
    let home = PathBuf::from(env_nonempty("HOME").ok_or_else(|| {
        LinkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot locate user state; set {STATE_DIR_ENV}"),
        ))
    })?);
    #[cfg(not(windows))]
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
    let http = hub_agent(cfg)?;
    request_raw_with_agent(cfg, &http, method, path, body, auth, max_response_bytes)
}

fn request_raw_with_agent(
    cfg: &HubConfig,
    http: &ureq::Agent,
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
            details: None,
        }),
        Err(error) => match *error {
            // Immutable uploads use If-None-Match. A concurrent writer may
            // win the same content address; the commit/confirm endpoint reads
            // and hashes that object before accepting it, so 412 safely means
            // "continue to verification", never "trust the upload".
            ureq::Error::Status(412, _) => Ok(()),
            ureq::Error::Status(_, resp) => Err(LinkError::Http {
                what: "pack upload",
                status: resp.status(),
                message: "object store rejected the upload".to_string(),
                code: None,
                details: None,
            }),
            ureq::Error::Transport(err) => Err(LinkError::Transport {
                hub: "the object store".to_string(),
                message: err.to_string(),
            }),
        },
    }
}

fn one_past_bounded_limit(max_bytes: u64) -> Option<u64> {
    max_bytes.checked_add(1)
}

fn presigned_download_read_limit() -> u64 {
    one_past_bounded_limit(MAX_PACK_BYTES)
        .expect("the fixed presigned-download ceiling must leave room for the refusal byte")
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
                    details: None,
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
            details: None,
        });
    }
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(presigned_download_read_limit())
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
        let details = r.body.as_ref().and_then(|b| b.get("details")).cloned();
        return Err(LinkError::Http {
            what,
            status: r.status,
            message,
            code,
            details,
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
                    details: None,
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
            details: None,
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
            hub_signer: None,
            protocol_profile: None,
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
                details: None,
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
        details: None,
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
    /// Exact convergence result from the post-install barrier.
    #[serde(rename = "syncStatus")]
    pub sync_status: String,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2PointerBody {
    v: u8,
    brain: String,
    seq: u64,
    commit_hash: String,
    feed_hash: String,
    content_root: Option<String>,
    asset_root: Option<String>,
    materializer: String,
    signer_epoch: u64,
    control_revision: String,
    backup_preparation: String,
    prior_pointer_hash: Option<String>,
    signed_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct V2SignedPointer {
    pointer: V2PointerBody,
    hub_public_key: String,
    hub_fingerprint: String,
    sig: String,
}

#[derive(Debug, Clone, Deserialize)]
struct V2HeadIdentity {
    #[serde(default)]
    custody: String,
    fingerprint: String,
    public_key_spki: String,
    #[serde(default)]
    previous: Vec<V2PreviousIdentity>,
    #[serde(default)]
    rotations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct V2PreviousIdentity {
    fingerprint: String,
    public_key_spki: String,
}

#[derive(Debug, Deserialize)]
struct V2HeadResponse {
    v: u8,
    brain_id: String,
    profile: String,
    view: Option<V2HeadView>,
    pointer: Option<V2SignedPointer>,
    identity: Option<V2HeadIdentity>,
}

#[derive(Debug, Clone, Deserialize)]
struct V2HeadView {
    kind: String,
    control_revision: String,
}

#[derive(Debug, Clone)]
struct V2VerifiedHead {
    requested: String,
    brain_id: String,
    view_kind: String,
    view_revision: String,
    identity: V2HeadIdentity,
    pointer: Option<V2PointerBody>,
    trust: TrustState,
    alias: Option<AliasBinding>,
}

fn verify_v2_spki_signature(
    public_key: &str,
    message: &[u8],
    signature: &str,
) -> LinkResult<Vec<u8>> {
    let der = URL_SAFE_NO_PAD
        .decode(public_key)
        .map_err(|_| invalid_feed("v2 signer public key is not base64url"))?;
    if der.len() != ED25519_SPKI_PREFIX.len() + 32 || !der.starts_with(&ED25519_SPKI_PREFIX) {
        return Err(invalid_feed("v2 signer public key is not Ed25519 SPKI"));
    }
    let sig = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| invalid_feed("v2 signature is not base64url"))?;
    UnparsedPublicKey::new(&ED25519, &der[ED25519_SPKI_PREFIX.len()..])
        .verify(message, &sig)
        .map_err(|_| invalid_feed("v2 Ed25519 signature failed"))?;
    Ok(der)
}

fn verify_v2_pointer(pointer: &V2SignedPointer, expected_brain: &str) -> LinkResult<String> {
    if pointer.pointer.v != 2
        || pointer.pointer.brain != expected_brain
        || pointer.pointer.seq == 0
        || !is_sha256(&pointer.pointer.commit_hash)
        || !is_sha256(&pointer.pointer.feed_hash)
        || pointer
            .pointer
            .content_root
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || !is_sha256(&pointer.pointer.backup_preparation)
    {
        return Err(invalid_feed("v2 pointer fields are invalid"));
    }
    let value = serde_json::to_value(&pointer.pointer)
        .map_err(|_| invalid_feed("v2 pointer could not be canonicalized"))?;
    let message = crate::linkmd_v2::canonical_bytes(&value)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let der = verify_v2_spki_signature(&pointer.hub_public_key, &message, &pointer.sig)?;
    let fingerprint = format!("{:x}", Sha256::digest(&der));
    if fingerprint != pointer.hub_fingerprint {
        return Err(invalid_feed("v2 hub signer fingerprint mismatch"));
    }
    Ok(format!(
        "{}:{}",
        pointer.hub_fingerprint, pointer.hub_public_key
    ))
}

fn v2_identity(identity: &V2HeadIdentity) -> FeedIdentity {
    FeedIdentity {
        fingerprint: identity.fingerprint.clone(),
        public_key_spki: identity.public_key_spki.clone(),
        previous: identity
            .previous
            .iter()
            .map(|previous| PreviousIdentity {
                fingerprint: previous.fingerprint.clone(),
                public_key_spki: previous.public_key_spki.clone(),
            })
            .collect(),
        rotations: identity.rotations.clone(),
    }
}

fn verified_v2_commit_object(
    raw: &[u8],
    identity: &V2HeadIdentity,
) -> LinkResult<serde_json::Map<String, Value>> {
    let mut value: Value =
        serde_json::from_slice(raw).map_err(|_| invalid_feed("v2 commit is not JSON"))?;
    let canonical = crate::linkmd_v2::canonical_bytes(&value)
        .map_err(|error| invalid_feed(error.to_string()))?;
    if canonical != raw {
        return Err(invalid_feed("v2 commit is not canonical JSON"));
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_feed("v2 commit is not an object"))?;
    let sig = object
        .remove("sig")
        .and_then(|value| value.as_str().map(str::to_string))
        .ok_or_else(|| invalid_feed("v2 commit has no signature"))?;
    const FIELDS: [&str; 18] = [
        "actor_ref",
        "asset_root",
        "brain",
        "changes_sha256",
        "control_revision",
        "materializer",
        "op",
        "parent_asset_root",
        "parent_commit",
        "parent_root",
        "prev_entry_hash",
        "public_key",
        "seq",
        "signer_epoch",
        "state_root",
        "ts",
        "v",
        "v1_bridge",
    ];
    if object.len() != FIELDS.len() || FIELDS.iter().any(|field| !object.contains_key(*field)) {
        return Err(invalid_feed("v2 commit has a non-normative field set"));
    }
    let seq = object
        .get("seq")
        .and_then(Value::as_u64)
        .filter(|seq| *seq > 0)
        .ok_or_else(|| invalid_feed("v2 commit has an invalid sequence"))?;
    let signer_epoch = object
        .get("signer_epoch")
        .and_then(Value::as_u64)
        .filter(|epoch| *epoch > 0)
        .ok_or_else(|| invalid_feed("v2 commit has an invalid signer epoch"))?;
    let hash_or_null = |field: &str| {
        object
            .get(field)
            .is_some_and(|value| value.is_null() || value.as_str().is_some_and(is_sha256))
    };
    if object.get("v").and_then(Value::as_u64) != Some(2)
        || object.get("op").and_then(Value::as_str) != Some("changeset")
        || !object
            .get("changes_sha256")
            .and_then(Value::as_str)
            .is_some_and(is_sha256)
        || !object
            .get("actor_ref")
            .and_then(Value::as_str)
            .is_some_and(is_sha256)
        || !object
            .get("control_revision")
            .and_then(Value::as_str)
            .is_some_and(is_sha256)
        || !object
            .get("state_root")
            .and_then(Value::as_str)
            .is_some_and(is_sha256)
        || !hash_or_null("parent_commit")
        || !hash_or_null("parent_root")
        || !hash_or_null("parent_asset_root")
        || !hash_or_null("asset_root")
        || !hash_or_null("prev_entry_hash")
        || !object
            .get("materializer")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty() && value.len() <= 128)
        || !object
            .get("ts")
            .and_then(Value::as_str)
            .is_some_and(|value| value.len() == 24 && value.ends_with('Z'))
    {
        return Err(invalid_feed("v2 commit fields are invalid"));
    }
    if (seq == 1
        && [
            "parent_commit",
            "parent_root",
            "parent_asset_root",
            "prev_entry_hash",
        ]
        .iter()
        .any(|field| !object.get(*field).is_some_and(Value::is_null)))
        || (seq > 1
            && ["parent_commit", "parent_root", "prev_entry_hash"]
                .iter()
                .any(|field| object.get(*field).and_then(Value::as_str).is_none()))
    {
        return Err(invalid_feed("v2 commit parent shape is invalid"));
    }
    match object.get("v1_bridge") {
        Some(Value::Null) => {}
        Some(Value::Object(bridge))
            if seq == 1
                && bridge.len() == 3
                && bridge
                    .get("head_seq")
                    .and_then(Value::as_u64)
                    .is_some_and(|v| v > 0)
                && bridge
                    .get("feed_hash")
                    .and_then(Value::as_str)
                    .is_some_and(is_sha256)
                && bridge
                    .get("pack_sha256")
                    .and_then(Value::as_str)
                    .is_some_and(is_sha256) => {}
        _ => return Err(invalid_feed("v2 commit has an invalid v1 bridge")),
    }
    let public_key = object
        .get("public_key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("v2 commit has no public key"))?;
    let der = URL_SAFE_NO_PAD
        .decode(public_key)
        .map_err(|_| invalid_feed("v2 brain public key is not base64url"))?;
    let expected_multikey = format!("ed25519:{}", URL_SAFE_NO_PAD.encode(Sha256::digest(&der)));
    if object.get("brain").and_then(Value::as_str) != Some(expected_multikey.as_str()) {
        return Err(invalid_feed("v2 commit brain identity mismatch"));
    }
    // Verify the old-key-signed chain before trusting any prior public key.
    verify_identity_chain(&v2_identity(identity), None)?;
    // Old commits remain verifiable after brain-key rotation. `previous` is
    // newest-first, while signer epochs and rotation statements are oldest-first.
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
    let signer_index = chain.iter().position(|(fingerprint, spki)| {
        *fingerprint == expected_multikey.trim_start_matches("ed25519:") && *spki == public_key
    });
    let Some(signer_index) = signer_index else {
        return Err(invalid_feed("v2 commit uses an unrecognized brain key"));
    };
    if signer_epoch != signer_index as u64 + 1 {
        return Err(invalid_feed("v2 commit signer epoch differs from its key"));
    }
    let lower_boundary = if signer_index == 0 {
        None
    } else {
        let prior: RotationStatement = serde_json::from_str(&identity.rotations[signer_index - 1])
            .map_err(|_| invalid_feed("v2 rotation statement did not parse"))?;
        Some(prior.prior_head_seq)
    };
    let upper_boundary = if signer_index == identity.rotations.len() {
        None
    } else {
        let next: RotationStatement = serde_json::from_str(&identity.rotations[signer_index])
            .map_err(|_| invalid_feed("v2 rotation statement did not parse"))?;
        Some(next.prior_head_seq)
    };
    if lower_boundary.is_some_and(|boundary| seq <= boundary)
        || upper_boundary.is_some_and(|boundary| seq > boundary)
    {
        return Err(invalid_feed(
            "v2 commit signer is outside its authenticated rotation epoch",
        ));
    }
    let unsigned = crate::linkmd_v2::canonical_bytes(&Value::Object(object.clone()))
        .map_err(|error| invalid_feed(error.to_string()))?;
    verify_v2_spki_signature(public_key, &unsigned, &sig)?;
    Ok(object.clone())
}

#[derive(Debug, Deserialize)]
struct V2FeedWireEntry {
    seq: u64,
    commit_hash: String,
    feed_hash: String,
    bytes_base64: String,
}

#[derive(Debug, Deserialize)]
struct V2FeedPage {
    v: u8,
    head_seq: u64,
    head_commit_hash: String,
    head_feed_hash: String,
    entries: Vec<V2FeedWireEntry>,
    next_after: u64,
    complete: bool,
}

fn replay_v2_feed(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    identity: &V2HeadIdentity,
    start_after: u64,
    start_feed: Option<String>,
) -> LinkResult<()> {
    let mut after = start_after;
    let mut prior_feed = start_feed;
    let mut final_object = None;
    let mut replayed_entries = 0_u64;
    let mut replayed_bytes = 0_u64;
    while after < pointer.seq {
        let path = format!("/api/hub/brains/{brain}/v2/feed?after={after}&limit=100");
        let value = ensure_ok(
            request_capped(
                cfg,
                "GET",
                &path,
                None,
                Auth::Required,
                MAX_FEED_REPLAY_BYTES,
            )?,
            "v2 feed replay",
        )?;
        let page: V2FeedPage = serde_json::from_value(value)
            .map_err(|_| invalid_feed("v2 feed page has an invalid shape"))?;
        if page.v != 2
            || page.head_seq != pointer.seq
            || page.head_commit_hash != pointer.commit_hash
            || page.head_feed_hash != pointer.feed_hash
            || page.entries.is_empty()
            || page.entries.len() > FEED_PAGE_LIMIT
        {
            return Err(invalid_feed("v2 feed page differs from the signed head"));
        }
        for entry in page.entries {
            if entry.seq != after + 1
                || !is_sha256(&entry.commit_hash)
                || !is_sha256(&entry.feed_hash)
            {
                return Err(invalid_feed("v2 feed sequence is not contiguous"));
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&entry.bytes_base64)
                .map_err(|_| invalid_feed("v2 feed bytes are not canonical base64"))?;
            replayed_entries = replayed_entries
                .checked_add(1)
                .ok_or_else(|| invalid_feed("v2 feed replay count overflow"))?;
            replayed_bytes = replayed_bytes
                .checked_add(raw.len() as u64)
                .ok_or_else(|| invalid_feed("v2 feed replay byte count overflow"))?;
            if replayed_entries > MAX_FEED_REPLAY_ENTRIES || replayed_bytes > MAX_FEED_REPLAY_BYTES
            {
                return Err(invalid_feed("v2 feed replay exceeds its safety bound"));
            }
            if crate::linkmd_v2::domain_hash_bytes("v2/commit", &raw)
                .map_err(|error| invalid_feed(error.to_string()))?
                != entry.commit_hash
                || content_sha256(&raw) != entry.feed_hash
            {
                return Err(invalid_feed("v2 feed entry address mismatch"));
            }
            let object = verified_v2_commit_object(&raw, identity)?;
            if object.get("seq").and_then(Value::as_u64) != Some(entry.seq)
                || object.get("prev_entry_hash").and_then(Value::as_str) != prior_feed.as_deref()
            {
                return Err(invalid_feed(
                    "v2 feed entry does not extend its predecessor",
                ));
            }
            after = entry.seq;
            prior_feed = Some(entry.feed_hash);
            final_object = Some((entry.commit_hash, object));
        }
        if page.next_after != after || (page.complete != (after == pointer.seq)) {
            return Err(invalid_feed("v2 feed page cursor is inconsistent"));
        }
    }
    let (final_hash, object) =
        final_object.ok_or_else(|| invalid_feed("v2 feed replay made no progress"))?;
    if final_hash != pointer.commit_hash
        || prior_feed.as_deref() != Some(pointer.feed_hash.as_str())
        || object.get("state_root").and_then(Value::as_str) != pointer.content_root.as_deref()
        || object.get("asset_root").and_then(Value::as_str) != pointer.asset_root.as_deref()
        || object.get("control_revision").and_then(Value::as_str)
            != Some(pointer.control_revision.as_str())
        || object.get("materializer").and_then(Value::as_str) != Some(pointer.materializer.as_str())
    {
        return Err(invalid_feed(
            "v2 replay did not converge on the signed pointer",
        ));
    }
    Ok(())
}

fn verify_v1_to_v2_bridge(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    identity: &V2HeadIdentity,
    checkpoint: &TrustState,
) -> LinkResult<()> {
    let value = ensure_ok(
        request_capped(
            cfg,
            "GET",
            &format!("/api/hub/brains/{brain}/v2/feed?after=0&limit=1"),
            None,
            Auth::Required,
            MAX_FEED_RESPONSE_BYTES,
        )?,
        "v2 genesis bridge",
    )?;
    let page: V2FeedPage = serde_json::from_value(value)
        .map_err(|_| invalid_feed("v2 genesis bridge page has an invalid shape"))?;
    if page.v != 2
        || page.head_seq != pointer.seq
        || page.head_commit_hash != pointer.commit_hash
        || page.head_feed_hash != pointer.feed_hash
        || page.entries.len() != 1
        || page.entries[0].seq != 1
        || !is_sha256(&page.entries[0].commit_hash)
        || !is_sha256(&page.entries[0].feed_hash)
    {
        return Err(invalid_feed(
            "v2 genesis bridge page differs from the signed head",
        ));
    }
    let first = &page.entries[0];
    let raw = STANDARD
        .decode(&first.bytes_base64)
        .map_err(|_| invalid_feed("v2 genesis bridge bytes are not canonical base64"))?;
    if crate::linkmd_v2::domain_hash_bytes("v2/commit", &raw)
        .map_err(|error| invalid_feed(error.to_string()))?
        != first.commit_hash
        || content_sha256(&raw) != first.feed_hash
    {
        return Err(invalid_feed("v2 genesis bridge address mismatch"));
    }
    let object = verified_v2_commit_object(&raw, identity)?;
    if checkpoint.head_seq == 0 {
        if checkpoint.feed_hash.is_some() || object.get("v1_bridge") != Some(&Value::Null) {
            return Err(invalid_feed(
                "empty v1 checkpoint did not transition through an empty v2 genesis",
            ));
        }
        return Ok(());
    }
    let bridge = object
        .get("v1_bridge")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_feed("v2 genesis omitted the pinned v1 boundary"))?;
    let checkpoint_feed = checkpoint
        .feed_hash
        .as_deref()
        .ok_or_else(|| invalid_feed("non-empty v1 checkpoint has no feed hash"))?;
    if bridge.get("head_seq").and_then(Value::as_u64) != Some(checkpoint.head_seq)
        || bridge.get("feed_hash").and_then(Value::as_str) != Some(checkpoint_feed)
    {
        return Err(invalid_feed(
            "v2 genesis bridge differs from the pinned v1 checkpoint",
        ));
    }
    let legacy_raw = ensure_raw_ok(
        request_raw(
            cfg,
            "GET",
            &format!(
                "/api/hub/brains/{brain}/feed?after={}&limit=1",
                checkpoint.head_seq - 1
            ),
            None,
            Auth::Required,
            MAX_FEED_RESPONSE_BYTES,
        )?,
        "v1 bridge boundary",
    )?;
    let legacy: FeedResponse = serde_json::from_slice(&legacy_raw)
        .map_err(|_| invalid_feed("v1 bridge boundary has an invalid feed shape"))?;
    let legacy_identity = legacy
        .identity
        .ok_or_else(|| invalid_feed("v1 bridge boundary has no identity"))?;
    let item = legacy
        .entries
        .first()
        .filter(|_| legacy.entries.len() == 1)
        .ok_or_else(|| invalid_feed("v1 bridge boundary did not return one exact entry"))?;
    if legacy.scope_limited
        || legacy.head_seq != checkpoint.head_seq
        || legacy.feed_hash.as_deref() != Some(checkpoint_feed)
        || item.entry.seq != checkpoint.head_seq
        || item.hash != checkpoint_feed
        || legacy_identity != v2_identity(identity)
        || bridge.get("pack_sha256").and_then(Value::as_str)
            != Some(item.entry.pack_sha256.as_str())
    {
        return Err(invalid_feed(
            "v1 bridge boundary differs from its signed legacy head",
        ));
    }
    let anchor = verify_identity_chain(&legacy_identity, Some(checkpoint))?;
    if anchor != checkpoint.anchor {
        return Err(invalid_feed("v1 bridge changed the pinned identity anchor"));
    }
    verify_feed_item(item, &legacy_identity)?;
    verify_rotation_feed_boundaries(
        &legacy_identity,
        Some(checkpoint),
        std::slice::from_ref(item),
        checkpoint.head_seq,
    )?;
    Ok(())
}

fn verify_v2_commit(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    identity: &V2HeadIdentity,
    pinned: Option<&TrustState>,
) -> LinkResult<()> {
    let path = format!(
        "/api/hub/brains/{brain}/v2/commit?commit={}",
        pointer.commit_hash
    );
    let raw = ensure_raw_ok(
        request_raw(cfg, "GET", &path, None, Auth::Required, MAX_RESPONSE_BYTES)?,
        "v2 commit",
    )?;
    if crate::linkmd_v2::domain_hash_bytes("v2/commit", &raw)
        .map_err(|error| invalid_feed(error.to_string()))?
        != pointer.commit_hash
        || content_sha256(&raw) != pointer.feed_hash
    {
        return Err(invalid_feed("v2 commit address differs from the pointer"));
    }
    let object = verified_v2_commit_object(&raw, identity)?;
    if object.get("seq").and_then(Value::as_u64) != Some(pointer.seq)
        || object.get("state_root").and_then(Value::as_str) != pointer.content_root.as_deref()
        || object.get("asset_root").and_then(Value::as_str) != pointer.asset_root.as_deref()
        || object.get("control_revision").and_then(Value::as_str)
            != Some(pointer.control_revision.as_str())
        || object.get("materializer").and_then(Value::as_str) != Some(pointer.materializer.as_str())
    {
        return Err(invalid_feed("v2 commit fields differ from the pointer"));
    }
    if let Some(checkpoint) = pinned.filter(|checkpoint| accepted_as_v2(checkpoint)) {
        if pointer.seq == checkpoint.head_seq + 1
            && object.get("prev_entry_hash").and_then(Value::as_str)
                != checkpoint.feed_hash.as_deref()
        {
            return Err(invalid_feed(
                "v2 commit does not extend the pinned feed hash",
            ));
        }
        if pointer.seq > checkpoint.head_seq + 1 {
            return replay_v2_feed(
                cfg,
                brain,
                pointer,
                identity,
                checkpoint.head_seq,
                checkpoint.feed_hash.clone(),
            );
        }
    } else {
        if let Some(checkpoint) = pinned {
            verify_v1_to_v2_bridge(cfg, brain, pointer, identity, checkpoint)?;
        }
        if pointer.seq > 1 {
            return replay_v2_feed(cfg, brain, pointer, identity, 0, None);
        }
    }
    Ok(())
}

fn v2_verified_head(cfg: &HubConfig, brain: &str) -> LinkResult<Option<V2VerifiedHead>> {
    require_hardened_filesystem("verified link.md v2 state")?;
    require_safe_ref(brain)?;
    let path = format!("/api/hub/brains/{brain}/v2/head");
    let response = request(cfg, "GET", &path, None, Auth::Required)?;
    if response.status == 404 {
        if has_accepted_v2_ref(cfg, brain)? {
            return Err(LinkError::BrainUnavailable);
        }
        return Ok(None);
    }
    let body = ensure_ok(response, "v2 head")?;
    let head: V2HeadResponse = serde_json::from_value(body)
        .map_err(|_| invalid_feed("v2 head response has an invalid shape"))?;
    if head.v != 2 || !crate::ulid::is_ulid(&head.brain_id) {
        return Err(invalid_feed("v2 head has no canonical brain id"));
    }
    if crate::ulid::is_ulid(brain) && head.brain_id != brain {
        return Err(invalid_feed("v2 head resolved a different brain id"));
    }
    if head.profile == "v1" {
        return Ok(None);
    }
    if head.profile != "v2" && head.profile != "v2-empty" {
        return Err(invalid_feed("v2 head advertised an unknown profile"));
    }
    let view = head
        .view
        .as_ref()
        .ok_or_else(|| invalid_feed("v2 head has no permission view"))?;
    if !matches!(view.kind.as_str(), "full" | "scoped") || !is_sha256(&view.control_revision) {
        return Err(invalid_feed("v2 head has an invalid permission view"));
    }
    let view_kind = view.kind.clone();
    let view_revision = view.control_revision.clone();
    let identity = head
        .identity
        .as_ref()
        .ok_or_else(|| invalid_feed("v2 head has no brain identity"))?;
    let trust_directory = open_trust_dir(cfg)?;
    let _trust_locks = lock_trust_many(cfg, &trust_directory, &[brain, &head.brain_id])?;
    let (pinned, alias_binding) = load_canonical_pin(cfg, &trust_directory, brain, &head.brain_id)?;
    let feed_identity = v2_identity(identity);
    let anchor = verify_identity_chain(&feed_identity, pinned.as_ref())?;
    let (seq, feed_hash, hub_signer) = match &head.pointer {
        None => {
            if head.profile != "v2-empty" {
                return Err(invalid_feed("initialized v2 head has no pointer"));
            }
            (
                0,
                None,
                pinned.as_ref().and_then(|state| state.hub_signer.clone()),
            )
        }
        Some(signed) => {
            let signer = verify_v2_pointer(signed, &head.brain_id)?;
            if pinned
                .as_ref()
                .and_then(|state| state.hub_signer.as_ref())
                .is_some_and(|known| known != &signer)
            {
                return Err(invalid_feed(
                    "v2 hub pointer signer changed without a trust transition",
                ));
            }
            if let Some(checkpoint) = pinned.as_ref().filter(|state| accepted_as_v2(state)) {
                if signed.pointer.seq < checkpoint.head_seq
                    || (signed.pointer.seq == checkpoint.head_seq
                        && checkpoint.feed_hash.as_deref()
                            != Some(signed.pointer.feed_hash.as_str()))
                {
                    return Err(invalid_feed("v2 pointer rolled back or equivocated"));
                }
            }
            verify_v2_commit(
                cfg,
                &head.brain_id,
                &signed.pointer,
                identity,
                pinned.as_ref(),
            )?;
            (
                signed.pointer.seq,
                Some(signed.pointer.feed_hash.clone()),
                Some(signer),
            )
        }
    };
    let trust = TrustState {
        v: 2,
        origin: normalized_origin(&cfg.hub)?,
        requested: head.brain_id.clone(),
        brain: head.brain_id.clone(),
        home: None,
        anchor,
        current: format!("ed25519:{}", identity.fingerprint),
        head_seq: seq,
        feed_hash,
        rotations: identity.rotations.clone(),
        hub_signer,
        protocol_profile: Some("link-v2".to_string()),
    };
    Ok(Some(V2VerifiedHead {
        requested: brain.to_string(),
        brain_id: head.brain_id,
        view_kind,
        view_revision,
        identity: identity.clone(),
        pointer: head.pointer.map(|signed| signed.pointer),
        trust,
        alias: alias_binding,
    }))
}

fn accept_v2_head(cfg: &HubConfig, head: &V2VerifiedHead) -> LinkResult<()> {
    let directory = open_trust_dir(cfg)?;
    let _locks = lock_trust_many(cfg, &directory, &[&head.requested, &head.brain_id])?;
    let (current, alias) = load_canonical_pin(cfg, &directory, &head.requested, &head.brain_id)?;
    if let Some(current) = current {
        let common_invalid = head.trust.anchor != current.anchor
            || !head.trust.rotations.starts_with(&current.rotations);
        let profile_invalid = if accepted_as_v2(&current) {
            head.trust.head_seq < current.head_seq
                || (head.trust.head_seq == current.head_seq
                    && head.trust.feed_hash != current.feed_hash)
                || current
                    .hub_signer
                    .as_ref()
                    .is_some_and(|known| head.trust.hub_signer.as_ref() != Some(known))
        } else {
            head.trust.protocol_profile.as_deref() != Some("link-v2")
                || head.trust.hub_signer.is_none()
        };
        if common_invalid || profile_invalid {
            return Err(invalid_feed(
                "v2 head cannot advance the currently accepted trust checkpoint",
            ));
        }
    }
    save_canonical_pin_and_alias(
        cfg,
        &directory,
        &head.requested,
        &head.brain_id,
        head.trust.clone(),
        alias.as_ref().or(head.alias.as_ref()),
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2BaselineFile {
    sha256: String,
    bytes: u64,
    #[serde(skip)]
    proof: Option<Vec<V2ProofStep>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2SyncBaseline {
    v: u8,
    origin: String,
    brain: String,
    #[serde(default)]
    head_seq: Option<u64>,
    commit_hash: Option<String>,
    content_root: Option<String>,
    #[serde(default)]
    asset_root: Option<String>,
    #[serde(default)]
    assets: std::collections::BTreeMap<String, V2BaselineAsset>,
    #[serde(default)]
    view_kind: Option<String>,
    #[serde(default)]
    view_revision: Option<String>,
    #[serde(default)]
    projection_sha256: Option<String>,
    files: std::collections::BTreeMap<String, V2BaselineFile>,
    #[serde(default)]
    local_policy_digest: Option<String>,
    #[serde(default)]
    local_eligibility: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    remote_copy_remains: std::collections::BTreeMap<String, String>,
}

struct V2LocalView {
    riding: std::collections::BTreeMap<String, (String, u64)>,
    eligibility: std::collections::BTreeMap<String, bool>,
    policy: crate::linkmd_sync_policy::SyncPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2ProofStep {
    directory_root: String,
    component: String,
    proof: crate::linkmd_v2::HamtProof,
}

#[derive(Debug, Deserialize)]
struct V2ManifestFile {
    path: String,
    sha256: String,
    bytes: u64,
    proof: Vec<V2ProofStep>,
}

#[derive(Debug, Deserialize)]
struct V2ManifestPage {
    v: u8,
    commit: String,
    content_root: Option<String>,
    files: Vec<V2ManifestFile>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2BaselineAsset {
    blob_sha256: String,
    bytes: u64,
    media_type: String,
    wrappers: Vec<String>,
    required: bool,
    disposition: String,
    leaf_hash: String,
}

#[derive(Debug, Deserialize)]
struct V2AssetManifestItem {
    path: String,
    blob_sha256: String,
    bytes: u64,
    media_type: String,
    wrappers: Vec<String>,
    required: bool,
    disposition: String,
    leaf_hash: String,
    proof: crate::linkmd_v2::HamtProof,
}

#[derive(Debug, Deserialize)]
struct V2AssetManifestPage {
    v: u8,
    commit: String,
    asset_root: Option<String>,
    assets: Vec<V2AssetManifestItem>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct V2SigningCandidate {
    seq: u64,
    content_root: Option<String>,
    asset_root: Option<String>,
    signing_bytes_base64: String,
    changes_base64: String,
    actor_claim_base64: String,
}

#[derive(Debug, Deserialize)]
struct V2SigningCandidatePage {
    v: u8,
    challenge_id: String,
    mutation_id: String,
    request_hash: String,
    parent: V2SigningParent,
    candidate: V2SigningCandidate,
    files: Vec<V2ManifestFile>,
    #[serde(default)]
    assets: Vec<V2AssetManifestItem>,
    next_cursor: Option<String>,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct V2SigningParent {
    seq: u64,
    commit_hash: Option<String>,
}

fn verify_v2_file_proof(root: &str, file: &V2ManifestFile) -> LinkResult<()> {
    let normalized = crate::linkmd_v2::normalize_path(&file.path)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let components = normalized.split('/').collect::<Vec<_>>();
    if components.len() != file.proof.len() || !is_sha256(&file.sha256) {
        return Err(invalid_feed("v2 file proof has the wrong shape"));
    }
    let mut directory_root = root.to_string();
    for (index, step) in file.proof.iter().enumerate() {
        if step.directory_root != directory_root || step.component != components[index] {
            return Err(invalid_feed(
                "v2 file proof path chain differs from its manifest",
            ));
        }
        if !crate::linkmd_v2::verify_proof(&directory_root, &step.component, &step.proof)
            .map_err(|error| invalid_feed(error.to_string()))?
        {
            return Err(invalid_feed("v2 file proof failed verification"));
        }
        let entry = match &step.proof {
            crate::linkmd_v2::HamtProof::Inclusion { entry, .. } => entry,
            crate::linkmd_v2::HamtProof::NonInclusion { .. } => {
                return Err(invalid_feed("v2 manifest carried a non-inclusion proof"));
            }
        };
        if index + 1 == components.len() {
            if entry.kind != crate::linkmd_v2::EntryKind::Blob
                || entry.child_hash != file.sha256
                || entry.bytes != Some(file.bytes)
            {
                return Err(invalid_feed("v2 file proof leaf differs from its manifest"));
            }
        } else if entry.kind != crate::linkmd_v2::EntryKind::Tree {
            return Err(invalid_feed("v2 file proof traversed a non-directory"));
        } else {
            directory_root = entry.child_hash.clone();
        }
    }
    Ok(())
}

fn v2_manifest(
    cfg: &HubConfig,
    brain: &str,
    pointer: Option<&V2PointerBody>,
) -> LinkResult<std::collections::BTreeMap<String, V2BaselineFile>> {
    let Some(pointer) = pointer else {
        return Ok(std::collections::BTreeMap::new());
    };
    let Some(root) = pointer.content_root.as_deref() else {
        return Ok(std::collections::BTreeMap::new());
    };
    let mut files = std::collections::BTreeMap::new();
    let mut after = String::new();
    loop {
        let encoded_after: String =
            url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
        let path = format!(
            "/api/hub/brains/{brain}/v2/files?commit={}&limit=500&after={encoded_after}",
            pointer.commit_hash
        );
        let value = ensure_ok(
            request_capped(
                cfg,
                "GET",
                &path,
                None,
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "v2 file manifest",
        )?;
        let page: V2ManifestPage = serde_json::from_value(value)
            .map_err(|_| invalid_feed("v2 file manifest has an invalid shape"))?;
        if page.v != 2
            || page.commit != pointer.commit_hash
            || page.content_root.as_deref() != Some(root)
            || page.files.len() > 500
        {
            return Err(invalid_feed(
                "v2 file manifest is not bound to the verified head",
            ));
        }
        for file in page.files {
            verify_v2_file_proof(root, &file)?;
            if files
                .insert(
                    file.path.clone(),
                    V2BaselineFile {
                        sha256: file.sha256,
                        bytes: file.bytes,
                        proof: Some(file.proof),
                    },
                )
                .is_some()
            {
                return Err(invalid_feed("v2 file manifest repeats a path"));
            }
            if files.len() > MAX_PUSH_FILES {
                return Err(invalid_feed(
                    "v2 file manifest exceeds the file-count bound",
                ));
            }
        }
        match page.next_cursor {
            None => break,
            Some(next) if next > after => after = next,
            Some(_) => return Err(invalid_feed("v2 file manifest cursor did not advance")),
        }
    }
    Ok(files)
}

fn verify_v2_asset_proof(root: &str, item: &V2AssetManifestItem) -> LinkResult<()> {
    crate::linkmd_v2::normalize_path(&item.path)
        .map_err(|error| invalid_feed(error.to_string()))?;
    if !is_sha256(&item.blob_sha256)
        || !is_sha256(&item.leaf_hash)
        || item.wrappers.is_empty()
        || !matches!(item.disposition.as_str(), "hosted" | "withheld")
        || item
            .wrappers
            .iter()
            .any(|wrapper| crate::linkmd_v2::normalize_path(wrapper).is_err())
    {
        return Err(invalid_feed("v2 asset manifest item is invalid"));
    }
    let leaf = json!({
        "blob_sha256": item.blob_sha256,
        "bytes": item.bytes,
        "disposition": item.disposition,
        "media_type": item.media_type,
        "path": item.path,
        "required": item.required,
        "v": 2,
        "wrappers": item.wrappers,
    });
    if crate::linkmd_v2::domain_hash("v2/asset-leaf", &leaf)
        .map_err(|error| invalid_feed(error.to_string()))?
        != item.leaf_hash
        || !crate::linkmd_v2::verify_proof_with_domain(
            root,
            &item.path,
            &item.proof,
            crate::linkmd_v2::ASSET_TREE_HASH_DOMAIN,
        )
        .map_err(|error| invalid_feed(error.to_string()))?
    {
        return Err(invalid_feed("v2 asset inclusion proof failed"));
    }
    match &item.proof {
        crate::linkmd_v2::HamtProof::Inclusion { entry, .. }
            if entry.name == item.path
                && entry.kind == crate::linkmd_v2::EntryKind::Blob
                && entry.child_hash == item.leaf_hash
                && entry.bytes == Some(item.bytes) =>
        {
            Ok(())
        }
        _ => Err(invalid_feed(
            "v2 asset proof leaf differs from its manifest",
        )),
    }
}

fn v2_asset_manifest(
    cfg: &HubConfig,
    brain: &str,
    pointer: Option<&V2PointerBody>,
) -> LinkResult<std::collections::BTreeMap<String, V2BaselineAsset>> {
    let Some(pointer) = pointer else {
        return Ok(std::collections::BTreeMap::new());
    };
    let Some(root) = pointer.asset_root.as_deref() else {
        return Ok(std::collections::BTreeMap::new());
    };
    let mut assets = std::collections::BTreeMap::new();
    let mut after = String::new();
    loop {
        let encoded_after: String =
            url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
        let path = format!(
            "/api/hub/brains/{brain}/v2/assets?commit={}&limit=500&after={encoded_after}",
            pointer.commit_hash
        );
        let value = ensure_ok(
            request_capped(
                cfg,
                "GET",
                &path,
                None,
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "v2 asset manifest",
        )?;
        let page: V2AssetManifestPage = serde_json::from_value(value)
            .map_err(|_| invalid_feed("v2 asset manifest has an invalid shape"))?;
        if page.v != 2
            || page.commit != pointer.commit_hash
            || page.asset_root.as_deref() != Some(root)
            || page.assets.len() > 500
        {
            return Err(invalid_feed(
                "v2 asset manifest is not bound to the verified head",
            ));
        }
        for item in page.assets {
            verify_v2_asset_proof(root, &item)?;
            let path = item.path.clone();
            if assets
                .insert(
                    path,
                    V2BaselineAsset {
                        blob_sha256: item.blob_sha256,
                        bytes: item.bytes,
                        media_type: item.media_type,
                        wrappers: item.wrappers,
                        required: item.required,
                        disposition: item.disposition,
                        leaf_hash: item.leaf_hash,
                    },
                )
                .is_some()
            {
                return Err(invalid_feed("v2 asset manifest repeats a path"));
            }
            if assets.len() > MAX_PUSH_FILES {
                return Err(invalid_feed(
                    "v2 asset manifest exceeds the item-count bound",
                ));
            }
        }
        match page.next_cursor {
            None => break,
            Some(next) if next > after => after = next,
            Some(_) => return Err(invalid_feed("v2 asset manifest cursor did not advance")),
        }
    }
    Ok(assets)
}

fn v2_asset_record(asset: &V2BaselineAsset, path: &str) -> crate::AssetRecord {
    crate::AssetRecord {
        path: path.to_string(),
        sha256: asset.blob_sha256.clone(),
        bytes: asset.bytes,
        media_type: asset.media_type.clone(),
        wrappers: asset.wrappers.clone(),
        required: asset.required,
    }
}

fn v2_asset_manifest_bytes(
    assets: &std::collections::BTreeMap<String, V2BaselineAsset>,
) -> LinkResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for (path, asset) in assets {
        serde_json::to_writer(&mut bytes, &v2_asset_record(asset, path))
            .map_err(|_| invalid_feed("could not materialize v2 assets.jsonl"))?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn sign_verified_v2_candidate(
    cfg: &HubConfig,
    head: &V2VerifiedHead,
    expected: &std::collections::BTreeMap<String, V2BaselineFile>,
    expected_assets: &std::collections::BTreeMap<String, V2BaselineAsset>,
    mutation_id: &str,
    request_body: &Value,
    challenge_value: &Value,
) -> LinkResult<(String, String, String)> {
    if head.view_kind != "full" {
        return Err(invalid_feed(
            "a scoped self-custody writer must use the proposal workflow",
        ));
    }
    if head.identity.custody != "self" {
        return Err(invalid_feed(
            "a hub-custodied brain unexpectedly requested an external signature",
        ));
    }
    let key = cfg
        .brain_key
        .as_ref()
        .ok_or_else(|| bad_agent_key("this self-custodied brain requires DBMD_BRAIN_KEY_FILE"))?;
    if key.multikey != format!("ed25519:{}", head.identity.fingerprint)
        || key.public_key_spki != head.identity.public_key_spki
    {
        return Err(bad_agent_key(
            "DBMD_BRAIN_KEY_FILE does not match the verified brain identity",
        ));
    }
    let challenge_id = challenge_value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| crate::ulid::is_ulid(id))
        .ok_or_else(|| invalid_feed("self-custody challenge has no canonical id"))?;
    let expected_endpoint = format!(
        "/api/hub/brains/{}/v2/signing-challenges/{challenge_id}",
        head.brain_id
    );
    if challenge_value
        .get("candidate_endpoint")
        .and_then(Value::as_str)
        != Some(expected_endpoint.as_str())
    {
        return Err(invalid_feed(
            "self-custody challenge candidate endpoint is not origin-bound",
        ));
    }

    let mut files = std::collections::BTreeMap::new();
    let mut after = String::new();
    type CandidateCoordinate = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        u64,
        Option<String>,
    );
    let mut pinned: Option<CandidateCoordinate> = None;
    loop {
        let encoded_after: String =
            url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
        let path = format!("{expected_endpoint}?limit=500&after={encoded_after}");
        let value = ensure_ok(
            request_capped(
                cfg,
                "GET",
                &path,
                None,
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "v2 self-custody candidate",
        )?;
        let page: V2SigningCandidatePage = serde_json::from_value(value)
            .map_err(|_| invalid_feed("self-custody candidate has an invalid shape"))?;
        if page.v != 2
            || page.challenge_id != challenge_id
            || page.mutation_id != mutation_id
            || page.candidate.seq != page.parent.seq + 1
            || page.files.len() > 500
            || page.expires_at.is_empty()
        {
            return Err(invalid_feed(
                "self-custody candidate is not bound to this mutation",
            ));
        }
        let coordinate = (
            page.request_hash.clone(),
            page.candidate.signing_bytes_base64.clone(),
            page.candidate.changes_base64.clone(),
            page.candidate.actor_claim_base64.clone(),
            page.candidate.content_root.clone(),
            page.candidate.asset_root.clone(),
            page.parent.seq,
            page.parent.commit_hash.clone(),
        );
        if pinned.as_ref().is_some_and(|prior| prior != &coordinate) {
            return Err(invalid_feed(
                "self-custody candidate changed between manifest pages",
            ));
        }
        pinned = Some(coordinate);
        let root = page
            .candidate
            .content_root
            .as_deref()
            .ok_or_else(|| invalid_feed("self-custody candidate has no content root"))?;
        for file in page.files {
            verify_v2_file_proof(root, &file)?;
            if files
                .insert(
                    file.path.clone(),
                    V2BaselineFile {
                        sha256: file.sha256,
                        bytes: file.bytes,
                        proof: Some(file.proof),
                    },
                )
                .is_some()
            {
                return Err(invalid_feed(
                    "self-custody candidate repeats a manifest path",
                ));
            }
            if files.len() > MAX_PUSH_FILES {
                return Err(invalid_feed(
                    "self-custody candidate exceeds the file-count bound",
                ));
            }
        }
        match page.next_cursor {
            None => break,
            Some(next) if next > after => after = next,
            Some(_) => {
                return Err(invalid_feed(
                    "self-custody candidate cursor did not advance",
                ))
            }
        }
    }
    if files.len() != expected.len()
        || files.iter().any(|(path, file)| {
            expected.get(path).is_none_or(|expected| {
                expected.sha256 != file.sha256 || expected.bytes != file.bytes
            })
        })
    {
        return Err(invalid_feed(
            "self-custody candidate contains an unexpected file mutation",
        ));
    }
    let mut assets = std::collections::BTreeMap::new();
    after.clear();
    loop {
        let encoded_after: String =
            url::form_urlencoded::byte_serialize(after.as_bytes()).collect();
        let path = format!("{expected_endpoint}?kind=assets&limit=500&after={encoded_after}");
        let value = ensure_ok(
            request_capped(
                cfg,
                "GET",
                &path,
                None,
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "v2 self-custody asset candidate",
        )?;
        let page: V2SigningCandidatePage = serde_json::from_value(value)
            .map_err(|_| invalid_feed("self-custody asset candidate has an invalid shape"))?;
        let coordinate = (
            page.request_hash.clone(),
            page.candidate.signing_bytes_base64.clone(),
            page.candidate.changes_base64.clone(),
            page.candidate.actor_claim_base64.clone(),
            page.candidate.content_root.clone(),
            page.candidate.asset_root.clone(),
            page.parent.seq,
            page.parent.commit_hash.clone(),
        );
        if page.v != 2
            || page.challenge_id != challenge_id
            || page.mutation_id != mutation_id
            || page.assets.len() > 500
            || pinned.as_ref() != Some(&coordinate)
        {
            return Err(invalid_feed(
                "self-custody asset candidate changed or is not bound",
            ));
        }
        let root = page.candidate.asset_root.as_deref();
        if !page.assets.is_empty() && root.is_none() {
            return Err(invalid_feed("asset candidate has no asset root"));
        }
        for item in page.assets {
            verify_v2_asset_proof(root.expect("non-empty assets checked"), &item)?;
            if assets
                .insert(
                    item.path.clone(),
                    V2BaselineAsset {
                        blob_sha256: item.blob_sha256,
                        bytes: item.bytes,
                        media_type: item.media_type,
                        wrappers: item.wrappers,
                        required: item.required,
                        disposition: item.disposition,
                        leaf_hash: item.leaf_hash,
                    },
                )
                .is_some()
            {
                return Err(invalid_feed("self-custody candidate repeats an asset"));
            }
        }
        match page.next_cursor {
            None => break,
            Some(next) if next > after => after = next,
            Some(_) => {
                return Err(invalid_feed(
                    "self-custody asset candidate cursor did not advance",
                ))
            }
        }
    }
    if assets.len() != expected_assets.len()
        || assets.iter().any(|(path, asset)| {
            expected_assets.get(path).is_none_or(|expected| {
                asset.blob_sha256 != expected.blob_sha256
                    || asset.bytes != expected.bytes
                    || asset.media_type != expected.media_type
                    || asset.wrappers != expected.wrappers
                    || asset.required != expected.required
                    || asset.disposition != expected.disposition
            })
        })
    {
        return Err(invalid_feed(
            "self-custody candidate contains an unexpected asset mutation",
        ));
    }
    let Some((
        request_hash,
        signing_b64,
        changes_b64,
        actor_b64,
        root,
        asset_root,
        parent_seq,
        parent,
    )) = pinned
    else {
        return Err(invalid_feed("self-custody candidate has no manifest"));
    };
    let current_seq = head.pointer.as_ref().map_or(0, |pointer| pointer.seq);
    let current_commit = head
        .pointer
        .as_ref()
        .map(|pointer| pointer.commit_hash.clone());
    if parent_seq != current_seq || parent != current_commit {
        return Err(LinkError::RemoteAdvancedDuringSync);
    }
    let changes = STANDARD
        .decode(changes_b64)
        .map_err(|_| invalid_feed("self-custody changeset is not base64"))?;
    let expected_changes = json!({
        "mutation_id": mutation_id,
        "operations": request_body.get("operations").cloned().unwrap_or(Value::Null),
        "reason": request_body.get("reason").cloned().unwrap_or(Value::Null),
        "v": 2,
    });
    let expected_changes_bytes = crate::linkmd_v2::canonical_bytes(&expected_changes)
        .map_err(|error| invalid_feed(error.to_string()))?;
    if changes != expected_changes_bytes {
        return Err(invalid_feed(
            "self-custody changeset differs from the requested mutation",
        ));
    }
    let changes_hash = crate::linkmd_v2::domain_hash_bytes("v2/changeset", &changes)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let request_value = json!({
        "base": request_body.get("base").cloned().unwrap_or(Value::Null),
        "brain": head.brain_id,
        "changes_sha256": changes_hash,
        "rebase": request_body.get("rebase").cloned().unwrap_or(Value::Null),
        "v": 2,
        "v1_bridge": Value::Null,
    });
    let expected_request_hash = crate::linkmd_v2::domain_hash("v2/request", &request_value)
        .map_err(|error| invalid_feed(error.to_string()))?;
    if request_hash != expected_request_hash {
        return Err(invalid_feed(
            "self-custody request hash differs from the requested mutation",
        ));
    }
    let actor = STANDARD
        .decode(actor_b64)
        .map_err(|_| invalid_feed("self-custody actor claim is not base64"))?;
    let actor_value: Value = serde_json::from_slice(&actor)
        .map_err(|_| invalid_feed("self-custody actor claim is not JSON"))?;
    if crate::linkmd_v2::canonical_bytes(&actor_value)
        .map_err(|error| invalid_feed(error.to_string()))?
        != actor
    {
        return Err(invalid_feed("self-custody actor claim is not canonical"));
    }
    let actor_object = actor_value
        .as_object()
        .ok_or_else(|| invalid_feed("self-custody actor claim is not an object"))?;
    let actor_claim = actor_object
        .get("claim")
        .ok_or_else(|| invalid_feed("self-custody actor claim body is missing"))?;
    let actor_public_key = actor_object
        .get("public_key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("self-custody actor signer is missing"))?;
    let actor_fingerprint = actor_object
        .get("fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("self-custody actor fingerprint is missing"))?;
    let actor_signature = actor_object
        .get("sig")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("self-custody actor signature is missing"))?;
    let actor_message = crate::linkmd_v2::canonical_bytes(actor_claim)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let actor_der = verify_v2_spki_signature(actor_public_key, &actor_message, actor_signature)?;
    let expected_actor_signer = format!("{actor_fingerprint}:{actor_public_key}");
    let expected_actor_root = root.clone().map(Value::String).unwrap_or(Value::Null);
    let expected_actor_asset_root = asset_root.clone().map(Value::String).unwrap_or(Value::Null);
    let impact = actor_claim
        .get("result")
        .and_then(|result| result.get("impact"))
        .and_then(Value::as_object);
    let impact_fields = [
        "creates",
        "updates",
        "deletes",
        "withdrawals",
        "renames",
        "restores",
        "asset_changes",
        "public_expansions",
        "executable_activations",
    ];
    let impact_is_valid = impact.is_some_and(|impact| {
        impact.len() == impact_fields.len() + 1
            && impact.get("v").and_then(Value::as_u64) == Some(1)
            && impact_fields
                .iter()
                .all(|field| impact.get(*field).and_then(Value::as_u64).is_some())
    });
    if format!("{:x}", Sha256::digest(&actor_der)) != actor_fingerprint
        || head
            .trust
            .hub_signer
            .as_ref()
            .is_some_and(|known| known != &expected_actor_signer)
        || actor_claim.get("mutation_id").and_then(Value::as_str) != Some(mutation_id)
        || actor_claim.get("request_hash").and_then(Value::as_str) != Some(request_hash.as_str())
        || actor_claim
            .get("candidate")
            .and_then(|candidate| candidate.get("changes_sha256"))
            .and_then(Value::as_str)
            != Some(changes_hash.as_str())
        || actor_claim
            .get("candidate")
            .and_then(|candidate| candidate.get("state_root"))
            != Some(&expected_actor_root)
        || actor_claim
            .get("candidate")
            .and_then(|candidate| candidate.get("asset_root"))
            != Some(&expected_actor_asset_root)
        || actor_claim
            .get("candidate")
            .and_then(|candidate| candidate.get("control_revision"))
            .and_then(Value::as_str)
            != Some(head.view_revision.as_str())
        || !impact_is_valid
    {
        return Err(invalid_feed(
            "self-custody actor claim does not bind the verified authority",
        ));
    }
    let actor_hash = crate::linkmd_v2::domain_hash_bytes("v2/actor-claim", &actor)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let signing = STANDARD
        .decode(signing_b64)
        .map_err(|_| invalid_feed("self-custody signing bytes are not base64"))?;
    let signing_value: Value = serde_json::from_slice(&signing)
        .map_err(|_| invalid_feed("self-custody signing bytes are not JSON"))?;
    if crate::linkmd_v2::canonical_bytes(&signing_value)
        .map_err(|error| invalid_feed(error.to_string()))?
        != signing
    {
        return Err(invalid_feed("self-custody signing bytes are not canonical"));
    }
    let pointer = head.pointer.as_ref();
    let expected_materializer = pointer
        .map(|value| value.materializer.as_str())
        .unwrap_or("dbmd-projection-v1");
    let expected_parent_commit = request_body
        .get("base")
        .and_then(|base| base.get("commit_hash"))
        .cloned()
        .unwrap_or(Value::Null);
    let expected_parent_root = request_body
        .get("base")
        .and_then(|base| base.get("content_root"))
        .cloned()
        .unwrap_or(Value::Null);
    let expected_state_root = root.clone().map(Value::String).unwrap_or(Value::Null);
    let expected_parent_asset_root = request_body
        .get("base")
        .and_then(|base| base.get("asset_root"))
        .cloned()
        .unwrap_or(Value::Null);
    let expected_asset_root = asset_root.map(Value::String).unwrap_or(Value::Null);
    let expected_prev_entry = pointer
        .map(|value| Value::String(value.feed_hash.clone()))
        .unwrap_or(Value::Null);
    let expected_signer_epoch = u64::try_from(head.identity.previous.len())
        .map_err(|_| invalid_feed("brain identity history is too large"))?
        + 1;
    if signing_value.get("v").and_then(Value::as_u64) != Some(2)
        || signing_value.get("seq").and_then(Value::as_u64) != Some(current_seq + 1)
        || signing_value.get("signer_epoch").and_then(Value::as_u64) != Some(expected_signer_epoch)
        || signing_value.get("brain").and_then(Value::as_str) != Some(key.multikey.as_str())
        || signing_value.get("public_key").and_then(Value::as_str)
            != Some(key.public_key_spki.as_str())
        || signing_value.get("parent_commit") != Some(&expected_parent_commit)
        || signing_value.get("parent_root") != Some(&expected_parent_root)
        || signing_value.get("state_root") != Some(&expected_state_root)
        || signing_value.get("parent_asset_root") != Some(&expected_parent_asset_root)
        || signing_value.get("asset_root") != Some(&expected_asset_root)
        || signing_value.get("materializer").and_then(Value::as_str) != Some(expected_materializer)
        || signing_value.get("changes_sha256").and_then(Value::as_str)
            != Some(changes_hash.as_str())
        || signing_value.get("actor_ref").and_then(Value::as_str) != Some(actor_hash.as_str())
        || signing_value
            .get("control_revision")
            .and_then(Value::as_str)
            != Some(head.view_revision.as_str())
        || signing_value.get("prev_entry_hash") != Some(&expected_prev_entry)
        || signing_value.get("v1_bridge") != Some(&Value::Null)
        || signing_value.get("op").and_then(Value::as_str) != Some("changeset")
    {
        return Err(invalid_feed(
            "self-custody signing bytes do not bind the verified candidate",
        ));
    }
    let pair = agent_keypair(&key.pkcs8)?;
    let signature = URL_SAFE_NO_PAD.encode(pair.sign(&signing).as_ref());
    Ok((challenge_id.to_string(), signature, expected_actor_signer))
}

fn v2_baseline_name(cfg: &HubConfig, brain: &str, checkout: &Path) -> LinkResult<String> {
    let origin = normalized_origin(&cfg.hub)?;
    let absolute = if checkout.is_absolute() {
        checkout.to_path_buf()
    } else {
        std::env::current_dir()?.join(checkout)
    };
    Ok(format!(
        "sync-{}.json",
        content_sha256(format!("{origin}\0{brain}\0{}", absolute.display()).as_bytes())
    ))
}

#[cfg(any(unix, windows))]
fn lock_v2_sync_operation(cfg: &HubConfig, brain: &str) -> LinkResult<TrustLock> {
    let directory = open_trust_dir(cfg)?;
    let origin = normalized_origin(&cfg.hub)?;
    let name = format!(
        "operation-{}.lock",
        content_sha256(format!("{origin}\0{brain}").as_bytes())
    );
    lock_trust_name(&directory, &name)
}

#[cfg(not(any(unix, windows)))]
fn lock_v2_sync_operation(_cfg: &HubConfig, _brain: &str) -> LinkResult<()> {
    Err(LinkError::UnsupportedPlatform {
        operation: "serialized link.md v2 sync",
    })
}

fn same_v2_head(left: &V2VerifiedHead, right: &V2VerifiedHead) -> bool {
    left.brain_id == right.brain_id
        && left.view_kind == right.view_kind
        && left.view_revision == right.view_revision
        && match (&left.pointer, &right.pointer) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.seq == right.seq
                    && left.commit_hash == right.commit_hash
                    && left.content_root == right.content_root
                    && left.asset_root == right.asset_root
                    && left.feed_hash == right.feed_hash
            }
            _ => false,
        }
}

fn scoped_projection_bytes(brain: &str) -> Vec<u8> {
    format!(
        "---\ntype: db-md\nscope: company\nowner: link.md scoped view\n---\n\n# Scoped brain view\n\nThis DB.md is generated locally by dbmd. It is not the brain's canonical contract and is never uploaded.\n\nCanonical brain: @{brain}\n"
    )
    .into_bytes()
}

fn scoped_projection_sha256(brain: &str) -> String {
    content_sha256(&scoped_projection_bytes(brain))
}

#[derive(Deserialize)]
struct LocalScopedViewMarker {
    v: u8,
    kind: String,
    authoritative: bool,
    brain: String,
    projection_sha256: String,
}

/// True only when this store carries the exact generated marker for a local
/// link.md scoped view. This is presentation context, never authorization;
/// the hub remains the only authority for reads and writes.
pub fn has_verified_local_scoped_view(store: &Store) -> bool {
    let marker = store
        .read_bounded(Path::new(".dbmd/view.json"), 64 * 1024)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LocalScopedViewMarker>(&bytes).ok());
    let Some(marker) = marker else {
        return false;
    };
    if marker.v != 1
        || marker.kind != "link.md-scoped-view"
        || marker.authoritative
        || !crate::ulid::is_ulid(&marker.brain)
        || marker.projection_sha256 != scoped_projection_sha256(&marker.brain)
    {
        return false;
    }
    store
        .read_bounded(Path::new("DB.md"), crate::parser::MAX_DBMD_FILE_BYTES)
        .is_ok_and(|bytes| content_sha256(&bytes) == marker.projection_sha256)
}

fn scoped_view_metadata(head: &V2VerifiedHead, files: usize) -> LinkResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&json!({
        "v": 1,
        "kind": "link.md-scoped-view",
        "authoritative": false,
        "brain": head.brain_id,
        "view_revision": head.view_revision,
        "head_seq": head.pointer.as_ref().map_or(0, |pointer| pointer.seq),
        "commit_hash": head.pointer.as_ref().map(|pointer| &pointer.commit_hash),
        "content_root": head.pointer.as_ref().and_then(|pointer| pointer.content_root.as_ref()),
        "visible_files": files,
        "projection_sha256": scoped_projection_sha256(&head.brain_id),
    }))
    .map_err(|_| invalid_feed("could not serialize scoped view metadata"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn refresh_scoped_view_marker(
    store: &Store,
    head: &V2VerifiedHead,
    files: usize,
) -> LinkResult<()> {
    if head.view_kind == "scoped" {
        store.write_atomic(
            Path::new(".dbmd/view.json"),
            &scoped_view_metadata(head, files)?,
        )?;
    }
    Ok(())
}

fn ensure_v2_view_compatible(
    head: &V2VerifiedHead,
    baseline: Option<&V2SyncBaseline>,
) -> LinkResult<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    match (
        baseline.view_kind.as_deref(),
        baseline.view_revision.as_deref(),
    ) {
        (None, None) if head.view_kind == "full" => Ok(()),
        (Some(kind), Some(revision))
            if kind == head.view_kind && revision == head.view_revision =>
        {
            Ok(())
        }
        _ => Err(LinkError::ScopedViewChanged),
    }
}

fn remove_scoped_projection(
    head: &V2VerifiedHead,
    baseline: Option<&V2SyncBaseline>,
    view: &mut V2LocalView,
) -> LinkResult<()> {
    if head.view_kind != "scoped" {
        return Ok(());
    }
    let expected = scoped_projection_sha256(&head.brain_id);
    if baseline
        .and_then(|state| state.projection_sha256.as_deref())
        .is_some_and(|pinned| pinned != expected)
    {
        return Err(LinkError::ScopedViewChanged);
    }
    if view.riding.get("DB.md").map(|(sha256, _)| sha256.as_str()) != Some(expected.as_str()) {
        return Err(LinkError::ScopedProjectionModified);
    }
    view.riding.remove("DB.md");
    view.eligibility.remove("DB.md");
    Ok(())
}

fn files_for_v2_view(
    head: &V2VerifiedHead,
    mut files: std::collections::BTreeMap<String, V2BaselineFile>,
) -> std::collections::BTreeMap<String, V2BaselineFile> {
    if head.view_kind == "scoped" {
        // Even when a grant explicitly includes canonical DB.md, a partial
        // checkout uses a generated marker. Canonical validation remains a
        // server-side operation over the complete brain.
        files.remove("DB.md");
    }
    files
}

fn parse_v2_baseline(cfg: &HubConfig, brain: &str, bytes: &[u8]) -> LinkResult<V2SyncBaseline> {
    let baseline: V2SyncBaseline =
        serde_json::from_slice(bytes).map_err(|_| invalid_feed("v2 sync baseline is corrupt"))?;
    if baseline.v != 2
        || baseline.origin != normalized_origin(&cfg.hub)?
        || baseline.brain != brain
        || baseline
            .commit_hash
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || baseline
            .content_root
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || baseline
            .asset_root
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || baseline
            .local_policy_digest
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || baseline
            .view_kind
            .as_deref()
            .is_some_and(|kind| !matches!(kind, "full" | "scoped"))
        || baseline
            .view_revision
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || baseline
            .projection_sha256
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || (baseline.view_kind.as_deref() == Some("scoped")
            && (baseline.view_revision.is_none() || baseline.projection_sha256.is_none()))
        || baseline.files.len() > MAX_PUSH_FILES
        || baseline.assets.len() > MAX_PUSH_FILES
        || baseline.local_eligibility.len() > MAX_PUSH_FILES
        || baseline.remote_copy_remains.len() > MAX_PUSH_FILES
        || baseline.files.iter().any(|(path, file)| {
            crate::linkmd_v2::normalize_path(path).is_err()
                || !is_sha256(&file.sha256)
                || file.bytes > MAX_STORE_BYTES
        })
        || baseline.assets.iter().any(|(path, asset)| {
            crate::linkmd_v2::normalize_path(path).is_err()
                || !is_sha256(&asset.blob_sha256)
                || !is_sha256(&asset.leaf_hash)
                || asset.bytes > MAX_STORE_BYTES
                || !matches!(asset.disposition.as_str(), "hosted" | "withheld")
                || asset.wrappers.is_empty()
                || asset
                    .wrappers
                    .iter()
                    .any(|wrapper| crate::linkmd_v2::normalize_path(wrapper).is_err())
        })
        || baseline
            .local_eligibility
            .keys()
            .chain(baseline.remote_copy_remains.keys())
            .any(|path| crate::linkmd_v2::normalize_path(path).is_err())
        || baseline
            .remote_copy_remains
            .values()
            .any(|hash| !is_sha256(hash))
    {
        return Err(invalid_feed("v2 sync baseline failed validation"));
    }
    Ok(baseline)
}

#[cfg(unix)]
fn load_v2_baseline(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
) -> LinkResult<Option<V2SyncBaseline>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let directory = open_trust_dir(cfg)?;
    let name_string = v2_baseline_name(cfg, brain, checkout)?;
    let _lock = lock_trust_name(&directory, &name_string)?;
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
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(LinkError::UnsafePath { path: name_string })
        };
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.take(MAX_FEED_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FEED_RESPONSE_BYTES {
        return Err(invalid_feed("v2 sync baseline is oversized"));
    }
    Ok(Some(parse_v2_baseline(cfg, brain, &bytes)?))
}

#[cfg(windows)]
fn load_v2_baseline(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
) -> LinkResult<Option<V2SyncBaseline>> {
    let directory = open_trust_dir(cfg)?;
    let name = v2_baseline_name(cfg, brain, checkout)?;
    let _lock = lock_trust_name(&directory, &name)?;
    let mut reader = crate::fsx::BoundedDirReader::from_root(&directory)?;
    match reader.read(Path::new(&name), MAX_FEED_RESPONSE_BYTES) {
        Ok(bytes) => Ok(Some(parse_v2_baseline(cfg, brain, &bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(LinkError::UnsafePath { path: name }),
    }
}

#[cfg(not(any(unix, windows)))]
fn load_v2_baseline(
    _cfg: &HubConfig,
    _brain: &str,
    _checkout: &Path,
) -> LinkResult<Option<V2SyncBaseline>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md v2 baseline",
    })
}

#[cfg(unix)]
fn save_v2_baseline(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
    baseline: &V2SyncBaseline,
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let directory = open_trust_dir(cfg)?;
    let name_string = v2_baseline_name(cfg, brain, checkout)?;
    let _lock = lock_trust_name(&directory, &name_string)?;
    let name = c_name(name_string.as_bytes(), &name_string)?;
    let mut bytes = serde_json::to_vec(baseline)
        .map_err(|_| invalid_feed("could not serialize v2 sync baseline"))?;
    bytes.push(b'\n');
    let temp_string = format!(
        ".{name_string}.tmp.{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
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

#[cfg(windows)]
fn save_v2_baseline(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
    baseline: &V2SyncBaseline,
) -> LinkResult<()> {
    let directory = open_trust_dir(cfg)?;
    let name = v2_baseline_name(cfg, brain, checkout)?;
    let _lock = lock_trust_name(&directory, &name)?;
    let mut bytes = serde_json::to_vec(baseline)
        .map_err(|_| invalid_feed("could not serialize v2 sync baseline"))?;
    bytes.push(b'\n');
    crate::fsx::write_atomic_beneath(&directory, Path::new(&name), &bytes, false, true)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn save_v2_baseline(
    _cfg: &HubConfig,
    _brain: &str,
    _checkout: &Path,
    _baseline: &V2SyncBaseline,
) -> LinkResult<()> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md v2 baseline",
    })
}

fn v2_baseline_from_head(
    cfg: &HubConfig,
    head: &V2VerifiedHead,
    files: std::collections::BTreeMap<String, V2BaselineFile>,
    assets: std::collections::BTreeMap<String, V2BaselineAsset>,
    local: Option<&V2LocalView>,
) -> LinkResult<V2SyncBaseline> {
    let mut local_eligibility = local
        .map(|view| view.eligibility.clone())
        .unwrap_or_default();
    if let Some(view) = local {
        for path in files.keys() {
            local_eligibility
                .entry(path.clone())
                .or_insert_with(|| !view.policy.keeps_home(path));
        }
    }
    let remote_copy_remains = local_eligibility
        .iter()
        .filter(|(_, riding)| !**riding)
        .filter_map(|(path, _)| {
            files
                .get(path)
                .map(|file| (path.clone(), file.sha256.clone()))
        })
        .collect();
    Ok(V2SyncBaseline {
        v: 2,
        origin: normalized_origin(&cfg.hub)?,
        brain: head.brain_id.clone(),
        head_seq: Some(head.pointer.as_ref().map_or(0, |pointer| pointer.seq)),
        commit_hash: head
            .pointer
            .as_ref()
            .map(|pointer| pointer.commit_hash.clone()),
        content_root: head
            .pointer
            .as_ref()
            .and_then(|pointer| pointer.content_root.clone()),
        asset_root: head
            .pointer
            .as_ref()
            .and_then(|pointer| pointer.asset_root.clone()),
        assets,
        view_kind: Some(head.view_kind.clone()),
        view_revision: Some(head.view_revision.clone()),
        projection_sha256: (head.view_kind == "scoped")
            .then(|| scoped_projection_sha256(&head.brain_id)),
        files,
        local_policy_digest: local.map(|view| view.policy.digest.clone()),
        local_eligibility,
        remote_copy_remains,
    })
}

fn v2_local_files(store: &Store) -> LinkResult<V2LocalView> {
    let policy = crate::linkmd_sync_policy::load(store)
        .map_err(|message| LinkError::InvalidPack { message })?;
    let asset_paths = crate::assets::read_manifest(store)
        .map_err(|error| invalid_feed(format!("local asset manifest is invalid: {error}")))?
        .into_iter()
        .map(|asset| asset.path)
        .collect::<std::collections::BTreeSet<_>>();
    let mut result = std::collections::BTreeMap::new();
    let mut eligibility = std::collections::BTreeMap::new();
    let mut total = 0_u64;
    let mut paths = vec![PathBuf::from("DB.md")];
    paths.extend(store.walk()?);
    for relative in paths {
        let path = relative.to_string_lossy().replace('\\', "/");
        // v2 catalogs and asset inventory are materialized/signed separately.
        if matches!(path.as_str(), "assets.jsonl" | "index.md" | "index.jsonl") {
            continue;
        }
        if asset_paths.contains(&path) {
            continue;
        }
        crate::linkmd_v2::normalize_path(&path).map_err(|error| LinkError::UnsafePath {
            path: error.to_string(),
        })?;
        let riding = !policy.keeps_home(&path);
        eligibility.insert(path.clone(), riding);
        if !riding {
            continue;
        }
        let remaining = MAX_STORE_BYTES.saturating_sub(total);
        let bytes = store.read_bounded(&relative, remaining)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| LinkError::PushTooLarge {
                detail: "v2 local byte count overflow".to_string(),
            })?;
        if total > MAX_STORE_BYTES {
            return Err(LinkError::PushTooLarge {
                detail: format!("{total} uncompressed bytes"),
            });
        }
        if std::str::from_utf8(&bytes).is_err() {
            return Err(LinkError::NotUtf8 { path });
        }
        result.insert(path, (content_sha256(&bytes), bytes.len() as u64));
    }
    Ok(V2LocalView {
        riding: result,
        eligibility,
        policy,
    })
}

#[derive(Debug, Deserialize)]
struct V2DownloadItem {
    path: String,
    sha256: String,
    bytes: u64,
    url: String,
    method: String,
}

#[derive(Debug, Deserialize)]
struct V2DownloadWindow {
    v: u8,
    commit: String,
    downloads: Vec<V2DownloadItem>,
}

#[derive(Debug, Deserialize)]
struct V2BulkStreamHeader {
    v: u8,
    path: String,
    sha256: String,
    bytes: u64,
}

fn parse_v2_bulk_stream(
    bytes: &[u8],
    expected: &[(&String, &V2BaselineFile)],
) -> LinkResult<Vec<(String, Vec<u8>)>> {
    if !bytes.starts_with(V2_BULK_STREAM_MAGIC) {
        return Err(invalid_feed("v2 bulk stream has an invalid magic"));
    }
    let mut cursor = V2_BULK_STREAM_MAGIC.len();
    let mut result = Vec::with_capacity(expected.len());
    for (expected_path, expected_file) in expected {
        let length_bytes = bytes
            .get(cursor..cursor + 4)
            .ok_or_else(|| invalid_feed("v2 bulk stream ended before a frame header"))?;
        cursor += 4;
        let header_len = u32::from_be_bytes(length_bytes.try_into().expect("four bytes")) as usize;
        if header_len == 0 || header_len > 4 * 1024 {
            return Err(invalid_feed("v2 bulk stream has an invalid frame length"));
        }
        let header_bytes = bytes
            .get(cursor..cursor + header_len)
            .ok_or_else(|| invalid_feed("v2 bulk stream ended inside a frame header"))?;
        cursor += header_len;
        let header: V2BulkStreamHeader = serde_json::from_slice(header_bytes)
            .map_err(|_| invalid_feed("v2 bulk stream has an invalid frame header"))?;
        if header.v != 2
            || &header.path != *expected_path
            || header.sha256 != expected_file.sha256
            || header.bytes != expected_file.bytes
            || header.bytes > V2_BULK_STREAM_CONTENT_BYTES
        {
            return Err(invalid_feed(
                "v2 bulk stream frame differs from its proven manifest entry",
            ));
        }
        let body_len = usize::try_from(header.bytes)
            .map_err(|_| invalid_feed("v2 bulk stream frame length overflows this platform"))?;
        let body = bytes
            .get(cursor..cursor + body_len)
            .ok_or_else(|| invalid_feed("v2 bulk stream ended inside file bytes"))?;
        cursor += body_len;
        if content_sha256(body) != header.sha256 {
            return Err(invalid_feed(
                "v2 bulk stream file differs from its proven manifest entry",
            ));
        }
        result.push((header.path, body.to_vec()));
    }
    if bytes.get(cursor..cursor + 4) != Some(&[0, 0, 0, 0]) {
        return Err(invalid_feed("v2 bulk stream has no exact end marker"));
    }
    cursor += 4;
    if cursor != bytes.len() {
        return Err(invalid_feed("v2 bulk stream carries trailing data"));
    }
    Ok(result)
}

fn download_v2_bulk_stream(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    pending: &[(&String, &V2BaselineFile)],
) -> LinkResult<Vec<(String, Vec<u8>)>> {
    let claims = pending
        .iter()
        .map(|(path, file)| {
            Ok(json!({
                "path": path,
                "sha256": file.sha256,
                "bytes": file.bytes,
                "proof": file.proof.as_ref().ok_or_else(|| {
                    invalid_feed("v2 manifest omitted a bulk-stream proof")
                })?,
            }))
        })
        .collect::<LinkResult<Vec<_>>>()?;
    let raw = request_raw(
        cfg,
        "POST",
        &format!("/api/hub/brains/{brain}/v2/stream"),
        Some(&json!({
            "commit": pointer.commit_hash,
            "files": claims,
        })),
        Auth::Required,
        V2_BULK_STREAM_RESPONSE_BYTES,
    )?;
    let body = ensure_raw_ok(raw, "download v2 bulk stream")?;
    parse_v2_bulk_stream(&body, pending)
}

fn prepare_v2_downloads(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    pending: &[(&String, &V2BaselineFile)],
) -> LinkResult<Vec<V2DownloadItem>> {
    let mut result = Vec::with_capacity(pending.len());
    for chunk in pending.chunks(128) {
        let claims = chunk
            .iter()
            .map(|(path, file)| {
                Ok(json!({
                    "path": path,
                    "sha256": file.sha256,
                    "bytes": file.bytes,
                    "proof": file.proof.as_ref().ok_or_else(|| {
                        invalid_feed("v2 manifest omitted a download proof")
                    })?,
                }))
            })
            .collect::<LinkResult<Vec<_>>>()?;
        let value = ensure_ok(
            request_capped(
                cfg,
                "POST",
                &format!("/api/hub/brains/{brain}/v2/downloads"),
                Some(&json!({
                    "commit": pointer.commit_hash,
                    "files": claims,
                })),
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "prepare v2 blob downloads",
        )?;
        let window: V2DownloadWindow = serde_json::from_value(value)
            .map_err(|_| invalid_feed("v2 download window has an invalid shape"))?;
        if window.v != 2
            || window.commit != pointer.commit_hash
            || window.downloads.len() != chunk.len()
        {
            return Err(invalid_feed(
                "v2 download window is not bound to the requested files",
            ));
        }
        let mut by_path = window
            .downloads
            .into_iter()
            .map(|item| (item.path.clone(), item))
            .collect::<std::collections::BTreeMap<_, _>>();
        if by_path.len() != chunk.len() {
            return Err(invalid_feed("v2 download window repeats a path"));
        }
        for (path, file) in chunk {
            let item = by_path
                .remove(*path)
                .ok_or_else(|| invalid_feed("v2 download window omitted a path"))?;
            if item.method != "GET"
                || item.sha256 != file.sha256
                || item.bytes != file.bytes
                || item.url.is_empty()
            {
                return Err(invalid_feed(
                    "v2 download capability differs from its proven file",
                ));
            }
            result.push(item);
        }
    }
    Ok(result)
}

fn prepare_v2_asset_downloads(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    pending: &[(&String, &V2BaselineAsset)],
) -> LinkResult<Vec<V2DownloadItem>> {
    let mut result = Vec::with_capacity(pending.len());
    for chunk in pending.chunks(128) {
        let claims = chunk
            .iter()
            .map(|(path, asset)| {
                json!({
                    "path": path,
                    "sha256": asset.blob_sha256,
                    "bytes": asset.bytes,
                    "leaf_hash": asset.leaf_hash,
                })
            })
            .collect::<Vec<_>>();
        let value = ensure_ok(
            request_capped(
                cfg,
                "POST",
                &format!("/api/hub/brains/{brain}/v2/assets/downloads"),
                Some(&json!({
                    "commit": pointer.commit_hash,
                    "assets": claims,
                })),
                Auth::Required,
                MAX_FEED_RESPONSE_BYTES,
            )?,
            "prepare v2 asset downloads",
        )?;
        let window: V2DownloadWindow = serde_json::from_value(value)
            .map_err(|_| invalid_feed("v2 asset download window has an invalid shape"))?;
        if window.v != 2
            || window.commit != pointer.commit_hash
            || window.downloads.len() != chunk.len()
        {
            return Err(invalid_feed(
                "v2 asset download window is not bound to the requested assets",
            ));
        }
        let mut by_path = window
            .downloads
            .into_iter()
            .map(|item| (item.path.clone(), item))
            .collect::<std::collections::BTreeMap<_, _>>();
        if by_path.len() != chunk.len() {
            return Err(invalid_feed("v2 asset download window repeats a path"));
        }
        for (path, asset) in chunk {
            let item = by_path
                .remove(*path)
                .ok_or_else(|| invalid_feed("v2 asset download window omitted a path"))?;
            if item.method != "GET"
                || item.sha256 != asset.blob_sha256
                || item.bytes != asset.bytes
                || item.url.is_empty()
            {
                return Err(invalid_feed(
                    "v2 asset download capability differs from its signed leaf",
                ));
            }
            result.push(item);
        }
    }
    Ok(result)
}

fn download_v2_blob(cfg: &HubConfig, item: &V2DownloadItem) -> LinkResult<Vec<u8>> {
    let bytes = get_presigned(cfg, &item.url)?;
    if bytes.len() as u64 != item.bytes || content_sha256(&bytes) != item.sha256 {
        return Err(invalid_feed("v2 blob differs from its proven path entry"));
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
struct V2StagedFile {
    path: String,
    source: PathBuf,
    sha256: String,
    bytes: u64,
}

#[cfg(unix)]
fn v2_download_cache_dir(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
) -> LinkResult<PathBuf> {
    v2_download_cache_dir_for(cfg, brain, &pointer.commit_hash)
}

#[cfg(unix)]
fn v2_download_cache_dir_for(
    cfg: &HubConfig,
    brain: &str,
    transaction: &str,
) -> LinkResult<PathBuf> {
    if !crate::ulid::is_ulid(brain) || !is_sha256(transaction) {
        return Err(invalid_feed("v2 download cache address is invalid"));
    }
    let path = cfg
        .state_dir
        .join("downloads")
        .join(brain)
        .join(transaction);
    let directory = open_or_create_dir_nofollow(&path)?;
    use std::os::fd::AsRawFd as _;
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    directory.sync_all()?;
    Ok(path)
}

#[cfg(windows)]
fn v2_download_cache_dir(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
) -> LinkResult<PathBuf> {
    v2_download_cache_dir_for(cfg, brain, &pointer.commit_hash)
}

#[cfg(windows)]
fn v2_download_cache_dir_for(
    cfg: &HubConfig,
    brain: &str,
    transaction: &str,
) -> LinkResult<PathBuf> {
    if !crate::ulid::is_ulid(brain) || !is_sha256(transaction) {
        return Err(invalid_feed("v2 download cache address is invalid"));
    }
    let path = cfg
        .state_dir
        .join("downloads")
        .join(brain)
        .join(transaction);
    crate::fsx::write_atomic(&path.join(".directory"), b"v2 download cache\n")?;
    crate::fsx::open_directory_nofollow(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn cleanup_v2_download_cache(cfg: &HubConfig, brain: &str, transaction: &str) {
    use std::os::fd::AsRawFd as _;
    let parent = cfg.state_dir.join("downloads").join(brain);
    let Ok(directory) = open_existing_dir_nofollow(&parent) else {
        return;
    };
    let Ok(name) = c_name(transaction.as_bytes(), transaction) else {
        return;
    };
    let _ = remove_tree_at(directory.as_raw_fd(), &name, &parent.display().to_string());
    let _ = directory.sync_all();
}

#[cfg(windows)]
fn cleanup_v2_download_cache(cfg: &HubConfig, brain: &str, transaction: &str) {
    if !crate::ulid::is_ulid(brain) || !is_sha256(transaction) {
        return;
    }
    let parent = cfg.state_dir.join("downloads").join(brain);
    let Ok(root) = crate::fsx::open_directory_nofollow(&parent) else {
        return;
    };
    let _ = crate::fsx::remove_tree_beneath(&root, Path::new(transaction));
}

#[cfg(not(any(unix, windows)))]
fn cleanup_v2_download_cache(_cfg: &HubConfig, _brain: &str, _transaction: &str) {}

#[cfg(not(any(unix, windows)))]
fn v2_download_cache_dir_for(
    _cfg: &HubConfig,
    _brain: &str,
    _transaction: &str,
) -> LinkResult<PathBuf> {
    Err(LinkError::UnsupportedPlatform {
        operation: "resumable v2 download staging",
    })
}

#[cfg(any(unix, windows))]
fn cached_blob_is_exact(path: &Path, sha256: &str, bytes: u64) -> LinkResult<bool> {
    let file = match crate::fsx::open_regular_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() != bytes {
        return Ok(false);
    }
    Ok(content_sha256_reader(file)? == sha256)
}

#[cfg(any(unix, windows))]
fn cache_v2_blob_bytes(
    cache_dir: &Path,
    sha256: &str,
    expected_bytes: u64,
    bytes: &[u8],
) -> LinkResult<PathBuf> {
    if bytes.len() as u64 != expected_bytes || content_sha256(bytes) != sha256 {
        return Err(invalid_feed("v2 cached blob differs from its declaration"));
    }
    let path = cache_dir.join(sha256);
    if !cached_blob_is_exact(&path, sha256, expected_bytes)? {
        crate::fsx::write_atomic(&path, bytes)?;
    }
    Ok(path)
}

#[cfg(not(any(unix, windows)))]
fn cache_v2_blob_bytes(
    _cache_dir: &Path,
    _sha256: &str,
    _expected_bytes: u64,
    _bytes: &[u8],
) -> LinkResult<PathBuf> {
    Err(LinkError::UnsupportedPlatform {
        operation: "resumable v2 download staging",
    })
}

#[cfg(unix)]
fn download_presigned_to_cache(
    cfg: &HubConfig,
    url: &str,
    cache_dir: &Path,
    sha256: &str,
    expected_bytes: u64,
) -> LinkResult<PathBuf> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let target = cache_dir.join(sha256);
    if cached_blob_is_exact(&target, sha256, expected_bytes)? {
        return Ok(target);
    }
    let directory = open_existing_dir_nofollow(cache_dir)?;
    let mut nonce = [0_u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
        .map_err(|_| invalid_feed("could not mint a download cache name"))?;
    let temp_string = format!(".download-{}", URL_SAFE_NO_PAD.encode(nonce));
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
    let mut output = unsafe { std::fs::File::from_raw_fd(fd) };
    let response = match presigned_agent(cfg, url)?.get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => {
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(LinkError::Http {
                what: "v2 direct download",
                status: response.status(),
                message: "object store rejected the download".to_string(),
                code: None,
                details: None,
            });
        }
        Err(ureq::Error::Transport(error)) => {
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(LinkError::Transport {
                hub: cfg.hub.clone(),
                message: error.to_string(),
            });
        }
    };
    let mut reader = response
        .into_reader()
        .take(expected_bytes.saturating_add(1));
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    let write_result = (|| -> std::io::Result<()> {
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            digest.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(output);
    if total != expected_bytes || format!("{:x}", digest.finalize()) != sha256 {
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(invalid_feed(
            "v2 direct download failed integrity verification",
        ));
    }
    let target_name = c_name(sha256.as_bytes(), sha256)?;
    // Replace only a previously inspected cache leaf. Cache bytes are private
    // and content-addressed; final install re-verifies them again.
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            directory.as_raw_fd(),
            target_name.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    directory.sync_all()?;
    Ok(target)
}

#[cfg(windows)]
fn download_presigned_to_cache(
    cfg: &HubConfig,
    url: &str,
    cache_dir: &Path,
    sha256: &str,
    expected_bytes: u64,
) -> LinkResult<PathBuf> {
    use std::fs::OpenOptions;

    let target = cache_dir.join(sha256);
    if cached_blob_is_exact(&target, sha256, expected_bytes)? {
        return Ok(target);
    }
    // Keep the checked directory handle alive while the private cache path is
    // used. Windows opens omit FILE_SHARE_DELETE, so neither this directory nor
    // an ancestor can be swapped after validation.
    let _directory = crate::fsx::open_directory_nofollow(cache_dir)?;
    let temp = cache_dir.join(format!(".download-{}", crate::ulid::mint()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let response = match presigned_agent(cfg, url)?.get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => {
            let _ = std::fs::remove_file(&temp);
            return Err(LinkError::Http {
                what: "v2 direct download",
                status: response.status(),
                message: "object store rejected the download".to_string(),
                code: None,
                details: None,
            });
        }
        Err(ureq::Error::Transport(error)) => {
            let _ = std::fs::remove_file(&temp);
            return Err(LinkError::Transport {
                hub: cfg.hub.clone(),
                message: error.to_string(),
            });
        }
    };
    let mut reader = response
        .into_reader()
        .take(expected_bytes.saturating_add(1));
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    let copied = (|| -> std::io::Result<()> {
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            digest.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.sync_all()
    })();
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    drop(output);
    if total != expected_bytes || format!("{:x}", digest.finalize()) != sha256 {
        let _ = std::fs::remove_file(&temp);
        return Err(invalid_feed(
            "v2 direct download failed integrity verification",
        ));
    }
    if target.exists() {
        std::fs::remove_file(&target)?;
    }
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(target)
}

#[cfg(not(any(unix, windows)))]
fn download_presigned_to_cache(
    _cfg: &HubConfig,
    _url: &str,
    _cache_dir: &Path,
    _sha256: &str,
    _expected_bytes: u64,
) -> LinkResult<PathBuf> {
    Err(LinkError::UnsupportedPlatform {
        operation: "resumable v2 download staging",
    })
}

fn download_v2_blobs(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    pending: Vec<(&String, &V2BaselineFile)>,
) -> LinkResult<Vec<(String, Vec<u8>)>> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let expected_order = pending
        .iter()
        .map(|(path, _)| (*path).clone())
        .collect::<Vec<_>>();
    let mut streamed = std::collections::BTreeMap::new();
    let mut direct = Vec::new();
    let mut window = Vec::new();
    let mut window_bytes = 0_u64;
    let flush = |window: &mut Vec<(&String, &V2BaselineFile)>,
                 window_bytes: &mut u64,
                 streamed: &mut std::collections::BTreeMap<String, Vec<u8>>|
     -> LinkResult<()> {
        if window.is_empty() {
            return Ok(());
        }
        for (path, bytes) in download_v2_bulk_stream(cfg, brain, pointer, window)? {
            if streamed.insert(path, bytes).is_some() {
                return Err(invalid_feed("v2 bulk streams repeated a path"));
            }
        }
        window.clear();
        *window_bytes = 0;
        Ok(())
    };
    for &(path, file) in &pending {
        if file.bytes > V2_BULK_STREAM_CONTENT_BYTES {
            flush(&mut window, &mut window_bytes, &mut streamed)?;
            direct.push((path, file));
            continue;
        }
        if window.len() == V2_BULK_STREAM_FILES
            || window_bytes.saturating_add(file.bytes) > V2_BULK_STREAM_CONTENT_BYTES
        {
            flush(&mut window, &mut window_bytes, &mut streamed)?;
        }
        window.push((path, file));
        window_bytes += file.bytes;
    }
    flush(&mut window, &mut window_bytes, &mut streamed)?;

    let downloads = prepare_v2_downloads(cfg, brain, pointer, &direct)?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker_count = downloads.len().min(V2_BLOB_DOWNLOAD_WORKERS);
    let mut results = std::iter::repeat_with(|| None)
        .take(downloads.len())
        .collect::<Vec<Option<LinkResult<(String, Vec<u8>)>>>>();
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        for _ in 0..worker_count {
            let sender = sender.clone();
            let downloads = &downloads;
            let next = &next;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(item) = downloads.get(index) else {
                    break;
                };
                let result = download_v2_blob(cfg, item).map(|bytes| (item.path.clone(), bytes));
                if sender.send((index, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        for (index, result) in receiver {
            results[index] = Some(result);
        }
    });
    for result in results.into_iter().map(|result| {
        result.ok_or_else(|| LinkError::Transport {
            hub: cfg.hub.clone(),
            message: "a bounded v2 blob worker stopped before reporting its result".to_string(),
        })?
    }) {
        let (path, bytes) = result?;
        if streamed.insert(path, bytes).is_some() {
            return Err(invalid_feed("v2 download lanes repeated a path"));
        }
    }
    expected_order
        .into_iter()
        .map(|path| {
            streamed
                .remove(&path)
                .map(|bytes| (path, bytes))
                .ok_or_else(|| invalid_feed("v2 download lanes omitted a proven path"))
        })
        .collect()
}

/// Resume an exact-head materialization into the private global cache. Each
/// verified frame is durable before the next network window begins, so a
/// retry skips it and memory never grows with total changed bytes.
#[cfg(any(unix, windows))]
fn stage_v2_blobs(
    cfg: &HubConfig,
    brain: &str,
    pointer: &V2PointerBody,
    pending: Vec<(&String, &V2BaselineFile)>,
) -> LinkResult<Vec<V2StagedFile>> {
    let cache_dir = v2_download_cache_dir(cfg, brain, pointer)?;
    let mut staged = std::collections::BTreeMap::<String, V2StagedFile>::new();
    let mut direct = Vec::new();
    let mut window = Vec::new();
    let mut window_bytes = 0_u64;
    let flush = |window: &mut Vec<(&String, &V2BaselineFile)>,
                 window_bytes: &mut u64,
                 staged: &mut std::collections::BTreeMap<String, V2StagedFile>|
     -> LinkResult<()> {
        if window.is_empty() {
            return Ok(());
        }
        let missing = window
            .iter()
            .filter_map(|(path, file)| {
                let target = cache_dir.join(&file.sha256);
                match cached_blob_is_exact(&target, &file.sha256, file.bytes) {
                    Ok(true) => {
                        staged.insert(
                            (*path).clone(),
                            V2StagedFile {
                                path: (*path).clone(),
                                source: target,
                                sha256: file.sha256.clone(),
                                bytes: file.bytes,
                            },
                        );
                        None
                    }
                    Ok(false) => Some(Ok((*path, *file))),
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<LinkResult<Vec<_>>>()?;
        if !missing.is_empty() {
            for (path, bytes) in download_v2_bulk_stream(cfg, brain, pointer, &missing)? {
                let file = missing
                    .iter()
                    .find_map(|(expected_path, file)| (*expected_path == &path).then_some(*file))
                    .ok_or_else(|| invalid_feed("v2 stream returned an unrequested cache path"))?;
                let source = cache_v2_blob_bytes(&cache_dir, &file.sha256, file.bytes, &bytes)?;
                staged.insert(
                    path.clone(),
                    V2StagedFile {
                        path,
                        source,
                        sha256: file.sha256.clone(),
                        bytes: file.bytes,
                    },
                );
            }
        }
        window.clear();
        *window_bytes = 0;
        Ok(())
    };
    for &(path, file) in &pending {
        if file.bytes > V2_BULK_STREAM_CONTENT_BYTES {
            flush(&mut window, &mut window_bytes, &mut staged)?;
            direct.push((path, file));
            continue;
        }
        if window.len() == V2_BULK_STREAM_FILES
            || window_bytes.saturating_add(file.bytes) > V2_BULK_STREAM_CONTENT_BYTES
        {
            flush(&mut window, &mut window_bytes, &mut staged)?;
        }
        window.push((path, file));
        window_bytes += file.bytes;
    }
    flush(&mut window, &mut window_bytes, &mut staged)?;
    for item in prepare_v2_downloads(cfg, brain, pointer, &direct)? {
        let source =
            download_presigned_to_cache(cfg, &item.url, &cache_dir, &item.sha256, item.bytes)?;
        staged.insert(
            item.path.clone(),
            V2StagedFile {
                path: item.path,
                source,
                sha256: item.sha256,
                bytes: item.bytes,
            },
        );
    }
    pending
        .into_iter()
        .map(|(path, _)| {
            staged
                .remove(path)
                .ok_or_else(|| invalid_feed("v2 download cache omitted a proven path"))
        })
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn stage_v2_blobs(
    _cfg: &HubConfig,
    _brain: &str,
    _pointer: &V2PointerBody,
    _pending: Vec<(&String, &V2BaselineFile)>,
) -> LinkResult<Vec<V2StagedFile>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "resumable v2 download staging",
    })
}

const V2_CONFLICT_BUNDLE_MAX: usize = 32;
const V2_CONFLICT_BUNDLE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const V2_CONFLICT_REMOTE_BYTES_MAX: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2ConflictCoordinate {
    sha256: Option<String>,
    bytes: Option<u64>,
    file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2ConflictFile {
    path: String,
    base: V2ConflictCoordinate,
    local: V2ConflictCoordinate,
    remote: V2ConflictCoordinate,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct V2ConflictPlan {
    v: u8,
    class: String,
    bundle: String,
    brain: String,
    origin: String,
    created_unix: u64,
    expires_unix: u64,
    base_seq: Option<u64>,
    base_commit: Option<String>,
    remote_seq: u64,
    remote_commit: Option<String>,
    remote_content_root: Option<String>,
    view_kind: String,
    view_revision: String,
    files: Vec<V2ConflictFile>,
}

fn v2_conflict_relative(bundle: &str, suffix: &str) -> PathBuf {
    PathBuf::from(".dbmd")
        .join("conflicts")
        .join(bundle)
        .join(suffix)
}

fn read_historical_conflict_blob(
    cfg: &HubConfig,
    brain: &str,
    baseline: &V2SyncBaseline,
    path: &str,
    file: &V2BaselineFile,
) -> LinkResult<Option<Vec<u8>>> {
    let (Some(seq), Some(commit)) = (baseline.head_seq, baseline.commit_hash.as_deref()) else {
        return Ok(None);
    };
    if seq == 0 {
        return Ok(None);
    }
    let encoded_path: String = url::form_urlencoded::byte_serialize(path.as_bytes()).collect();
    let endpoint = format!(
        "/api/hub/brains/{brain}/v2/history/blob?seq={seq}&commit={commit}&path={encoded_path}&sha256={}",
        file.sha256
    );
    let raw = request_raw(cfg, "GET", &endpoint, None, Auth::Required, file.bytes)?;
    if raw.status == 404 || raw.status == 403 {
        return Ok(None);
    }
    let bytes = ensure_raw_ok(raw, "v2 conflict base")?;
    if bytes.len() as u64 != file.bytes || content_sha256(&bytes) != file.sha256 {
        return Err(invalid_feed(
            "v2 conflict base failed integrity verification",
        ));
    }
    Ok(Some(bytes))
}

/// Persist a readable content conflict without modifying either side. The
/// plan is written last, so an interrupted bundle build is never actionable.
fn create_v2_conflict_bundle(
    cfg: &HubConfig,
    store: &Store,
    head: &V2VerifiedHead,
    baseline: Option<&V2SyncBaseline>,
    local: &std::collections::BTreeMap<String, (String, u64)>,
    remote: &std::collections::BTreeMap<String, V2BaselineFile>,
    paths: &[String],
) -> LinkResult<(String, Vec<String>)> {
    let conflicts_root = Path::new(".dbmd/conflicts");
    store.create_dir_all(conflicts_root)?;
    let completed = store
        .directory_names(conflicts_root)?
        .into_iter()
        .filter(|name| name.to_str().is_some_and(crate::ulid::is_ulid))
        .count();
    if completed >= V2_CONFLICT_BUNDLE_MAX {
        return Err(LinkError::InvalidPack {
            message: format!(
                "private conflict cache has {completed} bundles; resolve or prune one before syncing"
            ),
        });
    }

    // A conflict bundle is diagnostic state, not a second whole-brain cache.
    // Materialize a deterministic prefix whose remote artifacts fit one
    // bounded window. Resolving it exposes the next prefix on the next sync.
    let mut selected_paths = Vec::new();
    let mut selected_remote_bytes = 0_u64;
    for path in paths {
        let bytes = remote.get(path).map_or(0, |file| file.bytes);
        if !selected_paths.is_empty()
            && selected_remote_bytes.saturating_add(bytes) > V2_CONFLICT_REMOTE_BYTES_MAX
        {
            break;
        }
        selected_remote_bytes = selected_remote_bytes.saturating_add(bytes);
        selected_paths.push(path.clone());
        if selected_remote_bytes >= V2_CONFLICT_REMOTE_BYTES_MAX {
            break;
        }
    }
    if selected_paths.is_empty() {
        return Err(invalid_feed("content conflict set is empty"));
    }
    let bundle = crate::ulid::mint();
    let bundle_root = v2_conflict_relative(&bundle, "");
    store.create_dir_all(&bundle_root.join("files"))?;
    let pointer = head.pointer.as_ref();
    let remote_bytes = match pointer {
        Some(pointer) => download_v2_blobs(
            cfg,
            &head.brain_id,
            pointer,
            selected_paths
                .iter()
                .filter_map(|path| {
                    remote
                        .get(path)
                        .filter(|file| file.bytes <= V2_CONFLICT_REMOTE_BYTES_MAX)
                        .map(|file| (path, file))
                })
                .collect(),
        )?
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>(),
        None => std::collections::BTreeMap::new(),
    };

    let mut files = Vec::with_capacity(selected_paths.len());
    for (index, path) in selected_paths.iter().enumerate() {
        let base_file = baseline.and_then(|state| state.files.get(path));
        let base_bytes = match (baseline, base_file) {
            (Some(state), Some(file)) => {
                read_historical_conflict_blob(cfg, &head.brain_id, state, path, file)?
            }
            _ => None,
        };
        let local_file = local.get(path);
        let remote_file = remote.get(path);
        let remote_content = remote_bytes.get(path);
        let prefix = format!("files/{index:04}");
        let base_name = base_bytes.as_ref().map(|_| format!("{prefix}.base"));
        let local_name = local_file.as_ref().map(|_| format!("{prefix}.local"));
        let remote_name = remote_content.as_ref().map(|_| format!("{prefix}.remote"));
        if let (Some(name), Some(bytes)) = (&base_name, &base_bytes) {
            store.write_atomic_new(&v2_conflict_relative(&bundle, name), bytes)?;
        }
        if let (Some(name), Some((expected_hash, expected_bytes))) = (&local_name, local_file) {
            let bytes = store.read_bounded(Path::new(path), *expected_bytes)?;
            if bytes.len() as u64 != *expected_bytes || content_sha256(&bytes) != *expected_hash {
                return Err(LinkError::InvalidPack {
                    message: format!("local conflict path `{path}` changed while bundling"),
                });
            }
            store.write_atomic_new(&v2_conflict_relative(&bundle, name), &bytes)?;
        }
        if let (Some(name), Some(bytes)) = (&remote_name, remote_content) {
            store.write_atomic_new(&v2_conflict_relative(&bundle, name), bytes)?;
        }
        files.push(V2ConflictFile {
            path: path.clone(),
            base: V2ConflictCoordinate {
                sha256: base_file.map(|file| file.sha256.clone()),
                bytes: base_file.map(|file| file.bytes),
                file: base_name,
            },
            local: V2ConflictCoordinate {
                sha256: local_file.map(|(sha256, _)| sha256.clone()),
                bytes: local_file.map(|(_, bytes)| *bytes),
                file: local_name,
            },
            remote: V2ConflictCoordinate {
                sha256: remote_file.map(|file| file.sha256.clone()),
                bytes: remote_file.map(|file| file.bytes),
                file: remote_name,
            },
        });
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let plan = V2ConflictPlan {
        v: 2,
        class: "content_resolution_required".to_string(),
        bundle: bundle.clone(),
        brain: head.brain_id.clone(),
        origin: normalized_origin(&cfg.hub)?,
        created_unix: now,
        expires_unix: now.saturating_add(V2_CONFLICT_BUNDLE_TTL_SECS),
        base_seq: baseline.and_then(|state| state.head_seq),
        base_commit: baseline.and_then(|state| state.commit_hash.clone()),
        remote_seq: pointer.map_or(0, |value| value.seq),
        remote_commit: pointer.map(|value| value.commit_hash.clone()),
        remote_content_root: pointer.and_then(|value| value.content_root.clone()),
        view_kind: head.view_kind.clone(),
        view_revision: head.view_revision.clone(),
        files,
    };
    let mut bytes = serde_json::to_vec_pretty(&plan)
        .map_err(|_| invalid_feed("could not serialize v2 conflict plan"))?;
    bytes.push(b'\n');
    store.write_atomic_new(&v2_conflict_relative(&bundle, "plan.json"), &bytes)?;
    Ok((bundle, selected_paths))
}

fn v2_sync_pull(
    cfg: &HubConfig,
    requested_brain: &str,
    head: V2VerifiedHead,
    out: Option<&Path>,
) -> LinkResult<PullReport> {
    let dest = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(requested_brain));
    let _operation_lock = lock_v2_sync_operation(cfg, &head.brain_id)?;
    recover_windows_v2_pull(cfg, &head.brain_id, &dest)?;
    let head = v2_verified_head(cfg, requested_brain)?
        .ok_or_else(|| invalid_feed("v2 head disappeared while waiting for the checkout lock"))?;
    let remote = files_for_v2_view(
        &head,
        v2_manifest(cfg, &head.brain_id, head.pointer.as_ref())?,
    );
    let remote_assets = v2_asset_manifest(cfg, &head.brain_id, head.pointer.as_ref())?;
    let baseline = load_v2_baseline(cfg, &head.brain_id, &dest)?;
    ensure_v2_view_compatible(&head, baseline.as_ref())?;
    let local_store = Store::open_strict(&dest).ok();
    let mut local_view = local_store.as_ref().map(v2_local_files).transpose()?;
    if head.view_kind == "scoped" && baseline.is_none() && local_view.is_some() {
        return Err(LinkError::ScopedViewChanged);
    }
    if let Some(view) = local_view.as_mut() {
        remove_scoped_projection(&head, baseline.as_ref(), view)?;
    }
    let local = local_view
        .as_ref()
        .map(|view| &view.riding)
        .cloned()
        .unwrap_or_default();
    let kept_home = |path: &str| {
        local_view
            .as_ref()
            .is_some_and(|view| view.policy.keeps_home(path))
    };
    let base = baseline
        .as_ref()
        .map(|state| &state.files)
        .cloned()
        .unwrap_or_default();
    let base_assets = baseline
        .as_ref()
        .map(|state| state.assets.clone())
        .unwrap_or_default();
    let local_assets = local_store
        .as_ref()
        .map(|store| {
            crate::assets::read_manifest(store)
                .map_err(|error| invalid_feed(format!("local asset manifest is invalid: {error}")))
        })
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|asset| (asset.path.clone(), asset))
        .collect::<std::collections::BTreeMap<_, _>>();
    let all_paths = base
        .keys()
        .chain(remote.keys())
        .chain(local.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut conflicts = Vec::new();
    for path in &all_paths {
        if kept_home(path) {
            continue;
        }
        let base_hash = base.get(path).map(|file| file.sha256.as_str());
        let remote_hash = remote.get(path).map(|file| file.sha256.as_str());
        let local_hash = local.get(path).map(|file| file.0.as_str());
        if local_hash != base_hash && remote_hash != base_hash && local_hash != remote_hash {
            conflicts.push(path.clone());
        }
    }
    if !conflicts.is_empty() {
        conflicts.truncate(100);
        if let Some(store) = local_store.as_ref() {
            let (bundle, paths) = create_v2_conflict_bundle(
                cfg,
                store,
                &head,
                baseline.as_ref(),
                &local,
                &remote,
                &conflicts,
            )?;
            return Err(LinkError::ConflictBundle { bundle, paths });
        }
        return Err(LinkError::Conflict { paths: conflicts });
    }
    let asset_paths = base_assets
        .keys()
        .chain(remote_assets.keys())
        .chain(local_assets.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for path in &asset_paths {
        let base_record = base_assets
            .get(path)
            .map(|asset| v2_asset_record(asset, path));
        let remote_record = remote_assets
            .get(path)
            .map(|asset| v2_asset_record(asset, path));
        let local_record = local_assets.get(path).cloned();
        if local_record != base_record
            && remote_record != base_record
            && local_record != remote_record
        {
            conflicts.push(path.clone());
        }
    }
    if !conflicts.is_empty() {
        conflicts.truncate(100);
        return Err(LinkError::Conflict { paths: conflicts });
    }
    let pointer = head.pointer.as_ref();
    let cache_transaction = pointer.map_or_else(
        || content_sha256(format!("empty\0{}", head.view_revision).as_bytes()),
        |value| value.commit_hash.clone(),
    );
    let cache_dir = v2_download_cache_dir_for(cfg, &head.brain_id, &cache_transaction)?;
    let mut changed = match pointer {
        Some(pointer) => stage_v2_blobs(
            cfg,
            &head.brain_id,
            pointer,
            remote
                .iter()
                .filter(|(path, file)| {
                    !kept_home(path)
                        && local.get(*path).map(|value| value.0.as_str())
                            != Some(file.sha256.as_str())
                })
                .collect(),
        )?,
        None => Vec::new(),
    };
    let mut deleted = base
        .iter()
        .filter(|(path, file)| {
            !remote.contains_key(*path)
                && !kept_home(path)
                && local.get(*path).map(|value| value.0.as_str()) == Some(file.sha256.as_str())
        })
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    if local_assets
        != remote_assets
            .iter()
            .map(|(path, asset)| (path.clone(), v2_asset_record(asset, path)))
            .collect()
    {
        if remote_assets.is_empty() {
            deleted.push("assets.jsonl".to_string());
        } else {
            let bytes = v2_asset_manifest_bytes(&remote_assets)?;
            let sha256 = content_sha256(&bytes);
            let source = cache_v2_blob_bytes(&cache_dir, &sha256, bytes.len() as u64, &bytes)?;
            changed.push(V2StagedFile {
                path: "assets.jsonl".to_string(),
                source,
                sha256,
                bytes: bytes.len() as u64,
            });
        }
    }
    if let Some(pointer) = pointer {
        let mut pending_assets = Vec::new();
        for (path, asset) in &remote_assets {
            if asset.disposition != "hosted" || kept_home(path) {
                continue;
            }
            let already_current = local_store.as_ref().is_some_and(|store| {
                matches!(store.regular_file_exists(Path::new(path)), Ok(true))
                    && store
                        .read_bounded(Path::new(path), asset.bytes)
                        .ok()
                        .is_some_and(|bytes| {
                            bytes.len() as u64 == asset.bytes
                                && content_sha256(&bytes) == asset.blob_sha256
                        })
            });
            if !already_current {
                pending_assets.push((path, asset));
            }
        }
        for item in prepare_v2_asset_downloads(cfg, &head.brain_id, pointer, &pending_assets)? {
            let source =
                download_presigned_to_cache(cfg, &item.url, &cache_dir, &item.sha256, item.bytes)?;
            changed.push(V2StagedFile {
                path: item.path,
                source,
                sha256: item.sha256,
                bytes: item.bytes,
            });
        }
    }
    for (path, prior) in &base_assets {
        if remote_assets.contains_key(path) || kept_home(path) {
            continue;
        }
        let unchanged = local_store.as_ref().is_some_and(|store| {
            matches!(store.regular_file_exists(Path::new(path)), Ok(true))
                && store
                    .read_bounded(Path::new(path), prior.bytes)
                    .ok()
                    .is_some_and(|bytes| content_sha256(&bytes) == prior.blob_sha256)
        });
        if unchanged {
            deleted.push(path.clone());
        }
    }
    let extra_local = local
        .keys()
        .filter(|path| !remote.contains_key(*path) && !deleted.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    if head.view_kind == "scoped" {
        for (path, bytes) in [
            ("DB.md".to_string(), scoped_projection_bytes(&head.brain_id)),
            (
                ".dbmd/view.json".to_string(),
                scoped_view_metadata(&head, remote.len())?,
            ),
        ] {
            let sha256 = content_sha256(&bytes);
            let source = cache_v2_blob_bytes(&cache_dir, &sha256, bytes.len() as u64, &bytes)?;
            changed.push(V2StagedFile {
                path,
                source,
                sha256,
                bytes: bytes.len() as u64,
            });
        }
    }
    install_pulled_delta_sources(&dest, &changed, &deleted, true, baseline.as_ref(), &head)?;
    let finalized = (|| -> LinkResult<bool> {
        let installed_store =
            Store::open_strict(&dest).map_err(|error| LinkError::InvalidPack {
                message: format!("installed v2 checkout is not a valid db.md store: {error}"),
            })?;
        let mut installed_local = v2_local_files(&installed_store)?;
        remove_scoped_projection(&head, baseline.as_ref(), &mut installed_local)?;
        let mut expected_local = local.clone();
        for (path, file) in &remote {
            if !kept_home(path) {
                expected_local.insert(path.clone(), (file.sha256.clone(), file.bytes));
            }
        }
        for path in &deleted {
            expected_local.remove(path);
        }
        let local_dirty = installed_local.riding.iter().any(|(path, (hash, _))| {
            expected_local.get(path).map(|expected| &expected.0) != Some(hash)
        }) || expected_local.iter().any(|(path, (hash, _))| {
            installed_local.riding.get(path).map(|actual| &actual.0) != Some(hash)
        });
        let final_head = v2_verified_head(cfg, requested_brain)?
            .ok_or_else(|| invalid_feed("v2 head disappeared during pull"))?;
        if !same_v2_head(&head, &final_head) {
            return Err(LinkError::RemoteAdvancedDuringSync);
        }
        accept_v2_head(cfg, &final_head)?;
        save_v2_baseline(
            cfg,
            &head.brain_id,
            &dest,
            &v2_baseline_from_head(
                cfg,
                &head,
                remote.clone(),
                remote_assets.clone(),
                Some(&installed_local),
            )?,
        )?;
        complete_windows_v2_pull(&dest)?;
        Ok(local_dirty)
    })();
    let local_dirty = match finalized {
        Ok(value) => value,
        Err(error) => {
            if let Err(recovery) = recover_windows_v2_pull(cfg, &head.brain_id, &dest) {
                return Err(LinkError::InvalidPack {
                    message: format!("{error}; durable pull recovery also failed: {recovery}"),
                });
            }
            return Err(error);
        }
    };
    cleanup_v2_download_cache(cfg, &head.brain_id, &cache_transaction);
    Ok(PullReport {
        brain: head.brain_id,
        slug: requested_brain.to_string(),
        head_seq: pointer.map_or(0, |value| value.seq),
        files: remote.len() + remote_assets.len(),
        dest: dest.to_string_lossy().into_owned(),
        extra_local,
        sync_status: if local_dirty {
            "local_dirty_after_install".to_string()
        } else {
            "synced".to_string()
        },
    })
}

fn v2_expected(remote: Option<&V2BaselineFile>) -> Value {
    match remote {
        Some(file) => json!({ "kind": "blob", "hash": file.sha256 }),
        None => json!({ "kind": "absent" }),
    }
}

fn v2_asset_expected(remote: Option<&V2BaselineAsset>) -> Value {
    match remote {
        Some(asset) => json!({ "kind": "asset", "hash": asset.leaf_hash }),
        None => json!({ "kind": "absent" }),
    }
}

fn v2_asset_value(record: &crate::AssetRecord, disposition: &str) -> Value {
    json!({
        "blob_sha256": record.sha256,
        "bytes": record.bytes,
        "media_type": record.media_type,
        "wrappers": record.wrappers,
        "required": record.required,
        "disposition": disposition,
    })
}

fn v2_riding_matches_remote(
    local: &std::collections::BTreeMap<String, (String, u64)>,
    remote: &std::collections::BTreeMap<String, V2BaselineFile>,
    keeps_home: impl Fn(&str) -> bool,
) -> bool {
    remote.iter().all(|(path, file)| {
        keeps_home(path)
            || local.get(path).map(|value| value.0.as_str()) == Some(file.sha256.as_str())
    }) && local.iter().all(|(path, (hash, _))| {
        remote.get(path).map(|file| file.sha256.as_str()) == Some(hash.as_str())
    })
}

#[derive(Debug, Clone)]
struct V2ResolutionOverride {
    expected_remote: Option<String>,
    selected_local: Option<String>,
}

#[derive(Debug, Clone)]
struct V2UploadSource {
    path: String,
    bytes: u64,
}

fn verify_v2_upload_source(
    store: &Store,
    path: &str,
    sha256: &str,
    expected_bytes: u64,
) -> LinkResult<()> {
    let file = store.open_regular(Path::new(path))?;
    if file.metadata()?.len() != expected_bytes || content_sha256_reader(file)? != sha256 {
        return Err(LinkError::InvalidPack {
            message: format!("local path `{path}` changed during sync planning"),
        });
    }
    Ok(())
}

fn put_presigned_source(
    cfg: &HubConfig,
    raw: &str,
    headers: &Value,
    store: &Store,
    source: &V2UploadSource,
) -> LinkResult<()> {
    let http = presigned_agent(cfg, raw)?;
    let mut attempt = 0;
    let result = loop {
        let file = store.open_regular(Path::new(&source.path))?;
        if file.metadata()?.len() != source.bytes {
            return Err(LinkError::InvalidPack {
                message: format!("local path `{}` changed before upload", source.path),
            });
        }
        let mut req = http
            .put(raw)
            .set("Content-Length", &source.bytes.to_string());
        if let Some(map) = headers.as_object() {
            for (name, value) in map {
                if let Some(value) = value.as_str() {
                    req = req.set(name, value);
                }
            }
        }
        match req.send(file) {
            Err(ureq::Error::Transport(error))
                if is_pre_request_transport(error.kind()) && attempt + 1 < CONNECT_ATTEMPTS =>
            {
                std::thread::sleep(std::time::Duration::from_millis(
                    CONNECT_RETRY_BACKOFF_MS[attempt],
                ));
                attempt += 1;
            }
            result => break result,
        }
    };
    match result {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => Err(LinkError::Http {
            what: "v2 changed-byte upload",
            status: response.status(),
            message: "object store rejected the upload".to_string(),
            code: None,
            details: None,
        }),
        Err(error) => match error {
            ureq::Error::Status(412, _) => Ok(()),
            ureq::Error::Status(_, response) => Err(LinkError::Http {
                what: "v2 changed-byte upload",
                status: response.status(),
                message: "object store rejected the upload".to_string(),
                code: None,
                details: None,
            }),
            ureq::Error::Transport(error) => Err(LinkError::Transport {
                hub: "the object store".to_string(),
                message: error.to_string(),
            }),
        },
    }
}

fn v2_sync_push(
    cfg: &HubConfig,
    requested_brain: &str,
    store: &Store,
    head: V2VerifiedHead,
    resume_local_policy: bool,
    bulk_confirmation: Option<&V2BulkConfirmation>,
    resolution: Option<&std::collections::BTreeMap<String, V2ResolutionOverride>>,
) -> LinkResult<Value> {
    let _operation_lock = lock_v2_sync_operation(cfg, &head.brain_id)?;
    let head = v2_verified_head(cfg, requested_brain)?
        .ok_or_else(|| invalid_feed("v2 head disappeared while waiting for the checkout lock"))?;
    let remote = files_for_v2_view(
        &head,
        v2_manifest(cfg, &head.brain_id, head.pointer.as_ref())?,
    );
    let remote_assets = v2_asset_manifest(cfg, &head.brain_id, head.pointer.as_ref())?;
    let baseline = load_v2_baseline(cfg, &head.brain_id, &store.root)?;
    ensure_v2_view_compatible(&head, baseline.as_ref())?;
    if head.view_kind == "scoped" && baseline.is_none() {
        return Err(LinkError::ScopedViewChanged);
    }
    let mut local_view = v2_local_files(store)?;
    remove_scoped_projection(&head, baseline.as_ref(), &mut local_view)?;
    let local = &local_view.riding;
    let local_assets = crate::assets::read_manifest(store)
        .map_err(|error| invalid_feed(format!("local asset manifest is invalid: {error}")))?
        .into_iter()
        .map(|asset| (asset.path.clone(), asset))
        .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(previous) = baseline.as_ref() {
        if previous.local_policy_digest.as_deref() != Some(&local_view.policy.digest)
            && !resume_local_policy
        {
            let mut newly_eligible = previous
                .local_eligibility
                .iter()
                .filter(|(path, riding)| !**riding && !local_view.policy.keeps_home(path))
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            if !newly_eligible.is_empty() {
                newly_eligible.truncate(100);
                return Err(LinkError::LocalPolicyTransition {
                    paths: newly_eligible,
                });
            }
        }
    }
    let base = match baseline {
        Some(ref state) => state.files.clone(),
        None if remote.is_empty() => std::collections::BTreeMap::new(),
        None => {
            let mut conflicts = remote
                .iter()
                .filter(|(path, file)| {
                    local.get(*path).map(|value| value.0.as_str()) != Some(file.sha256.as_str())
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            if !conflicts.is_empty() {
                conflicts.truncate(100);
                let (bundle, paths) =
                    create_v2_conflict_bundle(cfg, store, &head, None, local, &remote, &conflicts)?;
                return Err(LinkError::ConflictBundle { bundle, paths });
            }
            remote.clone()
        }
    };
    let all_paths = base
        .keys()
        .chain(remote.keys())
        .chain(local.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut conflicts = Vec::new();
    let mut operations = Vec::new();
    let mut upload_sources = std::collections::BTreeMap::<String, V2UploadSource>::new();
    for path in all_paths {
        let base_hash = base.get(&path).map(|file| file.sha256.as_str());
        let remote_file = remote.get(&path);
        let remote_hash = remote_file.map(|file| file.sha256.as_str());
        let local_file = local.get(&path);
        let local_hash = local_file.map(|file| file.0.as_str());
        if local_hash == base_hash {
            continue;
        }
        if resolution.is_some_and(|allowed| !allowed.contains_key(&path)) {
            continue;
        }
        if local_view.policy.keeps_home(&path) {
            // Kept-home is a local transfer exclusion, never an implicit
            // delete of the company's already-hosted coordinate.
            continue;
        }
        if remote_hash != base_hash && local_hash != remote_hash {
            let explicitly_resolved = resolution
                .and_then(|allowed| allowed.get(&path))
                .is_some_and(|selected| {
                    selected.expected_remote.as_deref() == remote_hash
                        && selected.selected_local.as_deref() == local_hash
                });
            if !explicitly_resolved {
                conflicts.push(path);
                continue;
            }
        }
        match local_file {
            Some((sha256, byte_count)) => {
                verify_v2_upload_source(store, &path, sha256, *byte_count)?;
                operations.push(json!({
                    "op": "put",
                    "path": path,
                    "expected": v2_expected(remote_file),
                    "blob": sha256,
                    "bytes": byte_count,
                }));
                upload_sources
                    .entry(sha256.clone())
                    .or_insert_with(|| V2UploadSource {
                        path: path.clone(),
                        bytes: *byte_count,
                    });
            }
            None => {
                let Some(current) = remote_file else {
                    continue;
                };
                operations.push(json!({
                    "op": "delete",
                    "path": path,
                    "expected": { "kind": "blob", "hash": current.sha256 },
                }));
            }
        }
    }
    if !conflicts.is_empty() {
        conflicts.truncate(100);
        let (bundle, paths) = create_v2_conflict_bundle(
            cfg,
            store,
            &head,
            baseline.as_ref(),
            local,
            &remote,
            &conflicts,
        )?;
        return Err(LinkError::ConflictBundle { bundle, paths });
    }
    let base_assets = match baseline.as_ref() {
        Some(state) => state.assets.clone(),
        None if remote_assets.is_empty() => std::collections::BTreeMap::new(),
        None => {
            let mismatched = remote_assets.iter().any(|(path, remote)| {
                local_assets.get(path) != Some(&v2_asset_record(remote, path))
            }) || local_assets.len() != remote_assets.len();
            if mismatched {
                return Err(LinkError::Conflict {
                    paths: vec!["assets.jsonl".to_string()],
                });
            }
            remote_assets.clone()
        }
    };
    let asset_paths = base_assets
        .keys()
        .chain(remote_assets.keys())
        .chain(local_assets.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for path in asset_paths {
        let base_record = base_assets
            .get(&path)
            .map(|asset| v2_asset_record(asset, &path));
        let remote = remote_assets.get(&path);
        let remote_record = remote.map(|asset| v2_asset_record(asset, &path));
        let local_record = local_assets.get(&path);
        if local_record == base_record.as_ref() {
            continue;
        }
        if remote_record != base_record && local_record != remote_record.as_ref() {
            conflicts.push(path);
            continue;
        }
        let Some(record) = local_record else {
            if let Some(remote) = remote {
                operations.push(json!({
                    "op": "asset_delete",
                    "path": path,
                    "expected": v2_asset_expected(Some(remote)),
                }));
            }
            continue;
        };
        crate::linkmd_v2::normalize_path(&record.path)
            .map_err(|error| invalid_feed(error.to_string()))?;
        let kept_home = local_view.policy.keeps_home(&path);
        let raw = if matches!(store.regular_file_exists(Path::new(&path)), Ok(true)) {
            verify_v2_upload_source(store, &path, &record.sha256, record.bytes)?;
            Some(())
        } else {
            None
        };
        let disposition = if kept_home || raw.is_none() {
            "withheld"
        } else {
            "hosted"
        };
        if raw.is_none() && record.required && !kept_home {
            return Err(LinkError::InvalidPack {
                message: format!("required asset {path} is missing"),
            });
        }
        let op = if remote.is_some_and(|asset| {
            asset.disposition == "withheld"
                && disposition == "hosted"
                && v2_asset_record(asset, &path) == *record
        }) {
            if !resume_local_policy {
                continue;
            }
            "asset_resume"
        } else {
            "asset_put"
        };
        operations.push(json!({
            "op": op,
            "path": path,
            "expected": v2_asset_expected(remote),
            "asset": v2_asset_value(record, disposition),
        }));
        if disposition == "hosted" {
            raw.expect("hosted asset was checked present");
            upload_sources
                .entry(record.sha256.clone())
                .or_insert_with(|| V2UploadSource {
                    path: path.clone(),
                    bytes: record.bytes,
                });
        }
    }
    if !conflicts.is_empty() {
        conflicts.truncate(100);
        return Err(LinkError::Conflict { paths: conflicts });
    }
    if operations.is_empty() {
        let final_head = v2_verified_head(cfg, requested_brain)?
            .ok_or_else(|| invalid_feed("v2 head disappeared during sync"))?;
        if !same_v2_head(&head, &final_head) {
            return Err(LinkError::RemoteAdvancedDuringSync);
        }
        let mut final_local = v2_local_files(store)?;
        remove_scoped_projection(&head, baseline.as_ref(), &mut final_local)?;
        let local_changed = final_local.riding != local_view.riding;
        let remote_ahead = !v2_riding_matches_remote(&final_local.riding, &remote, |path| {
            final_local.policy.keeps_home(path)
        });
        let next = v2_baseline_from_head(cfg, &head, remote, remote_assets, Some(&final_local))?;
        let split_count = next.remote_copy_remains.len();
        accept_v2_head(cfg, &final_head)?;
        if !local_changed && !remote_ahead {
            refresh_scoped_view_marker(store, &head, next.files.len())?;
            save_v2_baseline(cfg, &head.brain_id, &store.root, &next)?;
        }
        return Ok(json!({
            "v": 2,
            "outcome": if local_changed { "local_dirty" } else if remote_ahead { "remote_ahead" } else { "no_change" },
            "sync_status": if local_changed { "local_dirty" } else if remote_ahead { "remote_ahead" } else { "synced" },
            "seq": head.pointer.as_ref().map_or(0, |pointer| pointer.seq),
            "commit_hash": head.pointer.as_ref().map(|pointer| &pointer.commit_hash),
            "local_policy": {
                "remote_copy_remains": split_count,
            },
        }));
    }
    let includes_contract = operations
        .iter()
        .any(|operation| operation.get("path").and_then(Value::as_str) == Some("DB.md"));
    let rebase = if head.pointer.is_none() || includes_contract {
        "strict"
    } else {
        "disjoint"
    };
    let base_value = head.pointer.as_ref().map(|pointer| {
        json!({
            "seq": pointer.seq,
            "commit_hash": pointer.commit_hash,
            "content_root": pointer.content_root,
            "asset_root": pointer.asset_root,
        })
    });
    // The mutation coordinate is stable for the exact observed base and local
    // operation set. A lost response can therefore be retried without a
    // duplicate commit/proposal even across process restarts.
    let entropy = format!(
        "{}\0{}\0{}\0{}",
        normalized_origin(&cfg.hub)?,
        head.brain_id,
        serde_json::to_string(&base_value).unwrap_or_default(),
        serde_json::to_string(&operations).unwrap_or_default()
    );
    let mutation_id = format!("dbmd-{}", content_sha256(entropy.as_bytes()));
    let mut expected_candidate = remote.clone();
    let mut expected_candidate_assets = remote_assets.clone();
    for operation in &operations {
        match operation.get("op").and_then(Value::as_str) {
            Some("put") => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 put has no path"))?;
                let sha256 = operation
                    .get("blob")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 put has no blob"))?;
                let bytes = operation
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_feed("v2 put has no byte count"))?;
                expected_candidate.insert(
                    path.to_string(),
                    V2BaselineFile {
                        sha256: sha256.to_string(),
                        bytes,
                        proof: None,
                    },
                );
            }
            Some("delete") => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 delete has no path"))?;
                expected_candidate.remove(path);
            }
            Some("asset_delete") => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 asset delete has no path"))?;
                expected_candidate_assets.remove(path);
            }
            Some("asset_put" | "asset_resume") => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 asset write has no path"))?;
                let record = local_assets
                    .get(path)
                    .ok_or_else(|| invalid_feed("v2 asset write has no local record"))?;
                let disposition = operation
                    .get("asset")
                    .and_then(|asset| asset.get("disposition"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 asset write has no disposition"))?;
                expected_candidate_assets.insert(
                    path.to_string(),
                    V2BaselineAsset {
                        blob_sha256: record.sha256.clone(),
                        bytes: record.bytes,
                        media_type: record.media_type.clone(),
                        wrappers: record.wrappers.clone(),
                        required: record.required,
                        disposition: disposition.to_string(),
                        leaf_hash: String::new(),
                    },
                );
            }
            _ => return Err(invalid_feed("dbmd generated an unsupported v2 operation")),
        }
    }
    let changed_bytes = upload_sources.values().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.bytes)
            .ok_or_else(|| LinkError::PushTooLarge {
                detail: "v2 changed-byte total overflow".to_string(),
            })
    })?;
    let inline = changed_bytes <= 3 * 1024 * 1024;
    let inline_blobs = if inline {
        upload_sources
            .iter()
            .map(|(sha256, source)| {
                let bytes = store.read_bounded(Path::new(&source.path), source.bytes)?;
                if bytes.len() as u64 != source.bytes || content_sha256(&bytes) != *sha256 {
                    return Err(LinkError::InvalidPack {
                        message: format!("local path `{}` changed before upload", source.path),
                    });
                }
                Ok(json!({
                    "sha256": sha256,
                    "bytes": source.bytes,
                    "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                }))
            })
            .collect::<LinkResult<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let mut body = json!({
        "mutation_id": mutation_id,
        "base": base_value,
        "rebase": rebase,
        "reason": "dbmd sync",
        "operations": operations,
        "blobs": inline_blobs,
    });
    if let Some(confirmation) = bulk_confirmation {
        if !crate::ulid::is_ulid(&confirmation.id) || !is_sha256(&confirmation.digest) {
            return Err(LinkError::InvalidPack {
                message: "bulk confirmation must contain a lowercase ULID and SHA-256 digest"
                    .to_string(),
            });
        }
        // A preview is bound to the exact observed head. Confirmation can
        // therefore never use disjoint rebase semantics, even when the normal
        // small-mutation path could.
        body["rebase"] = Value::String("strict".to_string());
        body["bulk_preview_id"] = Value::String(confirmation.id.clone());
        body["bulk_preview_digest"] = Value::String(confirmation.digest.clone());
    }
    if !inline || body.to_string().len() > MAX_PUSH_BYTES - 64 * 1024 {
        let mut coordinates_by_hash: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for operation in &operations {
            let Some(kind) = operation.get("op").and_then(Value::as_str) else {
                return Err(invalid_feed("v2 upload operation has no kind"));
            };
            let hash = match kind {
                "put" | "restore" | "rename" => operation.get("blob").and_then(Value::as_str),
                "asset_put" | "asset_resume" => operation
                    .get("asset")
                    .and_then(|asset| asset.get("blob_sha256"))
                    .and_then(Value::as_str),
                _ => None,
            };
            let Some(hash) = hash else { continue };
            let coordinates = coordinates_by_hash.entry(hash.to_string()).or_default();
            if kind == "rename" {
                for field in ["from", "to"] {
                    coordinates.insert(
                        operation
                            .get(field)
                            .and_then(Value::as_str)
                            .ok_or_else(|| invalid_feed("v2 rename upload has no coordinate"))?
                            .to_string(),
                    );
                }
            } else {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("v2 upload has no coordinate"))?;
                coordinates.insert(if kind.starts_with("asset_") {
                    format!("assets/{path}")
                } else {
                    path.to_string()
                });
            }
        }
        let declarations = upload_sources
            .iter()
            .map(|(sha256, source)| {
                json!({
                    "sha256": sha256,
                    "bytes": source.bytes,
                    "coordinates": coordinates_by_hash
                        .get(sha256)
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let reserved = ensure_ok(
            request(
                cfg,
                "POST",
                &format!("/api/hub/brains/{requested_brain}/v2/uploads"),
                Some(&json!({ "blobs": declarations })),
                Auth::Required,
            )?,
            "prepare v2 changed-byte uploads",
        )?;
        let items = reserved
            .get("uploads")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_feed("v2 upload reservation response has no items"))?;
        if items.len() != upload_sources.len() {
            return Err(invalid_feed(
                "v2 upload reservation response changed the requested set",
            ));
        }
        let mut references = Vec::with_capacity(items.len());
        let mut seen = std::collections::BTreeSet::new();
        for item in items {
            let sha256 = item
                .get("sha256")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_feed("v2 upload reservation has no hash"))?;
            let source = upload_sources
                .get(sha256)
                .ok_or_else(|| invalid_feed("v2 upload reservation introduced a blob"))?;
            let declared_bytes = item
                .get("bytes")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid_feed("v2 upload reservation has no byte length"))?;
            let reservation_id = item
                .get("reservation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_feed("v2 upload reservation has no opaque id"))?;
            let expected_coordinates = coordinates_by_hash
                .get(sha256)
                .ok_or_else(|| invalid_feed("v2 upload reservation has no coordinate binding"))?;
            let returned_coordinates = item
                .get("coordinates")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_feed("v2 upload reservation has no coordinates"))?;
            if declared_bytes != source.bytes
                || !crate::ulid::is_ulid(reservation_id)
                || !seen.insert(sha256.to_string())
                || returned_coordinates.len() != expected_coordinates.len()
                || returned_coordinates
                    .iter()
                    .zip(expected_coordinates)
                    .any(|(actual, expected)| actual.as_str() != Some(expected.as_str()))
            {
                return Err(invalid_feed("v2 upload reservation item is inconsistent"));
            }
            match item.get("status").and_then(Value::as_str) {
                Some("upload") => {
                    let url = item
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid_feed("v2 upload reservation has no URL"))?;
                    put_presigned_source(
                        cfg,
                        url,
                        item.get("headers").unwrap_or(&Value::Null),
                        store,
                        source,
                    )?;
                    verify_v2_upload_source(store, &source.path, sha256, source.bytes)?;
                }
                Some("already_present") => {}
                _ => return Err(invalid_feed("v2 upload reservation has an unknown status")),
            }
            references.push(json!({
                "sha256": sha256,
                "bytes": source.bytes,
                "reservation_id": reservation_id,
            }));
        }
        body["blobs"] = Value::Array(references);
    }
    if body.to_string().len() > MAX_PUSH_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: "v2 operation metadata exceeds the bounded commit request".to_string(),
        });
    }
    let path = format!("/api/hub/brains/{requested_brain}/v2/commits");
    let mut candidate_hub_signer: Option<String> = None;
    let mut response = request(cfg, "POST", &path, Some(&body), Auth::Required)?;
    let bulk_preview_required = !(200..300).contains(&response.status)
        && response.body.as_ref().is_some_and(|value| {
            value.get("code").and_then(Value::as_str) == Some("bulk_preview_required")
                || value
                    .get("details")
                    .and_then(|details| details.get("code"))
                    .and_then(Value::as_str)
                    == Some("bulk_preview_required")
        });
    if bulk_preview_required && bulk_confirmation.is_none() {
        body["rebase"] = Value::String("strict".to_string());
        body["preview_only"] = Value::Bool(true);
        let preview = ensure_ok(
            request(cfg, "POST", &path, Some(&body), Auth::Required)?,
            "v2 bulk preview",
        )?;
        let preview_code = preview.get("code").and_then(Value::as_str);
        let required = preview.get("required").and_then(Value::as_bool);
        if preview.get("v").and_then(Value::as_u64) != Some(2)
            || preview.get("mutation_id").and_then(Value::as_str) != Some(mutation_id.as_str())
            || !matches!(
                preview_code,
                Some("bulk_preview_created" | "bulk_preview_not_required")
            )
            || required.is_none()
        {
            return Err(invalid_feed(
                "bulk preview response is not bound to the requested mutation",
            ));
        }
        if required == Some(true) {
            let preview_id = preview.get("bulk_preview_id").and_then(Value::as_str);
            let preview_digest = preview.get("bulk_preview_digest").and_then(Value::as_str);
            if preview_code != Some("bulk_preview_created")
                || preview_id.is_none_or(|value| !crate::ulid::is_ulid(value))
                || preview_digest.is_none_or(|value| !is_sha256(value))
                || preview.get("expires_at").and_then(Value::as_str).is_none()
                || !preview.get("impact").is_some_and(Value::is_object)
            {
                return Err(invalid_feed("bulk preview receipt is malformed"));
            }
            return Err(LinkError::BulkPreviewRequired { preview });
        }
        if preview_code != Some("bulk_preview_not_required") {
            return Err(invalid_feed("bulk preview requirement is inconsistent"));
        }
        // The rolling activity window can expire between the refusal and
        // preview. Retry once, still pinned strictly to the observed head.
        body.as_object_mut()
            .expect("v2 commit request is an object")
            .remove("preview_only");
        response = request(cfg, "POST", &path, Some(&body), Auth::Required)?;
    }
    let mut result = ensure_ok(response, "v2 sync push")?;
    if result.get("code").and_then(Value::as_str) == Some("proposal_queued") {
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "sync_status".to_string(),
                Value::String("proposal_pending".to_string()),
            );
        }
        return Ok(result);
    }
    if result.get("code").and_then(Value::as_str) == Some("brain_signature_required") {
        let challenge = result
            .get("signing_challenge")
            .ok_or_else(|| invalid_feed("self-custody response has no signing challenge"))?;
        let (challenge_id, signature, actor_signer) = sign_verified_v2_candidate(
            cfg,
            &head,
            &expected_candidate,
            &expected_candidate_assets,
            &mutation_id,
            &body,
            challenge,
        )?;
        body["signing_challenge_id"] = Value::String(challenge_id);
        body["signature_base64url"] = Value::String(signature);
        candidate_hub_signer = Some(actor_signer);
        result = ensure_ok(
            request(cfg, "POST", &path, Some(&body), Auth::Required)?,
            "v2 self-custody commit",
        )?;
    }
    let refreshed = v2_verified_head(cfg, requested_brain)?
        .ok_or_else(|| invalid_feed("v2 head disappeared after commit"))?;
    if candidate_hub_signer
        .as_ref()
        .is_some_and(|expected| refreshed.trust.hub_signer.as_ref() != Some(expected))
    {
        return Err(invalid_feed(
            "self-custody actor signer differs from the committed hub pointer signer",
        ));
    }
    let accepted_hash = result.get("commit_hash").and_then(Value::as_str);
    if refreshed
        .pointer
        .as_ref()
        .map(|pointer| pointer.commit_hash.as_str())
        != accepted_hash
    {
        return Err(LinkError::RemoteAdvancedDuringSync);
    }
    ensure_v2_view_compatible(&refreshed, baseline.as_ref())?;
    let refreshed_files = files_for_v2_view(
        &refreshed,
        v2_manifest(cfg, &refreshed.brain_id, refreshed.pointer.as_ref())?,
    );
    let refreshed_assets = v2_asset_manifest(cfg, &refreshed.brain_id, refreshed.pointer.as_ref())?;
    let mut final_local = v2_local_files(store)?;
    remove_scoped_projection(&refreshed, baseline.as_ref(), &mut final_local)?;
    let local_dirty = final_local.riding != local_view.riding
        || !v2_riding_matches_remote(&final_local.riding, &refreshed_files, |path| {
            final_local.policy.keeps_home(path)
        });
    let next = v2_baseline_from_head(
        cfg,
        &refreshed,
        refreshed_files,
        refreshed_assets,
        Some(&final_local),
    )?;
    let split_count = next.remote_copy_remains.len();
    accept_v2_head(cfg, &refreshed)?;
    if !local_dirty {
        refresh_scoped_view_marker(store, &refreshed, next.files.len())?;
        save_v2_baseline(cfg, &refreshed.brain_id, &store.root, &next)?;
    }
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "local_policy".to_string(),
            json!({ "remote_copy_remains": split_count }),
        );
        object.insert(
            "sync_status".to_string(),
            Value::String(if local_dirty {
                "remote_committed_local_dirty".to_string()
            } else {
                "synced".to_string()
            }),
        );
    }
    Ok(result)
}

/// Negotiate v2 and send only local changes. A v1 hub retains the existing
/// whole-snapshot behavior until its brain advertises the new profile.
pub fn sync_push_incremental(cfg: &HubConfig, brain: &str, store: &Store) -> LinkResult<Value> {
    sync_push_incremental_with_policy(cfg, brain, store, false)
}

/// The explicit `.sevralocal` adoption form. Ordinary sync never uploads a
/// path that became eligible because local policy was weakened or removed.
pub fn sync_push_incremental_with_policy(
    cfg: &HubConfig,
    brain: &str,
    store: &Store,
    resume_local_policy: bool,
) -> LinkResult<Value> {
    sync_push_incremental_with_options(cfg, brain, store, resume_local_policy, None)
}

/// Incremental push with both local-policy adoption and an optional exact
/// permissioned bulk-preview confirmation.
pub fn sync_push_incremental_with_options(
    cfg: &HubConfig,
    brain: &str,
    store: &Store,
    resume_local_policy: bool,
    bulk_confirmation: Option<&V2BulkConfirmation>,
) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    if let Some(head) = v2_verified_head(cfg, brain)? {
        return v2_sync_push(
            cfg,
            brain,
            store,
            head,
            resume_local_policy,
            bulk_confirmation,
            None,
        );
    }
    legacy_sync_push_incremental(cfg, brain, store, resume_local_policy, bulk_confirmation)
}

#[cfg(windows)]
fn legacy_sync_push_incremental(
    _cfg: &HubConfig,
    _brain: &str,
    _store: &Store,
    _resume_local_policy: bool,
    _bulk_confirmation: Option<&V2BulkConfirmation>,
) -> LinkResult<Value> {
    Err(LinkError::UnsupportedPlatform {
        operation: "legacy v1 whole-snapshot push on Windows; upgrade the brain to link.md v2",
    })
}

#[cfg(not(windows))]
fn legacy_sync_push_incremental(
    cfg: &HubConfig,
    brain: &str,
    store: &Store,
    resume_local_policy: bool,
    bulk_confirmation: Option<&V2BulkConfirmation>,
) -> LinkResult<Value> {
    if resume_local_policy || bulk_confirmation.is_some() {
        return Err(LinkError::InvalidPack {
            message: "v2 sync options require a link.md v2 brain".to_string(),
        });
    }
    let files = collect_push_files(store)?;
    sync_push(cfg, brain, &files)
}

/// One explicit resolution for a previously preserved private conflict bundle.
#[derive(Debug, Clone)]
pub enum V2ConflictChoice {
    KeepLocal,
    TakeRemote,
    From(PathBuf),
}

fn load_v2_conflict_plan(store: &Store, bundle: &str) -> LinkResult<V2ConflictPlan> {
    if !crate::ulid::is_ulid(bundle) {
        return Err(LinkError::InvalidPack {
            message: "conflict bundle must be a lowercase ULID".to_string(),
        });
    }
    let bytes = store.read_bounded(&v2_conflict_relative(bundle, "plan.json"), 1024 * 1024)?;
    let plan: V2ConflictPlan = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_feed("private conflict plan is corrupt"))?;
    if plan.v != 2
        || plan.class != "content_resolution_required"
        || plan.bundle != bundle
        || !crate::ulid::is_ulid(&plan.brain)
        || plan.files.is_empty()
        || plan.files.len() > 100
        || plan.files.iter().any(|file| {
            crate::linkmd_v2::normalize_path(&file.path).is_err()
                || [&file.base, &file.local, &file.remote]
                    .into_iter()
                    .any(|coordinate| {
                        coordinate
                            .sha256
                            .as_deref()
                            .is_some_and(|hash| !is_sha256(hash))
                            || coordinate.file.as_deref().is_some_and(|name| {
                                name.starts_with('/')
                                    || name
                                        .split('/')
                                        .any(|part| part.is_empty() || part == "." || part == "..")
                            })
                    })
        })
    {
        return Err(invalid_feed("private conflict plan failed validation"));
    }
    Ok(plan)
}

/// Inspect or prune private conflict-control state. Ordinary pruning removes
/// only expired completed bundles and interrupted bundles with no `plan.json`;
/// corrupt completed plans fail closed. `all` is an explicit local discard of
/// every well-addressed bundle and never changes hosted brain data.
pub fn sync_conflicts(checkout: &Path, prune: bool, all: bool) -> LinkResult<Value> {
    require_hardened_filesystem("private conflict maintenance")?;
    if all && !prune {
        return Err(LinkError::InvalidPack {
            message: "discarding all conflict bundles requires prune=true".to_string(),
        });
    }
    let store = Store::open_strict(checkout).map_err(|error| LinkError::InvalidPack {
        message: format!("conflict checkout is not a valid db.md store: {error}"),
    })?;
    let _transaction = store.transaction()?;
    let root = Path::new(".dbmd/conflicts");
    let names = match store.directory_names(root) {
        Ok(names) => names,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut bundles = Vec::new();
    let mut pruned = 0_u64;
    for name in names {
        let Some(bundle) = name.to_str().filter(|value| crate::ulid::is_ulid(value)) else {
            continue;
        };
        let plan_path = v2_conflict_relative(bundle, "plan.json");
        let plan_exists = store.regular_file_exists(&plan_path)?;
        let expired = if plan_exists {
            match load_v2_conflict_plan(&store, bundle) {
                Ok(plan) => plan.expires_unix < now,
                Err(error) if all => {
                    let _ = error;
                    true
                }
                Err(error) => return Err(error),
            }
        } else {
            true
        };
        if prune && (all || expired) {
            store.remove_private_tree(&v2_conflict_relative(bundle, ""))?;
            pruned += 1;
            continue;
        }
        bundles.push(json!({
            "bundle": bundle,
            "complete": plan_exists,
            "expired": expired,
        }));
    }
    Ok(json!({
        "v": 2,
        "class": "private_conflict_state",
        "bundles": bundles.len(),
        "pruned": pruned,
        "items": bundles,
    }))
}

/// Resolve one exact private conflict plan. Drift never lowers a precondition:
/// the remote head/view and every original local coordinate are rechecked
/// before either an explicit local install or a fresh normal commit.
pub fn sync_resolve_conflict(
    cfg: &HubConfig,
    checkout: &Path,
    bundle: &str,
    choice: V2ConflictChoice,
    bulk_confirmation: Option<&V2BulkConfirmation>,
) -> LinkResult<Value> {
    require_hardened_filesystem("conflict resolution")?;
    let store = Store::open_strict(checkout).map_err(|error| LinkError::InvalidPack {
        message: format!("conflict checkout is not a valid db.md store: {error}"),
    })?;
    let plan = load_v2_conflict_plan(&store, bundle)?;
    if plan.origin != normalized_origin(&cfg.hub)? {
        return Err(invalid_feed(
            "conflict bundle belongs to another hub origin",
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if now > plan.expires_unix {
        return Err(LinkError::InvalidPack {
            message: "conflict bundle expired; rerun sync to obtain current coordinates"
                .to_string(),
        });
    }
    let head = v2_verified_head(cfg, &plan.brain)?
        .ok_or_else(|| invalid_feed("conflict brain no longer advertises v2"))?;
    let pointer = head.pointer.as_ref();
    if pointer.map_or(0, |value| value.seq) != plan.remote_seq
        || pointer.map(|value| value.commit_hash.as_str()) != plan.remote_commit.as_deref()
        || pointer.and_then(|value| value.content_root.as_deref())
            != plan.remote_content_root.as_deref()
        || head.view_kind != plan.view_kind
        || head.view_revision != plan.view_revision
    {
        return Err(LinkError::RemoteAdvancedDuringSync);
    }

    // A resolution never overwrites a post-bundle local edit.
    for file in &plan.files {
        let actual = match store.regular_file_exists(Path::new(&file.path))? {
            true => Some(content_sha256(&store.read_bounded(
                Path::new(&file.path),
                file.local.bytes.unwrap_or(MAX_STORE_BYTES),
            )?)),
            false => None,
        };
        if actual.as_deref() != file.local.sha256.as_deref() {
            return Err(LinkError::InvalidPack {
                message: format!(
                    "local conflict path `{}` changed after the bundle was created",
                    file.path
                ),
            });
        }
    }

    let from_source = match &choice {
        V2ConflictChoice::From(source) => Some(source.clone()),
        _ => None,
    };
    let result = match choice {
        V2ConflictChoice::TakeRemote => {
            if bulk_confirmation.is_some() {
                return Err(LinkError::InvalidPack {
                    message: "bulk confirmation applies to keep-local/from commits, not a local take-remote install".to_string(),
                });
            }
            let mut remote_files = std::collections::BTreeMap::new();
            let mut deleted = Vec::new();
            for file in &plan.files {
                match (&file.remote.sha256, file.remote.bytes) {
                    (Some(sha256), Some(bytes)) => {
                        remote_files.insert(
                            file.path.clone(),
                            V2BaselineFile {
                                sha256: sha256.clone(),
                                bytes,
                                proof: None,
                            },
                        );
                    }
                    (None, None) => deleted.push(file.path.clone()),
                    _ => return Err(invalid_feed("conflict remote coordinate is incomplete")),
                }
            }
            let staged = match head.pointer.as_ref() {
                Some(pointer) => {
                    stage_v2_blobs(cfg, &plan.brain, pointer, remote_files.iter().collect())?
                }
                None if remote_files.is_empty() => Vec::new(),
                None => return Err(invalid_feed("conflict head has no content pointer")),
            };
            let baseline = load_v2_baseline(cfg, &plan.brain, checkout)?;
            install_pulled_delta_sources(
                checkout,
                &staged,
                &deleted,
                true,
                baseline.as_ref(),
                &head,
            )?;
            complete_windows_v2_pull(checkout)?;
            let refreshed = v2_verified_head(cfg, &plan.brain)?
                .ok_or_else(|| invalid_feed("conflict brain disappeared during resolution"))?;
            serde_json::to_value(v2_sync_pull(cfg, &plan.brain, refreshed, Some(checkout))?)
                .map_err(|_| invalid_feed("could not serialize conflict pull receipt"))?
        }
        V2ConflictChoice::KeepLocal | V2ConflictChoice::From(_) => {
            if let Some(source) = from_source.as_ref() {
                if plan.files.len() != 1 {
                    return Err(LinkError::InvalidPack {
                        message: "--from requires a bundle with exactly one conflict".to_string(),
                    });
                }
                let candidate = crate::fsx::read_bounded_nofollow(source, MAX_STORE_BYTES)?;
                if std::str::from_utf8(&candidate).is_err() {
                    return Err(LinkError::NotUtf8 {
                        path: source.display().to_string(),
                    });
                }
                store.write_atomic(Path::new(&plan.files[0].path), &candidate)?;
            }
            let refreshed_store =
                Store::open_strict(checkout).map_err(|error| LinkError::InvalidPack {
                    message: format!("resolved checkout is not a valid db.md store: {error}"),
                })?;
            let mut overrides = std::collections::BTreeMap::new();
            for file in &plan.files {
                let selected_local = match refreshed_store
                    .regular_file_exists(Path::new(&file.path))?
                {
                    true => Some(content_sha256(
                        &refreshed_store.read_bounded(Path::new(&file.path), MAX_STORE_BYTES)?,
                    )),
                    false => None,
                };
                overrides.insert(
                    file.path.clone(),
                    V2ResolutionOverride {
                        expected_remote: file.remote.sha256.clone(),
                        selected_local,
                    },
                );
            }
            v2_sync_push(
                cfg,
                &plan.brain,
                &refreshed_store,
                head,
                true,
                bulk_confirmation,
                Some(&overrides),
            )?
        }
    };

    if result.get("code").and_then(Value::as_str) != Some("proposal_queued") {
        let installed = Store::open_strict(checkout).map_err(|error| LinkError::InvalidPack {
            message: format!("resolved checkout is not a valid db.md store: {error}"),
        })?;
        installed.remove_private_tree(&v2_conflict_relative(bundle, ""))?;
    }
    Ok(json!({
        "v": 2,
        "class": "auto_converged",
        "bundle": bundle,
        "receipt": result,
    }))
}

/// Converge one established permissioned-v2 checkout in both directions.
///
/// The pull half first performs the ordinary three-way conflict check and
/// crash-safe install against the accepted baseline. The push half then opens
/// that exact installed checkout, takes its local transaction guard, computes
/// a fresh three-way delta, and submits it with mandatory preconditions. A
/// remote movement between halves is therefore a normal rebase/conflict input,
/// never a blind overwrite. Legacy v1 remains explicit pull-only/push-only so
/// its whole-pack replacement semantics are never presented as granular
/// convergence.
pub fn sync_converge(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
    resume_local_policy: bool,
) -> LinkResult<Value> {
    sync_converge_with_options(cfg, brain, checkout, resume_local_policy, None)
}

/// Bidirectional convergence with optional exact bulk-preview confirmation.
pub fn sync_converge_with_options(
    cfg: &HubConfig,
    brain: &str,
    checkout: &Path,
    resume_local_policy: bool,
    bulk_confirmation: Option<&V2BulkConfirmation>,
) -> LinkResult<Value> {
    require_hardened_filesystem("bidirectional sync")?;
    require_safe_ref(brain)?;
    let head = v2_verified_head(cfg, brain)?.ok_or_else(|| LinkError::InvalidPack {
        message:
            "bidirectional sync requires link.md v2; use --pull-only or --push-only for a legacy brain"
                .to_string(),
    })?;
    let pulled = v2_sync_pull(cfg, brain, head, Some(checkout))?;
    let store = Store::open_strict(checkout).map_err(|error| LinkError::InvalidPack {
        message: format!("installed v2 checkout is not a valid db.md store: {error}"),
    })?;
    let _transaction = store.transaction()?;
    let fresh = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("v2 head disappeared between sync phases"))?;
    let mut result = v2_sync_push(
        cfg,
        brain,
        &store,
        fresh,
        resume_local_policy,
        bulk_confirmation,
        None,
    )?;
    if let Some(object) = result.as_object_mut() {
        object.insert("pulled_files".to_string(), json!(pulled.files));
        object.insert("checkout".to_string(), Value::String(pulled.dest));
        object.insert(
            "mode".to_string(),
            Value::String("bidirectional".to_string()),
        );
    }
    Ok(result)
}

/// Pull the granted slice of `brain` to `out` (default: `./<slug>`). Every
/// exported path is safety-gated before it touches disk; files are written
/// atomically; nothing local is ever deleted (locals the export lacks are
/// *reported* in `extra_local` instead). Returns the report; rebuilding the
/// local index catalog afterwards is the caller's (cheap, optional) step.
pub fn sync_pull(cfg: &HubConfig, brain: &str, out: Option<&Path>) -> LinkResult<PullReport> {
    require_hardened_filesystem("sync pull")?;
    require_safe_ref(brain)?;
    if let Some(head) = v2_verified_head(cfg, brain)? {
        return v2_sync_pull(cfg, brain, head, out);
    }
    legacy_sync_pull(cfg, brain, out)
}

#[cfg(windows)]
fn legacy_sync_pull(_cfg: &HubConfig, _brain: &str, _out: Option<&Path>) -> LinkResult<PullReport> {
    Err(LinkError::UnsupportedPlatform {
        operation: "legacy v1 whole-snapshot pull on Windows; upgrade the brain to link.md v2",
    })
}

#[cfg(not(windows))]
fn legacy_sync_pull(cfg: &HubConfig, brain: &str, out: Option<&Path>) -> LinkResult<PullReport> {
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
        sync_status: "synced".to_string(),
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
    // `libc::renameat2` is not exported for the musl targets we ship. Invoke
    // the kernel ABI directly, as `fsx::renameat_noreplace` does, so the same
    // atomic exchange/no-replace boundary compiles for glibc and musl.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent,
            stage.as_ptr(),
            parent,
            dest.as_ptr(),
            flags,
        )
    };
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
fn write_pull_sources_beneath_dir(
    root: &std::fs::File,
    entries: &[V2StagedFile],
) -> LinkResult<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    for entry in entries {
        let path = &entry.path;
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
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                leaf_name.as_ptr(),
                &mut existing,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && (existing.st_mode & libc::S_IFMT) == libc::S_IFLNK
        {
            return Err(LinkError::UnsafePath { path: path.clone() });
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_name = format!(".dbmd-pull-{}-{nonce}", std::process::id());
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
        let mut input = crate::fsx::open_regular_nofollow(&entry.source)?;
        let mut output = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        let copied = (|| -> std::io::Result<()> {
            loop {
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read as u64);
                if total > entry.bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "staged sync source grew beyond its verified length",
                    ));
                }
                digest.update(&buffer[..read]);
                output.write_all(&buffer[..read])?;
            }
            output.sync_all()
        })();
        if let Err(error) = copied {
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(error.into());
        }
        drop(output);
        if total != entry.bytes || format!("{:x}", digest.finalize()) != entry.sha256 {
            let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temp.as_ptr(), 0) };
            return Err(invalid_feed(
                "private staged sync source failed final integrity verification",
            ));
        }
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp.as_ptr(),
                directory.as_raw_fd(),
                leaf_name.as_ptr(),
            )
        } != 0
        {
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
fn remove_pull_paths_beneath_dir(root: &std::fs::File, paths: &[String]) -> LinkResult<()> {
    use std::os::fd::AsRawFd as _;
    for path in paths {
        if !safe_store_rel_path(path) {
            return Err(LinkError::UnsafePath { path: path.clone() });
        }
        let components = path.split('/').collect::<Vec<_>>();
        let Some((leaf, parents)) = components.split_last() else {
            return Err(LinkError::UnsafePath { path: path.clone() });
        };
        let mut directory = root.try_clone()?;
        let mut missing = false;
        for component in parents {
            let name = c_name(component.as_bytes(), path)?;
            match entry_is_dir_at(directory.as_raw_fd(), &name)? {
                None => {
                    missing = true;
                    break;
                }
                Some(false) => return Err(LinkError::UnsafePath { path: path.clone() }),
                Some(true) => {
                    directory = open_dir_at(directory.as_raw_fd(), &name, path)?;
                }
            }
        }
        if missing {
            continue;
        }
        let leaf = c_name(leaf.as_bytes(), path)?;
        match entry_is_dir_at(directory.as_raw_fd(), &leaf)? {
            None => {}
            Some(true) => return Err(LinkError::UnsafePath { path: path.clone() }),
            Some(false) => {
                if unsafe { libc::unlinkat(directory.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                directory.sync_all()?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn install_pulled_delta(
    dest: &Path,
    entries: &[(String, Vec<u8>)],
    deleted: &[String],
    rebuild_indexes: bool,
) -> LinkResult<()> {
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
        remove_pull_paths_beneath_dir(&stage_dir, deleted)?;
        write_pull_entries_beneath_dir(&stage_dir, entries)?;
        if rebuild_indexes {
            let stage_store =
                Store::from_held_root_strict(&parent.join(&stage_label), stage_dir.try_clone()?)
                    .map_err(|error| LinkError::InvalidPack {
                        message: format!("v2 staging tree is not a valid db.md store: {error}"),
                    })?;
            crate::index::Index::rebuild_all(&stage_store).map_err(|error| {
                LinkError::InvalidPack {
                    message: format!("could not materialize v2 local catalogs: {error}"),
                }
            })?;
        }
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

#[cfg(unix)]
fn install_pulled_delta_sources(
    dest: &Path,
    entries: &[V2StagedFile],
    deleted: &[String],
    rebuild_indexes: bool,
    _previous: Option<&V2SyncBaseline>,
    _next: &V2VerifiedHead,
) -> LinkResult<()> {
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
            })
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
        remove_pull_paths_beneath_dir(&stage_dir, deleted)?;
        write_pull_sources_beneath_dir(&stage_dir, entries)?;
        if rebuild_indexes {
            let stage_store =
                Store::from_held_root_strict(&parent.join(&stage_label), stage_dir.try_clone()?)
                    .map_err(|error| LinkError::InvalidPack {
                        message: format!("v2 staging tree is not a valid db.md store: {error}"),
                    })?;
            crate::index::Index::rebuild_all(&stage_store).map_err(|error| {
                LinkError::InvalidPack {
                    message: format!("could not materialize v2 local catalogs: {error}"),
                }
            })?;
        }
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
        let _ = remove_tree_at(
            parent_dir.as_raw_fd(),
            &stage_name,
            &dest.display().to_string(),
        );
        let _ = parent_dir.sync_all();
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct WindowsPullCoordinate {
    head_seq: Option<u64>,
    commit_hash: Option<String>,
    view_kind: Option<String>,
    view_revision: Option<String>,
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct WindowsPullFileCoordinate {
    sha256: String,
    bytes: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Deserialize, Serialize)]
struct WindowsPullJournalEntry {
    path: String,
    old: Option<WindowsPullFileCoordinate>,
    new: Option<WindowsPullFileCoordinate>,
    backup: Option<String>,
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum WindowsPullPhase {
    Preparing,
    Ready,
}

#[cfg(windows)]
#[derive(Debug, Clone, Deserialize, Serialize)]
struct WindowsPullJournal {
    v: u8,
    phase: WindowsPullPhase,
    brain: String,
    previous: WindowsPullCoordinate,
    next: WindowsPullCoordinate,
    backup_dir: String,
    entries: Vec<WindowsPullJournalEntry>,
}

#[cfg(windows)]
const WINDOWS_PULL_JOURNAL: &str = ".dbmd/pull-journal.json";

#[cfg(windows)]
fn windows_baseline_coordinate(baseline: Option<&V2SyncBaseline>) -> WindowsPullCoordinate {
    WindowsPullCoordinate {
        head_seq: baseline.and_then(|value| value.head_seq),
        commit_hash: baseline.and_then(|value| value.commit_hash.clone()),
        view_kind: baseline.and_then(|value| value.view_kind.clone()),
        view_revision: baseline.and_then(|value| value.view_revision.clone()),
    }
}

#[cfg(windows)]
fn windows_head_coordinate(head: &V2VerifiedHead) -> WindowsPullCoordinate {
    WindowsPullCoordinate {
        head_seq: head.pointer.as_ref().map(|value| value.seq),
        commit_hash: head.pointer.as_ref().map(|value| value.commit_hash.clone()),
        view_kind: Some(head.view_kind.clone()),
        view_revision: Some(head.view_revision.clone()),
    }
}

#[cfg(windows)]
fn windows_pull_state(
    store: &Store,
    path: &str,
    limit: u64,
) -> LinkResult<Option<WindowsPullFileCoordinate>> {
    let file = match store.open_regular(Path::new(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes = file.metadata()?.len();
    if bytes > limit || bytes > MAX_STORE_BYTES {
        return Err(invalid_feed(
            "pull transaction file exceeds its declared bound",
        ));
    }
    Ok(Some(WindowsPullFileCoordinate {
        sha256: content_sha256_reader(file)?,
        bytes,
    }))
}

#[cfg(windows)]
fn windows_pull_journal_bytes(journal: &WindowsPullJournal) -> LinkResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(journal)
        .map_err(|_| invalid_feed("could not serialize Windows pull journal"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(windows)]
fn validate_windows_pull_journal(journal: &WindowsPullJournal) -> LinkResult<()> {
    let backup_prefix = ".dbmd/pull-backup-";
    let suffix = journal
        .backup_dir
        .strip_prefix(backup_prefix)
        .ok_or_else(|| invalid_feed("Windows pull journal backup address is invalid"))?;
    let mut paths = std::collections::BTreeSet::new();
    if journal.v != 1
        || !crate::ulid::is_ulid(&journal.brain)
        || !crate::ulid::is_ulid(suffix)
        || journal.entries.is_empty()
        || journal.entries.len() > MAX_PUSH_FILES + 4
        || journal.previous == journal.next
    {
        return Err(invalid_feed("Windows pull journal failed validation"));
    }
    for (index, entry) in journal.entries.iter().enumerate() {
        if !safe_store_rel_path(&entry.path)
            || entry.path == WINDOWS_PULL_JOURNAL
            || entry.path.starts_with(backup_prefix)
            || !paths.insert(entry.path.clone())
            || (entry.old.is_none() && entry.new.is_none())
            || entry
                .old
                .iter()
                .chain(entry.new.iter())
                .any(|value| !is_sha256(&value.sha256) || value.bytes > MAX_STORE_BYTES)
            || entry.backup.as_deref()
                != entry
                    .old
                    .as_ref()
                    .map(|_| format!("{index:08x}"))
                    .as_deref()
        {
            return Err(invalid_feed("Windows pull journal entry failed validation"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn load_windows_pull_journal(store: &Store) -> LinkResult<Option<WindowsPullJournal>> {
    let bytes = match store.read_bounded(Path::new(WINDOWS_PULL_JOURNAL), 64 * 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: WindowsPullJournal = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_feed("Windows pull journal is corrupt"))?;
    validate_windows_pull_journal(&journal)?;
    Ok(Some(journal))
}

#[cfg(windows)]
fn cleanup_windows_pull_journal(store: &Store, journal: &WindowsPullJournal) -> LinkResult<()> {
    // The journal is the only recovery authority. Remove it first only after
    // rollback or baseline commit is durable; an orphan private backup is safe
    // and can be pruned, while a journal without its backups is not recoverable.
    match store.remove_file(Path::new(WINDOWS_PULL_JOURNAL)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match store.remove_private_tree(Path::new(&journal.backup_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn prune_orphan_windows_pull_backups(store: &Store) -> LinkResult<()> {
    let names = match store.directory_names(Path::new(".dbmd")) {
        Ok(names) => names,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("pull-backup-") else {
            continue;
        };
        if crate::ulid::is_ulid(suffix) {
            store.remove_private_tree(&Path::new(".dbmd").join(name))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn rollback_windows_pull(store: &Store, journal: &WindowsPullJournal) -> LinkResult<()> {
    // Validate every live coordinate and every backup before the first restore.
    for entry in &journal.entries {
        let limit = entry
            .old
            .as_ref()
            .into_iter()
            .chain(entry.new.iter())
            .map(|value| value.bytes)
            .max()
            .unwrap_or(0);
        let current = windows_pull_state(store, &entry.path, limit)?;
        if current != entry.old && current != entry.new {
            return Err(LinkError::InvalidPack {
                message: format!(
                    "cannot recover interrupted pull because `{}` changed afterward",
                    entry.path
                ),
            });
        }
        if let (Some(old), Some(backup)) = (&entry.old, &entry.backup) {
            let path = Path::new(&journal.backup_dir).join(backup);
            let file = store.open_regular(&path)?;
            if file.metadata()?.len() != old.bytes || content_sha256_reader(file)? != old.sha256 {
                return Err(invalid_feed(
                    "Windows pull recovery backup failed verification",
                ));
            }
        }
    }
    for entry in journal.entries.iter().rev() {
        match (&entry.old, &entry.backup) {
            (Some(old), Some(backup)) => {
                let bytes =
                    store.read_bounded(&Path::new(&journal.backup_dir).join(backup), old.bytes)?;
                store.write_atomic(Path::new(&entry.path), &bytes)?;
            }
            (None, None) if store.regular_file_exists(Path::new(&entry.path))? => {
                store.remove_file(Path::new(&entry.path))?;
            }
            (None, None) => {}
            _ => return Err(invalid_feed("Windows pull recovery entry is inconsistent")),
        }
    }
    crate::index::Index::rebuild_all(store).map_err(|error| LinkError::InvalidPack {
        message: format!("could not rebuild catalogs after pull recovery: {error}"),
    })?;
    cleanup_windows_pull_journal(store, journal)
}

#[cfg(windows)]
fn recover_windows_v2_pull(cfg: &HubConfig, brain: &str, dest: &Path) -> LinkResult<()> {
    let Ok(store) = Store::open_strict(dest) else {
        return Ok(());
    };
    if let Some(journal) = load_windows_pull_journal(&store)? {
        if journal.brain != brain {
            return Err(invalid_feed(
                "Windows pull journal belongs to another brain",
            ));
        }
        if journal.phase == WindowsPullPhase::Preparing {
            cleanup_windows_pull_journal(&store, &journal)?;
        } else {
            let baseline = load_v2_baseline(cfg, brain, dest)?;
            let current = windows_baseline_coordinate(baseline.as_ref());
            if current == journal.next {
                cleanup_windows_pull_journal(&store, &journal)?;
            } else {
                if current != journal.previous {
                    return Err(invalid_feed(
                        "cannot recover interrupted pull because its baseline changed afterward",
                    ));
                }
                rollback_windows_pull(&store, &journal)?;
            }
        }
    }
    // Cleanup removes the journal first after the state is durably old or new.
    // A hard kill in the tiny interval before deleting its backup directory can
    // therefore leave only an inert, private orphan. The per-brain operation
    // lock is held by the caller, so no live pull can own one here.
    prune_orphan_windows_pull_backups(&store)
}

#[cfg(windows)]
fn complete_windows_v2_pull(dest: &Path) -> LinkResult<()> {
    let store = Store::open_strict(dest).map_err(|error| LinkError::InvalidPack {
        message: format!("installed Windows checkout is not a valid db.md store: {error}"),
    })?;
    if let Some(journal) = load_windows_pull_journal(&store)? {
        cleanup_windows_pull_journal(&store, &journal)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn recover_windows_v2_pull(_cfg: &HubConfig, _brain: &str, _dest: &Path) -> LinkResult<()> {
    Ok(())
}

#[cfg(not(windows))]
fn complete_windows_v2_pull(_dest: &Path) -> LinkResult<()> {
    Ok(())
}

#[cfg(windows)]
fn install_windows_initial_sources(
    dest: &Path,
    entries: &[V2StagedFile],
    rebuild_indexes: bool,
) -> LinkResult<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().ok_or_else(|| LinkError::UnsafePath {
        path: dest.display().to_string(),
    })?;
    let parent_capability = crate::fsx::open_or_create_directory_nofollow(parent)?;
    if crate::fsx::directory_exists_beneath(&parent_capability, Path::new(name))? {
        return Err(LinkError::UnsafePath {
            path: dest.display().to_string(),
        });
    }
    let stage_name = format!(
        ".{}.dbmd-pull-stage-{}",
        name.to_string_lossy(),
        crate::ulid::mint()
    );
    let stage_path = parent.join(&stage_name);
    let stage_capability =
        crate::fsx::open_directory_beneath(&parent_capability, Path::new(&stage_name), true)?;
    let stage = Store::from_root_and_config(&stage_path, crate::Config::default())?;
    let prepared = (|| -> LinkResult<()> {
        for entry in entries {
            let bytes = crate::fsx::read_bounded_nofollow(&entry.source, entry.bytes)?;
            if bytes.len() as u64 != entry.bytes || content_sha256(&bytes) != entry.sha256 {
                return Err(invalid_feed(
                    "private staged sync source failed final integrity verification",
                ));
            }
            stage.write_atomic(Path::new(&entry.path), &bytes)?;
        }
        let strict = Store::from_held_root_strict(&stage_path, stage_capability.try_clone()?)
            .map_err(|error| LinkError::InvalidPack {
                message: format!("v2 staging tree is not a valid db.md store: {error}"),
            })?;
        if rebuild_indexes {
            crate::index::Index::rebuild_all(&strict).map_err(|error| LinkError::InvalidPack {
                message: format!("could not materialize v2 local catalogs: {error}"),
            })?;
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        let _ = crate::fsx::remove_tree_beneath(&parent_capability, Path::new(&stage_name));
        return Err(error);
    }
    crate::fsx::rename_directory_beneath(
        &parent_capability,
        Path::new(&stage_name),
        Path::new(name),
    )?;
    Ok(())
}

#[cfg(windows)]
fn install_pulled_delta_sources(
    dest: &Path,
    entries: &[V2StagedFile],
    deleted: &[String],
    rebuild_indexes: bool,
    previous: Option<&V2SyncBaseline>,
    next: &V2VerifiedHead,
) -> LinkResult<()> {
    let store = match Store::open_strict(dest) {
        Ok(store) => store,
        Err(_) => return install_windows_initial_sources(dest, entries, rebuild_indexes),
    };
    if load_windows_pull_journal(&store)?.is_some() {
        return Err(invalid_feed(
            "an interrupted Windows pull must be recovered before installing",
        ));
    }
    let mut sources = std::collections::BTreeMap::new();
    for entry in entries {
        if sources.insert(entry.path.clone(), entry).is_some() || deleted.contains(&entry.path) {
            return Err(invalid_feed("Windows pull mutation repeats a path"));
        }
    }
    let mut paths = sources.keys().cloned().collect::<Vec<_>>();
    paths.extend(deleted.iter().cloned());
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Ok(());
    }
    let backup_dir = format!(".dbmd/pull-backup-{}", crate::ulid::mint());
    let mut journal = WindowsPullJournal {
        v: 1,
        phase: WindowsPullPhase::Preparing,
        brain: next.brain_id.clone(),
        previous: windows_baseline_coordinate(previous),
        next: windows_head_coordinate(next),
        backup_dir: backup_dir.clone(),
        entries: Vec::with_capacity(paths.len()),
    };
    for (index, path) in paths.iter().enumerate() {
        let old = windows_pull_state(&store, path, MAX_STORE_BYTES)?;
        let new = sources.get(path).map(|entry| WindowsPullFileCoordinate {
            sha256: entry.sha256.clone(),
            bytes: entry.bytes,
        });
        journal.entries.push(WindowsPullJournalEntry {
            path: path.clone(),
            backup: old.as_ref().map(|_| format!("{index:08x}")),
            old,
            new,
        });
    }
    validate_windows_pull_journal(&journal)?;
    store.write_atomic_new(
        Path::new(WINDOWS_PULL_JOURNAL),
        &windows_pull_journal_bytes(&journal)?,
    )?;
    let prepared = (|| -> LinkResult<()> {
        store.create_dir_all(Path::new(&backup_dir))?;
        for entry in &journal.entries {
            if let (Some(old), Some(backup)) = (&entry.old, &entry.backup) {
                let bytes = store.read_bounded(Path::new(&entry.path), old.bytes)?;
                if content_sha256(&bytes) != old.sha256 {
                    return Err(invalid_feed("live pull source changed during backup"));
                }
                store.write_atomic_new(&Path::new(&backup_dir).join(backup), &bytes)?;
            }
        }
        journal.phase = WindowsPullPhase::Ready;
        store.write_atomic(
            Path::new(WINDOWS_PULL_JOURNAL),
            &windows_pull_journal_bytes(&journal)?,
        )?;
        Ok(())
    })();
    if let Err(error) = prepared {
        let cleanup = cleanup_windows_pull_journal(&store, &journal);
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(LinkError::InvalidPack {
                message: format!("{error}; recovery metadata cleanup also failed: {cleanup}"),
            }),
        };
    }
    let installed = (|| -> LinkResult<()> {
        for entry in &journal.entries {
            if windows_pull_state(&store, &entry.path, MAX_STORE_BYTES)? != entry.old {
                return Err(LinkError::InvalidPack {
                    message: format!("local path `{}` changed during pull", entry.path),
                });
            }
            if let Some(source) = sources.get(&entry.path) {
                let bytes = crate::fsx::read_bounded_nofollow(&source.source, source.bytes)?;
                if bytes.len() as u64 != source.bytes || content_sha256(&bytes) != source.sha256 {
                    return Err(invalid_feed(
                        "private staged sync source failed final integrity verification",
                    ));
                }
                store.write_atomic(Path::new(&entry.path), &bytes)?;
            } else if entry.old.is_some() {
                store.remove_file(Path::new(&entry.path))?;
            }
        }
        if rebuild_indexes {
            crate::index::Index::rebuild_all(&store).map_err(|error| LinkError::InvalidPack {
                message: format!("could not materialize v2 local catalogs: {error}"),
            })?;
        }
        Ok(())
    })();
    if let Err(error) = installed {
        return match rollback_windows_pull(&store, &journal) {
            Ok(()) => Err(error),
            Err(rollback) => Err(LinkError::InvalidPack {
                message: format!("{error}; durable pull rollback also failed: {rollback}"),
            }),
        };
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn install_pulled_delta_sources(
    _dest: &Path,
    _entries: &[V2StagedFile],
    _deleted: &[String],
    _rebuild_indexes: bool,
    _previous: Option<&V2SyncBaseline>,
    _next: &V2VerifiedHead,
) -> LinkResult<()> {
    Err(LinkError::UnsupportedPlatform {
        operation: "atomic v2 pull install",
    })
}

#[cfg(unix)]
fn install_pulled_snapshot(dest: &Path, entries: &[(String, Vec<u8>)]) -> LinkResult<()> {
    install_pulled_delta(dest, entries, &[], false)
}

#[cfg(not(windows))]
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
// permissioned v2 proposals — inspect, accept, reject
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct VerifiedV2Proposal {
    value: Value,
    changes: Value,
    blobs: Vec<(String, u64, String)>,
}

fn require_proposal_id(id: &str) -> LinkResult<()> {
    if crate::ulid::is_ulid(id) {
        Ok(())
    } else {
        Err(invalid_feed("proposal id is not a lowercase ULID"))
    }
}

fn verified_v2_proposal(
    cfg: &HubConfig,
    head: &V2VerifiedHead,
    proposal_id: &str,
) -> LinkResult<VerifiedV2Proposal> {
    require_proposal_id(proposal_id)?;
    if head.view_kind != "full" {
        return Err(invalid_feed(
            "proposal review requires a full readable view",
        ));
    }
    let path = format!(
        "/api/hub/brains/{}/v2/proposals/{proposal_id}",
        head.brain_id
    );
    let value = ensure_ok(
        request_capped(
            cfg,
            "GET",
            &path,
            None,
            Auth::Required,
            MAX_FEED_RESPONSE_BYTES,
        )?,
        "v2 proposal",
    )?;
    verify_v2_proposal_value(head, proposal_id, value)
}

fn verify_v2_proposal_value(
    head: &V2VerifiedHead,
    proposal_id: &str,
    value: Value,
) -> LinkResult<VerifiedV2Proposal> {
    if value.get("v").and_then(Value::as_u64) != Some(2) {
        return Err(invalid_feed("proposal response has an invalid version"));
    }
    let proposal = value
        .get("proposal")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_feed("proposal response has no proposal"))?;
    if proposal.get("id").and_then(Value::as_str) != Some(proposal_id) {
        return Err(invalid_feed("proposal response changed its id"));
    }
    let payload_hash = proposal
        .get("payload_sha256")
        .and_then(Value::as_str)
        .filter(|hash| is_sha256(hash))
        .ok_or_else(|| invalid_feed("proposal has no payload address"))?;
    let clear_hash = proposal
        .get("clear_sha256")
        .and_then(Value::as_str)
        .filter(|hash| is_sha256(hash))
        .ok_or_else(|| invalid_feed("proposal has no clear payload digest"))?;
    let submission_hash = proposal
        .get("submission_claim_sha256")
        .and_then(Value::as_str)
        .filter(|hash| is_sha256(hash))
        .ok_or_else(|| invalid_feed("proposal has no submission claim address"))?;
    let submission = STANDARD
        .decode(
            proposal
                .get("submission_claim_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_feed("proposal has no submission claim"))?,
        )
        .map_err(|_| invalid_feed("proposal submission claim is not base64"))?;
    let submission_value: Value = serde_json::from_slice(&submission)
        .map_err(|_| invalid_feed("proposal submission claim is not JSON"))?;
    if crate::linkmd_v2::canonical_bytes(&submission_value)
        .map_err(|error| invalid_feed(error.to_string()))?
        != submission
        || crate::linkmd_v2::domain_hash_bytes("v2/proposal-claim", &submission)
            .map_err(|error| invalid_feed(error.to_string()))?
            != submission_hash
    {
        return Err(invalid_feed(
            "proposal submission claim is not canonical or addressed",
        ));
    }
    let envelope = submission_value
        .as_object()
        .ok_or_else(|| invalid_feed("proposal submission claim is not an object"))?;
    let claim = envelope
        .get("claim")
        .ok_or_else(|| invalid_feed("proposal submission claim body is missing"))?;
    let claim_object = claim
        .as_object()
        .ok_or_else(|| invalid_feed("proposal submission claim body is not an object"))?;
    let actor_root = claim_object
        .get("actor_root")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_feed("proposal submission actor root is missing"))?;
    let public_key = envelope
        .get("public_key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("proposal submission signer is missing"))?;
    let fingerprint = envelope
        .get("fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("proposal submission fingerprint is missing"))?;
    let signature = envelope
        .get("sig")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("proposal submission signature is missing"))?;
    let claim_bytes = crate::linkmd_v2::canonical_bytes(claim)
        .map_err(|error| invalid_feed(error.to_string()))?;
    let der = verify_v2_spki_signature(public_key, &claim_bytes, signature)?;
    let signer = format!("{fingerprint}:{public_key}");
    let actor_class = actor_root.get("actor_class").and_then(Value::as_str);
    let grants = actor_root.get("grants").and_then(Value::as_array);
    let grants_are_canonical = grants.is_some_and(|items| {
        let mut prior: Option<&str> = None;
        items.iter().all(|item| {
            let Some(grant) = item.as_str() else {
                return false;
            };
            if !crate::ulid::is_ulid(grant) || prior.is_some_and(|value| value >= grant) {
                return false;
            }
            prior = Some(grant);
            true
        })
    });
    let optional_actor_field = |name: &str| {
        actor_root.get(name).is_some_and(|value| {
            value.is_null() || value.as_str().is_some_and(|text| !text.is_empty())
        })
    };
    let submitted_at = claim_object.get("submitted_at").and_then(Value::as_str);
    if claim_object.get("v").and_then(Value::as_u64) != Some(2)
        || format!("{:x}", Sha256::digest(&der)) != fingerprint
        || head
            .trust
            .hub_signer
            .as_ref()
            .is_some_and(|known| known != &signer)
        || !matches!(
            actor_class,
            Some(
                "user"
                    | "owned_agent"
                    | "foreign_key"
                    | "curation"
                    | "inbox"
                    | "restore"
                    | "migration"
                    | "operator_recovery"
            )
        )
        || actor_root
            .get("principal")
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty())
        || actor_root
            .get("credential")
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty())
        || !optional_actor_field("organization")
        || !optional_actor_field("role")
        || !grants_are_canonical
        || claim.get("brain").and_then(Value::as_str) != Some(head.brain_id.as_str())
        || claim.get("proposal_id").and_then(Value::as_str) != Some(proposal_id)
        || !claim_object
            .get("mutation_id")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value.chars().enumerate().all(|(index, char)| {
                        char.is_ascii_alphanumeric()
                            || (index > 0 && matches!(char, '.' | '_' | ':' | '-'))
                    })
            })
        || claim.get("payload_sha256").and_then(Value::as_str) != Some(payload_hash)
        || claim.get("clear_sha256").and_then(Value::as_str) != Some(clear_hash)
        || !claim_object
            .get("control_revision")
            .and_then(Value::as_str)
            .is_some_and(is_sha256)
        || submitted_at.is_none_or(|value| {
            chrono::DateTime::parse_from_rfc3339(value).is_err()
                || proposal.get("submitted_at").and_then(Value::as_str) != Some(value)
        })
        || !proposal
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|value| matches!(value, "pending" | "accepted" | "rejected" | "expired"))
        || proposal
            .get("expires_at")
            .and_then(Value::as_str)
            .is_none_or(|value| chrono::DateTime::parse_from_rfc3339(value).is_err())
        || proposal
            .get("proposer")
            .and_then(Value::as_object)
            .and_then(|value| value.get("class"))
            .and_then(Value::as_str)
            != actor_class
    {
        return Err(invalid_feed(
            "proposal submission claim does not bind the verified proposal",
        ));
    }
    let changes_b64 = proposal
        .get("changes_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_feed("proposal has no changeset"))?;
    let changes_bytes = STANDARD
        .decode(changes_b64)
        .map_err(|_| invalid_feed("proposal changeset is not base64"))?;
    let changes: Value = serde_json::from_slice(&changes_bytes)
        .map_err(|_| invalid_feed("proposal changeset is not JSON"))?;
    if crate::linkmd_v2::canonical_bytes(&changes)
        .map_err(|error| invalid_feed(error.to_string()))?
        != changes_bytes
        || changes.get("v").and_then(Value::as_u64) != Some(2)
        || !changes.get("operations").is_some_and(Value::is_array)
    {
        return Err(invalid_feed("proposal changeset is not canonical v2"));
    }
    let blob_values = proposal
        .get("blobs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_feed("proposal has no blob declarations"))?;
    let mut blobs = Vec::with_capacity(blob_values.len());
    let mut descriptor_blobs = Vec::with_capacity(blob_values.len());
    let mut prior_hash: Option<String> = None;
    for item in blob_values {
        let hash = item
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|hash| is_sha256(hash))
            .ok_or_else(|| invalid_feed("proposal blob has no address"))?;
        let bytes = item
            .get("bytes")
            .and_then(Value::as_u64)
            .filter(|bytes| *bytes <= MAX_STORE_BYTES)
            .ok_or_else(|| invalid_feed("proposal blob has an invalid size"))?;
        if prior_hash.as_deref().is_some_and(|prior| prior >= hash) {
            return Err(invalid_feed(
                "proposal blob declarations are not unique and sorted",
            ));
        }
        prior_hash = Some(hash.to_string());
        let endpoint = item
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_feed("proposal blob has no endpoint"))?;
        let expected_endpoint = format!(
            "/api/hub/brains/{}/v2/proposals/{proposal_id}/blob?sha256={hash}",
            head.brain_id
        );
        if endpoint != expected_endpoint {
            return Err(invalid_feed("proposal blob endpoint is not origin-bound"));
        }
        descriptor_blobs.push(json!({ "bytes": bytes, "sha256": hash }));
        blobs.push((hash.to_string(), bytes, endpoint.to_string()));
    }
    let descriptor = json!({
        "base": proposal.get("base").cloned().unwrap_or(Value::Null),
        "blobs": descriptor_blobs,
        "changes_base64": changes_b64,
        "rebase": proposal.get("rebase").cloned().unwrap_or(Value::Null),
        "v": 2,
    });
    let descriptor_bytes = crate::linkmd_v2::canonical_bytes(&descriptor)
        .map_err(|error| invalid_feed(error.to_string()))?;
    if content_sha256(&descriptor_bytes) != clear_hash {
        return Err(invalid_feed(
            "proposal clear payload differs from its signed submission claim",
        ));
    }
    Ok(VerifiedV2Proposal {
        value,
        changes,
        blobs,
    })
}

pub fn proposal_list(
    cfg: &HubConfig,
    brain: &str,
    state: &str,
    after: Option<&str>,
    limit: usize,
) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    if !matches!(state, "pending" | "accepted" | "rejected" | "expired") {
        return Err(invalid_feed("proposal state is invalid"));
    }
    if after.is_some_and(|value| !crate::ulid::is_ulid(value)) {
        return Err(invalid_feed("proposal cursor is invalid"));
    }
    let head = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("brain does not advertise link.md v2"))?;
    let path = format!(
        "/api/hub/brains/{}/v2/proposals?state={state}&limit={}{}",
        head.brain_id,
        limit.clamp(1, 100),
        after.map_or_else(String::new, |value| format!("&after={value}"))
    );
    ensure_ok(
        request_capped(
            cfg,
            "GET",
            &path,
            None,
            Auth::Required,
            MAX_FEED_RESPONSE_BYTES,
        )?,
        "v2 proposal list",
    )
}

pub fn proposal_show(cfg: &HubConfig, brain: &str, proposal_id: &str) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    let head = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("brain does not advertise link.md v2"))?;
    Ok(verified_v2_proposal(cfg, &head, proposal_id)?.value)
}

pub fn proposal_reject(
    cfg: &HubConfig,
    brain: &str,
    proposal_id: &str,
    mutation_id: &str,
    reason: &str,
) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    require_proposal_id(proposal_id)?;
    let head = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("brain does not advertise link.md v2"))?;
    let _ = verified_v2_proposal(cfg, &head, proposal_id)?;
    let body = json!({
        "mutation_id": mutation_id,
        "control_revision": head.view_revision,
        "reason": reason,
    });
    let path = format!(
        "/api/hub/brains/{}/v2/proposals/{proposal_id}",
        head.brain_id
    );
    ensure_ok(
        request(cfg, "DELETE", &path, Some(&body), Auth::Required)?,
        "v2 proposal rejection",
    )
}

pub fn proposal_accept_exact(
    cfg: &HubConfig,
    brain: &str,
    proposal_id: &str,
    mutation_id: &str,
    reason: &str,
) -> LinkResult<Value> {
    require_safe_ref(brain)?;
    require_proposal_id(proposal_id)?;
    let head = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("brain does not advertise link.md v2"))?;
    let proposal = verified_v2_proposal(cfg, &head, proposal_id)?;
    let operations = proposal
        .changes
        .get("operations")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid_feed("proposal changeset has no operations"))?;
    if operations.is_empty() || operations.len() > MAX_PUSH_FILES {
        return Err(invalid_feed("proposal operation count is invalid"));
    }
    let mut downloaded = std::collections::BTreeMap::new();
    for (hash, bytes, endpoint) in &proposal.blobs {
        let body = ensure_raw_ok(
            request_raw(cfg, "GET", endpoint, None, Auth::Required, *bytes)?,
            "v2 proposal blob",
        )?;
        if body.len() as u64 != *bytes || content_sha256(&body) != *hash {
            return Err(invalid_feed("proposal blob does not match its declaration"));
        }
        downloaded.insert(hash.clone(), body);
    }
    let remote = files_for_v2_view(
        &head,
        v2_manifest(cfg, &head.brain_id, head.pointer.as_ref())?,
    );
    let remote_assets = v2_asset_manifest(cfg, &head.brain_id, head.pointer.as_ref())?;
    let mut expected_candidate = remote.clone();
    let mut expected_candidate_assets = remote_assets;
    for operation in &operations {
        let op = operation
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_feed("proposal operation has no kind"))?;
        match op {
            "put" | "restore" => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal write has no path"))?;
                crate::linkmd_v2::normalize_path(path)
                    .map_err(|error| invalid_feed(error.to_string()))?;
                let hash = operation
                    .get("blob")
                    .and_then(Value::as_str)
                    .filter(|hash| is_sha256(hash))
                    .ok_or_else(|| invalid_feed("proposal write has no blob"))?;
                let bytes = operation
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_feed("proposal write has no size"))?;
                expected_candidate.insert(
                    path.to_string(),
                    V2BaselineFile {
                        sha256: hash.to_string(),
                        bytes,
                        proof: None,
                    },
                );
            }
            "delete" | "withdraw_from_hosting" => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal removal has no path"))?;
                crate::linkmd_v2::normalize_path(path)
                    .map_err(|error| invalid_feed(error.to_string()))?;
                expected_candidate.remove(path);
            }
            "rename" => {
                let from = operation
                    .get("from")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal rename has no source"))?;
                let to = operation
                    .get("to")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal rename has no destination"))?;
                crate::linkmd_v2::normalize_path(from)
                    .and_then(|_| crate::linkmd_v2::normalize_path(to))
                    .map_err(|error| invalid_feed(error.to_string()))?;
                let hash = operation
                    .get("blob")
                    .and_then(Value::as_str)
                    .filter(|hash| is_sha256(hash))
                    .ok_or_else(|| invalid_feed("proposal rename has no blob"))?;
                let bytes = operation
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_feed("proposal rename has no size"))?;
                expected_candidate.remove(from);
                expected_candidate.insert(
                    to.to_string(),
                    V2BaselineFile {
                        sha256: hash.to_string(),
                        bytes,
                        proof: None,
                    },
                );
            }
            "asset_delete" => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal asset delete has no path"))?;
                expected_candidate_assets.remove(path);
            }
            "asset_withdraw" => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal asset withdrawal has no path"))?;
                let asset = expected_candidate_assets
                    .get_mut(path)
                    .ok_or_else(|| invalid_feed("proposal withdraws an unknown asset"))?;
                asset.disposition = "withheld".to_string();
                asset.leaf_hash.clear();
            }
            "asset_put" | "asset_resume" => {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal asset write has no path"))?;
                let asset = operation
                    .get("asset")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid_feed("proposal asset write has no value"))?;
                let blob_sha256 = asset
                    .get("blob_sha256")
                    .and_then(Value::as_str)
                    .filter(|hash| is_sha256(hash))
                    .ok_or_else(|| invalid_feed("proposal asset has no blob hash"))?;
                let bytes = asset
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_feed("proposal asset has no byte count"))?;
                let media_type = asset
                    .get("media_type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal asset has no media type"))?;
                let wrappers = asset
                    .get("wrappers")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_feed("proposal asset has no wrappers"))?
                    .iter()
                    .map(|wrapper| {
                        wrapper
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| invalid_feed("proposal asset wrapper is invalid"))
                    })
                    .collect::<LinkResult<Vec<_>>>()?;
                let required = asset
                    .get("required")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| invalid_feed("proposal asset required flag is invalid"))?;
                let disposition = asset
                    .get("disposition")
                    .and_then(Value::as_str)
                    .filter(|value| matches!(*value, "hosted" | "withheld"))
                    .ok_or_else(|| invalid_feed("proposal asset disposition is invalid"))?;
                expected_candidate_assets.insert(
                    path.to_string(),
                    V2BaselineAsset {
                        blob_sha256: blob_sha256.to_string(),
                        bytes,
                        media_type: media_type.to_string(),
                        wrappers,
                        required,
                        disposition: disposition.to_string(),
                        leaf_hash: String::new(),
                    },
                );
            }
            _ => return Err(invalid_feed("proposal operation kind is unsupported")),
        }
    }
    let base = head.pointer.as_ref().map(|pointer| {
        json!({
            "seq": pointer.seq,
            "commit_hash": pointer.commit_hash,
            "content_root": pointer.content_root,
            "asset_root": pointer.asset_root,
        })
    });
    let mut body = json!({
        "mutation_id": mutation_id,
        "base": base,
        "rebase": "strict",
        "reason": reason,
        "operations": operations,
        "blobs": downloaded
            .iter()
            .map(|(sha256, bytes)| json!({
                "sha256": sha256,
                "bytes": bytes.len(),
                "content_base64": STANDARD.encode(bytes),
            }))
            .collect::<Vec<_>>(),
        "proposal_id": proposal_id,
        "proposal_mode": "exact",
    });
    let changed_bytes = downloaded.values().try_fold(0_usize, |total, bytes| {
        total
            .checked_add(bytes.len())
            .ok_or_else(|| LinkError::PushTooLarge {
                detail: "proposal changed-byte total overflow".to_string(),
            })
    })?;
    if changed_bytes > 3 * 1024 * 1024 || body.to_string().len() > MAX_PUSH_BYTES - 64 * 1024 {
        let mut coordinates_by_hash: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for operation in &operations {
            let Some(kind) = operation.get("op").and_then(Value::as_str) else {
                return Err(invalid_feed("proposal upload operation has no kind"));
            };
            let hash = match kind {
                "put" | "restore" | "rename" => operation.get("blob").and_then(Value::as_str),
                "asset_put" | "asset_resume" => operation
                    .get("asset")
                    .and_then(|asset| asset.get("blob_sha256"))
                    .and_then(Value::as_str),
                _ => None,
            };
            let Some(hash) = hash else { continue };
            let coordinates = coordinates_by_hash.entry(hash.to_string()).or_default();
            if kind == "rename" {
                for field in ["from", "to"] {
                    coordinates.insert(
                        operation
                            .get(field)
                            .and_then(Value::as_str)
                            .ok_or_else(|| invalid_feed("proposal rename has no coordinate"))?
                            .to_string(),
                    );
                }
            } else {
                let path = operation
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_feed("proposal upload has no coordinate"))?;
                coordinates.insert(if kind.starts_with("asset_") {
                    format!("assets/{path}")
                } else {
                    path.to_string()
                });
            }
        }
        let declarations = downloaded
            .iter()
            .map(|(sha256, bytes)| {
                json!({
                    "sha256": sha256,
                    "bytes": bytes.len(),
                    "coordinates": coordinates_by_hash
                        .get(sha256)
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let reserved = ensure_ok(
            request(
                cfg,
                "POST",
                &format!("/api/hub/brains/{}/v2/uploads", head.brain_id),
                Some(&json!({ "blobs": declarations })),
                Auth::Required,
            )?,
            "prepare proposal blob transport",
        )?;
        let items = reserved
            .get("uploads")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_feed("proposal upload reservation has no items"))?;
        if items.len() != downloaded.len() {
            return Err(invalid_feed("proposal upload reservation changed the set"));
        }
        let mut references = Vec::with_capacity(items.len());
        for item in items {
            let hash = item
                .get("sha256")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_feed("proposal upload reservation has no hash"))?;
            let bytes = downloaded
                .get(hash)
                .ok_or_else(|| invalid_feed("proposal upload reservation introduced a blob"))?;
            let reservation_id = item
                .get("reservation_id")
                .and_then(Value::as_str)
                .filter(|id| crate::ulid::is_ulid(id))
                .ok_or_else(|| invalid_feed("proposal upload reservation has no id"))?;
            let expected_coordinates = coordinates_by_hash
                .get(hash)
                .ok_or_else(|| invalid_feed("proposal upload has no coordinate binding"))?;
            let returned_coordinates = item
                .get("coordinates")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_feed("proposal upload reservation has no coordinates"))?;
            if returned_coordinates.len() != expected_coordinates.len()
                || returned_coordinates
                    .iter()
                    .zip(expected_coordinates)
                    .any(|(actual, expected)| actual.as_str() != Some(expected.as_str()))
            {
                return Err(invalid_feed(
                    "proposal upload reservation changed its coordinates",
                ));
            }
            match item.get("status").and_then(Value::as_str) {
                Some("upload") => put_presigned(
                    cfg,
                    item.get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid_feed("proposal upload has no URL"))?,
                    item.get("headers").unwrap_or(&Value::Null),
                    bytes,
                )?,
                Some("already_present") => {}
                _ => return Err(invalid_feed("proposal upload status is invalid")),
            }
            references.push(json!({
                "sha256": hash,
                "bytes": bytes.len(),
                "reservation_id": reservation_id,
            }));
        }
        body["blobs"] = Value::Array(references);
    }
    if body.to_string().len() > MAX_PUSH_BYTES {
        return Err(LinkError::PushTooLarge {
            detail: "proposal operation metadata exceeds the commit request cap".to_string(),
        });
    }
    let path = format!("/api/hub/brains/{}/v2/commits", head.brain_id);
    let mut result = ensure_ok(
        request(cfg, "POST", &path, Some(&body), Auth::Required)?,
        "exact proposal acceptance",
    )?;
    let mut candidate_hub_signer = None;
    if result.get("code").and_then(Value::as_str) == Some("brain_signature_required") {
        let challenge = result
            .get("signing_challenge")
            .ok_or_else(|| invalid_feed("proposal acceptance has no signing challenge"))?;
        let (challenge_id, signature, actor_signer) = sign_verified_v2_candidate(
            cfg,
            &head,
            &expected_candidate,
            &expected_candidate_assets,
            mutation_id,
            &body,
            challenge,
        )?;
        body["signing_challenge_id"] = Value::String(challenge_id);
        body["signature_base64url"] = Value::String(signature);
        candidate_hub_signer = Some(actor_signer);
        result = ensure_ok(
            request(cfg, "POST", &path, Some(&body), Auth::Required)?,
            "signed exact proposal acceptance",
        )?;
    }
    let refreshed = v2_verified_head(cfg, brain)?
        .ok_or_else(|| invalid_feed("v2 head disappeared after proposal acceptance"))?;
    if candidate_hub_signer
        .as_ref()
        .is_some_and(|expected| refreshed.trust.hub_signer.as_ref() != Some(expected))
        || refreshed
            .pointer
            .as_ref()
            .map(|pointer| pointer.commit_hash.as_str())
            != result.get("commit_hash").and_then(Value::as_str)
        || result.get("proposal_id").and_then(Value::as_str) != Some(proposal_id)
        || result.get("proposal_state").and_then(Value::as_str) != Some("accepted")
    {
        return Err(LinkError::RemoteAdvancedDuringSync);
    }
    accept_v2_head(cfg, &refreshed)?;
    Ok(result)
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V1DisclosureError {
    DuplicateFile,
    DuplicateRemoved,
    PushManifestMismatch,
    EditMissingChange,
    EditFalseFile,
    RemovedMismatch,
}

/// Validate wire-profile-v1 `files`/`removed` disclosure semantics against
/// the exact previous and resulting pack manifests. `edit` accepts the
/// historical superset form while still requiring every actual changed path.
#[cfg(test)]
fn verify_v1_manifest_disclosure(
    kind: &str,
    previous: &[FeedFile],
    resulting: &[FeedFile],
    files: &[FeedFile],
    removed: &[String],
) -> Result<(), V1DisclosureError> {
    fn as_map(
        files: &[FeedFile],
    ) -> Result<std::collections::BTreeMap<&str, (&str, u64)>, V1DisclosureError> {
        let mut result = std::collections::BTreeMap::new();
        for file in files {
            if result
                .insert(file.path.as_str(), (file.sha256.as_str(), file.bytes))
                .is_some()
            {
                return Err(V1DisclosureError::DuplicateFile);
            }
        }
        Ok(result)
    }
    let previous = as_map(previous)?;
    let resulting = as_map(resulting)?;
    let disclosed = as_map(files)?;
    let removed_set: std::collections::BTreeSet<&str> =
        removed.iter().map(String::as_str).collect();
    if removed_set.len() != removed.len() {
        return Err(V1DisclosureError::DuplicateRemoved);
    }
    let expected_removed: std::collections::BTreeSet<&str> = previous
        .keys()
        .copied()
        .filter(|path| !resulting.contains_key(path))
        .collect();
    if removed_set != expected_removed {
        return Err(V1DisclosureError::RemovedMismatch);
    }
    if kind == "push" {
        return if disclosed == resulting {
            Ok(())
        } else {
            Err(V1DisclosureError::PushManifestMismatch)
        };
    }
    if kind != "edit" {
        return Err(V1DisclosureError::EditFalseFile);
    }
    if disclosed
        .iter()
        .any(|(path, value)| resulting.get(path) != Some(value))
    {
        return Err(V1DisclosureError::EditFalseFile);
    }
    for (path, value) in &resulting {
        if previous.get(path) != Some(value) && !disclosed.contains_key(path) {
            return Err(V1DisclosureError::EditMissingChange);
        }
    }
    Ok(())
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
    /// Hub pointer-signing identity pinned on first v2 observation. This is
    /// separate from the brain key, which signs commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hub_signer: Option<String>,
    /// Explicit transport profile. Older accepted non-empty v2 checkpoints
    /// are recognized by their pinned hub signer during one-way migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol_profile: Option<String>,
}

fn accepted_as_v2(state: &TrustState) -> bool {
    state.protocol_profile.as_deref() == Some("link-v2") || state.hub_signer.is_some()
}

fn has_accepted_v2_ref(cfg: &HubConfig, requested: &str) -> LinkResult<bool> {
    let directory = open_trust_dir(cfg)?;
    if load_trust_in(cfg, &directory, requested)?.is_some_and(|state| accepted_as_v2(&state)) {
        return Ok(true);
    }
    let Some(alias) = load_alias_in(cfg, &directory, requested)? else {
        return Ok(false);
    };
    Ok(load_trust_in(cfg, &directory, &alias.brain)?.is_some_and(|state| accepted_as_v2(&state)))
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

#[cfg(any(unix, windows))]
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

#[cfg(windows)]
fn lock_trust_name(directory: &std::fs::File, state_name: &str) -> LinkResult<TrustLock> {
    let lock_name = format!(".{state_name}.lock");
    let file = crate::fsx::lock_exclusive_beneath(directory, Path::new(&lock_name))?;
    Ok(TrustLock { _file: file })
}

#[cfg(any(unix, windows))]
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

#[cfg(not(any(unix, windows)))]
fn lock_trust_many(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _refs: &[&str],
) -> LinkResult<Vec<()>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(any(unix, windows))]
type TrustDirectory = std::fs::File;

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
fn open_trust_dir(cfg: &HubConfig) -> LinkResult<TrustDirectory> {
    let marker = cfg.state_dir.join("trust").join(".directory");
    crate::fsx::write_atomic(&marker, b"link.md trust directory\n")?;
    Ok(crate::fsx::open_directory_nofollow(
        marker.parent().expect("trust marker has a parent"),
    )?)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
fn load_trust_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
) -> LinkResult<Option<TrustState>> {
    let name = trust_file_name(cfg, requested)?;
    let mut reader = crate::fsx::BoundedDirReader::from_root(directory)?;
    let bytes = match reader.read(Path::new(&name), 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LinkError::UnsafePath { path: name }),
    };
    let mut state: TrustState = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_feed("local identity/feed checkpoint is corrupt"))?;
    if !matches!(state.v, 1 | 2) || state.origin != normalized_origin(&cfg.hub)? {
        return Err(invalid_feed(
            "local identity/feed checkpoint does not match this hub and brain",
        ));
    }
    if state.v == 1 {
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

#[cfg(not(any(unix, windows)))]
fn load_trust_in(
    _cfg: &HubConfig,
    _directory: &TrustDirectory,
    _brain: &str,
) -> LinkResult<Option<TrustState>> {
    Err(LinkError::UnsupportedPlatform {
        operation: "verified link.md state",
    })
}

#[cfg(all(test, any(unix, windows)))]
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

#[cfg(windows)]
fn save_trust_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    state: &TrustState,
) -> LinkResult<()> {
    let name = trust_file_name(cfg, &state.requested)?;
    let mut bytes = serde_json::to_vec(state)
        .map_err(|_| invalid_feed("could not serialize local trust checkpoint"))?;
    bytes.push(b'\n');
    crate::fsx::write_atomic_beneath(directory, Path::new(&name), &bytes, false, true)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
fn load_alias_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    requested: &str,
) -> LinkResult<Option<AliasBinding>> {
    let name = alias_file_name(cfg, requested)?;
    let mut reader = crate::fsx::BoundedDirReader::from_root(directory)?;
    let bytes = match reader.read(Path::new(&name), 64 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LinkError::UnsafePath { path: name }),
    };
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

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
fn save_alias_in(
    cfg: &HubConfig,
    directory: &TrustDirectory,
    alias: &AliasBinding,
) -> LinkResult<()> {
    let name = alias_file_name(cfg, &alias.requested)?;
    let mut bytes = serde_json::to_vec(alias)
        .map_err(|_| invalid_feed("could not serialize local alias binding"))?;
    bytes.push(b'\n');
    crate::fsx::write_atomic_beneath(directory, Path::new(&name), &bytes, false, true)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
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
#[cfg_attr(windows, allow(unreachable_code, unused_variables))]
pub fn mirror(cfg: &HubConfig, brain: &str, dest: &Path) -> LinkResult<MirrorReport> {
    require_hardened_filesystem("mirror")?;
    require_safe_ref(brain)?;
    #[cfg(windows)]
    {
        let _ = (cfg, dest);
        return Err(LinkError::UnsupportedPlatform {
            operation: "atomic whole-mirror replacement on Windows",
        });
    }
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
                hub_signer: None,
                protocol_profile: None,
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
            hub_signer: pinned.as_ref().and_then(|state| state.hub_signer.clone()),
            protocol_profile: pinned
                .as_ref()
                .and_then(|state| state.protocol_profile.clone()),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_snapshot_install_is_atomic_for_create_and_exchange() {
        use std::os::fd::AsRawFd as _;

        let sandbox = tempfile::TempDir::new().unwrap();
        let parent = std::fs::File::open(sandbox.path()).unwrap();
        let stage = std::ffi::CString::new("stage").unwrap();
        let destination = std::ffi::CString::new("brain").unwrap();

        std::fs::create_dir(sandbox.path().join("stage")).unwrap();
        std::fs::write(sandbox.path().join("stage/value"), b"created").unwrap();
        install_stage_at(
            parent.as_raw_fd(),
            stage.as_c_str(),
            destination.as_c_str(),
            false,
        )
        .unwrap();
        assert!(!sandbox.path().join("stage").exists());
        assert_eq!(
            std::fs::read(sandbox.path().join("brain/value")).unwrap(),
            b"created"
        );

        std::fs::create_dir(sandbox.path().join("stage")).unwrap();
        std::fs::write(sandbox.path().join("stage/value"), b"replacement").unwrap();
        install_stage_at(
            parent.as_raw_fd(),
            stage.as_c_str(),
            destination.as_c_str(),
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(sandbox.path().join("brain/value")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(sandbox.path().join("stage/value")).unwrap(),
            b"created",
            "RENAME_EXCHANGE must leave the predecessor at the unique stage name"
        );
    }

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
    fn v1_edit_disclosure_accepts_minimal_and_superset_forms() {
        let file = |path: &str, byte: char| FeedFile {
            path: path.to_string(),
            sha256: byte.to_string().repeat(64),
            bytes: 1,
        };
        let a0 = file("records/a.md", 'a');
        let a1 = file("records/a.md", 'b');
        let stable = file("records/stable.md", 'c');
        let added = file("records/added.md", 'd');
        let removed_file = file("records/removed.md", 'e');
        let previous = vec![a0, stable.clone(), removed_file.clone()];
        let resulting = vec![a1.clone(), stable.clone(), added.clone()];
        let removed = vec![removed_file.path.clone()];

        assert_eq!(
            verify_v1_manifest_disclosure(
                "edit",
                &previous,
                &resulting,
                &[a1.clone(), added.clone()],
                &removed,
            ),
            Ok(())
        );
        assert_eq!(
            verify_v1_manifest_disclosure(
                "edit",
                &previous,
                &resulting,
                &[stable.clone(), added.clone(), a1.clone()],
                &removed,
            ),
            Ok(())
        );
        assert_eq!(
            verify_v1_manifest_disclosure(
                "edit",
                &previous,
                &resulting,
                std::slice::from_ref(&added),
                &removed,
            ),
            Err(V1DisclosureError::EditMissingChange)
        );
        assert_eq!(
            verify_v1_manifest_disclosure(
                "edit",
                &previous,
                &resulting,
                &[file("records/a.md", 'f'), added.clone()],
                &removed,
            ),
            Err(V1DisclosureError::EditFalseFile)
        );
        assert_eq!(
            verify_v1_manifest_disclosure(
                "edit",
                &previous,
                &resulting,
                &[a1.clone(), added.clone()],
                &[],
            ),
            Err(V1DisclosureError::RemovedMismatch)
        );
        assert_eq!(
            verify_v1_manifest_disclosure(
                "push",
                &previous,
                &resulting,
                &[added.clone(), stable, a1],
                &removed,
            ),
            Ok(())
        );
        assert_eq!(
            verify_v1_manifest_disclosure("push", &previous, &resulting, &[added], &removed,),
            Err(V1DisclosureError::PushManifestMismatch)
        );
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

    #[test]
    fn bulk_confirmation_parser_accepts_only_the_exact_wire_shape() {
        let id = "01arz3ndektsv4rrffq69g5fav";
        let digest = "a".repeat(64);
        assert_eq!(
            V2BulkConfirmation::parse(&format!("{id}:{digest}")).unwrap(),
            V2BulkConfirmation {
                id: id.to_string(),
                digest,
            }
        );
        for invalid in [
            "",
            "01arz3ndektsv4rrffq69g5fav",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "01arz3ndektsv4rrffq69g5fav:ABCDEF",
            "01arz3ndektsv4rrffq69g5fav:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(matches!(
                V2BulkConfirmation::parse(invalid),
                Err(LinkError::InvalidPack { .. })
            ));
        }
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
    fn v2_commit_requires_exact_fields_and_a_valid_genesis_bridge() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let (spki, multikey) = public_identity_for(&pair);
        let identity = V2HeadIdentity {
            custody: "self".to_string(),
            fingerprint: multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: spki.clone(),
            previous: Vec::new(),
            rotations: Vec::new(),
        };
        let unsigned = json!({
            "actor_ref": "a".repeat(64),
            "asset_root": Value::Null,
            "brain": multikey,
            "changes_sha256": "b".repeat(64),
            "control_revision": "c".repeat(64),
            "materializer": "dbmd-projection-v1",
            "op": "changeset",
            "parent_asset_root": Value::Null,
            "parent_commit": Value::Null,
            "parent_root": Value::Null,
            "prev_entry_hash": Value::Null,
            "public_key": spki,
            "seq": 1,
            "signer_epoch": 1,
            "state_root": "d".repeat(64),
            "ts": "2026-08-19T12:00:00.000Z",
            "v": 2,
            "v1_bridge": {
                "feed_hash": "e".repeat(64),
                "head_seq": 7,
                "pack_sha256": "f".repeat(64),
            },
        });
        let sign_value = |value: Value| {
            let message = crate::linkmd_v2::canonical_bytes(&value).unwrap();
            let mut object = value.as_object().unwrap().clone();
            object.insert(
                "sig".to_string(),
                Value::String(URL_SAFE_NO_PAD.encode(pair.sign(&message).as_ref())),
            );
            crate::linkmd_v2::canonical_bytes(&Value::Object(object)).unwrap()
        };
        assert!(verified_v2_commit_object(&sign_value(unsigned.clone()), &identity).is_ok());

        let mut extra = unsigned.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("future".to_string(), Value::Bool(true));
        assert!(verified_v2_commit_object(&sign_value(extra), &identity).is_err());

        let mut missing = unsigned.clone();
        missing.as_object_mut().unwrap().remove("v1_bridge");
        assert!(verified_v2_commit_object(&sign_value(missing), &identity).is_err());

        let mut invalid_bridge = unsigned;
        invalid_bridge.as_object_mut().unwrap().insert(
            "v1_bridge".to_string(),
            json!({"feed_hash": "e".repeat(64), "head_seq": 0, "pack_sha256": "f".repeat(64)}),
        );
        assert!(verified_v2_commit_object(&sign_value(invalid_bridge), &identity).is_err());
    }

    #[test]
    fn shared_v2_commit_bridge_vector_matches_the_typescript_signer() {
        let vector: Value = serde_json::from_str(include_str!(
            "../../../tests/vectors/linkmd-v2-commit-bridge.json"
        ))
        .unwrap();
        let identity_value = vector.get("identity").unwrap();
        let identity = V2HeadIdentity {
            custody: "self".to_string(),
            fingerprint: identity_value
                .get("fingerprint")
                .and_then(Value::as_str)
                .unwrap()
                .to_string(),
            public_key_spki: identity_value
                .get("public_key_spki")
                .and_then(Value::as_str)
                .unwrap()
                .to_string(),
            previous: Vec::new(),
            rotations: Vec::new(),
        };
        let private = URL_SAFE_NO_PAD
            .decode(
                identity_value
                    .get("private_key_pkcs8")
                    .and_then(Value::as_str)
                    .unwrap(),
            )
            .unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&private)
            .or_else(|_| ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&private))
            .unwrap();
        let base = vector.get("body").unwrap().as_object().unwrap();

        for item in vector.get("valid").unwrap().as_array().unwrap() {
            let mut body = base.clone();
            body.insert(
                "v1_bridge".to_string(),
                item.get("v1_bridge").unwrap().clone(),
            );
            body.insert(
                "sig".to_string(),
                item.get("signature_base64url").unwrap().clone(),
            );
            let signed = crate::linkmd_v2::canonical_bytes(&Value::Object(body)).unwrap();
            assert!(verified_v2_commit_object(&signed, &identity).is_ok());
            assert_eq!(
                crate::linkmd_v2::domain_hash_bytes("v2/commit", &signed).unwrap(),
                item.get("commit_hash").and_then(Value::as_str).unwrap()
            );
            assert_eq!(
                format!("{:x}", Sha256::digest(&signed)),
                item.get("feed_hash").and_then(Value::as_str).unwrap()
            );
        }

        for item in vector.get("invalid").unwrap().as_array().unwrap() {
            let mut body = base.clone();
            if let Some(remove) = item.get("remove").and_then(Value::as_array) {
                for field in remove {
                    body.remove(field.as_str().unwrap());
                }
            }
            if let Some(set) = item.get("set").and_then(Value::as_object) {
                for (field, value) in set {
                    body.insert(field.clone(), value.clone());
                }
            }
            let message = crate::linkmd_v2::canonical_bytes(&Value::Object(body.clone())).unwrap();
            body.insert(
                "sig".to_string(),
                Value::String(URL_SAFE_NO_PAD.encode(pair.sign(&message).as_ref())),
            );
            let signed = crate::linkmd_v2::canonical_bytes(&Value::Object(body)).unwrap();
            assert!(
                verified_v2_commit_object(&signed, &identity).is_err(),
                "accepted invalid shared vector {}",
                item.get("reason").and_then(Value::as_str).unwrap()
            );
        }
    }

    #[test]
    fn v2_profile_transition_reverifies_the_exact_signed_v1_bridge() {
        let remote = signed_remote_fixture();
        let legacy: FeedResponse = serde_json::from_str(&remote.feed).unwrap();
        let legacy_item = legacy.entries.first().unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&remote.key.pkcs8).unwrap();
        let body = json!({
            "actor_ref": "a".repeat(64),
            "asset_root": Value::Null,
            "brain": remote.key.multikey,
            "changes_sha256": "b".repeat(64),
            "control_revision": "c".repeat(64),
            "materializer": "dbmd-projection-v1",
            "op": "changeset",
            "parent_asset_root": Value::Null,
            "parent_commit": Value::Null,
            "parent_root": Value::Null,
            "prev_entry_hash": Value::Null,
            "public_key": remote.key.public_key_spki,
            "seq": 1,
            "signer_epoch": 1,
            "state_root": "d".repeat(64),
            "ts": "2026-08-19T12:00:00.000Z",
            "v": 2,
            "v1_bridge": {
                "feed_hash": legacy_item.hash,
                "head_seq": legacy_item.entry.seq,
                "pack_sha256": legacy_item.entry.pack_sha256,
            },
        });
        let message = crate::linkmd_v2::canonical_bytes(&body).unwrap();
        let mut signed = body.as_object().unwrap().clone();
        signed.insert(
            "sig".to_string(),
            Value::String(URL_SAFE_NO_PAD.encode(pair.sign(&message).as_ref())),
        );
        let raw = crate::linkmd_v2::canonical_bytes(&Value::Object(signed)).unwrap();
        let commit_hash = crate::linkmd_v2::domain_hash_bytes("v2/commit", &raw).unwrap();
        let feed_hash = content_sha256(&raw);
        let pointer = V2PointerBody {
            v: 2,
            brain: TEST_BRAIN_ID.to_string(),
            seq: 1,
            commit_hash: commit_hash.clone(),
            feed_hash: feed_hash.clone(),
            content_root: Some("d".repeat(64)),
            asset_root: None,
            materializer: "dbmd-projection-v1".to_string(),
            signer_epoch: 1,
            control_revision: "c".repeat(64),
            backup_preparation: "e".repeat(64),
            prior_pointer_hash: None,
            signed_at: "2026-08-19T12:00:00.000Z".to_string(),
        };
        let v2_page = json!({
            "v": 2,
            "head_seq": 1,
            "head_commit_hash": commit_hash,
            "head_feed_hash": feed_hash,
            "entries": [{
                "seq": 1,
                "commit_hash": pointer.commit_hash,
                "feed_hash": pointer.feed_hash,
                "bytes_base64": STANDARD.encode(&raw),
            }],
            "next_after": 1,
            "complete": true,
        })
        .to_string();
        let identity = V2HeadIdentity {
            custody: "self".to_string(),
            fingerprint: remote.identity.fingerprint.clone(),
            public_key_spki: remote.identity.public_key_spki.clone(),
            previous: Vec::new(),
            rotations: Vec::new(),
        };
        let checkpoint = TrustState {
            v: 2,
            origin: "unused".to_string(),
            requested: TEST_BRAIN_ID.to_string(),
            brain: TEST_BRAIN_ID.to_string(),
            home: None,
            anchor: remote.key.multikey.clone(),
            current: remote.key.multikey,
            head_seq: legacy_item.entry.seq,
            feed_hash: Some(legacy_item.hash.clone()),
            rotations: Vec::new(),
            hub_signer: None,
            protocol_profile: None,
        };
        let (hub, server) = scripted_json_hub(vec![(200, v2_page), (200, remote.feed)]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        verify_v1_to_v2_bridge(&cfg, TEST_BRAIN_ID, &pointer, &identity, &checkpoint).unwrap();
        server.join().unwrap();

        let mut wrong = checkpoint;
        wrong.feed_hash = Some("0".repeat(64));
        let (hub, server) = scripted_json_hub(vec![(
            200,
            json!({
                "v": 2,
                "head_seq": 1,
                "head_commit_hash": pointer.commit_hash,
                "head_feed_hash": pointer.feed_hash,
                "entries": [{
                    "seq": 1,
                    "commit_hash": pointer.commit_hash,
                    "feed_hash": pointer.feed_hash,
                    "bytes_base64": STANDARD.encode(&raw),
                }],
                "next_after": 1,
                "complete": true,
            })
            .to_string(),
        )]);
        let state = tempfile::tempdir().unwrap();
        let cfg = test_hub_config(hub, state.path().to_path_buf());
        assert!(verify_v1_to_v2_bridge(&cfg, TEST_BRAIN_ID, &pointer, &identity, &wrong,).is_err());
        server.join().unwrap();
    }

    #[test]
    fn v2_commit_signer_epoch_follows_the_authenticated_rotation_boundary() {
        let rng = ring::rand::SystemRandom::new();
        let old_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let old = ring::signature::Ed25519KeyPair::from_pkcs8(old_pkcs8.as_ref()).unwrap();
        let new_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let new = ring::signature::Ed25519KeyPair::from_pkcs8(new_pkcs8.as_ref()).unwrap();
        let (old_spki, old_multikey) = public_identity_for(&old);
        let (new_spki, new_multikey) = public_identity_for(&new);
        let rotation_unsigned = serde_json::to_string(&UnsignedRotation {
            v: 1,
            op: "rotate",
            brain: &old_multikey,
            public_key: &old_spki,
            new_brain: &new_multikey,
            new_public_key: &new_spki,
            prior_head_seq: 1,
            prior_feed_hash: Some(&"9".repeat(64)),
            ts: "2026-08-19T12:01:00.000Z".to_string(),
        })
        .unwrap();
        let rotation_sig = URL_SAFE_NO_PAD.encode(old.sign(rotation_unsigned.as_bytes()).as_ref());
        let rotation = format!(
            "{},\"sig\":\"{}\"}}",
            &rotation_unsigned[..rotation_unsigned.len() - 1],
            rotation_sig
        );
        let identity = V2HeadIdentity {
            custody: "self".to_string(),
            fingerprint: new_multikey.trim_start_matches("ed25519:").to_string(),
            public_key_spki: new_spki.clone(),
            previous: vec![V2PreviousIdentity {
                fingerprint: old_multikey.trim_start_matches("ed25519:").to_string(),
                public_key_spki: old_spki.clone(),
            }],
            rotations: vec![rotation],
        };
        let commit = |seq: u64,
                      epoch: u64,
                      multikey: &str,
                      spki: &str,
                      pair: &ring::signature::Ed25519KeyPair| {
            let value = json!({
                "actor_ref": "a".repeat(64),
                "asset_root": Value::Null,
                "brain": multikey,
                "changes_sha256": "b".repeat(64),
                "control_revision": "c".repeat(64),
                "materializer": "dbmd-projection-v1",
                "op": "changeset",
                "parent_asset_root": Value::Null,
                "parent_commit": if seq == 1 { Value::Null } else { Value::String("d".repeat(64)) },
                "parent_root": if seq == 1 { Value::Null } else { Value::String("e".repeat(64)) },
                "prev_entry_hash": if seq == 1 { Value::Null } else { Value::String("f".repeat(64)) },
                "public_key": spki,
                "seq": seq,
                "signer_epoch": epoch,
                "state_root": "1".repeat(64),
                "ts": "2026-08-19T12:00:00.000Z",
                "v": 2,
                "v1_bridge": Value::Null,
            });
            let message = crate::linkmd_v2::canonical_bytes(&value).unwrap();
            let mut object = value.as_object().unwrap().clone();
            object.insert(
                "sig".to_string(),
                Value::String(URL_SAFE_NO_PAD.encode(pair.sign(&message).as_ref())),
            );
            crate::linkmd_v2::canonical_bytes(&Value::Object(object)).unwrap()
        };

        assert!(verified_v2_commit_object(
            &commit(1, 1, &old_multikey, &old_spki, &old),
            &identity,
        )
        .is_ok());
        assert!(verified_v2_commit_object(
            &commit(2, 2, &new_multikey, &new_spki, &new),
            &identity,
        )
        .is_ok());
        assert!(verified_v2_commit_object(
            &commit(2, 1, &old_multikey, &old_spki, &old),
            &identity,
        )
        .is_err());
        assert!(verified_v2_commit_object(
            &commit(1, 2, &new_multikey, &new_spki, &new),
            &identity,
        )
        .is_err());
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
            hub_signer: None,
            protocol_profile: None,
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
    fn presigned_download_ceiling_cannot_overflow_at_u64_max() {
        assert_eq!(
            one_past_bounded_limit(MAX_PACK_BYTES),
            Some(MAX_PACK_BYTES + 1),
            "the presigned reader consumes exactly one refusal byte beyond its fixed pack cap"
        );
        assert_eq!(
            presigned_download_read_limit(),
            MAX_PACK_BYTES + 1,
            "the presigned reader is capped by the client constant, not a hub response"
        );
        assert_eq!(
            one_past_bounded_limit(u64::MAX),
            None,
            "the former attacker-controlled u64::MAX + 1 shape must fail without overflow"
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

    #[test]
    fn v2_final_barrier_refuses_to_advance_a_remote_ahead_checkout() {
        let mut local = std::collections::BTreeMap::new();
        local.insert("records/a.md".to_string(), ("a".repeat(64), 0));
        local.insert("records/b.md".to_string(), ("b".repeat(64), 0));
        let mut remote = std::collections::BTreeMap::new();
        remote.insert(
            "records/a.md".to_string(),
            V2BaselineFile {
                sha256: "c".repeat(64),
                bytes: 1,
                proof: None,
            },
        );
        remote.insert(
            "records/b.md".to_string(),
            V2BaselineFile {
                sha256: "b".repeat(64),
                bytes: 1,
                proof: None,
            },
        );
        assert!(!v2_riding_matches_remote(&local, &remote, |_| false));
    }

    #[test]
    fn v2_final_barrier_ignores_only_explicit_kept_home_paths() {
        let local = std::collections::BTreeMap::new();
        let mut remote = std::collections::BTreeMap::new();
        remote.insert(
            "private/local.md".to_string(),
            V2BaselineFile {
                sha256: "d".repeat(64),
                bytes: 1,
                proof: None,
            },
        );
        assert!(v2_riding_matches_remote(&local, &remote, |path| path == "private/local.md"));
        assert!(!v2_riding_matches_remote(&local, &remote, |_| false));
    }

    fn scoped_test_head(revision: &str) -> V2VerifiedHead {
        V2VerifiedHead {
            requested: TEST_BRAIN_ID.to_string(),
            brain_id: TEST_BRAIN_ID.to_string(),
            view_kind: "scoped".to_string(),
            view_revision: revision.to_string(),
            identity: V2HeadIdentity {
                custody: "hub".to_string(),
                fingerprint: "test".to_string(),
                public_key_spki: "test".to_string(),
                previous: Vec::new(),
                rotations: Vec::new(),
            },
            pointer: None,
            trust: TrustState {
                v: 2,
                origin: "https://hub.example".to_string(),
                requested: TEST_BRAIN_ID.to_string(),
                brain: TEST_BRAIN_ID.to_string(),
                home: None,
                anchor: "ed25519:test".to_string(),
                current: "ed25519:test".to_string(),
                head_seq: 0,
                feed_hash: None,
                rotations: Vec::new(),
                hub_signer: None,
                protocol_profile: Some("link-v2".to_string()),
            },
            alias: None,
        }
    }

    #[test]
    fn accepted_v2_checkpoint_never_downgrades_after_profile_migration() {
        let mut trust = scoped_test_head(&"a".repeat(64)).trust;
        assert!(accepted_as_v2(&trust));

        trust.protocol_profile = None;
        trust.hub_signer = Some("ed25519:hub".to_string());
        assert!(accepted_as_v2(&trust));

        trust.hub_signer = None;
        assert!(!accepted_as_v2(&trust));
    }

    fn scoped_test_baseline(revision: &str) -> V2SyncBaseline {
        V2SyncBaseline {
            v: 2,
            origin: "https://hub.example".to_string(),
            brain: TEST_BRAIN_ID.to_string(),
            head_seq: Some(0),
            commit_hash: None,
            content_root: None,
            asset_root: None,
            assets: std::collections::BTreeMap::new(),
            view_kind: Some("scoped".to_string()),
            view_revision: Some(revision.to_string()),
            projection_sha256: Some(scoped_projection_sha256(TEST_BRAIN_ID)),
            files: std::collections::BTreeMap::new(),
            local_policy_digest: None,
            local_eligibility: std::collections::BTreeMap::new(),
            remote_copy_remains: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn scoped_projection_is_a_valid_local_store_marker_but_never_rides() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("DB.md"),
            scoped_projection_bytes(TEST_BRAIN_ID),
        )
        .unwrap();
        let store = Store::open_strict(directory.path()).unwrap();
        let head = scoped_test_head(&"a".repeat(64));
        let baseline = scoped_test_baseline(&"a".repeat(64));
        let mut view = v2_local_files(&store).unwrap();
        remove_scoped_projection(&head, Some(&baseline), &mut view).unwrap();
        assert!(!view.riding.contains_key("DB.md"));
        assert!(!view.eligibility.contains_key("DB.md"));
    }

    #[test]
    fn scoped_projection_edit_and_scope_transition_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("DB.md"),
            b"---\ntype: db-md\nscope: company\nowner: someone\n---\n",
        )
        .unwrap();
        let store = Store::open_strict(directory.path()).unwrap();
        let head = scoped_test_head(&"a".repeat(64));
        let baseline = scoped_test_baseline(&"a".repeat(64));
        let mut view = v2_local_files(&store).unwrap();
        assert!(matches!(
            remove_scoped_projection(&head, Some(&baseline), &mut view),
            Err(LinkError::ScopedProjectionModified)
        ));

        let changed = scoped_test_head(&"b".repeat(64));
        assert!(matches!(
            ensure_v2_view_compatible(&changed, Some(&baseline)),
            Err(LinkError::ScopedViewChanged)
        ));
    }

    #[test]
    fn scoped_view_metadata_is_explicitly_non_authoritative() {
        let head = scoped_test_head(&"a".repeat(64));
        let value: Value =
            serde_json::from_slice(&scoped_view_metadata(&head, 7).unwrap()).unwrap();
        assert_eq!(value["kind"], "link.md-scoped-view");
        assert_eq!(value["authoritative"], false);
        assert_eq!(value["visible_files"], 7);
        assert_eq!(value["brain"], TEST_BRAIN_ID);
    }

    #[test]
    fn local_scoped_marker_requires_the_exact_generated_projection() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".dbmd")).unwrap();
        std::fs::write(
            directory.path().join("DB.md"),
            scoped_projection_bytes(TEST_BRAIN_ID),
        )
        .unwrap();
        let head = scoped_test_head(&"a".repeat(64));
        std::fs::write(
            directory.path().join(".dbmd/view.json"),
            scoped_view_metadata(&head, 0).unwrap(),
        )
        .unwrap();
        let store = Store::open_strict(directory.path()).unwrap();
        assert!(has_verified_local_scoped_view(&store));

        std::fs::write(
            directory.path().join("DB.md"),
            b"---\ntype: db-md\nscope: company\nowner: altered\n---\n",
        )
        .unwrap();
        let altered = Store::open_strict(directory.path()).unwrap();
        assert!(!has_verified_local_scoped_view(&altered));
    }

    fn signed_proposal_fixture() -> (V2VerifiedHead, String, Value) {
        use ring::signature::KeyPair as _;

        let proposal_id = "01k2r7bm9w5x6e8nq3tjhv4cya".to_string();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public_der = [ED25519_SPKI_PREFIX.as_slice(), pair.public_key().as_ref()].concat();
        let public_key = URL_SAFE_NO_PAD.encode(&public_der);
        let fingerprint = format!("{:x}", Sha256::digest(&public_der));
        let blob = b"new";
        let blob_hash = content_sha256(blob);
        let changes = json!({
            "mutation_id": "sync:proposal-fixture",
            "operations": [{
                "blob": blob_hash,
                "bytes": blob.len(),
                "expected": null,
                "op": "put",
                "path": "records/new.md",
            }],
            "reason": "fixture",
            "v": 2,
        });
        let changes_bytes = crate::linkmd_v2::canonical_bytes(&changes).unwrap();
        let changes_base64 = STANDARD.encode(&changes_bytes);
        let descriptor = json!({
            "base": null,
            "blobs": [{ "bytes": blob.len(), "sha256": blob_hash }],
            "changes_base64": changes_base64,
            "rebase": "strict",
            "v": 2,
        });
        let clear_hash = content_sha256(&crate::linkmd_v2::canonical_bytes(&descriptor).unwrap());
        let payload_hash = "b".repeat(64);
        let submitted_at = "2026-08-19T12:00:00.000Z";
        let claim = json!({
            "actor_root": {
                "actor_class": "foreign_key",
                "credential": "ed25519:fixture",
                "grants": ["01k2r7bm9w5x6e8nq3tjhv4cyb"],
                "organization": "01k2r7bm9w5x6e8nq3tjhv4cyc",
                "principal": "key:fixture",
                "role": null,
            },
            "brain": TEST_BRAIN_ID,
            "clear_sha256": clear_hash,
            "control_revision": "c".repeat(64),
            "mutation_id": "sync:proposal-fixture",
            "payload_sha256": payload_hash,
            "proposal_id": proposal_id,
            "submitted_at": submitted_at,
            "v": 2,
        });
        let claim_bytes = crate::linkmd_v2::canonical_bytes(&claim).unwrap();
        let envelope = json!({
            "claim": claim,
            "fingerprint": fingerprint,
            "public_key": public_key,
            "sig": URL_SAFE_NO_PAD.encode(pair.sign(&claim_bytes).as_ref()),
        });
        let envelope_bytes = crate::linkmd_v2::canonical_bytes(&envelope).unwrap();
        let submission_hash =
            crate::linkmd_v2::domain_hash_bytes("v2/proposal-claim", &envelope_bytes).unwrap();
        let mut head = scoped_test_head(&"c".repeat(64));
        head.view_kind = "full".to_string();
        head.trust.hub_signer = Some(format!("{fingerprint}:{public_key}"));
        let value = json!({
            "proposal": {
                "base": null,
                "blobs": [{
                    "bytes": blob.len(),
                    "endpoint": format!(
                        "/api/hub/brains/{TEST_BRAIN_ID}/v2/proposals/{proposal_id}/blob?sha256={blob_hash}"
                    ),
                    "sha256": blob_hash,
                }],
                "changes_base64": changes_base64,
                "clear_sha256": clear_hash,
                "expires_at": "2026-08-26T12:00:00.000Z",
                "id": proposal_id,
                "payload_sha256": payload_hash,
                "proposer": { "class": "foreign_key" },
                "rebase": "strict",
                "state": "pending",
                "submission_claim_base64": STANDARD.encode(envelope_bytes),
                "submission_claim_sha256": submission_hash,
                "submitted_at": submitted_at,
            },
            "v": 2,
        });
        (head, proposal_id, value)
    }

    #[test]
    fn v2_proposal_verifier_accepts_exact_signed_payload() {
        let (head, proposal_id, value) = signed_proposal_fixture();
        let verified = verify_v2_proposal_value(&head, &proposal_id, value).unwrap();
        assert_eq!(verified.blobs.len(), 1);
        assert_eq!(verified.changes["operations"][0]["path"], "records/new.md");
    }

    #[test]
    fn v2_proposal_verifier_refuses_payload_endpoint_and_signature_tampering() {
        let (head, proposal_id, value) = signed_proposal_fixture();

        let mut changed = value.clone();
        changed["proposal"]["changes_base64"] = Value::String(STANDARD.encode(b"{}"));
        assert!(verify_v2_proposal_value(&head, &proposal_id, changed).is_err());

        let mut redirected = value.clone();
        redirected["proposal"]["blobs"][0]["endpoint"] =
            Value::String("https://attacker.example/blob".to_string());
        assert!(verify_v2_proposal_value(&head, &proposal_id, redirected).is_err());

        let mut forged = value;
        let encoded = forged["proposal"]["submission_claim_base64"]
            .as_str()
            .unwrap();
        let mut envelope: Value =
            serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
        envelope["sig"] = Value::String(URL_SAFE_NO_PAD.encode([0_u8; 64]));
        let bytes = crate::linkmd_v2::canonical_bytes(&envelope).unwrap();
        forged["proposal"]["submission_claim_base64"] = Value::String(STANDARD.encode(&bytes));
        forged["proposal"]["submission_claim_sha256"] = Value::String(
            crate::linkmd_v2::domain_hash_bytes("v2/proposal-claim", &bytes).unwrap(),
        );
        assert!(verify_v2_proposal_value(&head, &proposal_id, forged).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn v2_atomic_install_materializes_only_local_catalogs_before_swap() {
        let sandbox = tempfile::tempdir().unwrap();
        let destination = sandbox.path().join("brain");
        let entries = vec![
            (
                "DB.md".to_string(),
                scoped_projection_bytes(TEST_BRAIN_ID),
            ),
            (
                "records/contacts/a.md".to_string(),
                b"---\ntype: contact\ncreated: 2026-08-19T00:00:00Z\nupdated: 2026-08-19T00:00:00Z\nsummary: Visible contact\n---\n\n# A\n"
                    .to_vec(),
            ),
        ];
        install_pulled_delta(&destination, &entries, &[], true).unwrap();
        assert!(destination.join("index.md").is_file());
        assert!(destination.join("records/index.md").is_file());
        assert!(destination.join("records/contacts/index.md").is_file());
        assert!(destination.join("records/contacts/index.jsonl").is_file());
    }

    #[test]
    fn v2_bulk_stream_is_exact_ordered_and_tamper_evident() {
        let body = b"bounded bytes";
        let path = "records/example.md".to_string();
        let file = V2BaselineFile {
            sha256: content_sha256(body),
            bytes: body.len() as u64,
            proof: None,
        };
        let header = serde_json::to_vec(&json!({
            "bytes": body.len(),
            "path": path,
            "sha256": file.sha256,
            "v": 2,
        }))
        .unwrap();
        let mut stream = V2_BULK_STREAM_MAGIC.to_vec();
        stream.extend_from_slice(&(header.len() as u32).to_be_bytes());
        stream.extend_from_slice(&header);
        stream.extend_from_slice(body);
        stream.extend_from_slice(&0_u32.to_be_bytes());
        let parsed = parse_v2_bulk_stream(&stream, &[(&path, &file)]).unwrap();
        assert_eq!(parsed, vec![(path.clone(), body.to_vec())]);

        let mut tampered = stream.clone();
        let body_offset = V2_BULK_STREAM_MAGIC.len() + 4 + header.len();
        tampered[body_offset] ^= 1;
        assert!(parse_v2_bulk_stream(&tampered, &[(&path, &file)]).is_err());

        let mut trailing = stream;
        trailing.push(0);
        assert!(parse_v2_bulk_stream(&trailing, &[(&path, &file)]).is_err());
    }

    #[test]
    fn conflict_cache_prunes_only_expired_or_incomplete_state_by_default() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let root = sandbox.path().join("brain");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("DB.md"),
            b"---\ntype: db-md\nscope: company\n---\n\n# Test\n",
        )
        .unwrap();
        let store = Store::open_strict(&root).unwrap();
        let incomplete = crate::ulid::mint();
        store
            .create_dir_all(&v2_conflict_relative(&incomplete, "files"))
            .unwrap();
        let expired = crate::ulid::mint();
        store
            .create_dir_all(&v2_conflict_relative(&expired, "files"))
            .unwrap();
        let plan = V2ConflictPlan {
            v: 2,
            class: "content_resolution_required".to_string(),
            bundle: expired.clone(),
            brain: TEST_BRAIN_ID.to_string(),
            origin: "https://example.test".to_string(),
            created_unix: 0,
            expires_unix: 0,
            base_seq: None,
            base_commit: None,
            remote_seq: 0,
            remote_commit: None,
            remote_content_root: None,
            view_kind: "full".to_string(),
            view_revision: "a".repeat(64),
            files: vec![V2ConflictFile {
                path: "records/value.md".to_string(),
                base: V2ConflictCoordinate {
                    sha256: None,
                    bytes: None,
                    file: None,
                },
                local: V2ConflictCoordinate {
                    sha256: None,
                    bytes: None,
                    file: None,
                },
                remote: V2ConflictCoordinate {
                    sha256: None,
                    bytes: None,
                    file: None,
                },
            }],
        };
        let mut bytes = serde_json::to_vec(&plan).unwrap();
        bytes.push(b'\n');
        store
            .write_atomic_new(&v2_conflict_relative(&expired, "plan.json"), &bytes)
            .unwrap();

        let listed = sync_conflicts(&root, false, false).unwrap();
        assert_eq!(listed["bundles"], 2);
        assert_eq!(listed["pruned"], 0);
        let pruned = sync_conflicts(&root, true, false).unwrap();
        assert_eq!(pruned["bundles"], 0);
        assert_eq!(pruned["pruned"], 2);
        assert!(!root.join(".dbmd/conflicts").join(incomplete).exists());
        assert!(!root.join(".dbmd/conflicts").join(expired).exists());
    }

    #[test]
    fn corrupt_completed_conflict_state_requires_explicit_discard_all() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let root = sandbox.path().join("brain");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("DB.md"),
            b"---\ntype: db-md\nscope: company\n---\n\n# Test\n",
        )
        .unwrap();
        let store = Store::open_strict(&root).unwrap();
        let bundle = crate::ulid::mint();
        store
            .create_dir_all(&v2_conflict_relative(&bundle, "files"))
            .unwrap();
        store
            .write_atomic_new(&v2_conflict_relative(&bundle, "plan.json"), b"not-json\n")
            .unwrap();

        assert!(sync_conflicts(&root, true, false).is_err());
        assert!(sync_conflicts(&root, false, true).is_err());
        let pruned = sync_conflicts(&root, true, true).unwrap();
        assert_eq!(pruned["pruned"], 1);
        assert!(!root.join(".dbmd/conflicts").join(bundle).exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_ready_pull_journal_rolls_back_exact_preimages() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let root = sandbox.path().join("brain");
        std::fs::create_dir_all(root.join("records")).unwrap();
        std::fs::write(
            root.join("DB.md"),
            b"---\ntype: db-md\nscope: company\n---\n\n# Test\n",
        )
        .unwrap();
        let path = "records/value.md";
        let old = b"---\ntype: note\n---\n\nold\n";
        let new = b"---\ntype: note\n---\n\nnew\n";
        std::fs::write(root.join(path), old).unwrap();
        let store = Store::open_strict(&root).unwrap();
        let bundle = crate::ulid::mint();
        let backup_dir = format!(".dbmd/pull-backup-{bundle}");
        store.create_dir_all(Path::new(&backup_dir)).unwrap();
        store
            .write_atomic_new(&Path::new(&backup_dir).join("00000000"), old)
            .unwrap();
        let journal = WindowsPullJournal {
            v: 1,
            phase: WindowsPullPhase::Ready,
            brain: TEST_BRAIN_ID.to_string(),
            previous: WindowsPullCoordinate {
                head_seq: Some(1),
                commit_hash: Some("a".repeat(64)),
                view_kind: Some("full".to_string()),
                view_revision: Some("b".repeat(64)),
            },
            next: WindowsPullCoordinate {
                head_seq: Some(2),
                commit_hash: Some("c".repeat(64)),
                view_kind: Some("full".to_string()),
                view_revision: Some("d".repeat(64)),
            },
            backup_dir: backup_dir.clone(),
            entries: vec![WindowsPullJournalEntry {
                path: path.to_string(),
                old: Some(WindowsPullFileCoordinate {
                    sha256: content_sha256(old),
                    bytes: old.len() as u64,
                }),
                new: Some(WindowsPullFileCoordinate {
                    sha256: content_sha256(new),
                    bytes: new.len() as u64,
                }),
                backup: Some("00000000".to_string()),
            }],
        };
        validate_windows_pull_journal(&journal).unwrap();
        store
            .write_atomic_new(
                Path::new(WINDOWS_PULL_JOURNAL),
                &windows_pull_journal_bytes(&journal).unwrap(),
            )
            .unwrap();
        store.write_atomic(Path::new(path), new).unwrap();

        let cfg = test_hub_config(
            "https://example.test".to_string(),
            sandbox.path().join("state"),
        );
        recover_windows_v2_pull(&cfg, TEST_BRAIN_ID, &root).unwrap();
        assert_eq!(std::fs::read(root.join(path)).unwrap(), old);
        assert!(!root.join(WINDOWS_PULL_JOURNAL).exists());
        assert!(!root.join(backup_dir).exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_preparing_pull_journal_discards_only_private_staging() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let root = sandbox.path().join("brain");
        std::fs::create_dir_all(root.join(".dbmd")).unwrap();
        std::fs::write(
            root.join("DB.md"),
            b"---\ntype: db-md\nscope: company\n---\n\n# Test\n",
        )
        .unwrap();
        let store = Store::open_strict(&root).unwrap();
        let bundle = crate::ulid::mint();
        let backup_dir = format!(".dbmd/pull-backup-{bundle}");
        store.create_dir_all(Path::new(&backup_dir)).unwrap();
        let journal = WindowsPullJournal {
            v: 1,
            phase: WindowsPullPhase::Preparing,
            brain: TEST_BRAIN_ID.to_string(),
            previous: WindowsPullCoordinate {
                head_seq: None,
                commit_hash: None,
                view_kind: None,
                view_revision: None,
            },
            next: WindowsPullCoordinate {
                head_seq: Some(1),
                commit_hash: Some("a".repeat(64)),
                view_kind: Some("full".to_string()),
                view_revision: Some("b".repeat(64)),
            },
            backup_dir: backup_dir.clone(),
            entries: vec![WindowsPullJournalEntry {
                path: "records/new.md".to_string(),
                old: None,
                new: Some(WindowsPullFileCoordinate {
                    sha256: "c".repeat(64),
                    bytes: 1,
                }),
                backup: None,
            }],
        };
        store
            .write_atomic_new(
                Path::new(WINDOWS_PULL_JOURNAL),
                &windows_pull_journal_bytes(&journal).unwrap(),
            )
            .unwrap();
        let cfg = test_hub_config(
            "https://example.test".to_string(),
            sandbox.path().join("state"),
        );

        recover_windows_v2_pull(&cfg, TEST_BRAIN_ID, &root).unwrap();

        assert!(root.join("DB.md").is_file());
        assert!(!root.join(WINDOWS_PULL_JOURNAL).exists());
        assert!(!root.join(backup_dir).exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_committed_baseline_keeps_installed_bytes_and_prunes_orphans() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let root = sandbox.path().join("brain");
        std::fs::create_dir_all(root.join("records")).unwrap();
        std::fs::write(
            root.join("DB.md"),
            b"---\ntype: db-md\nscope: company\n---\n\n# Test\n",
        )
        .unwrap();
        let new = b"---\ntype: note\n---\n\nnew\n";
        std::fs::write(root.join("records/value.md"), new).unwrap();
        let store = Store::open_strict(&root).unwrap();
        let bundle = crate::ulid::mint();
        let backup_dir = format!(".dbmd/pull-backup-{bundle}");
        store.create_dir_all(Path::new(&backup_dir)).unwrap();
        let orphan = format!(".dbmd/pull-backup-{}", crate::ulid::mint());
        store.create_dir_all(Path::new(&orphan)).unwrap();
        let next = WindowsPullCoordinate {
            head_seq: Some(2),
            commit_hash: Some("c".repeat(64)),
            view_kind: Some("full".to_string()),
            view_revision: Some("d".repeat(64)),
        };
        let journal = WindowsPullJournal {
            v: 1,
            phase: WindowsPullPhase::Ready,
            brain: TEST_BRAIN_ID.to_string(),
            previous: WindowsPullCoordinate {
                head_seq: Some(1),
                commit_hash: Some("a".repeat(64)),
                view_kind: Some("full".to_string()),
                view_revision: Some("b".repeat(64)),
            },
            next: next.clone(),
            backup_dir: backup_dir.clone(),
            entries: vec![WindowsPullJournalEntry {
                path: "records/value.md".to_string(),
                old: Some(WindowsPullFileCoordinate {
                    sha256: "e".repeat(64),
                    bytes: new.len() as u64,
                }),
                new: Some(WindowsPullFileCoordinate {
                    sha256: content_sha256(new),
                    bytes: new.len() as u64,
                }),
                backup: Some("00000000".to_string()),
            }],
        };
        store
            .write_atomic_new(
                Path::new(WINDOWS_PULL_JOURNAL),
                &windows_pull_journal_bytes(&journal).unwrap(),
            )
            .unwrap();
        let cfg = test_hub_config(
            "https://example.test".to_string(),
            sandbox.path().join("state"),
        );
        save_v2_baseline(
            &cfg,
            TEST_BRAIN_ID,
            &root,
            &V2SyncBaseline {
                v: 2,
                origin: "https://example.test".to_string(),
                brain: TEST_BRAIN_ID.to_string(),
                head_seq: next.head_seq,
                commit_hash: next.commit_hash.clone(),
                content_root: Some("f".repeat(64)),
                asset_root: None,
                assets: Default::default(),
                view_kind: next.view_kind.clone(),
                view_revision: next.view_revision.clone(),
                projection_sha256: None,
                files: Default::default(),
                local_policy_digest: None,
                local_eligibility: Default::default(),
                remote_copy_remains: Default::default(),
            },
        )
        .unwrap();

        recover_windows_v2_pull(&cfg, TEST_BRAIN_ID, &root).unwrap();

        assert_eq!(std::fs::read(root.join("records/value.md")).unwrap(), new);
        assert!(!root.join(WINDOWS_PULL_JOURNAL).exists());
        assert!(!root.join(backup_dir).exists());
        assert!(!root.join(orphan).exists());
    }
}
