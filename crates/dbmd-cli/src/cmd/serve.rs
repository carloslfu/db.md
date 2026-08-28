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

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;

use crate::cli::ServeArgs;
use crate::cmd::httpd::{
    self, respond, DeadlineWriter, Timing as ServeTiming, MAX_CONCURRENT_CLIENTS,
};
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
    /// Rotation history, served verbatim so old entries keep verifying.
    previous: serde_json::Value,
    /// Exact old-key-signed rotation statements, oldest first.
    rotations: serde_json::Value,
    /// Exact signed snapshot pack preserved by `mirror`, retained as the same
    /// no-follow file capability whose bytes were hashed at startup.
    snapshot_pack: Option<SnapshotPack>,
    pack_sha256: Option<String>,
    base_url: String,
}

struct SnapshotPack {
    file: std::fs::File,
    len: u64,
}

const MAX_SERVED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MIRROR_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_MIRROR_FEED_ENTRIES: u64 = 100_000;
const MAX_MIRROR_FEED_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(unix)]
fn open_dir_path_nofollow(path: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

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

    let mut directory = std::fs::File::open(if normalized.is_absolute() { "/" } else { "." })
        .map_err(|e| e.to_string())?;
    for component in normalized.components() {
        use std::path::Component;
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err("mirror path contains an unsafe parent component".to_string());
            }
        };
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| "mirror path contains a NUL byte".to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err("mirror path contains a symlink or non-directory".to_string());
        }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_regular_beneath(
    root: &std::fs::File,
    relative: &str,
    max_bytes: u64,
) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let components: Vec<&str> = relative.split('/').collect();
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| "empty mirror metadata path".to_string())?;
    let mut directory = root.try_clone().map_err(|e| e.to_string())?;
    for component in parents {
        let name = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| "invalid mirror metadata path".to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(format!("{relative} has a symlinked or missing ancestor"));
        }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let leaf = std::ffi::CString::new(leaf.as_bytes())
        .map_err(|_| "invalid mirror metadata path".to_string())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(format!("{relative} is missing, symlinked, or unreadable"));
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(format!("{relative} is not a bounded regular file"));
    }
    Ok(file)
}

#[cfg(unix)]
fn read_regular_beneath(
    root: &std::fs::File,
    relative: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let file = open_regular_beneath(root, relative, max_bytes)?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{relative} exceeds the read limit"));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn snapshot_pack_copy(
    mirror: &std::fs::File,
    max_bytes: u64,
) -> Result<(SnapshotPack, String), String> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let mut source = open_regular_beneath(mirror, "snapshot.pack", max_bytes)?;
    let expected_len = source.metadata().map_err(|e| e.to_string())?.len();
    let template = std::env::temp_dir().join("dbmd-serve-pack.XXXXXX");
    let mut template = std::ffi::CString::new(template.as_os_str().as_bytes())
        .map_err(|_| "temporary directory path contains a NUL byte".to_string())?
        .into_bytes_with_nul();
    let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
    if fd < 0 {
        return Err("could not create a private temporary snapshot copy".to_string());
    }
    let mut private = unsafe { std::fs::File::from_raw_fd(fd) };
    // The open descriptor survives unlink; no other process can subsequently
    // open or mutate the authenticated bytes we retain for serving.
    if unsafe { libc::unlink(template.as_ptr().cast()) } != 0 {
        return Err("could not unlink the private temporary snapshot copy".to_string());
    }
    let copied = std::io::copy(&mut source, &mut private).map_err(|e| e.to_string())?;
    if copied != expected_len {
        return Err("snapshot.pack changed length while it was copied".to_string());
    }
    private.sync_all().map_err(|e| e.to_string())?;
    use std::io::{Seek as _, SeekFrom};
    private
        .seek(SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    let sha256 = dbmd_core::linkmd::content_sha256_reader(&private).map_err(|e| e.to_string())?;
    Ok((
        SnapshotPack {
            file: private,
            len: copied,
        },
        sha256,
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn load_state(dir: &Path, expected_anchor: &str) -> Result<ServeState, String> {
    #[cfg(unix)]
    let mirror = open_dir_path_nofollow(&dir.join(dbmd_core::linkmd::MIRROR_REL_DIR))?;
    #[cfg(unix)]
    let head_bytes = read_regular_beneath(&mirror, "head.json", MAX_MIRROR_METADATA_BYTES)
        .map_err(|_| "no safe .dbmd/mirror/head.json — run `dbmd mirror` first".to_string())?;
    #[cfg(unix)]
    let head: serde_json::Value =
        serde_json::from_slice(&head_bytes).map_err(|e| format!("head.json did not parse: {e}"))?;
    let head_seq = head["headSeq"]
        .as_u64()
        .filter(|seq| *seq <= MAX_MIRROR_FEED_ENTRIES)
        .ok_or_else(|| "head.json carries an invalid or excessive sequence".to_string())?;
    #[cfg(unix)]
    let identity_bytes = read_regular_beneath(&mirror, "identity.json", MAX_MIRROR_METADATA_BYTES)?;
    #[cfg(unix)]
    let mut feed_bytes = Vec::with_capacity(head_seq as usize);
    #[cfg(unix)]
    let mut feed_total = 0u64;
    #[cfg(unix)]
    for seq in 1..=head_seq {
        let entry = read_regular_beneath(
            &mirror,
            &format!("feed/{seq}.json"),
            MAX_MIRROR_METADATA_BYTES,
        )?;
        feed_total = feed_total.saturating_add(entry.len() as u64);
        if feed_total > MAX_MIRROR_FEED_BYTES {
            return Err("mirror feed metadata exceeds the aggregate read limit".to_string());
        }
        feed_bytes.push(entry);
    }
    #[cfg(unix)]
    let (snapshot_pack, snapshot_hash) = if head_seq == 0 {
        (None, None)
    } else {
        let (pack, hash) = snapshot_pack_copy(&mirror, MAX_SERVED_BYTES)?;
        (Some(pack), Some(hash))
    };
    #[cfg(unix)]
    let verified = dbmd_core::linkmd::verify_mirror_material_with_pack_hash(
        &head_bytes,
        &identity_bytes,
        &feed_bytes,
        snapshot_hash.as_deref(),
        expected_anchor,
    )
    .map_err(|e| e.to_string())?;

    #[cfg(unix)]
    let identity = verified.identity;
    #[cfg(unix)]
    let fingerprint = identity["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    #[cfg(unix)]
    let public_key_spki = identity["publicKeySpki"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    #[cfg(unix)]
    let previous = identity
        .get("previous")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    #[cfg(unix)]
    let rotations = identity
        .get("rotations")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    #[cfg(unix)]
    Ok(ServeState {
        brain: verified.brain,
        head_seq: verified.head_seq,
        feed_hash: verified.feed_hash,
        fingerprint,
        public_key_spki,
        entries: verified.entries,
        previous,
        rotations,
        snapshot_pack,
        pack_sha256: verified.pack_sha256,
        base_url: String::new(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn load_state(_dir: &Path, _expected_anchor: &str) -> Result<ServeState, String> {
    Err("dbmd serve requires the official macOS/Linux build or WSL".to_string())
}

#[cfg(unix)]
fn write_file_response<W: Write>(
    stream: &mut W,
    content_type: &str,
    pack: &SnapshotPack,
) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;

    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        pack.len,
    )?;
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while offset < pack.len {
        let wanted = usize::try_from((pack.len - offset).min(buffer.len() as u64))
            .expect("bounded by the buffer length");
        let read = pack.file.read_at(&mut buffer[..wanted], offset)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "snapshot pack changed length while serving",
            ));
        }
        stream.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_file_response<W: Write>(
    _stream: &mut W,
    _content_type: &str,
    _pack: &SnapshotPack,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "dbmd serve requires the official macOS/Linux build or WSL",
    ))
}

fn write_feed_response<W: Write>(
    state: &ServeState,
    after: u64,
    limit: usize,
    identity: &serde_json::Value,
    stream: &mut W,
) -> std::io::Result<()> {
    let selected: Vec<&(u64, String, String)> = state
        .entries
        .iter()
        .filter(|(seq, _, _)| *seq > after)
        .take(limit)
        .collect();
    let next_after = after.saturating_add(selected.len() as u64);
    let brain = serde_json::to_string(&state.brain).expect("serializing a string cannot fail");
    let feed_hash = serde_json::to_string(&state.feed_hash)
        .expect("serializing an optional string cannot fail");
    let prefix = format!(
        "{{\"brain\":{brain},\"headSeq\":{},\"feedHash\":{feed_hash},\"identity\":{identity},\"entries\":[",
        state.head_seq,
    );
    let suffix = format!(
        "],\"nextAfter\":{next_after},\"hasMore\":{},\"scopeLimited\":false}}",
        next_after < state.head_seq,
    );
    const ENTRY_PREFIX: &[u8] = b"{\"hash\":\"";
    const ENTRY_MIDDLE: &[u8] = b"\",\"entry\":";
    const ENTRY_SUFFIX: &[u8] = b"}";
    let entries_len = selected.iter().try_fold(0usize, |total, (_, raw, hash)| {
        total
            .checked_add(ENTRY_PREFIX.len())
            .and_then(|value| value.checked_add(hash.len()))
            .and_then(|value| value.checked_add(ENTRY_MIDDLE.len()))
            .and_then(|value| value.checked_add(raw.len()))
            .and_then(|value| value.checked_add(ENTRY_SUFFIX.len()))
    });
    let content_length = entries_len
        .and_then(|value| value.checked_add(selected.len().saturating_sub(1)))
        .and_then(|value| value.checked_add(prefix.len()))
        .and_then(|value| value.checked_add(suffix.len()))
        .ok_or_else(|| std::io::Error::other("feed response length overflow"))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {content_length}\r\nconnection: close\r\n\r\n"
    )?;
    stream.write_all(prefix.as_bytes())?;
    for (index, (_, raw, hash)) in selected.iter().enumerate() {
        if index != 0 {
            stream.write_all(b",")?;
        }
        stream.write_all(ENTRY_PREFIX)?;
        stream.write_all(hash.as_bytes())?;
        stream.write_all(ENTRY_MIDDLE)?;
        stream.write_all(raw.as_bytes())?;
        stream.write_all(ENTRY_SUFFIX)?;
    }
    stream.write_all(suffix.as_bytes())
}

fn query_u64(query: &str, key: &str, fallback: u64) -> u64 {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn snapshot_token_matches(state: &ServeState, at_seq: u64, requested_hash: Option<&str>) -> bool {
    at_seq == state.head_seq && requested_hash == Some(state.feed_hash.as_deref().unwrap_or("none"))
}

fn handle<W: Write>(state: &ServeState, path_and_query: &str, stream: &mut W) {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };
    let base = format!("/api/hub/brains/{}", state.brain);
    let identity = serde_json::json!({
        "fingerprint": state.fingerprint,
        "publicKeySpki": state.public_key_spki,
        "previous": state.previous,
        "rotations": state.rotations,
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
        let _ = write_feed_response(state, after, limit as usize, &identity, stream);
        return;
    }
    if path == format!("{base}/export") {
        let at_seq = query_u64(query, "atSeq", u64::MAX);
        let requested_hash = query.split('&').find_map(|kv| kv.strip_prefix("feedHash="));
        if !snapshot_token_matches(state, at_seq, requested_hash) {
            respond(stream, 409, "{\"error\":\"snapshot token mismatch\"}");
            return;
        }
        if state.head_seq == 0 {
            let body = serde_json::json!({
                "brain": state.brain,
                "slug": "mirror",
                "headSeq": 0,
                "feedHash": serde_json::Value::Null,
                "files": [],
            });
            respond(stream, 200, &body.to_string());
            return;
        }
        let pack_url = format!("{base}/packs/snapshot");
        let body = serde_json::json!({
            "brain": state.brain,
            "slug": "mirror",
            "headSeq": state.head_seq,
            "feedHash": state.feed_hash,
            "sha256": state.pack_sha256,
            "url": format!("{}{}", state.base_url, pack_url),
        });
        respond(stream, 200, &body.to_string());
        return;
    }
    if path == format!("{base}/packs/snapshot") {
        if let Some(pack) = &state.snapshot_pack {
            let _ = write_file_response(stream, "application/zip", pack);
        } else {
            respond(stream, 404, "{\"error\":\"empty snapshot has no pack\"}");
        }
        return;
    }
    respond(stream, 404, "{\"error\":\"not found\"}");
}

fn serve_connection(mut stream: TcpStream, state: &ServeState, timing: ServeTiming) {
    // Read-only surface: max_body 0 skips the body phase entirely — the head
    // is processed and any body a client sends is ignored, as before the
    // shared-plumbing extraction.
    let Some(request) = httpd::read_request(&mut stream, timing, 0) else {
        return;
    };
    let mut writer = DeadlineWriter {
        stream: &mut stream,
        deadline: Instant::now() + timing.response_total,
        idle: timing.idle,
    };
    if request.method != "GET" {
        respond(&mut writer, 404, "{\"error\":\"not found\"}");
        return;
    }
    handle(state, &request.target, &mut writer);
}

fn dispatch_connection(
    stream: TcpStream,
    state: Arc<ServeState>,
    active: Arc<AtomicUsize>,
    timing: ServeTiming,
) {
    httpd::spawn_bounded(stream, active, MAX_CONCURRENT_CLIENTS, move |s| {
        serve_connection(s, &state, timing)
    });
}

/// Run `dbmd serve`.
pub fn run(ctx: &Context, args: &ServeArgs) -> CliResult {
    let mut state = load_state(Path::new(&args.dir), &args.pin).map_err(|message| {
        CliError::new(ExitCode::Runtime, "SERVE_STATE", message)
            .with_hint("run `dbmd mirror <brain> --dir <dir>` first")
    })?;
    let (listener, addr) = httpd::bind_checked(
        &args.addr,
        "SERVE_BIND",
        "SERVE_PUBLIC_REFUSED",
        args.unsafe_public,
        "bind loopback, or pass --unsafe-public only behind an authenticated proxy",
    )?;
    let url = format!("http://{addr}");
    state.base_url = url.clone();
    let state = Arc::new(state);
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

    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        dispatch_connection(
            stream,
            Arc::clone(&state),
            Arc::clone(&active),
            ServeTiming::default(),
        );
    }
    Ok(())
}

// Used via dbmd_core; keep the path alive for grep-ability with the mirror.
#[allow(unused)]
type _MirrorDir = PathBuf;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn load_state_refuses_symlinked_snapshot_pack() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let mirror = root.path().join(dbmd_core::linkmd::MIRROR_REL_DIR);
        std::fs::create_dir_all(mirror.join("feed")).unwrap();
        let secret = external.path().join("snapshot.pack");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        let digest = dbmd_core::linkmd::content_sha256(b"TOP SECRET");
        let entry = format!("{{\"pack_sha256\":\"{digest}\"}}");
        let feed_hash = dbmd_core::linkmd::feed_entry_hash(&entry);
        std::fs::write(mirror.join("feed/1.json"), format!("{entry}\n")).unwrap();
        std::fs::write(
            mirror.join("head.json"),
            format!("{{\"brain\":\"brain\",\"headSeq\":1,\"feedHash\":\"{feed_hash}\"}}\n"),
        )
        .unwrap();
        std::fs::write(
            mirror.join("identity.json"),
            "{\"fingerprint\":\"fp\",\"publicKeySpki\":\"spki\"}\n",
        )
        .unwrap();
        symlink(&secret, mirror.join("snapshot.pack")).unwrap();

        let err = load_state(root.path(), "ed25519:test").err().unwrap();
        assert!(err.contains("snapshot.pack"), "{err}");
        assert!(!err.contains("TOP SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn load_state_refuses_symlinked_identity_without_reading_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let mirror = root.path().join(dbmd_core::linkmd::MIRROR_REL_DIR);
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(
            mirror.join("head.json"),
            "{\"brain\":\"brain\",\"headSeq\":0,\"feedHash\":null}\n",
        )
        .unwrap();
        let secret = external.path().join("identity.json");
        std::fs::write(&secret, "{\"secret\":\"TOP SECRET\"}\n").unwrap();
        symlink(&secret, mirror.join("identity.json")).unwrap();

        let err = load_state(root.path(), "ed25519:test").err().unwrap();
        assert!(err.contains("identity.json"), "{err}");
        assert!(!err.contains("TOP SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn load_state_refuses_symlinked_feed_entry_without_reading_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let mirror = root.path().join(dbmd_core::linkmd::MIRROR_REL_DIR);
        std::fs::create_dir_all(mirror.join("feed")).unwrap();
        std::fs::write(
            mirror.join("head.json"),
            "{\"brain\":\"brain\",\"headSeq\":1,\"feedHash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
        )
        .unwrap();
        std::fs::write(mirror.join("identity.json"), "{}\n").unwrap();
        let secret = external.path().join("1.json");
        std::fs::write(&secret, "{\"secret\":\"TOP SECRET\"}\n").unwrap();
        symlink(&secret, mirror.join("feed/1.json")).unwrap();

        let err = load_state(root.path(), "ed25519:test").err().unwrap();
        assert!(err.contains("feed/1.json"), "{err}");
        assert!(!err.contains("TOP SECRET"));
    }

    #[test]
    fn empty_mirror_export_uses_the_none_snapshot_token() {
        let state = ServeState {
            brain: "brain".to_string(),
            head_seq: 0,
            feed_hash: None,
            fingerprint: "fingerprint".to_string(),
            public_key_spki: "spki".to_string(),
            entries: Vec::new(),
            previous: serde_json::json!([]),
            rotations: serde_json::json!([]),
            snapshot_pack: None,
            pack_sha256: None,
            base_url: "http://127.0.0.1:1".to_string(),
        };
        assert!(snapshot_token_matches(&state, 0, Some("none")));
        assert!(!snapshot_token_matches(&state, 0, None));
        assert!(!snapshot_token_matches(&state, 1, Some("none")));
    }

    #[test]
    fn concurrent_large_feed_responses_stream_bounded_chunks() {
        #[derive(Default)]
        struct CountingWriter {
            total: usize,
            largest_write: usize,
        }

        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.total += bytes.len();
                self.largest_write = self.largest_write.max(bytes.len());
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let raw = format!("{{\"padding\":\"{}\"}}", "x".repeat(128 * 1024));
        let entries = (1..=100)
            .map(|seq| (seq, raw.clone(), format!("{seq:064x}")))
            .collect();
        let state = Arc::new(ServeState {
            brain: "01k00000000000000000000000".to_string(),
            head_seq: 100,
            feed_hash: Some("a".repeat(64)),
            fingerprint: "fingerprint".to_string(),
            public_key_spki: "spki".to_string(),
            entries,
            previous: serde_json::json!([]),
            rotations: serde_json::json!([]),
            snapshot_pack: None,
            pack_sha256: None,
            base_url: "http://127.0.0.1:1".to_string(),
        });
        let identity = Arc::new(serde_json::json!({
            "fingerprint": state.fingerprint,
            "publicKeySpki": state.public_key_spki,
            "previous": state.previous,
            "rotations": state.rotations,
        }));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                let identity = Arc::clone(&identity);
                std::thread::spawn(move || {
                    let mut writer = CountingWriter::default();
                    write_feed_response(&state, 0, 100, &identity, &mut writer).unwrap();
                    writer
                })
            })
            .collect();

        for worker in workers {
            let writer = worker.join().unwrap();
            assert!(writer.total > 12 * 1024 * 1024);
            assert!(writer.largest_write < 256 * 1024);
        }
    }

    #[test]
    fn absolute_header_deadline_releases_all_trickle_slots() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(ServeState {
            brain: "01k00000000000000000000000".to_string(),
            head_seq: 0,
            feed_hash: None,
            fingerprint: "fingerprint".to_string(),
            public_key_spki: "spki".to_string(),
            entries: Vec::new(),
            previous: serde_json::json!([]),
            rotations: serde_json::json!([]),
            snapshot_pack: None,
            pack_sha256: None,
            base_url: format!("http://{address}"),
        });
        let active = Arc::new(AtomicUsize::new(0));
        let timing = ServeTiming {
            idle: Duration::from_millis(100),
            header_total: Duration::from_millis(250),
            response_total: Duration::from_secs(2),
        };
        let server_state = Arc::clone(&state);
        let server_active = Arc::clone(&active);
        let server = std::thread::spawn(move || {
            for _ in 0..=MAX_CONCURRENT_CLIENTS {
                let (stream, _) = listener.accept().unwrap();
                dispatch_connection(
                    stream,
                    Arc::clone(&server_state),
                    Arc::clone(&server_active),
                    timing,
                );
            }
        });

        let tricklers: Vec<_> = (0..MAX_CONCURRENT_CLIENTS)
            .map(|_| {
                let mut stream = TcpStream::connect(address).unwrap();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        if stream.write_all(b"x").is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    }
                })
            })
            .collect();
        // Every trickler remains active under the idle timeout, but the total
        // request-head deadline must have released all 16 bounded slots.
        std::thread::sleep(Duration::from_millis(400));
        let mut healthy = TcpStream::connect(address).unwrap();
        healthy
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            healthy,
            "GET /api/hub/brains/{} HTTP/1.1\r\nhost: localhost\r\n\r\n",
            state.brain
        )
        .unwrap();
        let mut response = String::new();
        healthy.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

        server.join().unwrap();
        for trickler in tricklers {
            trickler.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_response_streams_an_immutable_private_copy() {
        use std::os::unix::fs::FileExt as _;

        #[derive(Default)]
        struct CountingWriter {
            total: usize,
            largest_write: usize,
        }
        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.total += bytes.len();
                self.largest_write = self.largest_write.max(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("snapshot.pack"),
            vec![b'a'; 4 * 1024 * 1024],
        )
        .unwrap();
        let mirror = open_dir_path_nofollow(root.path()).unwrap();
        let (pack, hash) = snapshot_pack_copy(&mirror, MAX_SERVED_BYTES).unwrap();
        assert_eq!(
            hash,
            dbmd_core::linkmd::content_sha256(&vec![b'a'; 4 * 1024 * 1024])
        );

        // The source path may change after startup; the unlinked authenticated
        // capability must continue to expose only the bytes that were hashed.
        std::fs::write(
            root.path().join("snapshot.pack"),
            vec![b'b'; 4 * 1024 * 1024],
        )
        .unwrap();
        let mut first = [0u8; 1];
        pack.file.read_at(&mut first, 0).unwrap();
        assert_eq!(first, [b'a']);

        let mut writer = CountingWriter::default();
        write_file_response(&mut writer, "application/zip", &pack).unwrap();
        assert!(writer.total > 4 * 1024 * 1024);
        assert!(writer.largest_write <= 64 * 1024);
    }
}
