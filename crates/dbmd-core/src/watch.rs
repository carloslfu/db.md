// SPDX-License-Identifier: Apache-2.0

//! Local change detection — the snapshot/diff engine behind `dbmd watch`.
//!
//! Poll-based and dependency-free by design: a snapshot stats the store's
//! emit membership (every content file plus `DB.md` — [`emit::walk_rels`],
//! so `watch` and `emit` can never disagree about what is observable), and a
//! diff of two snapshots yields the created / modified / removed set in
//! deterministic path order. No OS file-event API is used — kernel watchers
//! differ per platform, mis-report on network filesystems, and would be the
//! toolkit's first such dependency; a bounded stat sweep per tick is simple,
//! portable, and honest about cost (the caller narrows big stores with a
//! prefix). Modification is detected by `(byte length, mtime)` — the
//! standard watcher tradeoff: a same-length rewrite inside one mtime
//! granule is invisible.
//!
//! Everything here is pure observation: no locks are taken and nothing is
//! written, so a watcher never blocks a writer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::emit;
use crate::store::Store;

/// One observed file's cheap identity: byte length plus mtime (absent where
/// the filesystem reports none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSig {
    /// File size in bytes.
    pub len: u64,
    /// Last-modification time, when the filesystem reports one.
    pub modified: Option<SystemTime>,
}

/// A point-in-time view of the watched membership: store-relative path →
/// signature, path-ordered.
pub type Snapshot = BTreeMap<PathBuf, FileSig>;

/// What happened to one path between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Present now, absent before.
    Created,
    /// Present in both with a different signature.
    Modified,
    /// Absent now, present before. A rename appears as removed + created.
    Removed,
}

impl ChangeKind {
    /// The event word used on the wire and in human output.
    pub fn word(self) -> &'static str {
        match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Removed => "removed",
        }
    }
}

/// One observed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happened.
    pub kind: ChangeKind,
    /// The store-relative path it happened to.
    pub path: PathBuf,
}

/// Snapshot the watched membership, optionally narrowed to a store-relative
/// `prefix`. A file that vanishes between the walk and its stat is simply
/// absent from this snapshot (the next diff reports it removed) — a benign
/// race, not an error.
pub fn snapshot(store: &Store, prefix: Option<&Path>) -> crate::Result<Snapshot> {
    let mut snap = Snapshot::new();
    for rel in emit::walk_rels(store)? {
        if let Some(p) = prefix {
            if !rel.starts_with(p) {
                continue;
            }
        }
        if let Ok(metadata) = store.regular_metadata(&rel) {
            snap.insert(
                rel,
                FileSig {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            );
        }
    }
    Ok(snap)
}

/// Diff two snapshots into the created / modified / removed set, in
/// deterministic store-path order.
pub fn diff(prev: &Snapshot, next: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();
    for (path, sig) in next {
        match prev.get(path) {
            None => changes.push(Change {
                kind: ChangeKind::Created,
                path: path.clone(),
            }),
            Some(before) if before != sig => changes.push(Change {
                kind: ChangeKind::Modified,
                path: path.clone(),
            }),
            Some(_) => {}
        }
    }
    for path in prev.keys() {
        if !next.contains_key(path) {
            changes.push(Change {
                kind: ChangeKind::Removed,
                path: path.clone(),
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: test\nowner: T\n---\n# T\n",
        )
        .unwrap();
        let notes = dir.path().join("records").join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join("a.md"),
            "---\ntype: note\nsummary: A\n---\nalpha\n",
        )
        .unwrap();
        let store = Store::open_strict(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn snapshot_covers_content_plus_db_md() {
        let (_tmp, store) = scratch_store();
        let snap = snapshot(&store, None).unwrap();
        let paths: Vec<String> = snap
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["DB.md", "records/notes/a.md"]);
    }

    #[test]
    fn diff_reports_created_modified_removed_in_path_order() {
        let (tmp, store) = scratch_store();
        let before = snapshot(&store, None).unwrap();

        let notes = tmp.path().join("records").join("notes");
        // Modified: a different byte length is visible regardless of mtime
        // granularity. Created + removed round out the set.
        std::fs::write(
            notes.join("a.md"),
            "---\ntype: note\nsummary: A\n---\nalpha extended\n",
        )
        .unwrap();
        std::fs::write(
            notes.join("b.md"),
            "---\ntype: note\nsummary: B\n---\nbeta\n",
        )
        .unwrap();
        std::fs::remove_file(tmp.path().join("DB.md")).unwrap();

        let after = snapshot(&store, None).unwrap();
        let changes = diff(&before, &after);
        let rendered: Vec<String> = changes
            .iter()
            .map(|c| format!("{} {}", c.kind.word(), c.path.to_string_lossy()))
            .collect();
        assert_eq!(
            rendered,
            vec![
                "removed DB.md",
                "modified records/notes/a.md",
                "created records/notes/b.md",
            ]
        );
    }

    #[test]
    fn prefix_scopes_the_membership() {
        let (tmp, store) = scratch_store();
        let widgets = tmp.path().join("records").join("widgets");
        std::fs::create_dir_all(&widgets).unwrap();
        std::fs::write(widgets.join("w.md"), "---\ntype: widget\nsummary: W\n---\n").unwrap();

        let scoped = snapshot(&store, Some(Path::new("records/widgets"))).unwrap();
        let paths: Vec<String> = scoped
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["records/widgets/w.md"]);
    }

    #[test]
    fn identical_snapshots_diff_empty() {
        let (_tmp, store) = scratch_store();
        let a = snapshot(&store, None).unwrap();
        let b = snapshot(&store, None).unwrap();
        assert!(diff(&a, &b).is_empty());
    }
}
