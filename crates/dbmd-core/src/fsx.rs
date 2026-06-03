//! `fsx` — the one atomic, durable file write for db.md's primary data.
//!
//! Every store-state file that holds **primary** data — content records
//! ([`crate::parser::write_file`]), `log.md` and its archives ([`crate::log`]),
//! and in-place link rewrites — is replaced through [`write_atomic`]:
//!
//! 1. write the bytes to a uniquely-named sibling temp file in the *same*
//!    directory (`create_new`, so a predictable temp name can never be
//!    clobbered — closing the temp-clobber race);
//! 2. `fsync` the temp file;
//! 3. `rename` it over the destination (atomic on a single filesystem, so a
//!    concurrent reader never observes a half-written file);
//! 4. `fsync` the parent directory so the rename survives a crash.
//!
//! This is the single primitive for durable writes — never `std::fs::write`,
//! which is neither atomic nor crash-durable.
//!
//! **Not for the index.** `index.md` / `index.jsonl` are *derived, rebuildable*
//! artifacts on the O(changed) write-through path; they use their own
//! atomic-but-not-`fsync`'d writer ([`crate::index`]'s `AtomicTemp`) on purpose
//! — a crash-lost index write is recovered by `dbmd index rebuild`, so paying an
//! `fsync` per catalog update on the hot loop would be cost without benefit.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically and durably replace `path` with `bytes` (see the module docs for
/// the write/fsync/rename/fsync sequence). The parent directory is created if
/// missing. On a rename failure the temp file is cleaned up rather than leaked.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dbmd-tmp");
    let (mut f, tmp) = create_temp_file(dir, file_name)?;

    // Scope the handle so it is flushed/closed before the rename.
    {
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    match fs::rename(&tmp, path) {
        Ok(()) => {
            sync_parent_dir(dir);
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Create a uniquely-named temp file in `dir` with `create_new` (never clobbers
/// a predictable name), retrying on the vanishingly-rare collision. The name is
/// hidden (`.`-prefixed) and tagged with pid + nanos + a process-wide counter so
/// concurrent writers in the same directory never pick the same path.
fn create_temp_file(dir: &Path, file_name: &str) -> std::io::Result<(File, PathBuf)> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for _ in 0..128 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{file_name}.tmp.{pid}.{nanos}.{seq}"));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique dbmd temp file",
    ))
}

/// Best-effort `fsync` of the directory so a completed `rename` is durable across
/// a crash. Non-fatal: some filesystems disallow directory `fsync`.
fn sync_parent_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_atomic_creates_then_replaces_durably() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("file.txt"); // parent missing

        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        // Replace in place — content swaps, no temp files left behind.
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files may be left behind");
    }

    #[test]
    fn write_atomic_is_byte_exact_including_empty() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("empty.txt");
        write_atomic(&target, b"").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"");
    }
}
