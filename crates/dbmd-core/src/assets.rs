//! `assets` — the db.md asset layer.
//!
//! Raw assets (PDFs, recordings, large exports — typically binaries too heavy
//! for Git) belong to a store but live outside the content plane. A content
//! file (the **wrapper**) declares one via an `asset:` / `assets:` frontmatter
//! key; this module records each in the root-level `assets.jsonl` manifest:
//! store-relative path, SHA-256, size, media type, the declaring wrapper(s),
//! and whether it is required for byte-completeness.
//!
//! Any in-store file may be declared — including a markdown content file, whose
//! bytes are then integrity-tracked here while the content layer keeps parsing,
//! indexing, and validating it as usual. [`paths`] still omits markdown, so an
//! ignore mechanism fed from it can never hide a content file from the VCS.
//!
//! The manifest is a **pure projection** of (wrappers + asset files on disk):
//! every field is derivable, so a [`scan`] where the bytes are present
//! reproduces it byte-for-byte, exactly like `index.jsonl`. db.md never
//! transports the bytes and never names a storage provider; that is the
//! hosting/transport layer's job, keyed off the SHA-256. This module never
//! shells out to git and never touches the network.
//!
//! Six operations — three writes, three reads:
//!   - [`scan`]   (write) discover declared assets, hash present files, rewrite the manifest
//!   - [`refresh`] (write) re-hash one declared asset and update its manifest row
//!   - [`refresh_wrapper`] (write) reconcile every declaration in one wrapper
//!     and update the manifest once
//!   - [`verify`] (read)  prove the local store is byte-complete for required assets
//!   - [`status`] (read)  report present / missing without failing
//!   - [`paths`]  (read)  the store-relative path list (for an ignore mechanism)
//!
//! Path safety: every declared path is validated store-relative (no `..`, no
//! absolute, no escape) via [`crate::store::ensure_path_within_store`] wherever
//! a path is read or resolved, so a poisoned manifest can never make `scan`
//! hash, or a restore write, outside the store.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_norway::Value;
use sha2::{Digest, Sha256};

use crate::parser;
use crate::projection::ProjectionPolicy;
use crate::store::Store;

/// The manifest file name at the store root.
pub const MANIFEST_FILE: &str = "assets.jsonl";

/// Frontmatter key used by an append-only wrapper to state that its single
/// declared asset is the portable replacement for an older asset coordinate.
pub const SUPERSEDES_ASSET_KEY: &str = "supersedes-asset";

/// One asset record — one line of `assets.jsonl`.
///
/// Every field is derivable from the store (wrapper frontmatter + the file on
/// disk), so the manifest rebuilds byte-for-byte. Field declaration order is
/// the canonical JSON key order; `wrappers` is always a sorted list (never a
/// bare string) so serialization is deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRecord {
    /// Store-relative path of the raw bytes, forward-slash, with extension. The
    /// record key. May differ from `wrappers` (the wrapper is the `.md`).
    pub path: String,
    /// Lowercase-hex SHA-256 of the bytes: the integrity check and the provider
    /// blob key. May repeat across records (identical bytes at two paths).
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Best-effort MIME type derived from the path extension.
    pub media_type: String,
    /// Store-relative path(s) of the content file(s) that declare this asset,
    /// sorted ascending. Usually one.
    pub wrappers: Vec<String>,
    /// Whether the asset is required for byte-completeness (default `true`;
    /// `false` only when every declaration marks it optional).
    pub required: bool,
}

/// A single `asset:` / `assets:` declaration read from a wrapper's frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The raw store-relative path string as written in frontmatter.
    pub path: String,
    /// Whether this declaration marks the asset required (bare string and
    /// object-without-`required` default to `true`).
    pub required: bool,
}

/// A value-free, append-only asset replacement declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetSupersession {
    /// The older asset coordinate retained as optional evidence.
    pub original: String,
    /// The wrapper's single required replacement coordinate.
    pub replacement: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Reports (serialized directly in `--json`; the CLI renders the text form)
// ─────────────────────────────────────────────────────────────────────────────

/// Result of [`scan`].
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub manifest: String,
    pub cataloged: usize,
    pub hashed: usize,
    pub preserved: usize,
    pub bytes: u64,
    pub wrote: bool,
    pub dry_run: bool,
    pub warnings: Vec<String>,
    pub untracked: Vec<String>,
}

/// Result of [`refresh`]. A refresh is the bounded write-through counterpart
/// to the full-store [`scan`]: it re-hashes one declared asset without touching
/// unrelated bytes.
#[derive(Debug, Serialize)]
pub struct RefreshReport {
    pub manifest: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub wrappers: Vec<String>,
    pub required: bool,
    /// Older asset coordinates made optional by this wrapper.
    pub superseded_assets: Vec<String>,
    pub wrote: bool,
}

/// Result of [`refresh_wrapper`]. This is the bounded batch counterpart to
/// [`refresh`]: every asset declared by one wrapper is reconciled, while
/// unrelated wrappers and bytes are untouched.
#[derive(Debug, Serialize)]
pub struct RefreshWrapperReport {
    pub manifest: String,
    pub wrapper: String,
    pub cataloged: usize,
    pub added: usize,
    pub removed: usize,
    pub hashed: usize,
    pub preserved: usize,
    pub bytes: u64,
    pub wrote: bool,
}

/// One asset's local state, used by [`status`] and [`verify`].
#[derive(Debug, Serialize)]
pub struct AssetState {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub required: bool,
    /// `present` / `missing` (status); `ok` / `missing` / `corrupt` (verify).
    pub state: String,
}

/// Result of [`status`].
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub total: usize,
    pub present: usize,
    pub missing: usize,
    pub required_missing: usize,
    pub optional_missing: usize,
    pub bytes_total: u64,
    pub bytes_missing: u64,
    pub assets: Vec<AssetState>,
}

/// Result of [`verify`].
#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub mode: String,
    pub checked: usize,
    pub ok: usize,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
    /// Exact absent paths declared outside this materialized projection.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projected_missing: Vec<String>,
    /// Whether every byte that belongs to this materialized projection passed.
    /// Present only for projection-aware verification; unlike `complete`, this
    /// may be true while explicitly excluded bytes remain absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_complete: Option<bool>,
    /// True only when every considered required byte is present and valid.
    pub complete: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Manifest read / write
// ─────────────────────────────────────────────────────────────────────────────

/// Read `assets.jsonl` into records, deduped by path (last line wins) and
/// sorted by path ascending. A missing manifest is an empty store, not an
/// error. A malformed line is an `InvalidData` error (the CLI surfaces it;
/// [`crate::validate`] flags it leniently as `ASSET_MANIFEST_MALFORMED`).
pub fn read_manifest(store: &Store) -> crate::Result<Vec<AssetRecord>> {
    let text = match store
        .read_text_bounded(Path::new(MANIFEST_FILE), crate::parser::MAX_DBMD_FILE_BYTES)
    {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut by_path: BTreeMap<String, AssetRecord> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: AssetRecord = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{MANIFEST_FILE} line {}: {e}", i + 1),
            )
        })?;
        by_path.insert(rec.path.clone(), rec);
    }
    Ok(by_path.into_values().collect())
}

/// The canonical serialized form of a record set: one JSON line per record,
/// records sorted by path ascending, trailing newline. An empty record set is
/// the empty string (the manifest file is removed, not written empty). This is
/// the SINGLE source of the manifest's byte layout — both [`write_manifest`] and
/// the [`scan`] no-change gate go through it, so "what scan would write" and
/// "what's on disk" are compared as the same bytes.
fn serialize_manifest(records: &[AssetRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }
    let mut sorted = records.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = String::new();
    for rec in &sorted {
        let line = serde_json::to_string(rec).expect("AssetRecord serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Write the manifest atomically (temp + fsync + rename through the store's
/// held root capability), records sorted by path ascending. An empty record set
/// removes the file.
pub fn write_manifest(store: &Store, records: &[AssetRecord]) -> crate::Result<()> {
    let abs = Path::new(MANIFEST_FILE);
    let out = serialize_manifest(records);
    if out.is_empty() {
        match store.remove_file(abs) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    store.write_atomic(abs, out.as_bytes())?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// scan (write) — rebuild the manifest from wrapper declarations
// ─────────────────────────────────────────────────────────────────────────────

/// Walk every content file, read its `asset`/`assets` declarations, hash the
/// present files, and (re)write the manifest. The manifest is a projection: a
/// path no longer declared by any wrapper drops out. Bytes absent locally but
/// previously cataloged are preserved (the eviction / disk-relief case) since
/// they cannot be re-hashed. `dry_run` computes without writing; `untracked`
/// additionally reports non-markdown files under `sources/` that no wrapper
/// declares. Never writes when nothing changed (keeps the Git diff and the
/// `--dry-run`-then-scan idempotent).
pub fn scan(store: &Store, dry_run: bool, untracked: bool) -> crate::Result<ScanReport> {
    // Tolerate a malformed existing manifest here: scan rebuilds from the files,
    // so a corrupt prior file is simply replaced. We still read it (best effort)
    // to preserve hashes for evicted (absent-but-cataloged) assets.
    let existing_by_path: BTreeMap<String, AssetRecord> = read_manifest(store)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.path.clone(), r))
        .collect();

    // Aggregate declarations across all content files: path -> (wrappers, required).
    let mut wrappers_by_path: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut required_by_path: BTreeMap<String, bool> = BTreeMap::new();
    let mut declared_paths: BTreeSet<String> = BTreeSet::new();
    let mut supersessions: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut ambiguous_supersessions: BTreeSet<String> = BTreeSet::new();
    let mut warnings: Vec<String> = Vec::new();

    for rel in store.walk()? {
        let text = match store.read_text_bounded(&rel, parser::MAX_DBMD_FILE_BYTES) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let parsed = match parser::split_frontmatter(&text, &rel) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let fm = match parser::Frontmatter::parse(&parsed.frontmatter_yaml, &rel) {
            Ok(frontmatter) => frontmatter,
            Err(_) => continue, // unparseable / not a content file: skip
        };
        let wrapper = rel_to_string(&rel);
        for decl in declared_assets(&fm) {
            let norm = match normalize_asset_path(&decl.path) {
                Ok(n) => n,
                Err(e) => {
                    warnings.push(format!("{wrapper}: {e}"));
                    continue;
                }
            };
            wrappers_by_path
                .entry(norm.clone())
                .or_default()
                .insert(wrapper.clone());
            let req = required_by_path.entry(norm.clone()).or_insert(false);
            *req = *req || decl.required;
            declared_paths.insert(norm);
        }
        match asset_supersession(&fm) {
            Ok(Some(supersession)) => {
                if let Some((prior, prior_wrapper)) = supersessions.get(&supersession.original) {
                    if prior != &supersession.replacement {
                        ambiguous_supersessions.insert(supersession.original.clone());
                        warnings.push(format!(
                            "{wrapper}: `{SUPERSEDES_ASSET_KEY}` conflicts with {prior_wrapper} for {}",
                            supersession.original
                        ));
                    }
                } else {
                    supersessions.insert(
                        supersession.original,
                        (supersession.replacement, wrapper.clone()),
                    );
                }
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!("{wrapper}: {error}")),
        }
    }

    let cyclic_supersessions = supersession_cycle_members(&supersessions);
    for original in &cyclic_supersessions {
        if let Some((_, wrapper)) = supersessions.get(original) {
            warnings.push(format!(
                "{wrapper}: `{SUPERSEDES_ASSET_KEY}` participates in a replacement cycle at {original}"
            ));
        }
    }
    for (original, (replacement, wrapper)) in supersessions {
        if ambiguous_supersessions.contains(&original) {
            continue;
        }
        if cyclic_supersessions.contains(&original) {
            continue;
        }
        if !wrappers_by_path.contains_key(&replacement) {
            warnings.push(format!(
                "{wrapper}: replacement asset `{replacement}` is not declared"
            ));
            continue;
        }
        if !wrappers_by_path.contains_key(&original) && !existing_by_path.contains_key(&original) {
            warnings.push(format!(
                "{wrapper}: superseded asset `{original}` is neither declared nor cataloged"
            ));
            continue;
        }
        wrappers_by_path
            .entry(original.clone())
            .or_default()
            .insert(wrapper);
        required_by_path.insert(original.clone(), false);
        declared_paths.insert(original);
    }

    // Build records.
    let mut records: Vec<AssetRecord> = Vec::new();
    let mut hashed = 0usize;
    let mut preserved = 0usize;
    for (path, wrappers) in &wrappers_by_path {
        let required = *required_by_path.get(path).unwrap_or(&true);
        let wrappers: Vec<String> = wrappers.iter().cloned().collect();

        // Belt-and-suspenders containment check before any disk read.
        let abs = match store.capability_relative(Path::new(path)) {
            Ok(p) => p,
            Err(_) => {
                warnings.push(format!("{path}: escapes the store root; skipped"));
                continue;
            }
        };

        match store.open_regular(abs) {
            Ok(file) => {
                let (sha256, bytes) = sha256_file(file)?;
                records.push(AssetRecord {
                    path: path.clone(),
                    sha256,
                    bytes,
                    media_type: media_type_for(path),
                    wrappers,
                    required,
                });
                hashed += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(prev) = existing_by_path.get(path) {
                    // Evicted: bytes gone locally but previously cataloged. Preserve the
                    // committed hash/size (we cannot re-hash what is not here).
                    records.push(AssetRecord {
                        path: path.clone(),
                        sha256: prev.sha256.clone(),
                        bytes: prev.bytes,
                        media_type: media_type_for(path),
                        wrappers,
                        required,
                    });
                    preserved += 1;
                } else {
                    warnings.push(format!(
                        "{path}: declared but absent and never cataloged; cannot hash (skipped)"
                    ));
                }
            }
            Err(error) => {
                warnings.push(format!(
                    "{path}: is not a readable regular in-store file: {error}"
                ));
            }
        }
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));

    // Saturating: poisoned-manifest `bytes` can overflow a plain `.sum()` (debug
    // abort / release wrap); see `status`.
    let bytes: u64 = records.iter().fold(0u64, |a, r| a.saturating_add(r.bytes));
    let cataloged = records.len();

    let untracked_list = if untracked {
        find_untracked(store, &declared_paths)?
    } else {
        Vec::new()
    };

    // Only write when the canonical BYTES differ from what's on disk. Comparing
    // parsed records would miss non-canonical on-disk state — duplicate lines
    // from a git `merge=union`, a wrong sort, a missing trailing newline — since
    // `read_manifest` dedupes-by-path and sorts, so a poisoned file parses back
    // equal to the freshly computed records and the no-op gate never repairs it.
    // We instead compare the canonical serialization against the raw on-disk
    // bytes, so `scan` recompacts a non-canonical manifest (mirroring how
    // `index::rebuild_all` always normalizes its artifacts). This is also the
    // documented `merge=union` recovery (SPEC § Assets).
    let mut wrote = false;
    if !dry_run {
        let canonical = serialize_manifest(&records);
        let on_disk = match store
            .read_bounded(Path::new(MANIFEST_FILE), crate::parser::MAX_DBMD_FILE_BYTES)
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if on_disk != canonical.as_bytes() {
            write_manifest(store, &records)?;
            wrote = true;
        }
    }

    Ok(ScanReport {
        manifest: MANIFEST_FILE.to_string(),
        cataloged,
        hashed,
        preserved,
        bytes,
        wrote,
        dry_run,
        warnings,
        untracked: untracked_list,
    })
}

fn supersession_cycle_members(
    supersessions: &BTreeMap<String, (String, String)>,
) -> BTreeSet<String> {
    let mut cyclic = BTreeSet::new();
    for origin in supersessions.keys() {
        let mut order = Vec::new();
        let mut positions = BTreeMap::new();
        let mut current = origin.as_str();
        while let Some((next, _)) = supersessions.get(current) {
            if let Some(start) = positions.get(current).copied() {
                cyclic.extend(order[start..].iter().cloned());
                break;
            }
            positions.insert(current.to_string(), order.len());
            order.push(current.to_string());
            current = next;
        }
    }
    cyclic
}

/// Re-hash one asset and write just its canonical manifest record.
///
/// `scan` remains the authoritative from-scratch projection. This bounded
/// operation exists for write-through workflows that just created or changed
/// one asset. The supplied wrapper must currently declare the exact path.
/// Existing wrappers recorded for that path are re-read and stale declarations
/// are dropped; unrelated manifest rows and asset bytes are never walked.
pub fn refresh(store: &Store, raw_path: &str, raw_wrapper: &str) -> crate::Result<RefreshReport> {
    let path = normalize_asset_path(raw_path)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    let wrapper_path = normalize_asset_path(raw_wrapper)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    if !is_markdown(&wrapper_path)
        || !(wrapper_path.starts_with("sources/") || wrapper_path.starts_with("records/"))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "wrapper must be a sources/ or records/ markdown content path",
        )
        .into());
    }

    let declaration = |wrapper: &str| -> crate::Result<Option<bool>> {
        let text =
            store.read_text_bounded(Path::new(wrapper), crate::parser::MAX_DBMD_FILE_BYTES)?;
        let parsed = parser::split_frontmatter(&text, Path::new(wrapper))?;
        let fm = parser::Frontmatter::parse(&parsed.frontmatter_yaml, Path::new(wrapper))?;
        let mut found = false;
        let mut required = false;
        for declaration in declared_assets(&fm) {
            let declared = normalize_asset_path(&declaration.path).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
            if declared == path {
                found = true;
                required |= declaration.required;
            }
        }
        Ok(found.then_some(required))
    };

    let Some(requested_required) = declaration(&wrapper_path)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("wrapper `{wrapper_path}` does not declare asset `{path}`"),
        )
        .into());
    };
    let requested_supersession = {
        let text = store
            .read_text_bounded(Path::new(&wrapper_path), crate::parser::MAX_DBMD_FILE_BYTES)?;
        let parsed = parser::split_frontmatter(&text, Path::new(&wrapper_path))?;
        let fm = parser::Frontmatter::parse(&parsed.frontmatter_yaml, Path::new(&wrapper_path))?;
        asset_supersession(&fm)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?
    };
    if requested_supersession
        .as_ref()
        .is_some_and(|supersession| supersession.replacement != path)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("wrapper `{wrapper_path}` supersedes an asset with a different replacement"),
        )
        .into());
    }

    let existing = read_manifest(store)?;
    let mut wrappers = BTreeSet::from([wrapper_path.clone()]);
    if let Some(record) = existing.iter().find(|record| record.path == path) {
        wrappers.extend(record.wrappers.iter().cloned());
    }
    let mut live_wrappers = Vec::new();
    let mut required = requested_required;
    for wrapper in wrappers {
        if wrapper != wrapper_path && !store.regular_file_exists(Path::new(&wrapper))? {
            // Missing historical wrappers are stale declarations. A wrapper
            // that still exists but cannot be parsed is not stale: refusing
            // keeps a targeted refresh from silently hiding store corruption.
            continue;
        }
        match declaration(&wrapper) {
            Ok(Some(wrapper_required)) => {
                required |= wrapper_required;
                live_wrappers.push(wrapper);
            }
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    live_wrappers.sort();

    let asset_path = store.capability_relative(Path::new(&path))?;
    let file = store.open_regular(asset_path)?;
    let (sha256, bytes) = sha256_file(file)?;
    let record = AssetRecord {
        path: path.clone(),
        sha256: sha256.clone(),
        bytes,
        media_type: media_type_for(&path),
        wrappers: live_wrappers.clone(),
        required,
    };
    let mut next = existing;
    next.retain(|candidate| candidate.path != path);
    next.push(record);
    let mut superseded_assets = Vec::new();
    if let Some(supersession) = requested_supersession {
        let original = next
            .iter_mut()
            .find(|candidate| candidate.path == supersession.original)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "superseded asset `{}` has no existing manifest row; run `dbmd assets scan` first",
                        supersession.original
                    ),
                )
            })?;
        original.required = false;
        if !original.wrappers.contains(&wrapper_path) {
            original.wrappers.push(wrapper_path.clone());
            original.wrappers.sort();
        }
        superseded_assets.push(supersession.original);
    }
    next.sort_by(|left, right| left.path.cmp(&right.path));

    let canonical = serialize_manifest(&next);
    let on_disk =
        match store.read_bounded(Path::new(MANIFEST_FILE), crate::parser::MAX_DBMD_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
    let wrote = on_disk != canonical.as_bytes();
    if wrote {
        write_manifest(store, &next)?;
    }

    Ok(RefreshReport {
        manifest: MANIFEST_FILE.to_string(),
        path,
        sha256,
        bytes,
        wrappers: live_wrappers,
        required,
        superseded_assets,
        wrote,
    })
}

/// Reconcile every asset declaration in one wrapper and write the canonical
/// manifest once. This is for bounded generators that own one wrapper with a
/// changing set of assets: it avoids a full-store [`scan`] and avoids N
/// manifest rewrites through repeated [`refresh`] calls.
///
/// Existing rows declared by other wrappers are preserved. If the requested
/// wrapper stops declaring a path, only its association is removed; the row
/// survives when another live wrapper still declares it. Missing bytes may be
/// preserved only when the exact path already has a manifest row. Asset
/// supersession remains a one-coordinate [`refresh`] operation because its
/// append-only provenance semantics are deliberately not batchable.
pub fn refresh_wrapper(store: &Store, raw_wrapper: &str) -> crate::Result<RefreshWrapperReport> {
    let wrapper_path = normalize_asset_path(raw_wrapper)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    if !is_markdown(&wrapper_path)
        || !(wrapper_path.starts_with("sources/") || wrapper_path.starts_with("records/"))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "wrapper must be a sources/ or records/ markdown content path",
        )
        .into());
    }

    let wrapper_declarations = |wrapper: &str| -> crate::Result<BTreeMap<String, bool>> {
        let text =
            store.read_text_bounded(Path::new(wrapper), crate::parser::MAX_DBMD_FILE_BYTES)?;
        let parsed = parser::split_frontmatter(&text, Path::new(wrapper))?;
        let fm = parser::Frontmatter::parse(&parsed.frontmatter_yaml, Path::new(wrapper))?;
        if asset_supersession(&fm)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?
            .is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refresh-wrapper does not accept supersedes-asset; use assets refresh for that replacement",
            )
            .into());
        }
        let mut declarations = BTreeMap::new();
        for declaration in declared_assets(&fm) {
            let path = normalize_asset_path(&declaration.path).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
            let required = declarations.entry(path).or_insert(false);
            *required |= declaration.required;
        }
        Ok(declarations)
    };

    // An empty declaration set is meaningful: a bounded generator may have
    // removed its last object. Reconciliation must then remove only this
    // wrapper's old associations instead of forcing a full-store scan.
    let requested = wrapper_declarations(&wrapper_path)?;

    let existing = read_manifest(store)?;
    let existing_by_path: BTreeMap<String, AssetRecord> = existing
        .iter()
        .cloned()
        .map(|record| (record.path.clone(), record))
        .collect();
    let old_paths: BTreeSet<String> = existing
        .iter()
        .filter(|record| record.wrappers.contains(&wrapper_path))
        .map(|record| record.path.clone())
        .collect();
    let requested_paths: BTreeSet<String> = requested.keys().cloned().collect();

    let mut wrapper_cache: BTreeMap<String, Option<BTreeMap<String, bool>>> = BTreeMap::new();
    let mut live_other_declaration = |wrapper: &str, path: &str| -> crate::Result<Option<bool>> {
        if !wrapper_cache.contains_key(wrapper) {
            let declarations = if store.regular_file_exists(Path::new(wrapper))? {
                Some(wrapper_declarations(wrapper)?)
            } else {
                None
            };
            wrapper_cache.insert(wrapper.to_string(), declarations);
        }
        Ok(wrapper_cache
            .get(wrapper)
            .and_then(Option::as_ref)
            .and_then(|declarations| declarations.get(path).copied()))
    };

    let mut next = Vec::new();
    for mut record in existing.iter().cloned() {
        if requested.contains_key(&record.path) {
            continue;
        }
        if record.wrappers.contains(&wrapper_path) {
            let mut live_wrappers = Vec::new();
            let mut required = false;
            for wrapper in &record.wrappers {
                if wrapper == &wrapper_path {
                    continue;
                }
                if let Some(wrapper_required) = live_other_declaration(wrapper, &record.path)? {
                    live_wrappers.push(wrapper.clone());
                    required |= wrapper_required;
                }
            }
            if live_wrappers.is_empty() {
                continue;
            }
            live_wrappers.sort();
            record.wrappers = live_wrappers;
            record.required = required;
        }
        next.push(record);
    }

    let mut hashed = 0usize;
    let mut preserved = 0usize;
    let mut bytes_total = 0u64;
    for (path, requested_required) in &requested {
        let mut wrappers = vec![wrapper_path.clone()];
        let mut required = *requested_required;
        if let Some(existing_record) = existing_by_path.get(path) {
            for wrapper in &existing_record.wrappers {
                if wrapper == &wrapper_path {
                    continue;
                }
                if let Some(wrapper_required) = live_other_declaration(wrapper, path)? {
                    wrappers.push(wrapper.clone());
                    required |= wrapper_required;
                }
            }
        }
        wrappers.sort();
        wrappers.dedup();

        let (sha256, bytes, media_type) = match store.open_regular(Path::new(path)) {
            Ok(file) => {
                let (sha256, bytes) = sha256_file(file)?;
                hashed += 1;
                (sha256, bytes, media_type_for(path))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let existing_record = existing_by_path.get(path).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "declared asset `{path}` is absent and has no manifest row to preserve"
                        ),
                    )
                })?;
                preserved += 1;
                (
                    existing_record.sha256.clone(),
                    existing_record.bytes,
                    existing_record.media_type.clone(),
                )
            }
            Err(error) => return Err(error.into()),
        };
        bytes_total = bytes_total.saturating_add(bytes);
        next.push(AssetRecord {
            path: path.clone(),
            sha256,
            bytes,
            media_type,
            wrappers,
            required,
        });
    }

    next.sort_by(|left, right| left.path.cmp(&right.path));
    let canonical = serialize_manifest(&next);
    let on_disk =
        match store.read_bounded(Path::new(MANIFEST_FILE), crate::parser::MAX_DBMD_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
    let wrote = on_disk != canonical.as_bytes();
    if wrote {
        write_manifest(store, &next)?;
    }

    Ok(RefreshWrapperReport {
        manifest: MANIFEST_FILE.to_string(),
        wrapper: wrapper_path,
        cataloged: requested.len(),
        added: requested_paths.difference(&old_paths).count(),
        removed: old_paths.difference(&requested_paths).count(),
        hashed,
        preserved,
        bytes: bytes_total,
        wrote,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// verify (read) — byte-completeness gate
// ─────────────────────────────────────────────────────────────────────────────

/// Check that every required asset (plus optional, under `include_optional`) is
/// present locally and matches the manifest. `quick` = presence + size only
/// (fast); otherwise a full SHA-256 re-hash. This is a SWEEP (O(asset bytes) in
/// deep mode), never a loop op. `complete` is true iff nothing is missing or
/// corrupt in the considered set.
pub fn verify(store: &Store, include_optional: bool, quick: bool) -> crate::Result<VerifyReport> {
    verify_inner(store, include_optional, quick, None)
}

/// Verify the byte completeness of an intentionally partial projection.
/// Exact listed paths may be absent and are reported as `projected_missing`;
/// if a listed path is present it is still hashed and any corruption blocks.
/// Unlisted missing assets and every corrupt asset remain blocking.
pub fn verify_projection(
    store: &Store,
    include_optional: bool,
    quick: bool,
    projection: &ProjectionPolicy,
) -> crate::Result<VerifyReport> {
    verify_inner(store, include_optional, quick, Some(projection))
}

fn verify_inner(
    store: &Store,
    include_optional: bool,
    quick: bool,
    projection: Option<&ProjectionPolicy>,
) -> crate::Result<VerifyReport> {
    let records = read_manifest(store)?;
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    let mut projected_missing = Vec::new();
    let mut checked = 0usize;

    for rec in &records {
        if !rec.required && !include_optional {
            continue;
        }
        let abs = match store.capability_relative(Path::new(&rec.path)) {
            Ok(p) => p,
            Err(_) => {
                // A manifest path that escapes the store is not restorable here.
                checked += 1;
                corrupt.push(rec.path.clone());
                continue;
            }
        };
        let file = match store.open_regular(abs) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if projection.is_some_and(|policy| policy.excludes_path(&rec.path)) {
                    projected_missing.push(rec.path.clone());
                } else {
                    checked += 1;
                    missing.push(rec.path.clone());
                }
                continue;
            }
            Err(_) => {
                checked += 1;
                corrupt.push(rec.path.clone());
                continue;
            }
        };
        checked += 1;
        if quick {
            let len = file.metadata()?.len();
            if len != rec.bytes {
                corrupt.push(rec.path.clone());
            }
        } else {
            let (sha, bytes) = sha256_file(file)?;
            if sha != rec.sha256 || bytes != rec.bytes {
                corrupt.push(rec.path.clone());
            }
        }
    }

    let ok = checked - missing.len() - corrupt.len();
    let projection_complete = projection.map(|_| missing.is_empty() && corrupt.is_empty());
    let complete = missing.is_empty() && corrupt.is_empty() && projected_missing.is_empty();
    Ok(VerifyReport {
        mode: if quick { "quick" } else { "deep" }.to_string(),
        checked,
        ok,
        missing,
        corrupt,
        projected_missing,
        projection_complete,
        complete,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// status (read) — non-failing presence report
// ─────────────────────────────────────────────────────────────────────────────

/// Report which cataloged assets are present locally and how many bytes remain
/// to restore. Never fails on a missing asset (that is `verify`'s job); it does
/// fail on a malformed manifest.
pub fn status(store: &Store) -> crate::Result<StatusReport> {
    let records = read_manifest(store)?;
    let mut present = 0usize;
    let mut missing = 0usize;
    let mut required_missing = 0usize;
    let mut optional_missing = 0usize;
    let mut bytes_total = 0u64;
    let mut bytes_missing = 0u64;
    let mut assets = Vec::with_capacity(records.len());

    for rec in &records {
        // Saturating: `rec.bytes` is deserialized verbatim from a hand-editable /
        // poisoned `assets.jsonl` with no clamp. An absurd value (~u64::MAX)
        // summed with unchecked `+=` ABORTS in debug (overflow-checks) and
        // silently WRAPS in release — and `status` is contractually non-failing.
        bytes_total = bytes_total.saturating_add(rec.bytes);
        // Resolve through the same containment guard `scan` and `verify` use:
        // the module contract is that the guard applies "wherever a path is read
        // or resolved", and an unguarded `is_file()` here let a poisoned/hand-
        // edited manifest path (`../outside.txt`) report `present` (and count its
        // bytes) while `verify` reported it `corrupt` — two read commands on the
        // same store disagreeing, plus a path-existence oracle outside the store.
        // An escaping record is treated as not-present (missing), matching verify.
        let is_present = store.open_regular(Path::new(&rec.path)).is_ok();
        let state = if is_present {
            present += 1;
            "present"
        } else {
            missing += 1;
            bytes_missing = bytes_missing.saturating_add(rec.bytes);
            if rec.required {
                required_missing += 1;
            } else {
                optional_missing += 1;
            }
            "missing"
        };
        assets.push(AssetState {
            path: rec.path.clone(),
            sha256: rec.sha256.clone(),
            bytes: rec.bytes,
            required: rec.required,
            state: state.to_string(),
        });
    }

    Ok(StatusReport {
        total: records.len(),
        present,
        missing,
        required_missing,
        optional_missing,
        bytes_total,
        bytes_missing,
        assets,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// paths (read) — the VCS-neutral path list
// ─────────────────────────────────────────────────────────────────────────────

/// The cataloged asset paths, sorted ascending. The VCS-neutral list a harness
/// feeds into a `.gitignore` managed block or a sync-service exclude. db.md
/// itself never writes any ignore file.
///
/// Every emitted path is routed through the same containment guard `scan`,
/// `verify`, and `status` use — the module contract is that the guard applies
/// "wherever a path is read or resolved" (SPEC § Assets > Path safety). A
/// poisoned / hand-edited manifest path that escapes the store (absolute, or a
/// `..` traversal — the `merge=union`-corruption state SPEC anticipates) is
/// OMITTED, so this list — which a harness pipes straight into a `.gitignore`
/// managed block or a sync-exclude — can never carry an out-of-store path. The
/// list analog of how `verify` counts an escaping record corrupt and `status`
/// counts it missing: a path that can't be a real store member is left out.
///
/// Markdown paths are omitted too: a declared `.md` asset is simultaneously a
/// content file, and an ignore feed carrying it could hide store content from
/// the VCS. Its bytes stay tracked and verified through the manifest; only this
/// ignore-oriented projection excludes it.
pub fn paths(store: &Store) -> crate::Result<Vec<String>> {
    Ok(read_manifest(store)?
        .into_iter()
        .filter(|r| store.capability_relative(Path::new(&r.path)).is_ok())
        .filter(|r| !is_markdown(&r.path))
        .map(|r| r.path)
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// Declaration parsing (shared with `validate`)
// ─────────────────────────────────────────────────────────────────────────────

/// Read all `asset:` / `assets:` declarations from a parsed frontmatter.
///
/// `asset: <path>` is a single required declaration. `assets:` is a list whose
/// items are either a bare path string (required) or a `{ path, required }`
/// mapping. Both keys may be present.
pub fn declared_assets(fm: &parser::Frontmatter) -> Vec<Declaration> {
    let mut out = Vec::new();
    if let Some(v) = fm.get("asset") {
        collect_declarations(&v, &mut out);
    }
    if let Some(v) = fm.get("assets") {
        collect_declarations(&v, &mut out);
    }
    out
}

/// Read declarations from an already-parsed YAML mapping. Used by
/// [`crate::validate`], which holds the parsed mapping and need not re-read the
/// file. Equivalent to [`declared_assets`] but keyed off a raw map.
pub fn declarations_from_yaml_map(map: &BTreeMap<String, Value>) -> Vec<Declaration> {
    let mut out = Vec::new();
    if let Some(v) = map.get("asset") {
        collect_declarations(v, &mut out);
    }
    if let Some(v) = map.get("assets") {
        collect_declarations(v, &mut out);
    }
    out
}

/// Parse the optional append-only asset supersession contract from typed
/// frontmatter. The wrapper must declare exactly one required replacement
/// asset; the older coordinate stays in the manifest as optional evidence.
pub fn asset_supersession(fm: &parser::Frontmatter) -> Result<Option<AssetSupersession>, String> {
    asset_supersession_from_parts(fm.get(SUPERSEDES_ASSET_KEY).as_ref(), declared_assets(fm))
}

/// Raw-map equivalent of [`asset_supersession`] for the validation sweep.
pub fn asset_supersession_from_yaml_map(
    map: &BTreeMap<String, Value>,
) -> Result<Option<AssetSupersession>, String> {
    asset_supersession_from_parts(
        map.get(SUPERSEDES_ASSET_KEY),
        declarations_from_yaml_map(map),
    )
}

fn asset_supersession_from_parts(
    value: Option<&Value>,
    declarations: Vec<Declaration>,
) -> Result<Option<AssetSupersession>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::String(original) = value else {
        return Err(format!("`{SUPERSEDES_ASSET_KEY}` must be one asset path"));
    };
    if declarations.len() != 1 || !declarations[0].required {
        return Err(format!(
            "a `{SUPERSEDES_ASSET_KEY}` wrapper must declare exactly one required replacement asset"
        ));
    }
    let original = normalize_asset_path(original)?;
    let replacement = normalize_asset_path(&declarations[0].path)?;
    if original == replacement {
        return Err(format!(
            "`{SUPERSEDES_ASSET_KEY}` cannot name the wrapper's replacement asset"
        ));
    }
    Ok(Some(AssetSupersession {
        original,
        replacement,
    }))
}

fn collect_declarations(v: &Value, out: &mut Vec<Declaration>) {
    match v {
        Value::String(s) => out.push(Declaration {
            path: s.clone(),
            required: true,
        }),
        Value::Sequence(items) => {
            for item in items {
                match item {
                    Value::String(s) => out.push(Declaration {
                        path: s.clone(),
                        required: true,
                    }),
                    Value::Mapping(m) => {
                        let path = m
                            .get(Value::String("path".to_string()))
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                        if let Some(path) = path {
                            let required = m
                                .get(Value::String("required".to_string()))
                                .and_then(|x| x.as_bool())
                                .unwrap_or(true);
                            out.push(Declaration { path, required });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Normalize a declared asset path to a CANONICAL store-relative forward-slash
/// string, rejecting absolute paths and any `..` / root component. This is the
/// lexical guard; [`crate::store::ensure_path_within_store`] is the resolved-path
/// guard applied before any disk read.
///
/// The result is the record key, so it MUST be canonical: `./sources/x.pdf`,
/// `sources/x.pdf`, and `sources/./x.pdf` all denote the same file and must fold
/// to the same key `sources/x.pdf`. The path is rebuilt from `Normal` components
/// only (dropping `CurDir`); hostile `..`/root/prefix components are still hard
/// errors (never silently sanitized), so a leading `./` is normalized away while
/// a traversal attempt is rejected.
pub fn normalize_asset_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty asset path".to_string());
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return Err(format!("absolute asset path not allowed: {raw}"));
    }
    let mut normal: Vec<&std::ffi::OsStr> = Vec::new();
    for c in p.components() {
        match c {
            Component::ParentDir => return Err(format!("`..` not allowed in asset path: {raw}")),
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("asset path escapes the store: {raw}"))
            }
            // A `.` (CurDir) carries no path information — drop it so the key is
            // canonical and `./x` does not split into a second record from `x`.
            Component::CurDir => {}
            Component::Normal(seg) => normal.push(seg),
        }
    }
    if normal.is_empty() {
        // The path was only `.`/`./` — no actual target.
        return Err(format!("asset path names no file: {raw}"));
    }
    let joined: PathBuf = normal.into_iter().collect();
    Ok(joined.to_string_lossy().replace('\\', "/"))
}

fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn rel_to_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Stream the file through SHA-256 (constant memory) and return
/// `(lowercase-hex digest, byte length)`.
fn sha256_file(mut f: std::fs::File) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    Ok((hex, total))
}

/// Best-effort MIME type from the path extension. Defaults to
/// `application/octet-stream`. This is deterministic (extension-driven), so it
/// does not break the manifest's rebuild equivalence.
fn media_type_for(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mt = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "tiff" | "tif" => "image/tiff",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "vtt" => "text/vtt",
        "srt" => "application/x-subrip",
        "html" | "htm" => "text/html",
        "epub" => "application/epub+zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        _ => "application/octet-stream",
    };
    mt.to_string()
}

/// Non-markdown files under `sources/` that no wrapper declares (the
/// un-wrappered-drop worklist). Walks the raw filesystem (so it sees files an
/// ignore mechanism would hide), skips `index.*` sidecars and hidden entries.
fn find_untracked(store: &Store, declared: &BTreeSet<String>) -> crate::Result<Vec<String>> {
    let mut out = Vec::new();
    let paths = match store.walk_regular_files(Path::new("sources")) {
        Ok(paths) => paths,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error.into()),
    };
    for path in paths {
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if is_markdown(name) || name == "index.jsonl" {
            continue;
        }
        let rel = rel_to_string(&path);
        if !declared.contains(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supersession_cycles_exclude_only_cycle_members() {
        let supersessions = BTreeMap::from([
            ("a".to_string(), ("b".to_string(), "a.md".to_string())),
            ("b".to_string(), ("a".to_string(), "b.md".to_string())),
            (
                "before".to_string(),
                ("a".to_string(), "before.md".to_string()),
            ),
            (
                "clean".to_string(),
                ("next".to_string(), "clean.md".to_string()),
            ),
        ]);
        assert_eq!(
            supersession_cycle_members(&supersessions),
            BTreeSet::from(["a".to_string(), "b".to_string()])
        );
    }

    /// Regression (adversarial review): `normalize_asset_path` must fold a
    /// leading/interior `.` (CurDir) into the canonical key, so `./sources/x.pdf`
    /// and `sources/x.pdf` are ONE record (not duplicated, byte-double-counted,
    /// and falsely reported untracked). Traversal / absolute / root stay hard
    /// errors — folding must never silently sanitize a hostile path.
    #[test]
    fn normalize_asset_path_folds_curdir_and_rejects_traversal() {
        assert_eq!(
            normalize_asset_path("./sources/x.pdf").unwrap(),
            "sources/x.pdf"
        );
        assert_eq!(
            normalize_asset_path("sources/x.pdf").unwrap(),
            "sources/x.pdf"
        );
        assert_eq!(
            normalize_asset_path("sources/./x.pdf").unwrap(),
            "sources/x.pdf"
        );
        assert_eq!(
            normalize_asset_path("sources/x.pdf/").unwrap(),
            "sources/x.pdf"
        );

        // Hostile / structural inputs are still rejected, not sanitized.
        assert!(normalize_asset_path("../outside.txt").is_err());
        assert!(normalize_asset_path("sources/../../etc/passwd").is_err());
        assert!(normalize_asset_path("/abs/x.pdf").is_err());
        // A `.`-only path (or empty) names no file.
        assert!(normalize_asset_path(".").is_err());
        assert!(normalize_asset_path("./").is_err());
        assert!(normalize_asset_path("").is_err());
    }

    /// Regression (adversarial review #16): a poisoned / hand-edited
    /// `assets.jsonl` whose `bytes` sum past u64::MAX must NOT abort `status`
    /// (debug overflow-checks) or silently WRAP (release). `status`/`scan` are
    /// non-failing reports over an editable manifest, so the byte totals SATURATE.
    #[test]
    fn status_and_scan_saturate_on_overflowing_manifest_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# store\n").unwrap();
        // Two in-store records whose byte sizes sum past u64::MAX.
        std::fs::write(
            root.join("assets.jsonl"),
            "{\"path\":\"records/a.bin\",\"sha256\":\"x\",\"bytes\":18446744073709551615,\
\"media_type\":\"application/octet-stream\",\"wrappers\":[\"records/w.md\"],\"required\":true}\n\
{\"path\":\"records/b.bin\",\"sha256\":\"y\",\"bytes\":1,\
\"media_type\":\"application/octet-stream\",\"wrappers\":[\"records/w.md\"],\"required\":true}\n",
        )
        .unwrap();
        let store = Store::from_root_and_config(root, crate::parser::Config::default()).unwrap();

        // status: must not panic; totals saturate at u64::MAX (both assets are
        // missing from disk, so bytes_missing accumulates them too).
        let report = status(&store).expect("status is non-failing on a poisoned manifest");
        assert_eq!(
            report.bytes_total,
            u64::MAX,
            "byte total must saturate, not wrap"
        );
        assert_eq!(
            report.bytes_missing,
            u64::MAX,
            "missing bytes must saturate too"
        );
        assert_eq!(report.total, 2);

        // scan's `.sum()` over the same records must likewise not overflow.
        scan(&store, true, false).expect("scan must not overflow on a poisoned manifest");
    }

    /// Build a minimal store with one wrapper declaring one present asset, and
    /// return `(store, canonical_manifest_string)` after an initial scan.
    fn store_with_one_asset() -> (tempfile::TempDir, Store, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# store\n").unwrap();
        std::fs::write(
            root.join("sources/a.pdf.md"),
            "---\ntype: pdf-source\nsummary: x\nasset: sources/a.pdf\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(root.join("sources/a.pdf"), b"PDFBYTES").unwrap();
        let store = Store::from_root_and_config(root, crate::parser::Config::default()).unwrap();
        let report = scan(&store, false, false).unwrap();
        assert!(
            report.wrote,
            "first scan writes the manifest; report: {report:?}"
        );
        let canonical = std::fs::read_to_string(root.join(MANIFEST_FILE)).unwrap();
        (tmp, store, canonical)
    }

    /// Regression (adversarial review): `assets scan`'s no-change gate must
    /// compare the canonical serialization against the on-disk BYTES, not parsed
    /// records. A duplicate-line manifest (the git `merge=union` recovery case,
    /// SPEC § Assets) parses — via `read_manifest`'s dedupe-by-path — back to the
    /// same records, so a records-vs-records gate would call it "no change" and
    /// leave the non-canonical bytes forever. `scan` must recompact it to the one
    /// canonical line and report `wrote: true` (mirroring `index::rebuild_all`,
    /// which always normalizes non-canonical artifacts).
    #[test]
    fn scan_recompacts_duplicate_line_manifest() {
        let (_tmp, store, canonical) = store_with_one_asset();
        let abs = store.root.join(MANIFEST_FILE);

        // Simulate a git `merge=union`: the same canonical line, twice.
        std::fs::write(&abs, format!("{canonical}{canonical}")).unwrap();
        assert_eq!(std::fs::read_to_string(&abs).unwrap().lines().count(), 2);

        let report = scan(&store, false, false).unwrap();
        assert!(
            report.wrote,
            "a non-canonical (duplicate-line) manifest must be recompacted and reported as updated"
        );
        let after = std::fs::read_to_string(&abs).unwrap();
        assert_eq!(
            after.lines().count(),
            1,
            "duplicate lines must collapse to the single canonical line"
        );
        assert_eq!(
            after, canonical,
            "scan must restore the exact canonical bytes"
        );
    }

    /// Regression (adversarial review): a wrongly-sorted / no-trailing-newline
    /// manifest is also non-canonical on-disk and must be repaired by `scan`,
    /// even though it parses (after the read-side sort) to the same records.
    #[test]
    fn scan_recompacts_noncanonical_byte_layout() {
        let (_tmp, store, canonical) = store_with_one_asset();
        let abs = store.root.join(MANIFEST_FILE);

        // Strip the trailing newline: same record, non-canonical bytes.
        std::fs::write(&abs, canonical.trim_end_matches('\n')).unwrap();
        let report = scan(&store, false, false).unwrap();
        assert!(
            report.wrote,
            "a manifest missing its trailing newline must be recompacted"
        );
        assert_eq!(
            std::fs::read_to_string(&abs).unwrap(),
            canonical,
            "scan must restore the canonical trailing newline"
        );
    }

    /// Regression (adversarial review): `paths` must enforce the containment
    /// guard "wherever it reads the manifest" (SPEC § Assets > Path safety),
    /// matching its sibling reads `verify`/`status`. A poisoned / hand-edited
    /// `assets.jsonl` (the `merge=union`-corruption state the SPEC anticipates)
    /// with an absolute (`/etc/hosts`) and a `..`-traversal recorded path must
    /// NOT leak those verbatim — they would flow straight into a harness's
    /// `.gitignore` managed block or sync-exclude. `paths` is a list, so the
    /// analog of verify-counts-corrupt / status-counts-missing is to OMIT them;
    /// the legitimate in-store path is still emitted unchanged.
    #[test]
    fn paths_omits_store_escaping_records() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# store\n").unwrap();
        // One legitimate in-store record plus two store-escaping ones.
        std::fs::write(
            root.join("assets.jsonl"),
            "{\"path\":\"sources/legit.pdf\",\"sha256\":\"a\",\"bytes\":9,\
\"media_type\":\"application/pdf\",\"wrappers\":[\"sources/legit.pdf.md\"],\"required\":true}\n\
{\"path\":\"../../../../../../etc/passwd\",\"sha256\":\"b\",\"bytes\":4096,\
\"media_type\":\"text/plain\",\"wrappers\":[\"sources/legit.pdf.md\"],\"required\":false}\n\
{\"path\":\"/etc/hosts\",\"sha256\":\"c\",\"bytes\":4096,\
\"media_type\":\"text/plain\",\"wrappers\":[\"sources/legit.pdf.md\"],\"required\":false}\n",
        )
        .unwrap();
        let store = Store::from_root_and_config(root, crate::parser::Config::default()).unwrap();

        let out = paths(&store).expect("paths is non-failing on a poisoned manifest");
        assert_eq!(
            out,
            vec!["sources/legit.pdf".to_string()],
            "only the safe in-store path is emitted; escaping paths are omitted"
        );
        assert!(
            !out.iter().any(|p| p.starts_with('/') || p.contains("..")),
            "no absolute or `..` path may ever leak from `paths`: {out:?}"
        );
    }

    /// A clean (all-in-store) manifest must be unchanged by the containment
    /// filter: every legitimate path is emitted, none dropped.
    #[test]
    fn paths_passes_a_clean_manifest_through_unchanged() {
        let (_tmp, store, _canonical) = store_with_one_asset();
        let out = paths(&store).expect("paths over a clean manifest");
        assert_eq!(out, vec!["sources/a.pdf".to_string()]);
    }

    /// Assets accept ANY in-store file — a markdown content file included
    /// (dual-plane: its bytes are tracked here while the content layer keeps
    /// owning its meaning). `scan`, `refresh`, and `refresh-wrapper` must all
    /// take the coordinate without a skip or an error, and `paths` must still
    /// OMIT the markdown path so an ignore feed can never hide a content file.
    #[test]
    fn markdown_content_files_are_accepted_as_assets_and_omitted_from_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# store\n").unwrap();
        std::fs::write(
            root.join("sources/bundle.md"),
            "---\ntype: pdf-source\nsummary: bundle wrapper\nassets:\n  - sources/notes.md\n  - sources/a.pdf\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("sources/notes.md"),
            "---\ntype: pdf-source\nsummary: a content file doubling as an asset\n---\nnotes\n",
        )
        .unwrap();
        std::fs::write(root.join("sources/a.pdf"), b"PDFBYTES").unwrap();
        let store = Store::from_root_and_config(root, crate::parser::Config::default()).unwrap();

        let report = scan(&store, false, false).unwrap();
        assert!(
            report.warnings.is_empty(),
            "a markdown asset must not be skipped or warned about: {:?}",
            report.warnings
        );
        assert_eq!(
            report.cataloged, 2,
            "both the pdf and the markdown asset are cataloged"
        );

        let refreshed = refresh(&store, "sources/notes.md", "sources/bundle.md")
            .expect("refresh accepts a markdown asset coordinate");
        assert_eq!(refreshed.path, "sources/notes.md");
        let reconciled = refresh_wrapper(&store, "sources/bundle.md")
            .expect("refresh-wrapper accepts a wrapper declaring a markdown asset");
        assert_eq!(reconciled.cataloged, 2);

        let manifest = read_manifest(&store).unwrap();
        let md_row = manifest
            .iter()
            .find(|record| record.path == "sources/notes.md")
            .expect("the markdown asset has a manifest row");
        assert_eq!(md_row.media_type, "text/markdown");

        let listed = paths(&store).expect("paths over the mixed manifest");
        assert_eq!(
            listed,
            vec!["sources/a.pdf".to_string()],
            "markdown assets are omitted from the ignore-feed list"
        );
    }

    /// Idempotency must survive the fix: a genuinely-canonical manifest is left
    /// byte-identical and `scan` reports `wrote: false`. (The old gate already
    /// did this for parsed-equal records; the byte gate must not regress it.)
    #[test]
    fn scan_canonical_manifest_is_left_untouched() {
        let (_tmp, store, canonical) = store_with_one_asset();
        let abs = store.root.join(MANIFEST_FILE);

        let report = scan(&store, false, false).unwrap();
        assert!(
            !report.wrote,
            "a canonical, unchanged manifest must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&abs).unwrap(),
            canonical,
            "a no-op rescan must leave the manifest byte-identical"
        );
    }

    #[test]
    fn refresh_wrapper_reconciles_one_generated_asset_set_in_one_manifest_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("records/package")).unwrap();
        std::fs::create_dir_all(root.join("sources/package/objects")).unwrap();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        let wrapper = "records/package/current.md";
        std::fs::write(
            root.join(wrapper),
            "---\ntype: package\nsummary: current\nassets:\n  - sources/package/objects/a.blob\n  - sources/package/objects/b.blob\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("sources/package/objects/a.blob"), b"a").unwrap();
        std::fs::write(root.join("sources/package/objects/b.blob"), b"bb").unwrap();
        let store = Store::from_root_and_config(root, crate::parser::Config::default()).unwrap();

        let first = refresh_wrapper(&store, wrapper).unwrap();
        assert_eq!(first.cataloged, 2);
        assert_eq!(first.added, 2);
        assert_eq!(first.removed, 0);
        assert_eq!(first.hashed, 2);
        assert_eq!(first.bytes, 3);
        assert!(first.wrote);

        let no_change = refresh_wrapper(&store, wrapper).unwrap();
        assert!(!no_change.wrote);
        assert_eq!(no_change.added, 0);
        assert_eq!(no_change.removed, 0);

        std::fs::write(
            root.join(wrapper),
            "---\ntype: package\nsummary: current\nassets:\n  - sources/package/objects/b.blob\n  - sources/package/objects/c.blob\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("sources/package/objects/c.blob"), b"ccc").unwrap();
        let changed = refresh_wrapper(&store, wrapper).unwrap();
        assert_eq!(changed.cataloged, 2);
        assert_eq!(changed.added, 1);
        assert_eq!(changed.removed, 1);
        assert_eq!(changed.bytes, 5);
        assert!(changed.wrote);

        let records = read_manifest(&store).unwrap();
        let paths: Vec<&str> = records.iter().map(|record| record.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "sources/package/objects/b.blob",
                "sources/package/objects/c.blob"
            ]
        );

        std::fs::write(
            root.join(wrapper),
            "---\ntype: package\nsummary: current\n---\n",
        )
        .unwrap();
        let cleared = refresh_wrapper(&store, wrapper).unwrap();
        assert_eq!(cleared.cataloged, 0);
        assert_eq!(cleared.added, 0);
        assert_eq!(cleared.removed, 2);
        assert!(cleared.wrote);
        assert!(read_manifest(&store).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_membership_reads_opened_root_after_path_replacement() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        std::fs::write(
            root.join(MANIFEST_FILE),
            "{\"path\":\"sources/owned.pdf\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"bytes\":1,\"media_type\":\"application/pdf\",\"wrappers\":[],\"required\":true}\n",
        )
        .unwrap();
        let store = Store::open_strict(&root).unwrap();
        let detached = sandbox.path().join("detached");
        std::fs::rename(&root, &detached).unwrap();

        let replacement = sandbox.path().join("replacement");
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(replacement.join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        std::fs::write(
            replacement.join(MANIFEST_FILE),
            "{\"path\":\"sources/replacement-secret.pdf\",\"sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"bytes\":1,\"media_type\":\"application/pdf\",\"wrappers\":[],\"required\":true}\n",
        )
        .unwrap();
        symlink(&replacement, &root).unwrap();

        assert_eq!(paths(&store).unwrap(), vec!["sources/owned.pdf"]);
    }
}
