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
#[cfg(any(unix, windows))]
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
    #[cfg(windows)]
    {
        windows_fs::atomic_write_absolute(path, bytes, false, true)
    }
    #[cfg(not(any(unix, windows)))]
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
    #[cfg(windows)]
    {
        windows_fs::atomic_write_absolute(path, bytes, true, true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, bytes);
        Err(secure_filesystem_unsupported())
    }
}

#[cfg(not(any(unix, windows)))]
fn secure_filesystem_unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure filesystem mutation requires handle-relative no-follow primitives on this platform",
    )
}

#[cfg(windows)]
mod windows_fs {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::Component;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        LockFileEx, MoveFileExW, RemoveDirectoryW, SetFileAttributesW, BY_HANDLE_FILE_INFORMATION,
        CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, LOCKFILE_EXCLUSIVE_LOCK, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, OPEN_ALWAYS, OPEN_EXISTING, VOLUME_NAME_DOS,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn open_raw(
        path: &Path,
        access: u32,
        directory: bool,
        creation: u32,
    ) -> std::io::Result<HANDLE> {
        let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
        if directory {
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }
        let path = wide(path);
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                // Omitting FILE_SHARE_DELETE is the capability boundary: a
                // checked component cannot be renamed away while held.
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                creation,
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    fn attributes(handle: HANDLE) -> std::io::Result<u32> {
        let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(info.dwFileAttributes)
        }
    }

    fn file_from_handle(handle: HANDLE) -> File {
        unsafe { File::from_raw_handle(handle as _) }
    }

    fn checked_file(handle: HANDLE, directory: bool) -> std::io::Result<File> {
        let attrs = match attributes(handle) {
            Ok(attrs) => attrs,
            Err(error) => {
                unsafe { CloseHandle(handle) };
                return Err(error);
            }
        };
        let wrong_kind = if directory {
            attrs & FILE_ATTRIBUTE_DIRECTORY == 0
        } else {
            attrs & FILE_ATTRIBUTE_DIRECTORY != 0
        };
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || wrong_kind {
            unsafe { CloseHandle(handle) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path component is a reparse point or has the wrong type",
            ));
        }
        Ok(file_from_handle(handle))
    }

    pub(super) fn open_directory(path: &Path) -> std::io::Result<File> {
        let resolved = std::path::absolute(path)?;
        let guards = hold_directory_chain(&resolved, false)?;
        guards.into_iter().last().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory path has no root",
            )
        })
    }

    pub(super) fn open_or_create_directory(path: &Path) -> std::io::Result<File> {
        let resolved = std::path::absolute(path)?;
        let guards = hold_directory_chain(&resolved, true)?;
        guards.into_iter().last().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory path has no root",
            )
        })
    }

    pub(super) fn directory_path(directory: &File) -> std::io::Result<PathBuf> {
        let handle = directory.as_raw_handle() as HANDLE;
        let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
        let needed = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, flags) };
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0_u16; needed as usize + 1];
        let written = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
        };
        if written == 0 || written as usize >= buffer.len() {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(written as usize);
        Ok(PathBuf::from(OsString::from_wide(&buffer)))
    }

    fn hold_directory_chain(path: &Path, create: bool) -> std::io::Result<Vec<File>> {
        let resolved = std::path::absolute(path)?;
        let mut current = PathBuf::new();
        let mut held = Vec::new();
        for component in resolved.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::RootDir | Component::Normal(_)) {
                continue;
            }
            let opened = open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)
                .and_then(|handle| checked_file(handle, true));
            match opened {
                Ok(directory) => held.push(directory),
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let path = wide(&current);
                    if unsafe { CreateDirectoryW(path.as_ptr(), std::ptr::null()) } == 0 {
                        let create_error = std::io::Error::last_os_error();
                        if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(create_error);
                        }
                    }
                    held.push(checked_file(
                        open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?,
                        true,
                    )?);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(held)
    }

    fn relative_parts(relative: &Path) -> std::io::Result<Vec<OsString>> {
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
        let parts = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no component",
            ));
        }
        Ok(parts)
    }

    fn held_parent(
        root: &File,
        relative: &Path,
        create: bool,
    ) -> std::io::Result<(Vec<File>, PathBuf, OsString)> {
        let parts = relative_parts(relative)?;
        let mut current = directory_path(root)?;
        let mut held = vec![root.try_clone()?];
        for component in &parts[..parts.len() - 1] {
            current.push(component);
            let opened = open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)
                .and_then(|handle| checked_file(handle, true));
            match opened {
                Ok(directory) => held.push(directory),
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let path = wide(&current);
                    if unsafe { CreateDirectoryW(path.as_ptr(), std::ptr::null()) } == 0 {
                        let create_error = std::io::Error::last_os_error();
                        if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(create_error);
                        }
                    }
                    held.push(checked_file(
                        open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?,
                        true,
                    )?);
                }
                Err(error) => return Err(error),
            }
            if contains_exact_regular_path(&current, OsStr::new("DB.md"))? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path crosses a nested db.md store boundary",
                ));
            }
        }
        Ok((held, current, parts.last().unwrap().clone()))
    }

    fn contains_exact_regular_path(path: &Path, wanted: &OsStr) -> std::io::Result<bool> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_name() != wanted {
                continue;
            }
            return match open_raw(&entry.path(), FILE_READ_ATTRIBUTES, false, OPEN_EXISTING) {
                Ok(handle) => checked_file(handle, false).map(|_| true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            };
        }
        Ok(false)
    }

    pub(super) fn contains_exact_regular(
        directory: &File,
        wanted: &OsStr,
    ) -> std::io::Result<bool> {
        contains_exact_regular_path(&directory_path(directory)?, wanted)
    }

    pub(super) fn open_regular(root: &File, relative: &Path) -> std::io::Result<File> {
        let (_held, parent, leaf) = held_parent(root, relative, false)?;
        checked_file(
            open_raw(&parent.join(leaf), GENERIC_READ, false, OPEN_EXISTING)?,
            false,
        )
    }

    pub(super) fn open_regular_absolute(path: &Path) -> std::io::Result<File> {
        let resolved = std::path::absolute(path)?;
        let parent = resolved.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
        })?;
        let _held = hold_directory_chain(parent, false)?;
        checked_file(
            open_raw(&resolved, GENERIC_READ, false, OPEN_EXISTING)?,
            false,
        )
    }

    fn atomic_write_target(
        parent: &Path,
        leaf: &OsStr,
        bytes: &[u8],
        create_new: bool,
        durable: bool,
    ) -> std::io::Result<()> {
        let target = parent.join(leaf);
        let prior_attrs = match open_raw(&target, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING) {
            Ok(handle) => {
                let attrs = attributes(handle)?;
                unsafe { CloseHandle(handle) };
                if attrs & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "destination is a reparse point or directory",
                    ));
                }
                if create_new {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "destination already exists",
                    ));
                }
                Some(attrs)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let nonce = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".dbmd-tmp-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            nonce
        ));
        let handle = open_raw(&temp, GENERIC_WRITE, false, CREATE_NEW)?;
        let mut file = file_from_handle(handle);
        if let Err(error) =
            file.write_all(bytes)
                .and_then(|_| if durable { file.sync_all() } else { Ok(()) })
        {
            drop(file);
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        drop(file);
        let temp_wide = wide(&temp);
        let target_wide = wide(&target);
        let was_readonly = prior_attrs.is_some_and(|attrs| attrs & FILE_ATTRIBUTE_READONLY != 0);
        if was_readonly {
            let attrs = prior_attrs.expect("readonly destination has attributes")
                & !FILE_ATTRIBUTE_READONLY;
            let writable_attrs = if attrs == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attrs
            };
            if unsafe { SetFileAttributesW(target_wide.as_ptr(), writable_attrs) } == 0 {
                let error = std::io::Error::last_os_error();
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
        }
        let flags = if create_new {
            MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        };
        if unsafe { MoveFileExW(temp_wide.as_ptr(), target_wide.as_ptr(), flags) } == 0 {
            let error = std::io::Error::last_os_error();
            if let Some(attrs) = prior_attrs {
                let _ = unsafe { SetFileAttributesW(target_wide.as_ptr(), attrs) };
            }
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        if was_readonly
            && unsafe { SetFileAttributesW(target_wide.as_ptr(), FILE_ATTRIBUTE_READONLY) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn atomic_write_absolute(
        path: &Path,
        bytes: &[u8],
        create_new: bool,
        durable: bool,
    ) -> std::io::Result<()> {
        let resolved = std::path::absolute(path)?;
        let parent = resolved.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
        })?;
        let _held = hold_directory_chain(parent, true)?;
        atomic_write_target(
            parent,
            resolved.file_name().unwrap(),
            bytes,
            create_new,
            durable,
        )
    }

    pub(super) fn atomic_write_beneath(
        root: &File,
        relative: &Path,
        bytes: &[u8],
        create_new: bool,
        durable: bool,
    ) -> std::io::Result<()> {
        let (_held, parent, leaf) = held_parent(root, relative, true)?;
        atomic_write_target(&parent, &leaf, bytes, create_new, durable)
    }

    pub(super) fn open_directory_beneath(
        root: &File,
        relative: &Path,
        create: bool,
    ) -> std::io::Result<File> {
        if relative.as_os_str().is_empty() {
            return root.try_clone();
        }
        let parts = relative_parts(relative)?;
        let mut current = directory_path(root)?;
        let mut held = vec![root.try_clone()?];
        for component in parts {
            current.push(component);
            let opened = open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)
                .and_then(|handle| checked_file(handle, true));
            match opened {
                Ok(directory) => held.push(directory),
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let path = wide(&current);
                    if unsafe { CreateDirectoryW(path.as_ptr(), std::ptr::null()) } == 0 {
                        let create_error = std::io::Error::last_os_error();
                        if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(create_error);
                        }
                    }
                    held.push(checked_file(
                        open_raw(&current, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?,
                        true,
                    )?);
                }
                Err(error) => return Err(error),
            }
            if contains_exact_regular_path(&current, OsStr::new("DB.md"))? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path crosses a nested db.md store boundary",
                ));
            }
        }
        held.into_iter().last().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "directory path is empty")
        })
    }

    pub(super) fn lock_beneath(root: &File, relative: &Path) -> std::io::Result<File> {
        let (_held, parent, leaf) = held_parent(root, relative, true)?;
        let handle = open_raw(
            &parent.join(leaf),
            GENERIC_READ | GENERIC_WRITE,
            false,
            OPEN_ALWAYS,
        )?;
        let file = checked_file(handle, false)?;
        let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
        if unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file)
    }
    pub(super) fn rename_beneath(root: &File, old: &Path, new: &Path) -> std::io::Result<()> {
        let (_old_held, old_parent, old_leaf) = held_parent(root, old, false)?;
        let (_new_held, new_parent, new_leaf) = held_parent(root, new, true)?;
        let old_path = old_parent.join(old_leaf);
        let new_path = new_parent.join(new_leaf);
        let source = checked_file(
            open_raw(&old_path, FILE_READ_ATTRIBUTES, false, OPEN_EXISTING)?,
            false,
        )?;
        drop(source);
        let old_wide = wide(&old_path);
        let new_wide = wide(&new_path);
        if unsafe { MoveFileExW(old_wide.as_ptr(), new_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn rename_directory_beneath(
        root: &File,
        old: &Path,
        new: &Path,
    ) -> std::io::Result<()> {
        let (_old_held, old_parent, old_leaf) = held_parent(root, old, false)?;
        let (_new_held, new_parent, new_leaf) = held_parent(root, new, true)?;
        let old_path = old_parent.join(old_leaf);
        let new_path = new_parent.join(new_leaf);
        let source = checked_file(
            open_raw(&old_path, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?,
            true,
        )?;
        drop(source);
        let old_wide = wide(&old_path);
        let new_wide = wide(&new_path);
        if unsafe { MoveFileExW(old_wide.as_ptr(), new_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn remove_file_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
        let (_held, parent, leaf) = held_parent(root, relative, false)?;
        let path = parent.join(leaf);
        let source = checked_file(
            open_raw(
                &path,
                FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
                false,
                OPEN_EXISTING,
            )?,
            false,
        )?;
        drop(source);
        let tombstone = parent.join(format!(
            ".dbmd-remove-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path_wide = wide(&path);
        let tombstone_wide = wide(&tombstone);
        if unsafe {
            MoveFileExW(
                path_wide.as_ptr(),
                tombstone_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        std::fs::remove_file(tombstone)
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(super) fn remove_tree_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
        let (_held, parent, leaf) = held_parent(root, relative, false)?;
        fn remove(path: &Path) -> std::io::Result<()> {
            let guard = checked_file(
                open_raw(path, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?,
                true,
            )?;
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let child = entry.path();
                let handle = open_raw(&child, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?;
                let attrs = attributes(handle)?;
                unsafe { CloseHandle(handle) };
                if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        let child_wide = wide(&child);
                        if unsafe { RemoveDirectoryW(child_wide.as_ptr()) } == 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else {
                        std::fs::remove_file(child)?;
                    }
                } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    remove(&child)?;
                } else {
                    std::fs::remove_file(child)?;
                }
            }
            drop(guard);
            let wide = wide(path);
            if unsafe { RemoveDirectoryW(wide.as_ptr()) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        remove(&parent.join(leaf))
    }

    pub(super) fn entry_attributes(path: &Path) -> std::io::Result<u32> {
        let handle = open_raw(path, FILE_READ_ATTRIBUTES, true, OPEN_EXISTING)?;
        let result = attributes(handle);
        unsafe { CloseHandle(handle) };
        result
    }
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

#[cfg(windows)]
pub(crate) fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    windows_fs::open_directory(path)
}

/// Open a Windows directory path through no-follow capabilities, creating
/// missing components without ever accepting a reparse-point component.
#[cfg(windows)]
pub(crate) fn open_or_create_directory_nofollow(path: &Path) -> std::io::Result<File> {
    windows_fs::open_or_create_directory(path)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn directory_contains_exact_regular(
    directory: &File,
    wanted: &std::ffi::OsStr,
) -> std::io::Result<bool> {
    windows_fs::contains_exact_regular(directory, wanted)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn walk_regular_files_beneath(
    root: &File,
    start: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    const MAX_WALK_ENTRIES: usize = 1_000_000;
    let start_dir = windows_fs::open_directory_beneath(root, start, false)?;
    let mut pending = vec![(start_dir, start.to_path_buf())];
    let mut files = Vec::new();
    let mut seen = 0usize;
    while let Some((directory, relative)) = pending.pop() {
        let path = windows_fs::directory_path(&directory)?;
        let mut entries = std::fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "store contains more than 1000000 visible entries",
                ));
            }
            let attrs = windows_fs::entry_attributes(&entry.path())?;
            let child_relative = relative.join(&name);
            if attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0 {
                let child = windows_fs::open_directory(&entry.path())?;
                if !directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                    pending.push((child, child_relative));
                }
            } else {
                let _ = windows_fs::open_regular(root, &child_relative)?;
                files.push(child_relative);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(windows)]
pub(crate) fn ownership_boundaries_beneath(
    root: &File,
) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut pending = vec![(root.try_clone()?, PathBuf::new())];
    let mut reparse = Vec::new();
    let mut nested = Vec::new();
    while let Some((directory, relative)) = pending.pop() {
        let path = windows_fs::directory_path(&directory)?;
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let child_relative = relative.join(&name);
            let attrs = windows_fs::entry_attributes(&entry.path())?;
            if attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                reparse.push(child_relative);
            } else if attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
            {
                let child = windows_fs::open_directory(&entry.path())?;
                if directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                    nested.push(child_relative);
                } else {
                    pending.push((child, child_relative));
                }
            }
        }
    }
    reparse.sort();
    nested.sort();
    Ok((reparse, nested))
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(not(any(unix, windows)))]
pub(crate) fn regular_file_names_beneath(
    _root: &File,
    _directory: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(windows)]
pub(crate) fn regular_file_names_beneath(
    root: &File,
    directory: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    let held = windows_fs::open_directory_beneath(root, directory, false)?;
    let path = windows_fs::directory_path(&held)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let attrs = windows_fs::entry_attributes(&entry.path())?;
        if attrs
            & (windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY)
            == 0
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn walk_regular_files_beneath(
    _root: &File,
    _start: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct BoundedDirReader {
    root: File,
}

#[cfg(windows)]
impl BoundedDirReader {
    #[allow(dead_code)]
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: open_directory_nofollow(root)?,
        })
    }

    pub(crate) fn from_root(root: &File) -> std::io::Result<Self> {
        Ok(Self {
            root: root.try_clone()?,
        })
    }

    pub(crate) fn read(&mut self, relative: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        read_bounded_file(self.open(relative)?, max_bytes)
    }

    pub(crate) fn open(&mut self, relative: &Path) -> std::io::Result<File> {
        windows_fs::open_regular(&self.root, relative)
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct BoundedDirReader;

#[cfg(not(any(unix, windows)))]
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
    #[cfg(windows)]
    {
        windows_fs::open_regular_absolute(path)
    }
    #[cfg(not(any(unix, windows)))]
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

#[cfg(not(any(unix, windows)))]
pub fn rename_nofollow(_old: &Path, _new: &Path) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(windows)]
pub fn rename_nofollow(old: &Path, new: &Path) -> std::io::Result<()> {
    let parent = std::path::absolute(old)?
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"))?
        .to_path_buf();
    let root = windows_fs::open_directory(&parent)?;
    let old_name = old.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let new_absolute = std::path::absolute(new)?;
    let new_parent = new_absolute.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    if std::path::absolute(&parent)? != std::path::absolute(new_parent)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "secure Windows no-replace rename currently requires one parent directory",
        ));
    }
    windows_fs::rename_beneath(
        &root,
        Path::new(old_name),
        Path::new(new.file_name().unwrap()),
    )
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
    write_atomic_at(directory, leaf, bytes, create_new, durable, None)
}

/// Atomically install private control/recovery material beneath a held root.
/// Unix creates the temporary inode at 0600 and forces the installed file back
/// to 0600 even if an interrupted/manual operation widened the destination.
#[cfg(unix)]
pub(crate) fn write_private_atomic_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
    create_new: bool,
) -> std::io::Result<()> {
    let (directory, leaf) = open_parent_beneath(root, relative, true)?;
    write_atomic_at(directory, leaf, bytes, create_new, true, Some(0o600))
}

#[cfg(windows)]
pub(crate) fn write_atomic_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
    create_new: bool,
    durable: bool,
) -> std::io::Result<()> {
    windows_fs::atomic_write_beneath(root, relative, bytes, create_new, durable)
}

#[cfg(windows)]
pub(crate) fn write_private_atomic_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
    create_new: bool,
) -> std::io::Result<()> {
    // Windows has no POSIX mode; the secure no-follow writer inherits the
    // current user's ACL from the private checkout directory.
    windows_fs::atomic_write_beneath(root, relative, bytes, create_new, true)
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
    write_atomic_at(directory, leaf, bytes, false, false, None)
}

#[cfg(windows)]
pub(crate) fn write_atomic_nondurable_beneath(
    root: &File,
    relative: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    windows_fs::atomic_write_beneath(root, relative, bytes, false, false)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn lock_exclusive_beneath(root: &File, relative: &Path) -> std::io::Result<File> {
    windows_fs::lock_beneath(root, relative)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn open_directory_beneath(
    root: &File,
    relative: &Path,
    create: bool,
) -> std::io::Result<File> {
    windows_fs::open_directory_beneath(root, relative, create)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn path_case_matches_beneath(root: &File, relative: &Path) -> std::io::Result<bool> {
    if relative.is_absolute() {
        return Ok(false);
    }
    let components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut directory = root.try_clone()?;
    for (index, name) in components.iter().enumerate() {
        let path = windows_fs::directory_path(&directory)?;
        let exact = std::fs::read_dir(&path)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|entry| entry.file_name() == *name);
        let Some(entry) = exact else { return Ok(false) };
        if index + 1 == components.len() {
            let attrs = windows_fs::entry_attributes(&entry.path())?;
            return Ok(attrs
                & (windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                    | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY)
                == 0);
        }
        directory = windows_fs::open_directory(&entry.path())?;
        if directory_contains_exact_regular(&directory, "DB.md".as_ref())? {
            return Ok(false);
        }
    }
    Ok(false)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn directory_names_beneath(
    root: &File,
    relative: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    let directory = windows_fs::open_directory_beneath(root, relative, false)?;
    let path = windows_fs::directory_path(&directory)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let attrs = windows_fs::entry_attributes(&entry.path())?;
        if attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT == 0
            && attrs & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
        {
            let child = windows_fs::open_directory(&entry.path())?;
            if !directory_contains_exact_regular(&child, "DB.md".as_ref())? {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn directory_names_beneath(
    _root: &File,
    _relative: &Path,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn write_atomic_beneath(
    _root: &File,
    _relative: &Path,
    _bytes: &[u8],
    _create_new: bool,
    _durable: bool,
) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn write_private_atomic_beneath(
    _root: &File,
    _relative: &Path,
    _bytes: &[u8],
    _create_new: bool,
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

#[cfg(windows)]
pub(crate) fn rename_beneath(root: &File, old: &Path, new: &Path) -> std::io::Result<()> {
    windows_fs::rename_beneath(root, old, new)
}

#[cfg(windows)]
pub(crate) fn rename_directory_beneath(root: &File, old: &Path, new: &Path) -> std::io::Result<()> {
    windows_fs::rename_directory_beneath(root, old, new)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
pub(crate) fn remove_file_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
    windows_fs::remove_file_beneath(root, relative)
}

/// Remove one private subtree beneath a held store root without following any
/// symlink. This is reserved for disposable control state such as completed
/// conflict bundles; riding store data uses explicit file operations instead.
#[cfg(unix)]
pub(crate) fn remove_tree_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
    fn remove_at(parent: &File, name: &std::ffi::CStr) -> std::io::Result<()> {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private cleanup target is not a directory",
            ));
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let directory = unsafe { File::from_raw_fd(fd) };
        let scan_fd = unsafe { libc::dup(directory.as_raw_fd()) };
        if scan_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(scan_fd) };
        if stream.is_null() {
            unsafe { libc::close(scan_fd) };
            return Err(std::io::Error::last_os_error());
        }
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let raw = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if !matches!(raw.to_bytes(), b"." | b"..") {
                names.push(raw.to_owned());
            }
        }
        if unsafe { libc::closedir(stream) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for child in names {
            let mut child_stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    child.as_ptr(),
                    &mut child_stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if (child_stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
                remove_at(&directory, &child)?;
            } else if unsafe { libc::unlinkat(directory.as_raw_fd(), child.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        drop(directory);
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        parent.sync_all()
    }

    let (parent, leaf) = open_parent_beneath(root, relative, false)?;
    remove_at(&parent, &leaf)
}

#[cfg(windows)]
pub(crate) fn remove_tree_beneath(root: &File, relative: &Path) -> std::io::Result<()> {
    windows_fs::remove_tree_beneath(root, relative)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn remove_file_beneath(_root: &File, _relative: &Path) -> std::io::Result<()> {
    Err(secure_filesystem_unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn remove_tree_beneath(_root: &File, _relative: &Path) -> std::io::Result<()> {
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
    write_atomic_at(directory, leaf, bytes, create_new, durable, None)
}

#[cfg(unix)]
fn write_atomic_at(
    directory: File,
    leaf: std::ffi::CString,
    bytes: &[u8],
    create_new: bool,
    durable: bool,
    exact_mode: Option<libc::mode_t>,
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
                exact_mode.unwrap_or(0o666) as libc::c_uint,
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
        let mode = exact_mode.unwrap_or(stat.st_mode & 0o7777);
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
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
        if let Some(mode) = exact_mode {
            if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
                let error = std::io::Error::last_os_error();
                let _ = cleanup(&temp);
                return Err(error);
            }
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

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_preserves_readonly_and_exact_bytes() {
        let sandbox = TempDir::new().unwrap();
        let target = sandbox.path().join("readonly.md");
        std::fs::write(&target, b"old").unwrap();
        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&target, permissions).unwrap();

        write_atomic(&target, b"replacement").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
        assert!(std::fs::metadata(&target).unwrap().permissions().readonly());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reader_refuses_a_reparse_leaf() {
        use std::os::windows::fs::symlink_file;

        let sandbox = TempDir::new().unwrap();
        let external = sandbox.path().join("external.md");
        let selected = sandbox.path().join("selected.md");
        std::fs::write(&external, b"secret").unwrap();
        symlink_file(&external, &selected).unwrap();
        assert!(read_bounded_nofollow(&selected, 1024).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_serializes_competing_handles() {
        let sandbox = TempDir::new().unwrap();
        let root = open_directory_nofollow(sandbox.path()).unwrap();
        let first = lock_exclusive_beneath(&root, Path::new("lock")).unwrap();
        let root_for_thread = root.try_clone().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(lock_exclusive_beneath(&root_for_thread, Path::new("lock")))
                .unwrap();
        });
        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first);
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .is_ok());
    }
}
