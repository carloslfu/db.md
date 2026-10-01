// SPDX-License-Identifier: Apache-2.0

//! What the default `dbmd validate` has already checked.
//!
//! The working set comes from the filesystem, never from `log.md`. The log is
//! written by the same agent whose work is being checked, so a file it forgot
//! to log, or logged under a custom kind, is exactly the file most likely to be
//! wrong. After each default run, dbmd records the metadata it observed (size,
//! modification time, change time) for every content file that validated with
//! no error and no warning. The next run re-checks every file whose metadata
//! differs, every new file, every file that still had findings, and every file
//! linking to a changed or removed path.
//!
//! Anything that could make an old record wrong discards it and the run falls
//! back to a full per-file sweep: another toolkit version, a changed `DB.md`
//! (schemas and policies), a moved or copied store (the root directory's
//! identity is bound into the record), or a missing, unreadable or malformed
//! record. The record lives in the store-local `.dbmd/validate-state.json`,
//! which never syncs, indexes or validates; a sibling `.dbmd/.gitignore` keeps
//! it out of version control.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Store;

/// Store-relative location of the record.
pub const STATE_FILE: &str = ".dbmd/validate-state.json";
const STATE_DIR: &str = ".dbmd";
const GITIGNORE_FILE: &str = ".dbmd/.gitignore";
const GITIGNORE_BODY: &str =
    "# Written by dbmd: local validation state, never shared.\nvalidate-state.json\n.gitignore\n";
const STATE_VERSION: u32 = 1;
/// Upper bound for reading the record (about 60 bytes per file).
const MAX_STATE_BYTES: u64 = 256 * 1024 * 1024;

/// Observed metadata for one content file: byte length, modification time and
/// change time, both in nanoseconds since the Unix epoch. The change time
/// catches copies that preserve an old modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp(pub u64, pub u64, pub u64);

/// A file recorded as clean, and whether it declares assets (so a changed
/// `assets.jsonl` knows which files to re-check).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry(pub Stamp, pub bool);

/// The persisted record. Every field except `files` is a binding: when any of
/// them no longer matches the live store, the whole record is discarded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationState {
    version: u32,
    toolkit: String,
    root: String,
    db_md: String,
    pub(crate) assets: String,
    pub(crate) files: BTreeMap<String, Entry>,
}

/// The live bindings a record must match to be trusted.
#[derive(Debug, Clone, PartialEq)]
pub struct Bindings {
    root: String,
    db_md: String,
    pub(crate) assets: String,
}

impl Bindings {
    /// Read the live bindings. `None` when the store cannot be identified, in
    /// which case the caller runs a full sweep and records nothing.
    pub fn read(store: &Store) -> Option<Bindings> {
        Some(Bindings {
            root: store.root_identity().ok()?,
            db_md: file_sha256(store, Path::new("DB.md")).ok()??,
            assets: file_sha256(store, Path::new(crate::assets::MANIFEST_FILE))
                .ok()?
                .unwrap_or_default(),
        })
    }
}

impl ValidationState {
    /// An empty record for these bindings.
    pub fn new(bindings: &Bindings) -> ValidationState {
        ValidationState {
            version: STATE_VERSION,
            toolkit: env!("CARGO_PKG_VERSION").to_string(),
            root: bindings.root.clone(),
            db_md: bindings.db_md.clone(),
            assets: bindings.assets.clone(),
            files: BTreeMap::new(),
        }
    }

    /// Load the record when it exists, parses, and matches every binding except
    /// the asset manifest (the caller re-checks asset-declaring files when that
    /// alone changed). Anything else yields `None`: validate everything.
    pub fn load(store: &Store, bindings: &Bindings) -> Option<ValidationState> {
        let bytes = store
            .read_bounded(Path::new(STATE_FILE), MAX_STATE_BYTES)
            .ok()?;
        let state: ValidationState = serde_json::from_slice(&bytes).ok()?;
        let trusted = state.version == STATE_VERSION
            && state.toolkit == env!("CARGO_PKG_VERSION")
            && state.root == bindings.root
            && state.db_md == bindings.db_md;
        trusted.then_some(state)
    }

    /// Persist the record. Failure is not an error for validation: the next
    /// run simply re-checks more.
    pub fn save(&self, store: &Store) {
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        if store.create_dir_all(Path::new(STATE_DIR)).is_err() {
            return;
        }
        if !store
            .regular_file_exists(Path::new(GITIGNORE_FILE))
            .unwrap_or(true)
        {
            let _ =
                store.write_atomic_nondurable(Path::new(GITIGNORE_FILE), GITIGNORE_BODY.as_bytes());
        }
        let _ = store.write_atomic_nondurable(Path::new(STATE_FILE), &bytes);
    }

    /// The recorded entry for `rel`, if any.
    pub fn entry(&self, rel: &Path) -> Option<Entry> {
        self.files.get(&key(rel)).copied()
    }

    /// Record `rel` as clean at `stamp`.
    pub fn record(&mut self, rel: &Path, stamp: Stamp, declares_assets: bool) {
        self.files.insert(key(rel), Entry(stamp, declares_assets));
    }

    /// Forget `rel` (it changed, was removed, or still has findings).
    pub fn forget(&mut self, rel: &Path) {
        self.files.remove(&key(rel));
    }

    /// Every recorded path.
    pub fn paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.files.keys().map(PathBuf::from)
    }

    /// Recorded paths that declare assets.
    pub fn asset_declaring_paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.files
            .iter()
            .filter(|(_, entry)| entry.1)
            .map(|(path, _)| PathBuf::from(path))
    }

    /// Rebind the record to the live asset manifest after its dependents were
    /// re-checked.
    pub fn rebind_assets(&mut self, bindings: &Bindings) {
        self.assets = bindings.assets.clone();
    }
}

/// The observed metadata of one store-relative regular file, or `None` when it
/// cannot be read (the caller treats it as changed).
pub fn stamp(store: &Store, rel: &Path) -> Option<Stamp> {
    let metadata = store.regular_metadata(rel).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    Some(Stamp(metadata.len(), modified, change_time(&metadata)))
}

#[cfg(unix)]
fn change_time(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    let seconds = u64::try_from(metadata.ctime()).unwrap_or(0);
    let nanos = u64::try_from(metadata.ctime_nsec()).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[cfg(not(unix))]
fn change_time(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn key(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// SHA-256 of a store-relative file's bytes; `Ok(None)` when it is absent.
fn file_sha256(store: &Store, rel: &Path) -> std::io::Result<Option<String>> {
    if !store.regular_file_exists(rel)? {
        return Ok(None);
    }
    let bytes = store.read_bounded(rel, MAX_STATE_BYTES)?;
    Ok(Some(format!("{:x}", Sha256::digest(&bytes))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &Path) -> Store {
        std::fs::write(dir.join("DB.md"), "---\ntype: db-md\n---\n# t\n").unwrap();
        Store::open_strict(dir).unwrap()
    }

    #[test]
    fn record_round_trips_and_is_ignored_by_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let bindings = Bindings::read(&store).unwrap();
        let mut state = ValidationState::new(&bindings);
        state.record(Path::new("records/a.md"), Stamp(1, 2, 3), true);
        state.save(&store);
        let loaded = ValidationState::load(&store, &bindings).unwrap();
        assert_eq!(loaded, state);
        let ignore = std::fs::read_to_string(tmp.path().join(GITIGNORE_FILE)).unwrap();
        assert!(ignore.contains("validate-state.json"));
    }

    #[test]
    fn changed_db_md_discards_the_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let bindings = Bindings::read(&store).unwrap();
        let mut state = ValidationState::new(&bindings);
        state.record(Path::new("records/a.md"), Stamp(1, 2, 3), false);
        state.save(&store);
        std::fs::write(
            tmp.path().join("DB.md"),
            "---\ntype: db-md\n---\n# changed\n",
        )
        .unwrap();
        let store = Store::open_strict(tmp.path()).unwrap();
        let rebound = Bindings::read(&store).unwrap();
        assert!(ValidationState::load(&store, &rebound).is_none());
    }

    #[test]
    fn a_copied_store_does_not_inherit_the_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let original = tmp.path().join("a");
        std::fs::create_dir_all(&original).unwrap();
        let store = store_in(&original);
        let bindings = Bindings::read(&store).unwrap();
        let mut state = ValidationState::new(&bindings);
        state.record(Path::new("records/a.md"), Stamp(1, 2, 3), false);
        state.save(&store);

        let copy = tmp.path().join("b");
        std::fs::create_dir_all(copy.join(".dbmd")).unwrap();
        std::fs::copy(original.join("DB.md"), copy.join("DB.md")).unwrap();
        std::fs::copy(original.join(STATE_FILE), copy.join(STATE_FILE)).unwrap();
        let copied = Store::open_strict(&copy).unwrap();
        let copied_bindings = Bindings::read(&copied).unwrap();
        assert!(ValidationState::load(&copied, &copied_bindings).is_none());
    }

    #[test]
    fn malformed_record_is_discarded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".dbmd")).unwrap();
        std::fs::write(tmp.path().join(STATE_FILE), "{not json").unwrap();
        let bindings = Bindings::read(&store).unwrap();
        assert!(ValidationState::load(&store, &bindings).is_none());
    }
}
