//! Capability-safe final install primitive used only by `scripts/install.sh`.

#[cfg(unix)]
use std::ffi::{CString, OsStr};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::path::{Component, Path};

use crate::cli::InstallVerifiedArgs;
use crate::error::{CliError, CliResult};

pub fn run(args: &InstallVerifiedArgs) -> CliResult {
    #[cfg(unix)]
    {
        install_verified(Path::new(&args.source), Path::new(&args.install_dir))
            .map_err(|error| CliError::runtime(format!("secure install failed: {error}")))
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err(CliError::runtime(
            "secure install is supported only on Unix targets",
        ))
    }
}

#[cfg(unix)]
fn c_name(name: &OsStr) -> std::io::Result<CString> {
    use std::os::unix::ffi::OsStrExt as _;
    CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "install path contains a NUL byte",
        )
    })
}

#[cfg(unix)]
fn open_directory_path(path: &Path, create: bool) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let mut directory = if path.is_absolute() {
        File::open("/")?
    } else {
        File::open(".")?
    };
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::ParentDir | Component::Prefix(_)) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "install path may not contain `..`",
                ));
            }
            continue;
        };
        let name = c_name(name)?;
        if create {
            let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
            if result != 0 {
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
    Ok(directory)
}

#[cfg(unix)]
fn open_regular_path(path: &Path) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let parent = open_directory_path(path.parent().unwrap_or_else(|| Path::new(".")), false)?;
    let leaf = c_name(path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "verified source has no filename",
        )
    })?)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "verified source is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
struct TempInstall {
    directory: File,
    name: CString,
    armed: bool,
}

#[cfg(unix)]
impl Drop for TempInstall {
    fn drop(&mut self) {
        if self.armed {
            use std::os::fd::AsRawFd as _;
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(unix)]
fn install_verified(source: &Path, install_dir: &Path) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let context = |step: &'static str, error: std::io::Error| {
        std::io::Error::new(error.kind(), format!("{step}: {error}"))
    };
    // The download directory is a private mktemp root. Canonicalize only this
    // source so macOS's trusted `/var` -> `/private/var` alias does not look
    // like a hostile symlink component; destination paths are never
    // canonicalized or reopened.
    let canonical_source = source
        .canonicalize()
        .map_err(|error| context("resolve verified source", error))?;
    let mut source = open_regular_path(&canonical_source)
        .map_err(|error| context("open verified source", error))?;
    let directory = open_directory_path(install_dir, true)
        .map_err(|error| context("open install directory", error))?;
    #[cfg(test)]
    AFTER_INSTALL_DIRECTORY_OPEN.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let destination = CString::new("dbmd").unwrap();

    let mut existing: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            destination.as_ptr(),
            &mut existing,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
        && (existing.st_mode & libc::S_IFMT) != libc::S_IFREG
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to replace a non-regular dbmd destination",
        ));
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let (temp_name, fd) = (0_u32..128)
        .find_map(|attempt| {
            let name = CString::new(format!(
                ".dbmd-install-{}-{nonce}-{attempt}",
                std::process::id()
            ))
            .ok()?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o700,
                )
            };
            (fd >= 0).then_some((name, fd))
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a private install stage",
            )
        })?;
    let mut guard = TempInstall {
        directory: directory
            .try_clone()
            .map_err(|error| context("retain install directory", error))?,
        name: temp_name,
        armed: true,
    };
    let mut staged = unsafe { File::from_raw_fd(fd) };
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| context("read verified source", error))?;
        if read == 0 {
            break;
        }
        staged
            .write_all(&buffer[..read])
            .map_err(|error| context("write private install stage", error))?;
    }
    if unsafe { libc::fchmod(staged.as_raw_fd(), 0o755) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    staged
        .sync_all()
        .map_err(|error| context("sync private install stage", error))?;
    drop(staged);

    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            guard.name.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(context(
            "atomically replace dbmd",
            std::io::Error::last_os_error(),
        ));
    }
    guard.armed = false;
    directory
        .sync_all()
        .map_err(|error| context("sync install directory", error))
}

#[cfg(test)]
thread_local! {
    static AFTER_INSTALL_DIRECTORY_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_after_install_directory_open(hook: impl FnOnce() + 'static) {
    AFTER_INSTALL_DIRECTORY_OPEN.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn verified_source(root: &Path) -> std::path::PathBuf {
        let source = root.join("verified-dbmd");
        std::fs::write(&source, b"verified binary bytes").unwrap();
        source
    }

    fn assert_ancestor_swap_is_held(precreate_destination: bool) {
        let sandbox = tempfile::tempdir().unwrap();
        let base = sandbox.path().canonicalize().unwrap();
        let root = base.join("install-root");
        let destination = root.join("bin");
        if precreate_destination {
            std::fs::create_dir_all(&destination).unwrap();
        }
        let source = verified_source(&base);
        let detached = base.join("detached-root");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("bin")).unwrap();
        std::fs::write(outside.join("bin/dbmd"), b"outside sentinel").unwrap();

        let root_for_hook = root.clone();
        let detached_for_hook = detached.clone();
        let outside_for_hook = outside.clone();
        set_after_install_directory_open(move || {
            std::fs::rename(&root_for_hook, &detached_for_hook).unwrap();
            symlink(&outside_for_hook, &root_for_hook).unwrap();
        });
        install_verified(&source, &destination).unwrap();

        assert_eq!(
            std::fs::read(detached.join("bin/dbmd")).unwrap(),
            b"verified binary bytes"
        );
        assert_eq!(
            std::fs::read(outside.join("bin/dbmd")).unwrap(),
            b"outside sentinel",
            "a swapped destination ancestor must never redirect the install"
        );
        assert!(
            std::fs::read_dir(detached.join("bin"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dbmd-install-")),
            "private stage must be removed through the held dirfd"
        );
    }

    #[test]
    fn existing_destination_root_swap_cannot_redirect_install() {
        assert_ancestor_swap_is_held(true);
    }

    #[test]
    fn newly_created_destination_root_swap_cannot_redirect_install() {
        assert_ancestor_swap_is_held(false);
    }

    #[test]
    fn symlink_destination_leaf_is_refused_without_touching_target() {
        let sandbox = tempfile::tempdir().unwrap();
        let base = sandbox.path().canonicalize().unwrap();
        let destination = base.join("bin");
        std::fs::create_dir_all(&destination).unwrap();
        let source = verified_source(&base);
        let outside = base.join("outside-dbmd");
        std::fs::write(&outside, b"outside sentinel").unwrap();
        symlink(&outside, destination.join("dbmd")).unwrap();

        install_verified(&source, &destination).unwrap_err();
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside sentinel");
        assert!(destination.join("dbmd").is_symlink());
    }

    #[test]
    fn directory_destination_leaf_is_refused_without_deletion() {
        let sandbox = tempfile::tempdir().unwrap();
        let base = sandbox.path().canonicalize().unwrap();
        let destination = base.join("bin");
        std::fs::create_dir_all(destination.join("dbmd")).unwrap();
        std::fs::write(destination.join("dbmd/sentinel"), b"keep").unwrap();
        let source = verified_source(&base);

        install_verified(&source, &destination).unwrap_err();
        assert_eq!(
            std::fs::read(destination.join("dbmd/sentinel")).unwrap(),
            b"keep"
        );
    }
}
