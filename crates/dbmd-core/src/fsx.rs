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

#[cfg(unix)]
use std::collections::BTreeMap;
use std::fs::File;
#[cfg(test)]
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::path::Component;
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
    #[cfg(unix)]
    {
        write_atomic_unix(path, bytes, false, true)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        Err(secure_filesystem_unsupported())
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
    #[cfg(unix)]
    {
        write_atomic_unix(path, bytes, true, true)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        Err(secure_filesystem_unsupported())
    }
}

#[cfg(not(unix))]
fn secure_filesystem_unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure filesystem mutation requires handle-relative no-follow primitives on this platform",
    )
}

/// Open one regular file exactly once through no-follow directory handles and
/// read at most `max_bytes`. The size check and read operate on the same inode.
pub fn read_bounded_nofollow(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = open_regular_nofollow(path)?;
    read_bounded_file(file, max_bytes)
}

fn read_bounded_file(file: File, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    // `u64::MAX + 1` wraps in release builds and panics in debug builds. A
    // caller is allowed to express "no smaller than the addressable stream" as
    // `u64::MAX`; in that case there is no representable sentinel byte beyond
    // the limit, so read to the saturated ceiling and rely on the descriptor
    // metadata check above as the only possible over-limit signal.
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file grew beyond the read limit",
        ));
    }
    Ok(bytes)
}

/// A held store-directory capability for bounded sweep reads. Parent directory
/// handles are cached by relative path, so a 10k-file scan pays one no-follow
/// traversal per folder and one `openat` per file instead of reopening the
/// entire ancestor chain for every record.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct BoundedDirReader {
    root: File,
    parents: BTreeMap<PathBuf, File>,
}

#[cfg(unix)]
impl BoundedDirReader {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let root = open_directory_nofollow(root)?;
        Ok(Self {
            root,
            parents: BTreeMap::new(),
        })
    }

    /// Start a bounded reader from an already-held directory capability. This
    /// is the store-safe constructor: once `Store::open` succeeds, no later
    /// operation re-resolves the user-supplied store pathname.
    pub(crate) fn from_root(root: &File) -> std::io::Result<Self> {
        Ok(Self {
            root: root.try_clone()?,
            parents: BTreeMap::new(),
        })
    }

    pub(crate) fn read(&mut self, relative: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        read_bounded_file(self.open(relative)?, max_bytes)
    }

    pub(crate) fn open(&mut self, relative: &Path) -> std::io::Result<File> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "bounded directory read requires a contained relative path",
            ));
        }
        let leaf = relative.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));

        if !self.parents.contains_key(parent) {
            let mut cursor = self.root.try_clone()?;
            for component in parent.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                let fd = unsafe {
                    libc::openat(
                        cursor.as_raw_fd(),
                        c_name(name)?.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                cursor = unsafe { File::from_raw_fd(fd) };
                if directory_contains_exact_regular(&cursor, "DB.md".as_ref())? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "refusing to cross a nested db.md store boundary",
                    ));
                }
            }
            self.parents.insert(parent.to_path_buf(), cursor);
        }

        let parent_fd = self
            .parents
            .get(parent)
            .expect("parent capability inserted above")
            .as_raw_fd();
        let fd = unsafe {
            libc::openat(
                parent_fd,
                c_name(leaf)?.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "refusing to read a non-regular file",
            ));
        }
        Ok(file)
    }
}

/// Open one directory exactly once without following its leaf or any ancestor.
/// Store roots retain this descriptor for their full lifetime so a rename or
/// symlink swap of the original pathname cannot redirect later operations.
#[cfg(unix)]
pub(crate) fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    // Reuse the ancestor traversal by appending a synthetic leaf and retaining
    // the returned parent. Unlike `path.file_name()`, this also handles the
    // ordinary store spellings `.` and `/`.
    let (directory, _) = open_parent_unix(&path.join(".dbmd-held-root-capability"), false)?;
    Ok(directory)
}

#[cfg(not(unix))]
pub(crate) fn open_directory_nofollow(_path: &Path) -> std::io::Result<File> {
    Err(secure_filesystem_unsupported())
}

/// Test for an exact byte-for-byte regular-file basename inside a held
/// directory. This is deliberately descriptor-relative: on a case-insensitive
/// filesystem `openat(dir, "DB.md")` can open a lowercase `db.md`, while the
/// db.md format requires the uppercase marker spelling.
#[cfg(unix)]
pub(crate) fn directory_contains_exact_regular(
    directory: &File,
    wanted: &std::ffi::OsStr,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let dot = c_name(std::ffi::OsStr::new("."))?;
    let scan_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if scan_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(scan_fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(scan_fd);
        }
        return Err(error);
    }

    let mut found = false;
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != wanted.as_bytes() {
            continue;
        }
        let c_wanted = c_name(wanted)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                c_wanted.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
        {
            found = true;
        }
        break;
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(found)
}

#[cfg(not(unix))]
pub(crate) fn directory_contains_exact_regular(
    _directory: &File,
    _wanted: &std::ffi::OsStr,
) -> std::io::Result<bool> {
    Err(secure_filesystem_unsupported())
}

/// Recursively enumerate regular files below a held directory capability.
///
/// Symlinks, hidden names, and nested db.md stores are never traversed. Paths
/// are returned relative to `root`, including the caller-supplied `start`
/// prefix. This is the sweep-side counterpart to [`BoundedDirReader`]: callers
/// do not reopen the mutable store pathname merely to discover what to read.
#[cfg(unix)]
pub(crate) fn walk_regular_files_beneath(
    root: &File,
    start: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    const MAX_WALK_ENTRIES: usize = 1_000_000;

    fn open_dir_at(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c_name(name)?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    if start.is_absolute()
        || start.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "store walk requires a contained relative path",
        ));
    }

    let mut start_dir = root.try_clone()?;
    for component in start.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        start_dir = open_dir_at(&start_dir, name)?;
        if directory_contains_exact_regular(&start_dir, "DB.md".as_ref())? {
            return Ok(Vec::new());
        }
    }

    let mut pending = vec![(start_dir, start.to_path_buf())];
    let mut files = Vec::new();
    let mut seen = 0usize;
    while let Some((directory, relative_dir)) = pending.pop() {
        let scan_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name(std::ffi::OsStr::new("."))?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if scan_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(scan_fd) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(scan_fd);
            }
            return Err(error);
        }

        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if matches!(bytes, b"." | b"..") || bytes.starts_with(b".") {
                continue;
            }
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
            seen = seen.saturating_add(1);
            if seen > MAX_WALK_ENTRIES {
                unsafe {
                    libc::closedir(stream);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "store contains more than 1000000 visible entries",
                ));
            }
        }
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        for name in names {
            let c_name = c_name(&name)?;
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    c_name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(error);
            }
            let relative = relative_dir.join(&name);
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => files.push(relative),
                libc::S_IFDIR => {
                    let child = open_dir_at(&directory, &name)?;
                    if !directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                        pending.push((child, relative));
                    }
                }
                _ => {}
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Discover visible symlinks and nested-store roots without following either.
#[cfg(unix)]
pub(crate) fn ownership_boundaries_beneath(
    root: &File,
) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let mut pending = vec![(root.try_clone()?, PathBuf::new())];
    let mut symlinks = Vec::new();
    let mut nested = Vec::new();
    while let Some((directory, relative_dir)) = pending.pop() {
        let scan_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name(std::ffi::OsStr::new("."))?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if scan_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(scan_fd) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(scan_fd);
            }
            return Err(error);
        }
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if matches!(bytes, b"." | b"..") || bytes.starts_with(b".") {
                continue;
            }
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for name in names {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    c_name(&name)?.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                continue;
            }
            let relative = relative_dir.join(&name);
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFLNK => symlinks.push(relative),
                libc::S_IFDIR => {
                    let fd = unsafe {
                        libc::openat(
                            directory.as_raw_fd(),
                            c_name(&name)?.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                        )
                    };
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let child = unsafe { File::from_raw_fd(fd) };
                    if directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                        nested.push(relative);
                    } else {
                        pending.push((child, relative));
                    }
                }
                _ => {}
            }
        }
    }
    symlinks.sort();
    nested.sort();
    Ok((symlinks, nested))
}

#[cfg(not(unix))]
pub(crate) fn ownership_boundaries_beneath(
    _root: &File,
) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    Err(secure_filesystem_unsupported())
}

#[cfg(unix)]
pub(crate) fn regular_file_names_beneath(
    root: &File,
    directory: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let probe = directory.join(".dbmd-directory-list-probe");
    let (directory, _) = open_parent_beneath(root, &probe, false)?;
    let scan_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c_name(std::ffi::OsStr::new("."))?.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if scan_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(scan_fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(scan_fd);
        }
        return Err(error);
    }

    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if matches!(bytes, b"." | b"..") || bytes.starts_with(b".") {
            continue;
        }
        let name = std::ffi::OsString::from_vec(bytes.to_vec());
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                c_name(&name)?.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
        {
            names.push(name);
        }
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

#[cfg(not(unix))]
pub(crate) fn regular_file_names_beneath(
    _root: &File,
    _directory: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(unix))]
pub(crate) fn walk_regular_files_beneath(
    _root: &File,
    _start: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(unix))]
pub(crate) struct BoundedDirReader;

#[cfg(not(unix))]
impl BoundedDirReader {
    pub(crate) fn new(_root: &Path) -> std::io::Result<Self> {
        Err(secure_filesystem_unsupported())
    }

    pub(crate) fn read(&mut self, _relative: &Path, _max_bytes: u64) -> std::io::Result<Vec<u8>> {
        Err(secure_filesystem_unsupported())
    }

    pub(crate) fn open(&mut self, _relative: &Path) -> std::io::Result<File> {
        Err(secure_filesystem_unsupported())
    }
}

/// Open one regular file through held no-follow parent descriptors. The caller
/// may safely perform metadata checks and reads on the returned inode without a
/// pathname reopen in between.
pub fn open_regular_nofollow(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let (directory, leaf) = open_parent_unix(path, false)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "path is not a regular file",
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(secure_filesystem_unsupported())
    }
}

/// Rename a single entry without re-resolving either parent through mutable
/// pathnames. Existing symlink ancestors are refused.
#[cfg(unix)]
pub fn rename_nofollow(old: &Path, new: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let (old_parent, old_leaf) = open_parent_unix(old, false)?;
    let (new_parent, new_leaf) = open_parent_unix(new, true)?;
    let mut source_stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            old_parent.as_raw_fd(),
            old_leaf.as_ptr(),
            &mut source_stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if (source_stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to rename a non-regular file",
        ));
    }
    renameat_noreplace(
        old_parent.as_raw_fd(),
        &old_leaf,
        new_parent.as_raw_fd(),
        &new_leaf,
    )?;
    old_parent.sync_all()?;
    new_parent.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub fn rename_nofollow(_old: &Path, _new: &Path) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn renameat_noreplace(
    old_dir: std::os::fd::RawFd,
    old: &std::ffi::CStr,
    new_dir: std::os::fd::RawFd,
    new: &std::ffi::CStr,
) -> std::io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_dir,
            old.as_ptr(),
            new_dir,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn renameat_noreplace(
    old_dir: std::os::fd::RawFd,
    old: &std::ffi::CStr,
    new_dir: std::os::fd::RawFd,
    new: &std::ffi::CStr,
) -> std::io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            old_dir,
            old.as_ptr(),
            new_dir,
            new.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn renameat_noreplace(
    _old_dir: std::os::fd::RawFd,
    _old: &std::ffi::CStr,
    _new_dir: std::os::fd::RawFd,
    _new: &std::ffi::CStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unsupported on this Unix platform",
    ))
}

#[cfg(unix)]
fn c_name(value: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(value.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))
}

#[cfg(unix)]
fn open_parent_unix(path: &Path, create: bool) -> std::io::Result<(File, std::ffi::CString)> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::path::Component;

    #[cfg(target_os = "macos")]
    let path = [("/var", "/private/var"), ("/tmp", "/private/tmp")]
        .into_iter()
        .find_map(|(alias, real)| {
            path.strip_prefix(alias)
                .ok()
                .map(|rest| Path::new(real).join(rest))
        })
        .unwrap_or_else(|| path.to_path_buf());
    #[cfg(not(target_os = "macos"))]
    let path = path.to_path_buf();

    let leaf = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut directory = if path.is_absolute() {
        File::open("/")?
    } else {
        File::open(".")?
    };
    for component in parent.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Windows path prefix is unsupported",
                ))
            }
        };
        let name = c_name(name)?;
        if create {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) };
            if made != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error);
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok((directory, c_name(leaf)?))
}

#[cfg(unix)]
fn open_parent_beneath(
    root: &File,
    relative: &Path,
    create: bool,
) -> std::io::Result<(File, std::ffi::CString)> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "store capability requires a contained relative path",
        ));
    }
    let leaf = relative.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    if !parent.as_os_str().is_empty() && leaf == std::ffi::OsStr::new("DB.md") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to create a nested store marker",
        ));
    }

    let mut directory = root.try_clone()?;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = c_name(name)?;
        if create {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) };
            if made != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error);
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if directory_contains_exact_regular(&directory, "DB.md".as_ref())? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path crosses a nested db.md store boundary",
            ));
        }
    }
    Ok((directory, c_name(leaf)?))
}

#[cfg(unix)]
pub(crate) fn write_atomic_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
    create_new: bool,
    durable: bool,
) -> std::io::Result<()> {
    let (directory, leaf) = open_parent_beneath(root, relative, true)?;
    write_atomic_at(directory, leaf, bytes, create_new, durable)
}

/// Atomically replace a rebuildable file beneath a held root without forcing
/// the bytes or directory entry to stable storage.
#[cfg(unix)]
pub(crate) fn write_atomic_nondurable_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let (directory, leaf) = open_parent_beneath(root, relative, true)?;
    write_atomic_at(directory, leaf, bytes, false, false)
}

#[cfg(not(unix))]
pub(crate) fn write_atomic_nondurable_beneath(
    _root: &File,
    _relative: &Path,
    _bytes: &[u8],
) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

/// Open or create a regular advisory-lock file beneath a held root, then take
/// an exclusive `flock` on the exact inode. The parent traversal and leaf open
/// are no-follow, so replacing the store's original pathname cannot redirect
/// the lock into another tree.
#[cfg(unix)]
pub(crate) fn lock_exclusive_beneath(root: &File, relative: &Path) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let (directory, leaf) = open_parent_beneath(root, relative, true)?;
    // Darwin can transiently report ENOENT when two creators race on the same
    // O_CREAT+O_NOFOLLOW leaf. Retry that lookup race (and EINTR) against the
    // same held parent; every successful contender still opens the one shared
    // inode and serializes on `flock`.
    let mut retries = 0_u8;
    let fd = loop {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd >= 0 {
            break fd;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted
            || (error.kind() == std::io::ErrorKind::NotFound && retries < 8)
        {
            retries = retries.saturating_add(1);
            std::thread::yield_now();
            continue;
        }
        return Err(error);
    };
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to lock a non-regular file",
        ));
    }
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(file)
}

#[cfg(not(unix))]
pub(crate) fn lock_exclusive_beneath(_root: &File, _relative: &Path) -> std::io::Result<File> {
    Err(secure_filesystem_unsupported())
}

/// Open a directory beneath a held root without following any component.
#[cfg(unix)]
pub(crate) fn open_directory_beneath(
    root: &File,
    relative: &Path,
    create: bool,
) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "store capability requires a contained relative directory",
        ));
    }
    let mut directory = root.try_clone()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = c_name(name)?;
        if create {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) };
            if made != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error);
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if directory_contains_exact_regular(&directory, "DB.md".as_ref())? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path crosses a nested db.md store boundary",
            ));
        }
    }
    Ok(directory)
}

#[cfg(not(unix))]
pub(crate) fn open_directory_beneath(
    _root: &File,
    _relative: &Path,
    _create: bool,
) -> std::io::Result<File> {
    Err(secure_filesystem_unsupported())
}

pub(crate) fn directory_exists_beneath(root: &File, relative: &Path) -> std::io::Result<bool> {
    match open_directory_beneath(root, relative, false) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Confirm that every component of a relative regular-file path has the exact
/// byte spelling present on disk. This keeps validation platform-independent
/// on case-insensitive filesystems without canonicalizing the mutable root
/// pathname.
#[cfg(unix)]
pub(crate) fn path_case_matches_beneath(root: &File, relative: &Path) -> std::io::Result<bool> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;

    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Ok(false);
    }
    let components: Vec<_> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let mut directory = root.try_clone()?;
    for (index, name) in components.iter().enumerate() {
        let scan_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name(std::ffi::OsStr::new("."))?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if scan_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(scan_fd) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(scan_fd);
            }
            return Err(error);
        }
        let mut exact = false;
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == name.as_bytes() {
                exact = true;
                break;
            }
        }
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if !exact {
            return Ok(false);
        }
        if index + 1 == components.len() {
            return directory_contains_exact_regular(&directory, name);
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c_name(name)?.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Ok(false);
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if directory_contains_exact_regular(&directory, "DB.md".as_ref())? {
            return Ok(false);
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
pub(crate) fn path_case_matches_beneath(_root: &File, _relative: &Path) -> std::io::Result<bool> {
    Err(secure_filesystem_unsupported())
}

/// Immediate no-follow child directories beneath a held root.
#[cfg(unix)]
pub(crate) fn directory_names_beneath(
    root: &File,
    relative: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let directory = open_directory_beneath(root, relative, false)?;
    let scan_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c_name(std::ffi::OsStr::new("."))?.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if scan_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(scan_fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(scan_fd);
        }
        return Err(error);
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if matches!(bytes, b"." | b"..") || bytes.starts_with(b".") {
            continue;
        }
        let name = std::ffi::OsString::from_vec(bytes.to_vec());
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                c_name(&name)?.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
        {
            let child = open_directory_beneath(root, &relative.join(&name), false)?;
            if !directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                names.push(name);
            }
        }
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

#[cfg(not(unix))]
pub(crate) fn directory_names_beneath(
    _root: &File,
    _relative: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(unix))]
pub(crate) fn write_atomic_beneath(
    _root: &File,
    _relative: &Path,
    _bytes: &[u8],
    _create_new: bool,
    _durable: bool,
) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(unix)]
pub(crate) fn rename_beneath(root: &File, old: &Path, new: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let (old_parent, old_leaf) = open_parent_beneath(root, old, false)?;
    let (new_parent, new_leaf) = open_parent_beneath(root, new, true)?;
    let mut source_stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            old_parent.as_raw_fd(),
            old_leaf.as_ptr(),
            &mut source_stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if (source_stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to rename a non-regular file",
        ));
    }
    renameat_noreplace(
        old_parent.as_raw_fd(),
        &old_leaf,
        new_parent.as_raw_fd(),
        &new_leaf,
    )?;
    old_parent.sync_all()?;
    new_parent.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn rename_beneath(_root: &File, _old: &Path, _new: &Path) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(unix)]
pub(crate) fn remove_file_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let (parent, leaf) = open_parent_beneath(root, relative, false)?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a non-regular file",
        ));
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    parent.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn remove_file_beneath(_root: &File, _relative: &Path) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(unix)]
fn write_atomic_unix(
    path: &Path,
    bytes: &[u8],
    create_new: bool,
    durable: bool,
) -> std::io::Result<()> {
    let (directory, leaf) = open_parent_unix(path, true)?;
    write_atomic_at(directory, leaf, bytes, create_new, durable)
}

#[cfg(unix)]
fn write_atomic_at(
    directory: File,
    leaf: std::ffi::CString,
    bytes: &[u8],
    create_new: bool,
    durable: bool,
) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut allocated = None;
    for _ in 0..128 {
        let name = std::ffi::OsString::from(format!(
            ".dbmd.tmp.{pid}.{nanos}.{}",
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let name = c_name(&name)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o666,
            )
        };
        if fd >= 0 {
            allocated = Some((name, unsafe { File::from_raw_fd(fd) }));
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    let (temp, mut file) = allocated.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate secure temporary file",
        )
    })?;
    let cleanup =
        |name: &std::ffi::CStr| unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    let write_result = file.write_all(bytes).and_then(|_| {
        if durable {
            file.sync_all()
        } else {
            file.flush()
        }
    });
    if let Err(error) = write_result {
        let _ = cleanup(&temp);
        return Err(error);
    }

    // Preserve an existing regular destination's exact Unix mode. A symlink is
    // never dereferenced; chmod failure aborts rather than silently widening.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let destination_stat = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if destination_stat == 0 {
        if (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            let _ = cleanup(&temp);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing a symlink destination",
            ));
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), stat.st_mode & 0o7777) } != 0 {
            let error = std::io::Error::last_os_error();
            let _ = cleanup(&temp);
            return Err(error);
        }
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            let _ = cleanup(&temp);
            return Err(error);
        }
    }
    drop(file);

    let installed = if create_new {
        unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                temp.as_ptr(),
                directory.as_raw_fd(),
                leaf.as_ptr(),
                0,
            )
        }
    } else {
        unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp.as_ptr(),
                directory.as_raw_fd(),
                leaf.as_ptr(),
            )
        }
    };
    if installed != 0 {
        let error = std::io::Error::last_os_error();
        let _ = cleanup(&temp);
        return Err(error);
    }
    if create_new {
        let _ = cleanup(&temp);
    }
    if durable {
        directory.sync_all()?;
    }
    Ok(())
}

/// Drop-based cleanup for the hidden temp file `write_atomic` creates. While
/// armed, dropping the guard removes `path`. [`TempGuard::disarm`] is called
/// only after a successful rename, or after a successful temp-link cleanup in
/// [`write_atomic_new`], so the final destination is never touched.
#[cfg(test)]
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

#[cfg(test)]
impl TempGuard {
    /// Stop cleaning up `path` on drop — used once the temp has been renamed
    /// into place and is no longer a stray temp file.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(test)]
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
#[cfg(test)]
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

    /// Exploit regression for the containment/write TOCTOU: a caller may have
    /// validated `store/records/safe.md`, then an attacker replaces `records`
    /// with a symlink to an external directory before the atomic writer opens
    /// it. Every ancestor is opened with `openat(O_DIRECTORY|O_NOFOLLOW)`, so
    /// the write is refused and the outside victim is byte-identical.
    #[cfg(unix)]
    #[test]
    fn write_atomic_refuses_symlinked_ancestor_without_touching_external_file() {
        use std::os::unix::fs::symlink;

        let sandbox = TempDir::new().unwrap();
        let store = sandbox.path().join("store");
        let external = sandbox.path().join("external");
        std::fs::create_dir_all(store.join("records")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let victim = external.join("safe.md");
        std::fs::write(&victim, b"external secret").unwrap();

        std::fs::remove_dir(store.join("records")).unwrap();
        symlink(&external, store.join("records")).unwrap();

        let error = write_atomic(&store.join("records/safe.md"), b"attacker output")
            .expect_err("a symlinked ancestor must fail closed");
        assert!(
            matches!(
                error.raw_os_error(),
                Some(code) if code == libc::ELOOP || code == libc::ENOTDIR
            ),
            "expected no-follow refusal, got {error:?}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"external secret");
    }

    /// A leaf swap is equally unsafe for reads: after containment validation an
    /// attacker can replace the selected record with a symlink to a secret.
    /// `read_bounded_nofollow` opens the leaf once with `O_NOFOLLOW`, then sizes
    /// and reads that same descriptor, so no external bytes are returned.
    #[cfg(unix)]
    #[test]
    fn bounded_read_refuses_symlink_leaf() {
        use std::os::unix::fs::symlink;

        let sandbox = TempDir::new().unwrap();
        let external = sandbox.path().join("secret");
        std::fs::write(&external, b"do not exfiltrate").unwrap();
        let selected = sandbox.path().join("selected.md");
        symlink(&external, &selected).unwrap();

        let error = read_bounded_nofollow(&selected, 1024)
            .expect_err("the no-follow reader must reject a symlink leaf");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }

    /// If the file grows after its descriptor metadata was read, the bounded
    /// descriptor read still enforces the actual byte ceiling (`take(max+1)`),
    /// rather than trusting the stale size.
    #[test]
    fn bounded_read_rejects_content_over_limit() {
        let sandbox = TempDir::new().unwrap();
        let selected = sandbox.path().join("selected.md");
        std::fs::write(&selected, b"12345").unwrap();
        let error = read_bounded_nofollow(&selected, 4)
            .expect_err("actual content above the cap must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn held_directory_reader_survives_ancestor_swap_without_disclosure() {
        use std::os::unix::fs::symlink;

        let sandbox = TempDir::new().unwrap();
        let store = sandbox.path().join("store");
        let contacts = store.join("records/contacts");
        std::fs::create_dir_all(&contacts).unwrap();
        std::fs::write(contacts.join("selected.md"), b"owned").unwrap();
        let outside = sandbox.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("selected.md"), b"secret").unwrap();

        let mut reader = BoundedDirReader::new(&store).unwrap();
        let relative = Path::new("records/contacts/selected.md");
        assert_eq!(reader.read(relative, 1024).unwrap(), b"owned");

        let detached = store.join("records/contacts-detached");
        std::fs::rename(&contacts, &detached).unwrap();
        symlink(&outside, &contacts).unwrap();

        assert_eq!(
            reader.read(relative, 1024).unwrap(),
            b"owned",
            "the cached directory capability must not reopen the swapped pathname"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn rename_nofollow_is_atomic_no_replace() {
        let sandbox = TempDir::new().unwrap();
        let source = sandbox.path().join("source.md");
        let destination = sandbox.path().join("destination.md");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&destination, b"existing").unwrap();

        let error = rename_nofollow(&source, &destination)
            .expect_err("a destination created after preflight must not be clobbered");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
    }
}
