//! `fsx` — the one atomic, durable file write for db.md's primary data.
//!
//! Every store-state file that holds **primary** data — content records
//! ([`crate::parser::write_file`]), `log.md` and its archives ([`crate::log`]),
//! and in-place link rewrites — is written through [`write_atomic`] or
//! [`write_atomic_new`]:
//!
//! 1. write the bytes to a uniquely-named sibling temp file in the *same*
//!    directory (`create_new`, so a predictable temp name can never be
//!    clobbered — closing the temp-clobber race);
//! 2. `fsync` the temp file;
//! 3. either `rename` it over the destination ([`write_atomic`]) or hard-link it
//!    into place with create-new semantics ([`write_atomic_new`]);
//! 4. `fsync` the parent directory so the committed directory entry survives a
//!    crash.
//!
//! These are the only primitives for durable writes — never `std::fs::write`,
//! which is neither atomic nor crash-durable. Use [`write_atomic`] when replacing
//! an existing file is intended; use [`write_atomic_new`] when the destination
//! must not already exist.
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
/// missing. On *any* early return between temp-file creation and a successful
/// rename — a `write_all`/`sync_all` failure (ENOSPC, EIO, quota) as well as a
/// rename failure — the temp file is cleaned up rather than leaked, via the
/// [`TempGuard`] `Drop` impl (mirroring `index.rs`'s `AtomicTemp`).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dbmd-tmp");
    let (mut f, mut guard) = create_temp_file(dir, file_name)?;

    // Scope the handle so it is flushed/closed before the rename. A failure here
    // returns via `?`; `guard` then drops and removes the orphaned temp file.
    {
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    // Preserve the destination's existing permission bits. The temp file was
    // created with the default mode (0666 & umask → 0644), and a bare
    // `rename(temp, dest)` would install *that* mode as the destination's new
    // mode — silently widening a deliberately-restricted file (e.g. `chmod 600`
    // on a record holding private data) to world-readable 0644 on every rewrite.
    // Copy the live destination mode onto the temp before the rename so an
    // in-place update keeps the file's permissions. Best-effort: if the
    // destination does not exist yet (a fresh create) or its metadata can't be
    // read, the default mode stands. A `set_permissions` failure is non-fatal —
    // the rewrite still commits with the default mode rather than aborting.
    copy_existing_permissions(path, &guard.path);

    // The rename either errors (guard drops, cleaning up the temp) or succeeds
    // (we disarm the guard so it does not remove the now-renamed destination).
    fs::rename(&guard.path, path)?;
    guard.disarm();
    sync_parent_dir(dir);
    Ok(())
}

/// Copy `dest`'s existing permission bits onto `temp` when `dest` already exists,
/// so a replace-by-rename preserves the original mode rather than resetting it to
/// the temp file's default. Best-effort and non-fatal: a missing destination (a
/// first create) or an unreadable mode simply leaves the temp's default in place.
fn copy_existing_permissions(dest: &Path, temp: &Path) {
    if let Ok(meta) = fs::metadata(dest) {
        let _ = fs::set_permissions(temp, meta.permissions());
    }
}

/// Atomically and durably create `path` with `bytes`, failing with
/// [`std::io::ErrorKind::AlreadyExists`] if the destination already exists.
///
/// This follows the same temp-file + file-fsync + parent-fsync sequence as
/// [`write_atomic`], but installs the temp file with `hard_link(temp, path)`
/// instead of `rename(temp, path)`. Hard-link creation is resolved atomically by
/// the OS and refuses an existing destination, so concurrent creators for the
/// same path produce exactly one winner and `AlreadyExists` for the rest. The
/// temporary link is removed after the destination link is established.
pub fn write_atomic_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dbmd-tmp");
    let (mut f, mut guard) = create_temp_file(dir, file_name)?;

    {
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    drop(f);

    fs::hard_link(&guard.path, path)?;
    if fs::remove_file(&guard.path).is_ok() {
        guard.disarm();
    }
    sync_parent_dir(dir);
    Ok(())
}

/// Drop-based cleanup for the hidden temp file `write_atomic` creates. While
/// armed, dropping the guard removes `path`. [`TempGuard::disarm`] is called
/// only after a successful rename, or after a successful temp-link cleanup in
/// [`write_atomic_new`], so the final destination is never touched.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    /// Stop cleaning up `path` on drop — used once the temp has been renamed
    /// into place and is no longer a stray temp file.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        // Best-effort cleanup if an error path bailed out before the rename.
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Create a uniquely-named temp file in `dir` with `create_new` (never clobbers
/// a predictable name), retrying on the vanishingly-rare collision. The name is
/// hidden (`.`-prefixed) and tagged with pid + nanos + a process-wide counter so
/// concurrent writers in the same directory never pick the same path. Returns the
/// open handle plus an armed [`TempGuard`] so any early return cleans up the temp.
fn create_temp_file(dir: &Path, file_name: &str) -> std::io::Result<(File, TempGuard)> {
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
            Ok(file) => {
                return Ok((
                    file,
                    TempGuard {
                        path: tmp,
                        armed: true,
                    },
                ))
            }
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

    #[test]
    fn write_atomic_new_creates_but_refuses_existing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("file.txt");

        write_atomic_new(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        let err = write_atomic_new(&target, b"second").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"first",
            "create-new failure must leave the existing destination untouched"
        );

        assert_no_temp_files(target.parent().unwrap());
    }

    #[test]
    fn write_atomic_new_allows_only_one_concurrent_creator() {
        use std::sync::{Arc, Barrier};

        for round in 0..40 {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("file.txt");
            let barrier = Arc::new(Barrier::new(8));

            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let target = target.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let payload = format!("payload-{i}");
                        barrier.wait();
                        let result = write_atomic_new(&target, payload.as_bytes())
                            .map(|_| ())
                            .map_err(|e| e.kind());
                        (payload, result)
                    })
                })
                .collect();

            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let winners: Vec<_> = results
                .iter()
                .filter_map(|(payload, result)| result.is_ok().then_some(payload))
                .collect();
            let already_exists = results
                .iter()
                .filter(|(_, result)| {
                    matches!(result, Err(kind) if *kind == std::io::ErrorKind::AlreadyExists)
                })
                .count();

            assert_eq!(
                winners.len(),
                1,
                "round {round}: exactly one creator may win, got {results:?}"
            );
            assert_eq!(
                already_exists, 7,
                "round {round}: every losing creator must get AlreadyExists, got {results:?}"
            );

            let written = std::fs::read_to_string(&target).unwrap();
            assert_eq!(
                written, *winners[0],
                "round {round}: destination must contain the winner's payload"
            );
            assert_no_temp_files(tmp.path());
        }
    }

    /// Regression for finding #22: an early return between temp-file creation and
    /// a successful rename (e.g. `write_all`/`sync_all` failing under ENOSPC/EIO)
    /// must NOT leave the hidden temp file orphaned in the data directory.
    ///
    /// Pre-fix, `create_temp_file` handed back a bare `PathBuf` with no `Drop`
    /// cleanup, so dropping it without a rename — exactly what `?` does on a
    /// write/sync failure — left the temp on disk. This reconstructs that path by
    /// dropping the guard without renaming and asserting the temp is gone.
    #[test]
    fn regression_armed_guard_removes_temp_on_early_drop() {
        let dir = TempDir::new().unwrap();
        let (file, guard) = create_temp_file(dir.path(), "file.txt").unwrap();
        let tmp_path = guard.path.clone();
        assert!(
            tmp_path.exists(),
            "temp file should exist after create_temp_file"
        );

        // Simulate a write/sync failure bailing out before the rename: the file
        // handle and the (still-armed) guard go out of scope without a rename.
        drop(file);
        drop(guard);

        assert!(
            !tmp_path.exists(),
            "armed guard must remove the orphaned temp file on early drop"
        );
        // No stray `.tmp.` files left in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files may be left behind");
    }

    /// Once disarmed (after a successful rename) the guard must NOT delete the
    /// path it was tracking — otherwise it would clobber the renamed destination.
    #[test]
    fn regression_disarmed_guard_leaves_file_intact() {
        let dir = TempDir::new().unwrap();
        let (file, mut guard) = create_temp_file(dir.path(), "kept.txt").unwrap();
        drop(file);
        let kept = guard.path.clone();

        guard.disarm();
        drop(guard);

        assert!(
            kept.exists(),
            "disarmed guard must leave the renamed destination untouched"
        );
    }

    fn assert_no_temp_files(dir: &Path) {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files may be left behind");
    }

    /// Regression: rewriting an existing file via `write_atomic` must PRESERVE
    /// its permission bits. Pre-fix the temp file's default mode (0644) replaced
    /// a deliberately-restricted destination (0600) on every rewrite — a quiet
    /// permission-widening on user data. A first create still uses the default
    /// mode (there is no destination mode to copy).
    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_existing_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("private.md");

        // Create, then restrict to 0600.
        write_atomic(&target, b"secret v1").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(before, 0o600, "fixture must start at 0600");

        // Rewrite in place: the 0600 mode must survive (not reset to 0644).
        write_atomic(&target, b"secret v2").unwrap();
        let after = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            after, 0o600,
            "write_atomic must preserve the destination's 0600 mode, got {after:o}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"secret v2");
    }
}
