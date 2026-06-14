//! `dbmd extract <file>` — document text extraction.
//!
//! Thin wrapper: parse [`ExtractArgs`], call [`dbmd_core::extract::extract`]
//! (which owns all format detection + adapter logic), and format the result —
//! plain text to stdout (or `--out <path>`), or a `{text, metadata}` object
//! under `--json`. No extraction logic lives here; this file only moves bytes
//! between `dbmd-core` and the chosen output sink and maps the typed
//! [`dbmd_core::extract::ExtractError`] onto the CLI's stable exit codes.

use std::path::Path;

use dbmd_core::extract::{self, ExtractError};

use crate::cli::ExtractArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd extract`.
pub fn run(ctx: &Context, args: &ExtractArgs) -> CliResult {
    let path = Path::new(&args.file);

    // All the real work — extension dispatch, PDF/docx/xlsx/epub/html adapters,
    // text normalization, metadata — happens in dbmd-core.
    let extracted = extract::extract(path).map_err(map_extract_error)?;

    if ctx.json {
        // `{text, metadata}` exactly as `Extracted` serializes. Pretty-printed
        // for human-inspectable piping; one object, newline-terminated.
        let json = serde_json::to_string_pretty(&extracted)
            .map_err(|e| CliError::runtime(format!("failed to encode JSON: {e}")))?;
        emit(&args.out, &json, true)
    } else {
        // Plain mode: just the text. Metadata is discarded (it's a `--json`
        // affordance). `extracted.text` already ends in a single newline (or is
        // empty for a no-text-layer document), so don't add another.
        emit(&args.out, &extracted.text, false)
    }
}

/// Write `content` to `--out <path>` when given, else to stdout.
///
/// `add_trailing_newline` appends a `\n` only for JSON output (so a redirected
/// `--json` file ends cleanly); plain text is emitted verbatim because the
/// extractor already normalizes its trailing newline.
fn emit(out: &Option<String>, content: &str, add_trailing_newline: bool) -> CliResult {
    match out {
        Some(path) => {
            // Refuse to write through a symlink anywhere on the destination path
            // (the leaf OR a parent the user named). `std::fs::write` follows every
            // symlink it traverses and overwrites the resolved *target*, which can
            // live anywhere on disk — an attacker who plants a symlink at an
            // innocent-looking `--out` path (or a symlinked parent directory) turns
            // extraction into an arbitrary-file overwrite with attacker-
            // influenceable content. See [`refuse_symlink_dest`] for the lstat
            // walk. A normal file at `--out` under a real directory is overwritten
            // as before; a missing path is created.
            refuse_symlink_dest(path)?;

            let mut body = content.to_string();
            if add_trailing_newline && !body.ends_with('\n') {
                body.push('\n');
            }
            std::fs::write(path, body).map_err(|e| {
                CliError::new(
                    ExitCode::Runtime,
                    "IO_ERROR",
                    format!("failed to write {path}: {e}"),
                )
            })?;
            Ok(())
        }
        None => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let res = if add_trailing_newline {
                writeln!(lock, "{content}")
            } else {
                write!(lock, "{content}")
            };
            // A broken pipe (downstream `head`/`grep` closed the read end) is a
            // benign truncation, not a failure: exit 0 with no error envelope, so
            // an agent branching on the exit code of `dbmd extract big.pdf | head`
            // doesn't see a spurious IO_ERROR. Every other write failure is a real
            // runtime error.
            match res {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                Err(e) => Err(CliError::new(
                    ExitCode::Runtime,
                    "IO_ERROR",
                    format!("write failed: {e}"),
                )),
            }
        }
    }
}

/// Refuse a `--out` destination that is reached through a symlink — either the
/// final component OR any parent directory the user named on its path.
///
/// `std::fs::write` opens the path with `O_CREAT|O_TRUNC` and *follows* every
/// symlink it traverses, so a symlinked destination silently overwrites the
/// resolved target — an arbitrary-file-overwrite primitive when the link points
/// outside the intended directory. Checking only the leaf with a single `lstat`
/// closes `--out link -> /etc/secret` but NOT `--out linkdir/victim.txt`, where a
/// *parent* (`linkdir -> ../external`) does the escaping and the leaf is a plain
/// file.
///
/// A naive per-component `lstat` walk over the *full* ancestor paths does not
/// close the parent case either: `lstat`-ing `store/linkdir/sub` still *follows*
/// the earlier `linkdir` symlink to reach `sub`, so once a real directory exists
/// below the link inside its target, the walk anchors on that real directory and
/// never inspects the symlink above it — the write escapes. This is the exact
/// hole the prior leaf-then-ancestors guard left open, the same class the sibling
/// `dbmd write` path fixed via `dbmd_core::store::ensure_path_within_store` by
/// resolving the whole parent chain.
///
/// `extract` opens no store, so there is no root to contain against; the safe
/// equivalent is to refuse a symlinked component *the user's `--out` introduced*.
/// Implementation (an `openat(…, O_NOFOLLOW)` descent emulated with `std`):
/// 1. `lstat` the leaf — a symlinked final component is refused outright (the
///    most precise diagnostic, and the only thing the *parent* walk does not
///    cover since the leaf is the file, not a directory on the path).
/// 2. Descend the parent's deepest-existing prefix one component at a time
///    against a *resolved* real prefix (so an earlier symlink can never be
///    followed silently to misjudge a later component), and refuse the first
///    symlink that appears once we are standing on real ground.
///
/// "Real ground" handles the system-symlink exemption precisely: an **absolute**
/// path starts at the filesystem root, whose *leading* run of symlinks is the
/// trusted mount prefix (`/var` → `/private/var`, `/tmp` → `/private/tmp`) and is
/// followed transparently until the first real directory; any symlink *after*
/// that is part of what the user named and is refused. A **relative** path is
/// resolved against the (trusted) current directory, so its first named component
/// is already on real ground and a symlink there is refused. Because the
/// inspection is per-component against a resolved prefix, a leaf or parent whose
/// name merely *coincides* with its symlink target's name is still caught, and a
/// chain of symlinks is caught at the first link.
fn refuse_symlink_dest(path: &str) -> Result<(), CliError> {
    use std::path::Component;

    let refuse = |p: &Path| {
        CliError::new(
            ExitCode::Runtime,
            "OUT_IS_SYMLINK",
            format!(
                "refusing to write {path}: the path is reached through a symlink ({})",
                p.display()
            ),
        )
        .with_hint(
            "extract --out will not follow a symlink (it could overwrite a file elsewhere); \
             remove the symlink or choose a destination with no symlinked component",
        )
    };
    let inspect_io_err = |p: &Path, e: std::io::Error| {
        CliError::new(
            ExitCode::Runtime,
            "IO_ERROR",
            format!("failed to inspect {}: {e}", p.display()),
        )
    };

    // The leaf first: a symlinked final component is the headline overwrite
    // vector. `symlink_metadata` is an `lstat` — it describes the component
    // without dereferencing it, so a leaf symlink is caught here precisely.
    let leaf = Path::new(path);
    match std::fs::symlink_metadata(leaf) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(refuse(leaf)),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(inspect_io_err(leaf, e)),
    }

    // The parent directory the user lexically named. A leaf with no parent
    // (a bare filename like `out.txt`, or the filesystem root) writes into the
    // current directory / its own location: there is no parent chain to escape
    // through, so the leaf check above is sufficient.
    let parent = match leaf.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Ok(()),
    };

    // Shrink the parent to its deepest already-existing prefix: components below
    // that do not exist yet, so the write creates them under a real, already-
    // verified anchor — none of them can be a pre-planted symlink.
    let mut existing = parent.to_path_buf();
    let exists = |p: &Path| match std::fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    };
    loop {
        match exists(&existing) {
            Ok(true) => break,
            Ok(false) => {
                // Peel one not-yet-existing component. If the whole parent chain
                // is new (nothing on disk to follow), no symlink can redirect the
                // write — accept.
                if !existing.pop() || existing.as_os_str().is_empty() {
                    return Ok(());
                }
            }
            Err(e) => return Err(inspect_io_err(&existing, e)),
        }
    }

    // Descend the existing prefix component-by-component against a *resolved* real
    // path, so each `lstat` probes exactly one level deep and never follows an
    // earlier (possibly attacker-planted) symlink to reach a later component.
    let mut real = std::path::PathBuf::new();
    let mut lexical = std::path::PathBuf::new();
    // A relative `--out` resolves against the trusted current directory: seed the
    // resolved prefix with it and treat the first named component as already on
    // real ground (no leading-mount-prefix exemption). An absolute `--out` starts
    // at the filesystem root, whose leading symlink run IS the trusted mount
    // prefix and is followed until the first real directory.
    let mut on_real_ground = false;
    if existing.is_relative() {
        real = match std::env::current_dir() {
            Ok(cwd) => match cwd.canonicalize() {
                Ok(c) => c,
                Err(e) => return Err(inspect_io_err(&cwd, e)),
            },
            Err(e) => return Err(inspect_io_err(Path::new("."), e)),
        };
        on_real_ground = true;
    }

    for comp in existing.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                real.push(comp.as_os_str());
                lexical.push(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                real.pop();
                lexical.pop();
            }
            Component::Normal(name) => {
                lexical.push(name);
                let probe = real.join(name);
                match std::fs::symlink_metadata(&probe) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        if on_real_ground {
                            // A symlink in the portion of the path the user named:
                            // following it would redirect the write outside the
                            // lexically-named directory. Refuse, naming the lexical
                            // location of the link.
                            return Err(refuse(&lexical));
                        }
                        // Still in the absolute mount prefix (e.g. `/var`): follow
                        // it transparently and keep the resolved real path in sync.
                        match probe.canonicalize() {
                            Ok(c) => real = c,
                            Err(e) => return Err(inspect_io_err(&probe, e)),
                        }
                    }
                    Ok(meta) => {
                        // A real component: the moment it is a directory we are on
                        // real ground and any deeper symlink is user-introduced.
                        if meta.is_dir() {
                            on_real_ground = true;
                        }
                        real = probe;
                    }
                    // The prefix was confirmed to exist above; a component going
                    // missing mid-walk is a TOCTOU race — treat as nothing left to
                    // follow rather than refuse a legitimate write.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(inspect_io_err(&probe, e)),
                }
            }
        }
    }
    Ok(())
}

/// Map a `dbmd_core::extract::ExtractError` onto a [`CliError`] with the right
/// exit code + stable machine code. Each variant carries a remediation hint
/// where one is actionable.
fn map_extract_error(err: ExtractError) -> CliError {
    match &err {
        // Bad/unknown extension — a usage-shaped problem, but `clap` owns exit
        // code 2, so a runtime failure with a distinct machine code is the
        // contract here (the file arg parsed fine; its *type* is the issue).
        ExtractError::UnsupportedFormat(_) => CliError::new(
            ExitCode::Runtime,
            err.code(),
            err.to_string(),
        )
        .with_hint(
            "supported document types: .pdf, .docx, .xlsx/.xlsm/.xlsb/.ods, .epub, .html/.htm/.xhtml (detected by extension)",
        ),
        // Encrypted/locked document — clean refusal, not a crash.
        ExtractError::Encrypted(_) => CliError::new(ExitCode::Runtime, err.code(), err.to_string())
            .with_hint("the document is password-protected; dbmd extract cannot open it"),
        // Corrupt/invalid document for its declared format.
        ExtractError::Parse { .. } => CliError::new(ExitCode::Runtime, err.code(), err.to_string()),
        // Missing/unreadable file.
        ExtractError::Io(_) => CliError::new(ExitCode::Runtime, "IO_ERROR", err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A symlinked `--out` is refused before any write, so the symlink's target
    /// (a file living outside the intended directory) is never overwritten. This
    /// is the regression guard for the arbitrary-file-overwrite primitive.
    #[test]
    #[cfg(unix)]
    fn out_symlink_is_refused_and_target_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim.conf");
        std::fs::write(&victim, "SENSITIVE-ORIGINAL\n").unwrap();

        let link = tmp.path().join("innocent-output.txt");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        let out = Some(link.to_string_lossy().into_owned());
        let err = emit(&out, "POISONED-BYTES-FROM-DOCUMENT", false)
            .expect_err("a symlinked --out must be refused");
        assert_eq!(err.code, "OUT_IS_SYMLINK", "got {err:?}");

        // The link's target keeps its original contents; the write never followed it.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "SENSITIVE-ORIGINAL\n",
            "the symlink target must not be overwritten",
        );
        // The in-store path is still a symlink (the write did not replace it).
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the --out path must remain a symlink",
        );
    }

    /// A `--out` whose PARENT directory is a symlink (the leaf itself a plain
    /// file in the link's target) is refused — closing the parent-directory
    /// arbitrary-overwrite vector the leaf-only `lstat` left open. The external
    /// target keeps its contents.
    #[test]
    #[cfg(unix)]
    fn out_through_symlinked_parent_is_refused_and_target_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        // The "store" the user thinks they're writing into.
        let store = tmp.path().join("store");
        std::fs::create_dir(&store).unwrap();
        // An external directory with a pre-existing victim file.
        let external = tmp.path().join("external");
        std::fs::create_dir(&external).unwrap();
        let victim = external.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL_SECRET\n").unwrap();

        // Attacker plants a symlinked directory inside the store pointing out.
        let linkdir = store.join("linkdir");
        std::os::unix::fs::symlink(&external, &linkdir).unwrap();

        // `--out store/linkdir/victim.txt`: the leaf is a regular file, but its
        // parent `linkdir` is a symlink that escapes the store.
        let out_path = linkdir.join("victim.txt");
        let out = Some(out_path.to_string_lossy().into_owned());
        let err = emit(&out, "POISONED_BY_EXTRACT", false)
            .expect_err("a --out reached through a symlinked parent must be refused");
        assert_eq!(err.code, "OUT_IS_SYMLINK", "got {err:?}");

        // The external target keeps its original contents.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL_SECRET\n",
            "the symlinked-parent target must not be overwritten",
        );
    }

    /// A `--out` whose symlinked parent is NOT the immediate parent — there is a
    /// real subdirectory sitting *below* the link inside its target
    /// (`store/linkdir/sub/victim.txt`, `linkdir -> external`, `external/sub` a
    /// real directory). The earlier guard `lstat`'d each full ancestor path:
    /// `lstat(store/linkdir/sub)` *follows* `linkdir`, sees a real directory,
    /// anchors there, and never inspects the `linkdir` symlink above it — so the
    /// write escaped to `external/sub/victim.txt` outside the lexical `store/`
    /// directory. This is the regression guard for that deep-parent escape: the
    /// write must be refused and the external target left untouched.
    #[test]
    #[cfg(unix)]
    fn out_through_symlinked_parent_with_real_subdir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        std::fs::create_dir(&store).unwrap();
        // External target with a REAL subdirectory below where the link lands, and
        // a pre-existing victim file inside it.
        let external = tmp.path().join("external");
        let external_sub = external.join("sub");
        std::fs::create_dir_all(&external_sub).unwrap();
        let victim = external_sub.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL_SECRET\n").unwrap();

        // Attacker plants a symlinked directory inside the store pointing out.
        let linkdir = store.join("linkdir");
        std::os::unix::fs::symlink(&external, &linkdir).unwrap();

        // `--out store/linkdir/sub/victim.txt`: leaf is a regular file, its parent
        // `sub` is a REAL directory (reached through `linkdir`), and the escaping
        // symlink `linkdir` is two levels above the leaf.
        let out_path = linkdir.join("sub").join("victim.txt");
        let out = Some(out_path.to_string_lossy().into_owned());
        let err = emit(&out, "POISONED_BY_EXTRACT", false).expect_err(
            "a --out reached through a symlinked parent (with a real subdir below \
             the link) must be refused",
        );
        assert_eq!(err.code, "OUT_IS_SYMLINK", "got {err:?}");

        // The external target keeps its original contents — the write never
        // followed `linkdir` out of the lexical store directory.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "ORIGINAL_SECRET\n",
            "the deep symlinked-parent target must not be overwritten",
        );
    }

    /// Over-correction guard: a `--out` into a *real* nested subdirectory (no
    /// symlink anywhere on the path, only an absolute temp prefix whose high
    /// ancestors `/var`/`/tmp` may be system symlinks) must still be written. The
    /// per-component descent must not mistake a deep real chain — or the trusted
    /// absolute mount prefix — for an escape.
    #[test]
    fn out_into_real_nested_subdir_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let dest = nested.join("out.txt");
        let out = Some(dest.to_string_lossy().into_owned());

        emit(&out, "deep but real", false).expect("a real deep-nested --out must succeed");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "deep but real");
    }

    /// A normal (non-symlink) `--out` destination is written as before — the
    /// symlink guard does not regress the ordinary redirect-to-file path. The
    /// destination is an ABSOLUTE temp path whose high ancestors (`/var`, `/tmp`)
    /// may be system symlinks; those must NOT trip the guard.
    #[test]
    fn out_regular_file_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        let out = Some(dest.to_string_lossy().into_owned());

        emit(&out, "hello extracted text", false).expect("a regular --out must succeed");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "hello extracted text",
        );

        // Overwriting an existing regular file is still allowed.
        emit(&out, "second write", false).expect("overwriting a regular file is allowed");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "second write");
    }
}
